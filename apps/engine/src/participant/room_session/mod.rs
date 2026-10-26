//! Room session control plane.
//!
//! One worker connects room selection, discovery, profiles, and the contact
//! service. The caller enqueues commands and reads ordered outputs. This module
//! does not open media sockets, call `Peer`, or draw a page.

mod apply;
mod types;
mod worker;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub use types::{
    EntryFailureKind, MemberView, OutputBody, Phase, ProjectedEndpoint, RuleFileStatus,
    RuleFilesState, RuleSaveState, SessionCommand, SessionConfig, SessionError, SessionOutput,
    SessionSnapshot,
};

use types::SessionCommand as Command;

const COMMANDS: usize = 16;
const OUTPUTS: usize = 64;

pub(crate) struct Shared {
    snapshot: Mutex<SessionSnapshot>,
    outputs: Mutex<OutputQueue>,
    generation: Mutex<u64>,
    stop_requested: AtomicBool,
    pending_leave: Mutex<Option<u64>>,
}

pub(crate) struct QueuedCommand {
    generation: u64,
    command: Command,
}

struct OutputQueue {
    items: std::collections::VecDeque<SessionOutput>,
    loss_revision: u64,
}

impl Shared {
    fn new() -> Self {
        Self {
            snapshot: Mutex::new(SessionSnapshot::idle()),
            outputs: Mutex::new(OutputQueue {
                items: std::collections::VecDeque::new(),
                loss_revision: 0,
            }),
            generation: Mutex::new(0),
            stop_requested: AtomicBool::new(false),
            pending_leave: Mutex::new(None),
        }
    }

    fn lock_generation(&self) -> std::sync::MutexGuard<'_, u64> {
        self.generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn generation(&self) -> u64 {
        *self.lock_generation()
    }

    pub(crate) fn is_current(&self, generation: u64) -> bool {
        !self.stop_requested.load(Ordering::Acquire)
            && apply::same_generation(generation, self.generation())
    }

    fn lock_snapshot(&self) -> std::sync::MutexGuard<'_, SessionSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_outputs(&self) -> std::sync::MutexGuard<'_, OutputQueue> {
        self.outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn push_output(&self, output: SessionOutput) {
        let mut queue = self.lock_outputs();
        if queue.items.len() >= OUTPUTS {
            if let Some(index) = queue.items.iter().position(|item| !item.is_terminal()) {
                queue.items.remove(index);
            } else {
                queue.loss_revision = queue.loss_revision.saturating_add(1);
                queue.items.pop_front();
            }
        }
        queue.items.push_back(output);
    }

    pub(crate) fn overflow(&self) -> bool {
        self.loss_revision() != 0
    }

    pub(crate) fn loss_revision(&self) -> u64 {
        self.lock_outputs().loss_revision
    }

    pub(crate) fn set_snapshot(&self, snapshot: SessionSnapshot) {
        *self.lock_snapshot() = snapshot;
    }

    fn snapshot(&self) -> SessionSnapshot {
        self.lock_snapshot().clone()
    }

    fn pop_output(&self) -> Option<SessionOutput> {
        self.lock_outputs().items.pop_front()
    }
}

/// Handle for one local room session.
///
/// `submit` only enqueues. The worker performs contact, discovery, and file
/// work. `stop` is idempotent and does not join the worker while holding the
/// snapshot lock.
pub struct RoomSession {
    commands: SyncSender<QueuedCommand>,
    shared: Arc<Shared>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl RoomSession {
    pub fn start(config: SessionConfig) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(COMMANDS);
        let shared = Arc::new(Shared::new());
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("rlb-room-session".to_owned())
            .spawn(move || worker::run(config, receiver, worker_shared))
            .map_err(|error| format!("The room session worker did not start: {error}"))?;
        Ok(Self {
            commands: sender,
            shared,
            worker: Arc::new(Mutex::new(Some(worker))),
        })
    }

    pub fn submit(&self, command: SessionCommand) -> Result<(), SessionError> {
        self.submit_generation(command).map(|_| ())
    }

    /// Return the accepted session generation for asynchronous media admission.
    /// Rule edits and reloads retain the current generation.
    pub fn submit_generation(&self, command: SessionCommand) -> Result<u64, SessionError> {
        if matches!(command, SessionCommand::Stop) {
            self.stop()?;
            return Ok(self.shared.generation());
        }
        validate_command(&command).map_err(SessionError::Invalid)?;
        let mut latest = self.shared.lock_generation();
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(SessionError::Stopped);
        }
        let generation = if supersedes(&command) {
            latest.checked_add(1).ok_or_else(|| {
                SessionError::Invalid("The room session generation is exhausted.".to_owned())
            })?
        } else {
            *latest
        };
        match self.commands.try_send(QueuedCommand {
            generation,
            command,
        }) {
            Ok(()) => {
                *latest = generation;
                Ok(generation)
            }
            Err(TrySendError::Full(_)) => Err(SessionError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(SessionError::Stopped),
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.shared.snapshot()
    }

    pub(crate) fn update_card_target(
        &self,
        path: std::path::PathBuf,
        target: crate::participant::rooms::RoomTarget,
        expected_revision: String,
    ) -> Result<crate::participant::management::CardDocument, String> {
        let (reply, response) = mpsc::sync_channel(1);
        self.submit(SessionCommand::UpdateCardTarget { path, target, expected_revision, reply })
            .map_err(|error| error.to_string())?;
        response.recv().map_err(|_| "The room-card operation was superseded or the room owner stopped.".to_owned())?
    }

    pub(crate) fn snapshot_settings(&self) -> Result<crate::participant::management::ActiveRoomSettings, String> {
        let (reply, response) = mpsc::sync_channel(1);
        self.submit(SessionCommand::SnapshotSettings { reply }).map_err(|error| error.to_string())?;
        response.recv().map_err(|_| "The settings export was superseded or the room owner stopped.".to_owned())?
    }

    /// Retire a room without terminating its local preferences owner. This
    /// close path remains available when ordinary commands fill the queue.
    pub(crate) fn force_leave(&self) -> Result<u64, SessionError> {
        let mut latest = self.shared.lock_generation();
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(SessionError::Stopped);
        }
        let generation = latest.checked_add(1).ok_or_else(|| {
            SessionError::Invalid("The room session generation is exhausted.".into())
        })?;
        *self
            .shared
            .pending_leave
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(generation);
        *latest = generation;
        Ok(generation)
    }

    pub fn poll_output(&self) -> Option<SessionOutput> {
        self.shared.pop_output()
    }

    /// Ask the worker to leave, stop a hosted service, and exit.
    pub fn stop(&self) -> Result<(), SessionError> {
        if !self.shared.stop_requested.swap(true, Ordering::AcqRel) {
            let mut latest = self.shared.lock_generation();
            *latest = latest.saturating_add(1);
        }
        let _ = self.commands.try_send(QueuedCommand {
            generation: self.shared.generation(),
            command: SessionCommand::Stop,
        });
        Ok(())
    }
}

fn validate_command(command: &SessionCommand) -> Result<(), String> {
    match command {
        SessionCommand::EditRoomRule { rule } => rule.validate(),
        SessionCommand::EditGlobalRule { rule } => rule.validate(),
        SessionCommand::UpdateCardTarget { target, .. } => target.validate(),
        SessionCommand::SetDisplayName { name } => {
            super::config::validate_name("display name", name)
        }
        SessionCommand::ReplaceGlobalRules { rules } => rules.validate(),
        SessionCommand::SetGlobalDefaults { send, listen } => super::rooms::Rule {
            outputs: None,
            r#match: super::rooms::RuleMatch::All,
            source_kind: None,
            send: *send,
            listen: *listen,
        }
        .validate(),
        SessionCommand::SetRoomSourceMix {
            condition,
            playback,
            ..
        } => {
            condition.validate()?;
            playback.validate("room source mix")
        }
        SessionCommand::JoinUnknown { target, rules, .. } => {
            target.validate()?;
            rules.iter().try_for_each(super::rooms::Rule::validate)
        }
        _ => Ok(()),
    }
}

fn supersedes(command: &SessionCommand) -> bool {
    matches!(
        command,
        SessionCommand::Create { .. }
            | SessionCommand::JoinSaved { .. }
            | SessionCommand::JoinUnknown { .. }
            | SessionCommand::Leave
            | SessionCommand::Cancel
            | SessionCommand::Stop
    )
}

impl Drop for RoomSession {
    fn drop(&mut self) {
        let _ = self.stop();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

/// Block until `pred` holds or `timeout` elapses. Returns the latest snapshot.
pub fn wait_until(
    session: &RoomSession,
    timeout: Duration,
    mut pred: impl FnMut(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    let start = std::time::Instant::now();
    loop {
        let snapshot = session.snapshot();
        if pred(&snapshot) || start.elapsed() >= timeout {
            return snapshot;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    use rlb_contact::{ContactClient, ContactConfig, ContactService, JoinParams, MediaSocket};
    use rlb_model::{MemberId, RunId};

    use super::super::rooms::{
        GlobalRules, JoinMode, RoomProfile, RoomTarget, SCHEMA, create_global_rules_file,
        create_room_profile_file,
    };
    use super::*;

    static NET: Mutex<()> = Mutex::new(());

    #[test]
    #[cfg(windows)]
    fn priority_leave_keeps_the_rule_owner_usable_with_a_full_queue() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_dir();
        let global_path = dir.join("global.json");
        create_global_rules_file(&global_path, &GlobalRules::baseline()).unwrap();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&global_path)
            .unwrap();
        let (member, run) = ids(8, 8);
        let config = SessionConfig::from_bound(
            member,
            run,
            "Local",
            "0.0.0.0:5004".parse().unwrap(),
            global_path.clone(),
        )
        .unwrap()
        .with_discovery(crate::participant::discovery::DiscoveryConfig {
            enable_interfaces: vec![crate::participant::discovery::InterfaceSelector::Name(
                String::new(),
            )],
            disable_interfaces: Vec::new(),
        });
        let shared = Arc::new(Shared::new());
        let (sender, receiver) = mpsc::sync_channel(1);
        let session = RoomSession {
            commands: sender,
            shared: Arc::clone(&shared),
            worker: Arc::new(Mutex::new(None)),
        };
        session
            .submit(SessionCommand::SetGlobalDefaults {
                send: Some(false),
                listen: None,
            })
            .unwrap();
        assert_eq!(session.force_leave().unwrap(), 1);
        assert!(!shared.stop_requested.load(Ordering::Acquire));
        let worker_shared = Arc::clone(&shared);
        *session.worker.lock().unwrap() = Some(thread::spawn(move || {
            worker::run(config, receiver, worker_shared)
        }));
        let state = wait_until(&session, Duration::from_secs(2), |state| {
            state.rule_files.global.outcome.unsaved()
        });
        assert_eq!(state.phase, Phase::Idle);
        assert_eq!(state.global_rules.unwrap().rules[0].send, Some(false));
        drop(blocker);
        session
            .submit(SessionCommand::SetGlobalDefaults {
                send: None,
                listen: Some(true),
            })
            .unwrap();
        let state = wait_until(&session, Duration::from_secs(2), |state| {
            state.rule_files.global.outcome == RuleSaveState::Saved
        });
        let rules = state.global_rules.unwrap();
        assert_eq!(
            (rules.rules[0].send, rules.rules[0].listen),
            (Some(false), Some(true))
        );
        drop(session);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_output_loss_is_observable_in_each_separate_episode() {
        let shared = Shared::new();
        let mut previous = 0;
        for _ in 0..2 {
            for _ in 0..=OUTPUTS {
                shared.push_output(SessionOutput {
                    generation: 1,
                    body: OutputBody::LocalSessionEnded,
                });
            }
            assert!(shared.loss_revision() > previous);
            previous = shared.loss_revision();
            while shared.pop_output().is_some() {}
        }
    }

    #[test]
    fn accepted_generation_changes_only_for_session_boundaries() {
        use super::super::rooms::{Rule, RuleMatch};

        let (commands, received) = mpsc::sync_channel(COMMANDS);
        let session = RoomSession {
            commands,
            shared: Arc::new(Shared::new()),
            worker: Arc::new(Mutex::new(None)),
        };
        assert_eq!(session.submit_generation(SessionCommand::Leave).unwrap(), 1);
        assert_eq!(received.recv().unwrap().generation, 1);
        assert_eq!(
            session
                .submit_generation(SessionCommand::EditRoomRule {
                    rule: Rule {
                        outputs: None,
                        r#match: RuleMatch::All,
                        source_kind: None,
                        send: Some(false),
                        listen: None
                    },
                })
                .unwrap(),
            1
        );
        assert_eq!(received.recv().unwrap().generation, 1);
        assert_eq!(
            session.submit_generation(SessionCommand::Reload).unwrap(),
            1
        );
        assert_eq!(received.recv().unwrap().generation, 1);
        assert!(matches!(
            session.submit_generation(SessionCommand::EditRoomRule {
                rule: Rule {
                    outputs: None,
                    r#match: RuleMatch::All,
                    source_kind: None,
                    send: None,
                    listen: None
                },
            }),
            Err(SessionError::Invalid(_))
        ));
        assert!(received.try_recv().is_err());
        assert_eq!(session.shared.generation(), 1);
    }

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rlb-c16-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn ids(member: u8, run: u8) -> (MemberId, RunId) {
        let mut member_bytes = [0_u8; 16];
        member_bytes[15] = member;
        let mut run_bytes = [0_u8; 16];
        run_bytes[0] = run;
        (
            MemberId::from_bytes(member_bytes).unwrap(),
            RunId::from_bytes(run_bytes).unwrap(),
        )
    }

    fn media(host: u8, port: u16) -> MediaSocket {
        MediaSocket::explicit(
            std::net::IpAddr::V4(Ipv4Addr::new(198, 51, 100, host)),
            port,
            0,
        )
        .unwrap()
    }

    fn write_card(
        dir: &Path,
        name: &str,
        address: Option<std::net::IpAddr>,
        port: Option<u16>,
        password: &str,
    ) -> PathBuf {
        let global = dir.join("global.json");
        if !global.exists() {
            create_global_rules_file(&global, &GlobalRules::baseline()).unwrap();
        }
        let room = dir.join(format!("{name}.json"));
        let profile = RoomProfile {
            schema: SCHEMA,
            room: RoomTarget {
                name: name.to_owned(),
                address,
                port,
                password: password.to_owned(),
                strict_auto_join: false,
            },
            rules: Vec::new(),
            playback: Vec::new(),
        };
        create_room_profile_file(&room, &profile).unwrap();
        room
    }

    fn open_session(dir: &Path, number: u8, endpoint: MediaSocket) -> RoomSession {
        let (member_id, run_id) = ids(number, number);
        let config = SessionConfig::try_new(
            member_id,
            run_id,
            format!("成员{number}"),
            endpoint,
            dir.join("global.json"),
        )
        .unwrap()
        .with_io_timeout(Duration::from_secs(1))
        .unwrap()
        .with_recovery_interval(Duration::from_millis(200))
        .unwrap();
        RoomSession::start(config).unwrap()
    }

    #[test]
    fn global_rules_are_loaded_before_entering_any_room() {
        let dir = temp_dir();
        create_global_rules_file(&dir.join("global.json"), &GlobalRules::baseline()).unwrap();
        let session = open_session(&dir, 9, media(29, 5010));
        let state = wait_until(&session, Duration::from_secs(2), |state| {
            state.global_rules.is_some()
        });
        assert_eq!(state.global_rules, Some(GlobalRules::baseline()));
        assert_eq!(state.phase, Phase::Idle);
        assert!(state.profile_path.is_none());
        assert!(state.members.is_empty());
        drop(session);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_leave_releases_the_bound_service_and_advertisement() {
        let _guard = NET.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_dir();
        let room = write_card(&dir, "Headphones", None, None, "");
        let session = open_session(&dir, 1, media(20, 5004));
        session
            .submit(SessionCommand::Create {
                profile_path: room,
                bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            })
            .unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(8), |state| {
            matches!(state.phase, Phase::Joined | Phase::Failed)
        });
        assert_eq!(
            snapshot.phase,
            Phase::Joined,
            "{}",
            snapshot.failure.unwrap_or_default()
        );
        let bound = snapshot.local_service.expect("hosted endpoint");
        assert!(snapshot.local_advertisement.is_some());
        assert!(snapshot.members.iter().any(|member| {
            matches!(
                member.endpoint,
                ProjectedEndpoint::Ready(address)
                    if address == SocketAddr::from((Ipv4Addr::new(198, 51, 100, 20), 5004))
            )
        }));
        assert!(snapshot.saved_address.is_none());
        session.submit(SessionCommand::Leave).unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(4), |state| {
            state.phase == Phase::Idle
        });
        assert_eq!(snapshot.phase, Phase::Idle);
        assert!(snapshot.local_advertisement.is_none());
        assert!(
            session
                .poll_output()
                .is_some_and(|output| matches!(output.body, OutputBody::LocalSessionEnded))
                || drain_has_local_end(&session)
        );
        let restarted =
            ContactService::start(ContactConfig::try_new(bound, "Headphones", "").expect("config"))
                .expect("the hosted port is free after leave");
        assert_eq!(restarted.local_addr(), bound);
        restarted.stop().expect("stop");
        session.stop().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn automatic_entry_retries_when_the_service_starts_later() {
        let _guard = NET.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_dir();
        let reserved = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let room = write_card(
            &dir,
            "Delayed",
            Some(Ipv4Addr::LOCALHOST.into()),
            Some(address.port()),
            "pw",
        );
        let session = open_session(&dir, 4, media(24, 5008));
        session
            .submit(SessionCommand::JoinSaved {
                mode: JoinMode::Automatic,
                profile_path: room,
            })
            .unwrap();
        let waiting = wait_until(&session, Duration::from_secs(3), |state| {
            matches!(state.phase, Phase::ContactUnavailable | Phase::Failed)
        });
        assert_eq!(
            waiting.phase,
            Phase::ContactUnavailable,
            "transient initial failure must retain join intent"
        );
        assert_eq!(waiting.failure_kind, Some(EntryFailureKind::Unavailable));
        let service =
            ContactService::start(ContactConfig::try_new(address, "Delayed", "pw").unwrap())
                .unwrap();
        let joined = wait_until(&session, Duration::from_secs(3), |state| {
            state.phase == Phase::Joined
        });
        assert_eq!(
            joined.phase,
            Phase::Joined,
            "no second join command was issued"
        );
        assert!(joined.failure_kind.is_none());
        session.stop().unwrap();
        drop(session);
        let mut wrong_profile =
            crate::participant::rooms::load_room_profile(&dir.join("Delayed.json")).unwrap();
        wrong_profile.room.password = "wrong".into();
        let wrong_path = dir.join("wrong.json");
        create_room_profile_file(&wrong_path, &wrong_profile).unwrap();
        let rejected = open_session(&dir, 5, media(25, 5009));
        rejected
            .submit(SessionCommand::JoinSaved {
                mode: JoinMode::Automatic,
                profile_path: wrong_path,
            })
            .unwrap();
        let result = wait_until(&rejected, Duration::from_secs(3), |state| {
            state.phase == Phase::Failed
        });
        assert_eq!(
            result.phase,
            Phase::Failed,
            "password rejection requires user action"
        );
        assert_eq!(
            result.failure_kind,
            Some(EntryFailureKind::PasswordRejected)
        );
        rejected.stop().unwrap();
        drop(rejected);
        service.stop().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn contact_loss_stays_visible_only_and_recovery_rejoins() {
        let _guard = NET.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_dir();
        let service = ContactService::start(
            ContactConfig::try_new(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                "Headphones",
                "pw",
            )
            .unwrap(),
        )
        .unwrap();
        let address = service.local_addr();
        let room = write_card(
            &dir,
            "Headphones",
            Some(Ipv4Addr::LOCALHOST.into()),
            Some(address.port()),
            "pw",
        );
        let session = open_session(&dir, 2, media(21, 5005));
        session
            .submit(SessionCommand::JoinSaved {
                mode: JoinMode::Manual,
                profile_path: room,
            })
            .unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(8), |state| {
            matches!(state.phase, Phase::Joined | Phase::Failed)
        });
        assert_eq!(
            snapshot.phase,
            Phase::Joined,
            "{}",
            snapshot.failure.unwrap_or_default()
        );
        let (member_id, run_id) = ids(3, 3);
        let mut guest = ContactClient::connect(address, Duration::from_secs(1)).unwrap();
        guest
            .join(JoinParams {
                room_name: "Headphones".to_owned(),
                password: "pw".to_owned(),
                member_id,
                run_id,
                display_name: "乙".to_owned(),
                media_port: 5006,
                media_ip: Some(Ipv4Addr::new(198, 51, 100, 22).into()),
                media_scope_id: 0,
                media_alternates: Vec::new(),
            })
            .unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(3), |state| {
            state
                .members
                .iter()
                .any(|member| member.display_name == "乙")
        });
        assert!(
            snapshot
                .members
                .iter()
                .any(|member| member.display_name == "乙" && member.visible)
        );
        while session.poll_output().is_some() {}
        service.stop().unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(4), |state| {
            state.phase == Phase::ContactUnavailable
        });
        assert_eq!(snapshot.phase, Phase::ContactUnavailable);
        assert!(snapshot.members.iter().any(|member| !member.visible));
        assert!(!outputs_include_departure(&session));
        let service =
            ContactService::start(ContactConfig::try_new(address, "Headphones", "pw").unwrap())
                .unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(5), |state| {
            matches!(state.phase, Phase::Joined | Phase::Failed)
        });
        assert_eq!(
            snapshot.phase,
            Phase::Joined,
            "{}",
            snapshot.failure.unwrap_or_default()
        );
        let (member_id, run_id) = ids(4, 4);
        let mut after_recovery = ContactClient::connect(address, Duration::from_secs(1)).unwrap();
        after_recovery
            .join(JoinParams {
                room_name: "Headphones".to_owned(),
                password: "pw".to_owned(),
                member_id,
                run_id,
                display_name: "after-recovery".to_owned(),
                media_port: 5007,
                media_ip: Some(Ipv4Addr::new(198, 51, 100, 23).into()),
                media_scope_id: 0,
                media_alternates: Vec::new(),
            })
            .unwrap();
        let snapshot = wait_until(&session, Duration::from_secs(3), |state| {
            state
                .members
                .iter()
                .any(|member| member.display_name == "after-recovery" && member.visible)
        });
        assert!(
            snapshot
                .members
                .iter()
                .any(|member| member.display_name == "after-recovery" && member.visible)
        );
        service.stop().unwrap();
        session.stop().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn drain_has_local_end(session: &RoomSession) -> bool {
        let mut found = false;
        while let Some(output) = session.poll_output() {
            if matches!(output.body, OutputBody::LocalSessionEnded) {
                found = true;
            }
        }
        found
    }

    #[test]
    fn saturated_queue_does_not_block_stop_or_drop() {
        for explicit_stop in [false, true] {
            let shared = Arc::new(Shared::new());
            let (sender, _receiver) = mpsc::sync_channel(1);
            sender
                .try_send(QueuedCommand {
                    generation: 0,
                    command: SessionCommand::Reload,
                })
                .unwrap_or_else(|_| panic!("empty fixture queue"));
            let worker_shared = Arc::clone(&shared);
            let worker = thread::spawn(move || {
                while !worker_shared.stop_requested.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            });
            let session = RoomSession {
                commands: sender,
                shared: Arc::clone(&shared),
                worker: Arc::new(Mutex::new(Some(worker))),
            };
            let (done_tx, done_rx) = mpsc::channel();
            let dropper = thread::spawn(move || {
                if explicit_stop {
                    session.stop().unwrap();
                    session.stop().unwrap();
                    assert_eq!(
                        session.submit(SessionCommand::Reload),
                        Err(SessionError::Stopped)
                    );
                }
                drop(session);
                done_tx.send(()).unwrap();
            });
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("stop/drop must bypass a full command queue");
            dropper.join().unwrap();
            assert!(shared.stop_requested.load(Ordering::Acquire));
            assert_eq!(shared.generation(), 1);
        }
    }

    #[test]
    fn only_accepted_superseding_commands_advance_the_generation() {
        let shared = Arc::new(Shared::new());
        let (sender, receiver) = mpsc::sync_channel(1);
        let session = RoomSession {
            commands: sender,
            shared: Arc::clone(&shared),
            worker: Arc::new(Mutex::new(None)),
        };
        session.submit(SessionCommand::Leave).unwrap();
        assert_eq!(shared.generation(), 1);
        assert_eq!(
            session.submit(SessionCommand::Cancel),
            Err(SessionError::Busy)
        );
        assert_eq!(shared.generation(), 1);
        let _ = receiver.try_recv().unwrap();
        session.submit(SessionCommand::Reload).unwrap();
        assert_eq!(shared.generation(), 1);
        let _ = receiver.try_recv().unwrap();
        session
            .submit(SessionCommand::Choose {
                generation: 0,
                instance_id: rlb_model::RoomId::from_bytes([1; 16]).unwrap(),
            })
            .unwrap();
        assert_eq!(shared.generation(), 1);
    }

    fn outputs_include_departure(session: &RoomSession) -> bool {
        let mut found = false;
        while let Some(output) = session.poll_output() {
            if matches!(output.body, OutputBody::ExplicitDeparture { .. }) {
                found = true;
            }
        }
        found
    }
}
