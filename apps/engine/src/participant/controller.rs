//! Supervisor owns preferences and persistence; the independent media owner owns UDP and pair state.

#[cfg(test)]
use crate::participant::audio::runtime::media::{MediaCore, peer_sentence, recoverable_udp_error};
use crate::participant::audio::runtime::media::{
    MediaHandle, MediaRuntime, MediaSnapshot, Resume, RoomTransition,
};
use rlb_model::{MemberId, RunId};
#[cfg(test)]
use rlb_model::{SourceId, StreamEpoch};
#[cfg(test)]
use rlb_peer::{ChannelLayout, MAX_DATAGRAM_BYTES, PcmEncoding, PcmFormat, PreparedStorage};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::participant::command::Command;
use crate::participant::config::{
    self, MixIntent, ParticipantConfig, SavedContact, SavedMember, SourceMixOverride,
    TRACKED_PEER_BUDGET,
};
use crate::participant::output_routing::validate_output_targets;
use crate::participant::snapshot::{
    AudioEffect, ChannelLayoutState, CommandOutcome, DirectoryStatus, GroupContact,
    LocalSourceState, MemberContact, MemberState, ParticipantSnapshot, PcmEncodingState,
    PcmFormatState, PublicationState, RemoteMixState, RemoteSourceState,
};

const COMMAND_CAPACITY: usize = 64;
const COMMANDS_PER_TURN: usize = 16;
const CONTROL_WAIT: Duration = Duration::from_millis(50);

#[path = "controller_rooms.rs"]
mod room_control;
#[path = "controller_management.rs"]
mod management_control;
pub use room_control::RoomControlState;

#[cfg(test)]
#[path = "controller_owner_tests.rs"]
mod owner_tests;

struct Worker {
    path: PathBuf,
    config: ParticipantConfig,
    media: MediaRuntime,
    local_address: SocketAddr,
    run_id: RunId,
    directory: DirectoryStatus,
    group_generation: u64,
    unsaved: bool,
    persistence_error: Option<String>,
    login_startup: super::login_startup::LoginStartupState,
    lan_firewall: super::firewall::LanFirewallState,
    transport_error: Option<String>,
    running: bool,
    room_epoch: u64,
    rooms: Option<room_control::RoomControl>,
    room_error: Option<String>,
    next_room_retry: Instant,
    next_network_refresh: Instant,
    last_network_revision: u64,
    room_cards: Vec<crate::participant::rooms::RoomCardView>,
    #[cfg(test)]
    persistence_pause: Option<Box<dyn FnOnce() + Send>>,
}

/// Cloneable supervisor control surface; native audio and media owners remain independent.
#[derive(Clone)]
pub struct ParticipantHandle {
    commands: SyncSender<Request>,
    snapshot: Arc<Mutex<ParticipantSnapshot>>,
    media: MediaHandle,
}

struct Request {
    command: Command,
    room_epoch: u64,
    reply: SyncSender<Result<CommandOutcome, String>>,
}

pub struct RunningParticipant {
    handle: ParticipantHandle,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    instance: Option<std::fs::File>,
}

pub fn start(path: &Path) -> Result<RunningParticipant, String> {
    start_with_room(path, None)
}

/// Select one startup intent before any owner starts. None restores saved intent.
pub fn start_with_room(
    path: &Path,
    room: Option<config::RoomStartup>,
) -> Result<RunningParticipant, String> {
    let instance = claim_config_instance(path)?;
    let path = path.to_path_buf();
    let (boot_tx, boot_rx) = mpsc::channel();
    let (command_tx, command_rx) = sync_channel(COMMAND_CAPACITY);
    let snapshot = Arc::new(Mutex::new(placeholder_snapshot()));
    let snapshot_for_worker = Arc::clone(&snapshot);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_worker = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("rlb-participant".into())
        .spawn(move || {
            worker_main(
                path,
                command_rx,
                snapshot_for_worker,
                boot_tx,
                stop_for_worker,
                room,
            )
        })
        .map_err(|error| format!("The participant worker could not be started: {error}"))?;
    match boot_rx.recv() {
        Ok(Ok(media)) => Ok(RunningParticipant {
            handle: ParticipantHandle {
                commands: command_tx,
                snapshot,
                media,
            },
            stop,
            worker: Some(worker),
            instance: Some(instance),
        }),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(_) => Err("The participant worker stopped before it published a snapshot.".into()),
    }
}

impl RunningParticipant {
    pub fn handle(&self) -> ParticipantHandle {
        self.handle.clone()
    }

    pub fn stop(&mut self) -> Result<(), String> {
        if self.worker.is_none() {
            self.instance.take();
            return Ok(());
        }
        self.handle.media.shutdown();
        self.stop.store(true, Ordering::Release);
        // Wake an idle owner when there is room. A saturated queue already wakes
        // it; shutdown remains independent of successful command delivery.
        let (reply, _) = sync_channel(1);
        let _ = self.handle.commands.try_send(Request {
            command: Command::Quit,
            room_epoch: self.handle.media.room_fence().epoch,
            reply,
        });
        let joined = if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "The participant worker stopped unexpectedly.".to_owned())
        } else {
            Ok(())
        };
        self.instance.take();
        joined
    }
}

pub(super) fn claim_config_instance(path: &Path) -> Result<std::fs::File, String> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("The participant configuration could not be resolved: {error}"))?;
    let mut name = path.as_os_str().to_owned();
    name.push(".instance-lock");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    #[cfg(not(windows))]
    return Err("The participant instance guard currently targets Windows.".to_owned());
    options.open(&name).map_err(|error| {
        if error.raw_os_error() == Some(32) {
            "This configuration is already running in another GUI or CLI instance. Stop that instance first.".to_owned()
        } else { format!("The participant configuration could not be claimed: {error}") }
    })
}

impl Drop for RunningParticipant {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl ParticipantHandle {
    pub fn snapshot(&self) -> ParticipantSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn command(&self, command: Command) -> Result<CommandOutcome, String> {
        room_control::validate_command(&command)?;
        if let Command::JoinGroup { name } = &command {
            config::validate_name("group name", name)?;
        }
        let transition = room_transition(&command);
        let (reply_tx, reply_rx) = sync_channel(1);
        self.media.submit_scope(transition,|room_epoch|{
            self.commands.try_send(Request{command,room_epoch,reply:reply_tx}).map_err(|error|match error{
                mpsc::TrySendError::Full(_)=>"The supervisor command queue is full. Explicit local leave still closes media and will be saved when the supervisor resumes.".to_owned(),
                mpsc::TrySendError::Disconnected(_)=>"The participant worker is no longer accepting commands.".to_owned(),
            })
        })?;
        reply_rx
            .recv()
            .map_err(|_| "The participant worker stopped before replying.".to_owned())?
    }
}

fn worker_main(
    path: PathBuf,
    commands: Receiver<Request>,
    snapshot: Arc<Mutex<ParticipantSnapshot>>,
    boot: mpsc::Sender<Result<MediaHandle, String>>,
    stop: Arc<AtomicBool>,
    room: Option<config::RoomStartup>,
) {
    let mut worker = match Worker::boot_with_room(&path, room) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = boot.send(Err(error));
            return;
        }
    };
    worker.publish(&snapshot);
    let _ = boot.send(Ok(worker.media.handle.clone()));
    'owner: loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        worker.observe_room_close();
        let wait = worker.wait_budget();
        match commands.recv_timeout(wait) {
            Ok(request) => {
                if worker.handle_request(request, &snapshot) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        for _ in 1..COMMANDS_PER_TURN {
            if stop.load(Ordering::Acquire) {
                break 'owner;
            }
            match commands.try_recv() {
                Ok(request) => {
                    if worker.handle_request(request, &snapshot) {
                        break 'owner;
                    }
                }
                Err(_) => break,
            }
        }
        if worker.pump().is_err() {
            worker.publish(&snapshot);
            break;
        }
        worker.publish(&snapshot);
    }
    worker.close_runtime();
    worker.publish(&snapshot);
}

impl Worker {
    #[cfg(test)]
    fn boot(path: &Path) -> Result<Self, String> {
        Self::boot_with_room(path, None)
    }

    fn boot_with_room(path: &Path, room: Option<config::RoomStartup>) -> Result<Self, String> {
        let mut config = config::load_config(path)?;
        if let Some(room) = room {
            config.room.selected = Some(room);
            config.room.auto_enter = true;
            config.group.joined = false;
            config.members.clear();
            config.contacts.clear();
        }
        let media = MediaRuntime::start(config.clone())?;
        let initial = media.handle.snapshot();
        let library = crate::participant::rooms::RoomLibrary::new(path);
        let (room_cards, storage_error) = match library.initialize().and_then(|()| library.cards())
        {
            Ok(cards) => (cards, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let mut worker = Self {
            path: path.to_owned(),
            group_generation: u64::from(config.group.joined),
            config,
            local_address: initial.local_address,
            run_id: initial.run_id,
            media,
            directory: DirectoryStatus::Unavailable,
            unsaved: false,
            persistence_error: None,
            login_startup: Default::default(),
            lan_firewall: Default::default(),
            transport_error: None,
            running: true,
            room_epoch: 1,
            rooms: None,
            room_error: storage_error,
            next_room_retry: Instant::now() + Duration::from_secs(2),
            next_network_refresh: Instant::now() + Duration::from_secs(1),
            last_network_revision: initial.network_revision,
            room_cards,
            #[cfg(test)]
            persistence_pause: None,
        };
        #[cfg(not(test))]
        {
            worker.login_startup = super::login_startup::inspect(path, &worker.config);
            worker.lan_firewall = super::firewall::inspect();
        }
        #[cfg(test)]
        {
            worker.login_startup.requested = worker.config.login_startup;
        }
        // Unit fixtures opt into room I/O through explicit room commands.
        #[cfg(not(test))]
        {
            if let Err(error) = worker.ensure_rooms().and_then(|()| worker.restore_room()) {
                worker.room_error = Some(error);
            }
        }
        #[cfg(test)]
        if worker.config.room.auto_enter {
            if let Err(error) = worker.restore_room() {
                worker.room_error = Some(error);
            }
        }
        Ok(worker)
    }

    fn observe_room_close(&mut self) {
        let fence = self.media.handle.room_fence();
        if fence.closed && fence.epoch > self.room_epoch {
            self.room_epoch = fence.epoch;
            self.config.group.joined = false;
            self.close_room_from_fence();
            let _ = self.advance_group_generation();
            self.directory = DirectoryStatus::Unavailable;
            self.persist();
        }
    }

    fn wait_budget(&self) -> Duration {
        CONTROL_WAIT
    }

    fn handle_request(&mut self, request: Request, snapshot: &Mutex<ParticipantSnapshot>) -> bool {
        let name = request.command.name().to_owned();
        let quit = matches!(request.command, Command::Quit);
        let result = if quit {
            self.close_runtime();
            Ok(self.outcome(
                &name,
                true,
                AudioEffect::NotApplicable,
                "The participant controller is stopping. Saved choices remain in the configuration file.",
            ))
        } else {
            self.apply_in_scope(request.command, request.room_epoch)
        };
        self.publish(snapshot);
        let _ = request.reply.send(result);
        quit || self.media.handle.snapshot().fatal_socket_error
    }

    #[cfg(test)]
    fn apply(&mut self, command: Command) -> Result<CommandOutcome, String> {
        let transition = room_transition(&command);
        let epoch = self.media.handle.submit_scope(transition, Ok)?;
        self.apply_in_scope(command, epoch)
    }

    fn apply_in_scope(&mut self, command: Command, epoch: u64) -> Result<CommandOutcome, String> {
        if epoch != self.media.handle.room_fence().epoch {
            return Err("The command was superseded by a newer room action.".into());
        }
        self.room_epoch = epoch;
        let entering_room = matches!(
            command,
            Command::CreateRoom { .. }
                | Command::CreateSavedRoom { .. }
                | Command::JoinSavedRoom { .. }
                | Command::JoinUnknownRoom { .. }
        );
        let outcome = self.apply_command(command);
        if entering_room {
            if let Err(error) = &outcome {
                self.close_room_from_fence();
                self.config.group.joined = false;
                self.config.room.auto_enter = false;
                self.room_error = Some(error.clone());
            }
        }
        // State-changing commands may fail after closing a peer or exhausting
        // a counter. Publish that resulting admission even on an error path.

        outcome.map(|mut outcome| {
            outcome.snapshot = self.snapshot();
            outcome
        })
    }

    fn apply_command(&mut self, command: Command) -> Result<CommandOutcome, String> {
        let name = command.name().to_owned();
        match command {
            Command::SetNetwork { policy, bind } => {
                policy.validate()?;
                let mut next = self.config.clone();
                next.network = policy;
                if let Some(bind) = bind { next.bind = bind; }
                next.validate()?;
                self.config = next;
                self.persist();
                Ok(self.outcome(&name, true, AudioEffect::Requested,
                    "Network settings applied. A restricted network never falls back to another interface."))
            }
            command @ (Command::ReadRoomCard { .. }
            | Command::SaveRoomCard { .. }
            | Command::DeleteRoomCard { .. }
            | Command::ExportSettings { .. }
            | Command::CheckPendingSettings) => self.apply_management_command(command),
            command @ (Command::CreateRoom { .. }
            | Command::CreateSavedRoom { .. }
            | Command::JoinSavedRoom { .. }
            | Command::JoinUnknownRoom { .. }
            | Command::ChooseRoom { .. }
            | Command::LeaveRoom
            | Command::CancelRoom
            | Command::ReloadRoomRules
            | Command::StartVirtualReturn
            | Command::StopVirtualReturn
            | Command::EditRoomRule { .. }
            | Command::EditGlobalRule { .. }
            | Command::SetRoomSourceMix { .. }
            | Command::ReplaceGlobalRules { .. }
            | Command::SetGlobalDefaults { .. }
            | Command::RefreshRooms) => self.apply_room_command(command),
            Command::SetName { name: display } => {
                config::validate_name("display name", &display)?;
                if display == self.config.display_name {
                    return Ok(self.outcome(
                        &name,
                        true,
                        AudioEffect::NotApplicable,
                        "The local name is unchanged.",
                    ));
                }
                if self.media_config().group.joined
                    || (self.room_active() && !matches!(self.room_phase(), "idle" | "failed"))
                {
                    return Err("加入房间后不能修改本机名称，请先离开房间。".into());
                }
                if let Some(room) = &self.rooms {
                    room.session
                        .submit(
                            crate::participant::room_session::SessionCommand::SetDisplayName {
                                name: display.clone(),
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
                self.config.display_name = display;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The display name was saved.",
                ))
            }
            Command::JoinGroup { name: group } => {
                // Direct-contact diagnostics retire room networking while
                // retaining the same local preferences owner.
                self.close_room_from_fence();
                if let Some(room) = &mut self.rooms {
                    room.active = false;
                }
                self.config.room.auto_enter = false;
                config::validate_name("group name", &group)?;
                let new_group = !self.config.group.joined || self.config.group.name != group;
                if new_group {
                    self.advance_group_generation()?;
                    self.directory = DirectoryStatus::Unavailable;
                }
                if !self.config.group.name.is_empty() && self.config.group.name != group {
                    // Explicit contacts belong to the selected group. Pair
                    // counters stay for this process run; saved preferences stay.
                    self.pause_all();
                    self.config.contacts.clear();
                }
                self.config.group.name = group;
                self.config.group.joined = true;
                self.media.handle.configure_in(
                    &self.config,
                    Resume::All(false),
                    self.room_epoch,
                )?;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The selected group is joined. Saved contacts are being established. The wire carries no group authentication.",
                ))
            }
            Command::LeaveGroup => {
                if self.room_active() {
                    return self.apply_room_command(Command::LeaveRoom);
                }
                if self.config.group.joined {
                    self.advance_group_generation()?;
                }
                self.config.group.joined = false;
                self.pause_all();
                self.directory = DirectoryStatus::Unavailable;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The group was left. Active contacts were closed and saved member choices were kept.",
                ))
            }
            Command::SetPeerIntent {
                member_id,
                send,
                receive,
            } => {
                if self.room_active() {
                    return self.edit_member_rule(member_id, send, receive);
                }
                let Some(member) = self.config.member_mut(member_id) else {
                    return Err(format!("Member {member_id} has no saved choices."));
                };
                member.send = send;
                member.receive = receive;
                self.configure_media(Resume::Automatic)?;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The local send and receive choices were applied.",
                ))
            }
            Command::SetAutoReconnect { enabled } => {
                self.config.auto_reconnect = enabled;
                if !enabled {
                    self.park_disconnected();
                } else if self.media_config().group.joined {
                    let contacts = self.media_config().contacts;
                    for contact in contacts {
                        self.resume_peer(contact.member_id)?;
                    }
                }
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "Automatic reconnection was updated. Connected contacts stay active.",
                ))
            }
            Command::SetLoginStartup { enabled } => {
                self.config.login_startup = enabled;
                self.login_startup = super::login_startup::set(&self.path, &self.config, enabled);
                self.persist();
                let applied = self.login_startup.last_error.is_none();
                Ok(self.outcome(
                    &name,
                    applied,
                    AudioEffect::NotApplicable,
                    self.login_startup.last_error.as_deref().unwrap_or(
                        "The current-user Windows login-startup setting was updated. Saved room and rule restoration uses the normal participant startup.",
                    ),
                ))
            }
            Command::RefreshLoginStartup => {
                self.login_startup = super::login_startup::inspect(&self.path, &self.config);
                Ok(self.outcome(&name, true, AudioEffect::NotApplicable,
                    "The Windows login-startup registration was observed without changing it."))
            }
            command @ (Command::AllowLanFirewall | Command::RefreshLanFirewall | Command::RemoveOwnLanFirewall) => {
                self.lan_firewall = match command {
                    Command::AllowLanFirewall => super::firewall::allow(),
                    Command::RemoveOwnLanFirewall => super::firewall::remove(),
                    _ => super::firewall::inspect(),
                };
                let applied = self.lan_firewall.last_error.is_none();
                let message = if applied {
                    match command {
                        Command::AllowLanFirewall => "This executable's LocalSubnet inbound TCP and UDP rules were registered. Other firewall and network policies still apply.",
                        Command::RemoveOwnLanFirewall => "Only this executable's owned LAN firewall rules were removed.",
                        _ => "This executable's owned LAN firewall rules were observed without changing them.",
                    }
                } else {
                    "The application LAN firewall operation could not be confirmed. See its registration status for details."
                };
                Ok(self.outcome(&name, applied, AudioEffect::NotApplicable, message))
            }
            Command::SetLocalSource {
                source_id,
                publish,
                monitor,
            } => {
                if publish.is_none() && monitor.is_none() {
                    return Err("A source edit must set publication, monitoring, or both.".into());
                }
                let Some(_) = self.config.source(source_id) else {
                    return Err(format!(
                        "Local source {source_id} is not part of this configuration."
                    ));
                };
                let source = self.config.source_mut(source_id).expect("source exists");
                if let Some(publish) = publish {
                    source.publish = publish;
                }
                if let Some(monitor) = monitor {
                    source.monitor = monitor;
                }
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "Publication and independent local monitoring were requested. Capture state shows the selected device's readiness.",
                ))
            }
            Command::SetLocalOutputs { source_id, targets } => {
                validate_output_targets(&targets)?;
                let Some(source) = self.config.source_mut(source_id) else {
                    return Err(format!(
                        "Local source {source_id} is not part of this configuration."
                    ));
                };
                source.output_targets = targets;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "The local source output choices were saved. Monitoring and publication choices stay independent.",
                ))
            }
            Command::SelectSourceEndpoint {
                source_id,
                endpoint_id,
            } => {
                config::validate_optional_endpoint(endpoint_id.as_deref())?;
                let Some(source) = self.config.source_mut(source_id) else {
                    return Err(format!(
                        "Local source {source_id} is not part of this configuration."
                    ));
                };
                source.endpoint_id = endpoint_id;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "The source endpoint choice was saved and sent to its native capture owner.",
                ))
            }
            Command::SelectOutput { endpoint_id } => {
                config::validate_optional_endpoint(endpoint_id.as_deref())?;
                self.config.output_endpoint_id = endpoint_id;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "The output choice was saved and sent to the native playback owner. Actual readiness is shown in output state.",
                ))
            }
            Command::SetSourceMix {
                member_id,
                source_id,
                gain_db,
                pan,
                mute,
                solo,
            } => {
                let playback = MixIntent {
                    gain_db,
                    pan,
                    mute,
                    solo,
                };
                playback.validate("source mix")?;
                if let Some(member_id) = member_id {
                    if self.room_active() {
                        return self.edit_room_source_mix(member_id, source_id, playback);
                    }
                    if self.config.source(source_id).is_some() {
                        return Err(
                            "A remote mix override must name a remote source, not a local source."
                                .into(),
                        );
                    }
                    let Some(member) = self.config.member_mut(member_id) else {
                        return Err(format!("Member {member_id} has no saved choices."));
                    };
                    if let Some(existing) = member
                        .source_overrides
                        .iter_mut()
                        .find(|choice| choice.source_id == source_id)
                    {
                        existing.playback = playback;
                    } else {
                        member.source_overrides.push(SourceMixOverride {
                            source_id,
                            playback,
                        });
                    }
                } else {
                    let Some(source) = self.config.source_mut(source_id) else {
                        return Err(format!(
                            "Local source {source_id} is not part of this configuration."
                        ));
                    };
                    source.playback = playback;
                }
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "The mix choice was saved and sent to the playback owner.",
                ))
            }
            Command::SetMaster {
                gain_db,
                limiter_enabled,
            } => {
                MixIntent {
                    gain_db,
                    pan: 0.0,
                    mute: false,
                    solo: false,
                }
                .validate("master")?;
                self.config.master_gain_db = gain_db;
                self.config.limiter_enabled = limiter_enabled;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "Master gain and the optional limiter choice were saved and sent to the playback owner.",
                ))
            }
            Command::RefreshDevices => {
                self.media.handle.refresh_output()?;
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::Requested,
                    "The native audio owners were asked to refresh their selected endpoints.",
                ))
            }
            Command::UpsertContact {
                member_id,
                address,
                name: member_name,
            } => {
                if self.room_active() {
                    return Err("Room destinations come from the contact service; use room rules to select members.".into());
                }
                self.upsert_contact(member_id, address, member_name)?;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The contact address was saved separately from the current connection state.",
                ))
            }
            Command::Reconnect => {
                if !self.media_config().group.joined {
                    return Err("Join the group before reconnecting contacts.".into());
                }
                self.configure_media(Resume::All(true))?;
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "Saved contacts were asked to establish a fresh session.",
                ))
            }
            Command::DirectoryRemove {
                group_generation,
                member_id,
            } => {
                self.require_directory_scope(group_generation)?;
                if self.config.member(member_id).is_none() {
                    return Err(format!("Member {member_id} has no saved choices."));
                }
                self.config
                    .contacts
                    .retain(|contact| contact.member_id != member_id);
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The directory removal closed that active contact. Saved member choices remain for a later reappearance.",
                ))
            }
            Command::DirectoryUpsertContact {
                group_generation,
                member_id,
                address,
                name: member_name,
            } => {
                self.require_directory_scope(group_generation)?;
                self.upsert_contact(member_id, address, member_name)?;
                self.persist();
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "The current directory contact was recorded.",
                ))
            }
            Command::DirectoryStatus {
                group_generation,
                status,
            } => {
                self.require_directory_scope(group_generation)?;
                self.directory = status;
                Ok(self.outcome(
                    &name,
                    true,
                    AudioEffect::NotApplicable,
                    "Directory status was recorded. Existing direct contacts were left in place.",
                ))
            }
            Command::Quit => unreachable!("quit is handled before apply"),
        }
    }

    fn advance_group_generation(&mut self) -> Result<(), String> {
        self.group_generation = self.group_generation.checked_add(1).ok_or_else(|| {
            "The group generation is exhausted. Restart the participant before changing groups."
                .to_owned()
        })?;
        Ok(())
    }

    fn require_directory_scope(&self, group_generation: u64) -> Result<(), String> {
        if !self.config.group.joined || self.group_generation != group_generation {
            return Err("The directory update belongs to an inactive group selection.".into());
        }
        Ok(())
    }

    fn upsert_contact(
        &mut self,
        member_id: MemberId,
        address: SocketAddr,
        name: Option<String>,
    ) -> Result<(), String> {
        self.admit_new_member(member_id)?;
        config::validate_contact_address(address)?;
        if address.is_ipv4() != self.local_address.is_ipv4() {
            return Err(
                "The contact address must use the same IP family as the participant bind address."
                    .into(),
            );
        }
        if member_id == self.config.member_id {
            return Err("A contact identity must differ from the local member identity.".into());
        }
        if let Some(name) = name.as_deref() {
            config::validate_name("member name", name)?;
        }
        let previous_address = self
            .config
            .contacts
            .iter()
            .find(|contact| contact.member_id == member_id)
            .map(|contact| contact.address);
        if let Some(member) = self.config.member_mut(member_id) {
            if let Some(name) = name {
                member.name = name;
            }
        } else {
            self.config.members.push(SavedMember::new(
                member_id,
                name.unwrap_or_else(|| member_id.to_string()),
            ));
        }
        if let Some(contact) = self
            .config
            .contacts
            .iter_mut()
            .find(|contact| contact.member_id == member_id)
        {
            contact.address = address;
        } else {
            self.config
                .contacts
                .push(SavedContact { member_id, address, alternates: Vec::new(), resolved_for: None });
        }
        if self.config.group.joined && self.config.auto_reconnect {
            let reset = previous_address.is_some_and(|current| current != address);
            self.ensure_peer(member_id, reset || previous_address.is_none())?;
        }
        Ok(())
    }

    fn ensure_peer(&mut self, id: MemberId, reset: bool) -> Result<(), String> {
        self.configure_media(Resume::Member(id, reset))
    }

    fn resume_peer(&mut self, id: MemberId) -> Result<(), String> {
        self.ensure_peer(id, false)
    }

    fn close_runtime(&mut self) {
        self.running = false;
        self.media.stop();
        self.stop_room();
    }

    fn pause_all(&mut self) {
        self.media.handle.pause();
        let mut config = self.config.clone();
        config.group.joined = false;
        if let Err(e) = self
            .media
            .handle
            .configure_in(&config, Resume::Automatic, self.room_epoch)
        {
            self.transport_error = Some(e);
        }
    }

    fn admit_new_member(&self, id: MemberId) -> Result<(), String> {
        let state = self.media.handle.snapshot();
        if state.peers.contains_key(&id) || self.config.contacts.iter().any(|c| c.member_id == id) {
            return Ok(());
        }
        let mut ids = state.peers.keys().copied().collect::<HashSet<_>>();
        ids.extend(self.config.contacts.iter().map(|c| c.member_id));
        if ids.len() >= TRACKED_PEER_BUDGET {
            Err("The participant is already tracking 64 member pairs for this run, including paused pairs. The new contact was not saved.".into())
        } else {
            Ok(())
        }
    }

    fn park_disconnected(&mut self) {
        if let Err(e) = self.configure_media(Resume::Automatic) {
            self.transport_error = Some(e);
        }
    }

    /// Observe owner faults; UDP and Peer deadlines run on the media reactor.
    fn pump(&mut self) -> Result<(), String> {
        if Instant::now() >= self.next_network_refresh {
            self.next_network_refresh = Instant::now() + Duration::from_secs(1);
            let _ = self.media.handle.update_network_catalogue(super::network::catalogue());
        }
        let transport = self.media.handle.snapshot();
        if transport.network_revision != self.last_network_revision {
            self.local_address = transport.local_address;
            let applied = if let Some(room) = &self.rooms {
                match room.session.submit(crate::participant::room_session::SessionCommand::UpdateTransport {
                    bind: transport.local_address,
                    requested_bind: self.config.bind,
                    addresses: transport.local_addresses.clone(),
                    interfaces: transport.network.interfaces.clone(),
                    network: self.config.network.clone(),
                }) {
                    Ok(()) => true,
                    Err(error) => { self.room_error = Some(error.to_string()); false }
                }
            } else { true };
            if applied { self.last_network_revision = transport.network_revision; }
        }
        if (self.rooms.is_none() || self.retry_creator_address())
            && self.config.room.auto_enter
            && Instant::now() >= self.next_room_retry
        {
            self.next_room_retry = Instant::now() + Duration::from_secs(2);
            if let Err(error) = self.restore_room() {
                self.room_error = Some(error);
            }
        }
        if let Err(error) = self.observe_rooms() {
            self.room_error = Some(error);
        }
        let state = self.media.handle.snapshot();
        if state.fatal_socket_error || !state.running {
            Err(state
                .socket_error
                .unwrap_or_else(|| "The media owner is stopped.".into()))
        } else {
            Ok(())
        }
    }

    fn persist(&mut self) {
        #[cfg(test)]
        if let Some(pause) = self.persistence_pause.take() {
            pause();
        }
        if self.room_epoch != self.media.handle.room_fence().epoch {
            self.unsaved = true;
            self.persistence_error = Some("The save was superseded by a newer room action.".into());
            return;
        }
        self.transport_error = self.configure_media(Resume::Automatic).err();
        match config::save_config(&self.path, &self.config) {
            Ok(()) => {
                self.unsaved = false;
                self.persistence_error = None;
            }
            Err(error) => {
                self.unsaved = true;
                self.persistence_error = Some(error);
            }
        }
    }

    fn outcome(
        &self,
        command: &str,
        applied: bool,
        audio: AudioEffect,
        message: &str,
    ) -> CommandOutcome {
        CommandOutcome {
            command: command.to_owned(),
            applied,
            audio,
            message: message.to_owned(),
            persistence_error: self.persistence_error.clone().or_else(|| {
                self.room_state()
                    .session
                    .and_then(|state| state.rules_save_error)
            }),
            snapshot: self.snapshot(),
            management: None,
        }
    }

    fn publish(&self, snapshot: &Mutex<ParticipantSnapshot>) {
        *snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = self.snapshot();
    }

    fn snapshot(&self) -> ParticipantSnapshot {
        let media = self.media.handle.snapshot();
        let config = self.media_config();
        let members = config
            .members
            .iter()
            .filter(|member| {
                config
                    .contacts
                    .iter()
                    .any(|contact| contact.member_id == member.id)
            })
            .map(|member| self.member_state(member, &media, &config))
            .collect::<Vec<_>>();
        let group_contact = group_contact(config.group.joined, &members);
        ParticipantSnapshot {
            room: self.room_state(),
            running: self.running,
            local_member_id: config.member_id,
            local_name: config.display_name.clone(),
            bind: config.bind,
            local_address: (self.running && self.local_address.port() != 0).then_some(self.local_address),
            local_addresses: if self.running && media.running { media.local_addresses.clone() } else { Vec::new() },
            network: media.network.clone(),
            network_revision: media.network_revision,
            run_id: self.running.then_some(self.run_id),
            group_name: config.group.name.clone(),
            joined: config.group.joined,
            group_generation: self.group_generation,
            group_contact,
            auto_reconnect: config.auto_reconnect,
            login_startup: self.login_startup.clone(),
            lan_firewall: self.lan_firewall.clone(),
            directory: self.directory,
            local_sources: config
                .sources
                .iter()
                .map(|source| {
                    let capture = media
                        .captures
                        .iter()
                        .find(|capture| capture.source == source.id);
                    LocalSourceState {
                        id: source.id,
                        kind: source.kind,
                        name: source.name.clone(),
                        publish: source.publish,
                        monitor: source.monitor,
                        output_targets: source.output_targets.clone(),
                        endpoint_id: source.endpoint_id.clone(),
                        playback: source.playback,
                        capture_state: capture.map(|capture| capture.phase).unwrap_or("disabled"),
                        format: capture.and_then(|capture| capture.acquisition)
                            .map(|acquisition| pcm_format_state(acquisition.format)),
                        peak_db: capture.filter(|capture| capture.phase == "ready").and_then(|capture|
                            crate::participant::snapshot::recent_peak_db(capture.peak_abs, capture.peak_observed)
                        ),
                        availability_reason: capture
                            .and_then(|capture| capture.error.clone())
                            .unwrap_or_else(|| {
                                match capture.map(|capture| capture.phase) {
                                    Some("ready") => "Native capture is running.",
                                    Some("starting") => {
                                        "The selected capture device is being prepared."
                                    }
                                    Some("idle") => "Waiting for a receiver or local monitoring; P2P control remains connected.",
                                    _ => "This source has no active capture stream.",
                                }
                                .into()
                            }),
                        publication: match media
                            .local_catalogue
                            .source(source.id)
                            .map(|source| source.state)
                        {
                            Some(rlb_peer::PublicationState::Active) => PublicationState::Active,
                            Some(rlb_peer::PublicationState::Unavailable) => {
                                PublicationState::Unavailable
                            }
                            _ => PublicationState::Disabled,
                        },
                        capture: capture.map(|capture| {
                            crate::participant::snapshot::CaptureDetails {
                                actual_endpoint_id: capture.endpoint.clone(),
                                acquisition_epoch: capture.acquisition.map(|value| value.epoch),
                                capture_instance: capture.acquisition.map(|value| value.instance),
                                sample_rate_hz: capture
                                    .acquisition
                                    .map(|value| value.native.sample_rate_hz),
                                channels: capture.acquisition.map(|value| value.native.channels),
                                channel_mask: capture
                                    .acquisition
                                    .map(|value| value.native.channel_mask),
                                period_frames: capture.acquisition.map(|value| value.period_frames),
                                buffer_frames: capture.acquisition.map(|value| value.buffer_frames),
                                captured_frames: capture.captured_frames,
                                dropped_blocks: capture.dropped_blocks,
                                last_source_frame_end: capture.last_packet.and_then(|packet| {
                                    packet.start_sample.checked_add(u64::from(packet.frames))
                                }),
                                first_frame_qpc_100ns: capture.last_packet.and_then(|packet| {
                                    packet.timing.first_frame.map(|frame| frame.at.ticks())
                                }),
                                observed_on_owner_qpc_100ns: capture.last_packet.and_then(
                                    |packet| packet.timing.observed_on_owner.map(|at| at.ticks()),
                                ),
                                scheduling_error: capture.scheduling_error.clone(),
                            }
                        }),
                    }
                })
                .collect(),
            output_endpoint_id: config.output_endpoint_id.clone(),
            endpoints: media.endpoints.clone(),
            master_gain_db: config.master_gain_db,
            limiter_enabled: config.limiter_enabled,
            overloaded: media.output.overloaded,
            members,
            unsaved: self.unsaved
                || self
                    .room_state()
                    .session
                    .is_some_and(|state| state.rules_unsaved),
            persistence_error: self.persistence_error.clone().or_else(|| {
                self.room_state()
                    .session
                    .and_then(|state| state.rules_save_error)
            }),
            socket_error: self
                .transport_error
                .clone()
                .or_else(|| media.socket_error.clone()),
            capture_error: media.capture_error.clone(),
            rejected_unconfigured: media.counters.rejected_unconfigured,
            rejected_malformed: media.counters.rejected_malformed,
            media_unavailable: media.counters.media_unavailable,
            socket_errors: media.counters.socket_errors,
            audio: media.output.state,
            group_authentication: if self.room_active() {
                "contact_password"
            } else {
                "controller_membership_only"
            },
            room_phase: self.room_phase(),
            output_submitted_frames: media.output_submitted_frames,
            output: media.output.presentation(),
            output_devices: media
                .output
                .outputs
                .iter()
                .map(|(endpoint, state)| super::snapshot::OutputDeviceState {
                    endpoint_id: endpoint.clone(),
                    state: state.presentation(),
                })
                .collect(),
            main_output_id: media.output.main_endpoint_id.clone(),
            admission_revision: media.admission_revision,
            return_loop: media.return_loop.clone(),
        }
    }

    fn member_state(
        &self,
        member: &SavedMember,
        media: &MediaSnapshot,
        config: &ParticipantConfig,
    ) -> MemberState {
        let address = config
            .contacts
            .iter()
            .find(|contact| contact.member_id == member.id)
            .map(|contact| contact.address);
        let slot = media.peers.get(&member.id);
        let address = slot.filter(|slot| slot.status.connected).map(|slot| slot.address).or(address);
        let (contact, send_ready, receive_ready, parked, last_error) = if let Some(slot) = slot {
            let status = slot.status;
            let contact = if !slot.allow_ticks {
                MemberContact::Offline
            } else if status.connected {
                MemberContact::Connected
            } else {
                MemberContact::Contacting
            };
            (
                contact,
                status.send_ready,
                status.receive_ready,
                !slot.allow_ticks,
                slot.last_error.clone(),
            )
        } else {
            (
                MemberContact::Offline,
                false,
                false,
                config.group.joined && !config.auto_reconnect,
                None,
            )
        };
        MemberState {
            id: member.id,
            name: member.name.clone(),
            address,
            send_allowed: member.send,
            receive_allowed: member.receive,
            contact,
            send_ready,
            receive_ready,
            sending_sources: slot
                .map(|slot| slot.sending_sources.iter().flatten().copied().collect())
                .unwrap_or_default(),
            parked,
            playback: member.playback,
            source_mix_overrides: member
                .source_overrides
                .iter()
                .map(|choice| RemoteMixState {
                    source_id: choice.source_id,
                    playback: choice.playback,
                })
                .collect(),
            sources: slot
                .and_then(|slot| slot.catalogue.as_ref())
                .map(|catalogue| {
                    catalogue
                        .as_slice()
                        .iter()
                        .map(|entry| RemoteSourceState {
                            peak_db: slot.filter(|slot| slot.admission.sources.iter().flatten()
                                .any(|source| source.entry == *entry && source.receive_ready))
                                .and_then(|slot| slot.source_peaks.iter().flatten()
                                .find(|(source, _, _)| *source == entry.id))
                                .and_then(|(_, peak, observed)| crate::participant::snapshot::recent_peak_db(Some(*peak), Some(*observed))),
                            receive_allowed: member
                                .source_policy
                                .as_ref()
                                .map(|policy| policy.for_kind(entry.kind).receive)
                                .unwrap_or(member.receive),
                            receive_ready: slot.is_some_and(|slot| {
                                slot.admission
                                    .sources
                                    .iter()
                                    .flatten()
                                    .any(|source| source.entry == *entry && source.receive_ready)
                            }),
                            id: entry.id,
                            kind: match entry.kind {
                                rlb_peer::SourceKind::System => config::SourceKind::System,
                                rlb_peer::SourceKind::Microphone => config::SourceKind::Microphone,
                            },
                            publication: match entry.state {
                                rlb_peer::PublicationState::Disabled => PublicationState::Disabled,
                                rlb_peer::PublicationState::Unavailable => {
                                    PublicationState::Unavailable
                                }
                                rlb_peer::PublicationState::Active => PublicationState::Active,
                            },
                            publication_epoch: entry.epoch.get(),
                            format: entry.format.map(pcm_format_state),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            last_error,
        }
    }
}

fn pcm_format_state(format: rlb_peer::PcmFormat) -> PcmFormatState {
    PcmFormatState {
        sample_rate_hz: format.sample_rate_hz(),
        channels: format.channels(),
        channel_mask: format.layout().channel_mask(),
        layout: match format.layout() {
            rlb_peer::ChannelLayout::Mono => ChannelLayoutState::Mono,
            rlb_peer::ChannelLayout::StereoLeftRight => ChannelLayoutState::StereoLeftRight,
            _ => ChannelLayoutState::Speakers,
        },
        encoding: match format.encoding() {
            rlb_peer::PcmEncoding::F32Le => PcmEncodingState::F32Le,
            rlb_peer::PcmEncoding::Signed32Le => PcmEncodingState::Signed32Le,
            rlb_peer::PcmEncoding::F64Le => PcmEncodingState::F64Le,
        },
        sample_bytes: format.sample_bytes(),
        valid_bits: format.valid_bits(),
    }
}

fn room_transition(command: &Command) -> RoomTransition {
    match command {
        Command::JoinGroup { .. }
        | Command::CreateRoom { .. }
        | Command::CreateSavedRoom { .. }
        | Command::JoinSavedRoom { .. }
        | Command::JoinUnknownRoom { .. } => RoomTransition::Enter,
        Command::LeaveGroup | Command::LeaveRoom | Command::CancelRoom | Command::Quit => {
            RoomTransition::Leave
        }
        _ => RoomTransition::Keep,
    }
}

fn group_contact(joined: bool, members: &[MemberState]) -> GroupContact {
    if !joined {
        return GroupContact::Offline;
    }
    let active: Vec<_> = members
        .iter()
        .filter(|member| member.address.is_some())
        .collect();
    if active.is_empty()
        || active
            .iter()
            .all(|member| member.contact == MemberContact::Offline)
    {
        GroupContact::Offline
    } else if active
        .iter()
        .any(|member| member.contact == MemberContact::Contacting)
    {
        GroupContact::Contacting
    } else if active
        .iter()
        .any(|member| member.contact == MemberContact::Connected)
    {
        GroupContact::Connected
    } else {
        GroupContact::Offline
    }
}

#[cfg(test)]
fn bind_mono(worker: &mut MediaCore, source_id: SourceId, remote: MemberId) -> Result<(), String> {
    let format = mono_format()?;
    let acquisition = StreamEpoch::from_bytes([9; 16]).expect("acquisition");
    worker
        .publisher
        .set_publication(source_id, true)
        .map_err(|error| error.to_string())?;
    let update = worker
        .publisher
        .begin_binding(source_id, acquisition, format)
        .map_err(|error| error.to_string())?;
    if let Some(request) = update.pending {
        let committed = worker
            .publisher
            .commit_binding(request, acquisition, 0)
            .map_err(|error| error.to_string())?;
        let now = worker.now_ms();
        worker
            .peers
            .get_mut(&remote)
            .ok_or_else(|| format!("Member {remote} has no peer."))?
            .peer
            .install_catalogue(&committed.catalogue, now)
            .map_err(peer_sentence)?;
        worker.drain_peer(remote)?;
    }
    Ok(())
}

#[cfg(test)]
fn mono_format() -> Result<PcmFormat, String> {
    PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
fn publish_mono(
    worker: &mut MediaCore,
    source_id: SourceId,
    samples: &[f32],
    remote: MemberId,
    start_frame: u64,
) -> Result<usize, String> {
    let format = mono_format()?;
    let acquisition = StreamEpoch::from_bytes([9; 16]).expect("acquisition");
    let now = worker.now_ms();
    let canonical: Vec<f64> = samples.iter().copied().map(f64::from).collect();
    let original = worker
        .publisher
        .local_original(source_id, acquisition, start_frame, &canonical, now)
        .map_err(|error| error.to_string())?;
    let ticket = worker.tickets.allocate()?;
    let mut storage = PreparedStorage::for_format(format).map_err(|error| error.to_string())?;
    let view = worker
        .publisher
        .prepare_pcm_into(original.with_ticket(ticket), &mut storage)
        .map_err(|error| error.to_string())?;
    let mut scratch = vec![0_u8; MAX_DATAGRAM_BYTES];
    let mut sent = 0;
    for index in 0..view.len() {
        let Some(fragment) = view.get(index) else {
            continue;
        };
        let datagram = worker
            .peers
            .get(&remote)
            .ok_or_else(|| format!("Member {remote} has no peer."))?
            .peer
            .write_original_into(&fragment, &mut scratch)
            .map_err(peer_sentence)?;
        if let Some(datagram) = datagram {
            worker.send_prepared_to_peer(remote, datagram.bytes)?;
            sent += 1;
        }
    }
    Ok(sent)
}

fn placeholder_snapshot() -> ParticipantSnapshot {
    ParticipantSnapshot {
        room: RoomControlState {
            session: None,
            cards: Vec::new(),
            routes: Vec::new(),
            global_rules_path: PathBuf::new(),
            auto_enter: false,
            error: None,
        },
        running: false,
        local_member_id: MemberId::from_bytes([1; 16]).expect("nonzero"),
        local_name: String::new(),
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        local_address: None,
        local_addresses: Vec::new(),
        network: Default::default(),
        network_revision: 0,
        run_id: None,
        group_name: String::new(),
        joined: false,
        group_generation: 0,
        group_contact: GroupContact::Offline,
        auto_reconnect: true,
        login_startup: Default::default(),
        lan_firewall: Default::default(),
        directory: DirectoryStatus::Unavailable,
        local_sources: Vec::new(),
        output_endpoint_id: None,
        endpoints: Vec::new(),
        master_gain_db: 0.0,
        limiter_enabled: false,
        overloaded: None,
        members: Vec::new(),
        unsaved: false,
        persistence_error: None,
        socket_error: None,
        capture_error: None,
        rejected_unconfigured: 0,
        rejected_malformed: 0,
        media_unavailable: 0,
        socket_errors: 0,
        audio: "unavailable",
        group_authentication: "controller_membership_only",
        room_phase: "idle",
        output_submitted_frames: 0,
        output: Default::default(),
        output_devices: Vec::new(),
        main_output_id: None,
        admission_revision: 0,
        return_loop: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::net::UdpSocket;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    fn scratch_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rlb-c03-run-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_new(dir: &Path, name: &str, bind: &str) -> (PathBuf, ParticipantConfig) {
        let path = dir.join(format!("{name}.json"));
        let config = ParticipantConfig::create(name, bind.parse().unwrap()).unwrap();
        config::create_config_file(&path, &config).unwrap();
        (path, config)
    }

    #[test]
    fn local_output_edits_persist_without_enabling_capture_or_publication() {
        use crate::participant::output_routing::OutputTarget;

        let dir = scratch_dir();
        let (path, mut config) = write_new(&dir, "Outputs", "127.0.0.1:0");
        let id = config.sources[0].id;
        config.sources[0].publish = false;
        config.sources[0].monitor = false;
        config::save_config(&path, &config).unwrap();
        let mut worker = Worker::boot(&path).unwrap();
        let destinations = vec![OutputTarget::Main, OutputTarget::Endpoint("cable".into())];
        worker
            .apply(Command::SetLocalOutputs {
                source_id: id,
                targets: destinations.clone(),
            })
            .unwrap();
        let saved = config::load_config(&path).unwrap();
        let source = saved.source(id).unwrap();
        assert_eq!(source.output_targets, destinations);
        assert!(!source.publish && !source.monitor);
        assert!(
            worker
                .apply(Command::SetLocalOutputs {
                    source_id: id,
                    targets: vec![OutputTarget::Main, OutputTarget::Main],
                })
                .is_err()
        );
        assert_eq!(worker.config.source(id).unwrap(), source);
        worker
            .apply(Command::SetLocalOutputs {
                source_id: id,
                targets: vec![],
            })
            .unwrap();
        let source = config::load_config(&path)
            .unwrap()
            .source(id)
            .unwrap()
            .clone();
        assert!(source.output_targets.is_empty());
        assert!(!source.publish && !source.monitor);
        drop(worker);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn reserve_port() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    }

    fn member(snapshot: &ParticipantSnapshot, id: MemberId) -> &MemberState {
        snapshot
            .members
            .iter()
            .find(|member| member.id == id)
            .expect("member")
    }

    fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if ready() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    #[cfg(windows)]
    fn refusal_stays_applied_when_saving_fails() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = scratch_dir();
        let (path, config) = write_new(&dir, "Ada", "127.0.0.1:0");
        let mut running = start(&path).unwrap();
        let handle = running.handle();
        let other = MemberId::generate().unwrap();
        handle
            .command(Command::UpsertContact {
                member_id: other,
                address: "127.0.0.1:9".parse().unwrap(),
                name: Some("Bea".into()),
            })
            .unwrap();
        handle
            .command(Command::SetPeerIntent {
                member_id: other,
                send: true,
                receive: true,
            })
            .unwrap();
        // Deny delete sharing so atomic replacement fails while the original
        // configuration remains readable and unchanged.
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let outcome = handle
            .command(Command::SetPeerIntent {
                member_id: other,
                send: true,
                receive: false,
            })
            .unwrap();
        assert!(outcome.applied);
        assert!(outcome.persistence_error.is_some());
        let state = member(&outcome.snapshot, other);
        assert!(state.send_allowed);
        assert!(!state.receive_allowed);
        let disk = config::load_config(&path).unwrap();
        assert!(disk.member(other).unwrap().receive);
        assert_eq!(disk.member_id, config.member_id);
        running.stop().unwrap();
        drop(blocker);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_removal_keeps_preferences_and_loss_keeps_contacts() {
        let dir = scratch_dir();
        let (path, _) = write_new(&dir, "Ada", "127.0.0.1:0");
        let mut running = start(&path).unwrap();
        let handle = running.handle();
        let other = MemberId::generate().unwrap();
        handle
            .command(Command::UpsertContact {
                member_id: other,
                address: "127.0.0.1:9".parse().unwrap(),
                name: Some("Bea".into()),
            })
            .unwrap();
        handle
            .command(Command::SetPeerIntent {
                member_id: other,
                send: false,
                receive: true,
            })
            .unwrap();
        handle
            .command(Command::JoinGroup {
                name: "ensemble".into(),
            })
            .unwrap();
        let group_generation = handle.snapshot().group_generation;
        let unavailable = handle
            .command(Command::DirectoryStatus {
                group_generation,
                status: DirectoryStatus::Unavailable,
            })
            .unwrap();
        assert_eq!(unavailable.snapshot.directory, DirectoryStatus::Unavailable);
        assert_eq!(
            member(&unavailable.snapshot, other).address.unwrap().port(),
            9
        );
        let removed = handle
            .command(Command::DirectoryRemove {
                group_generation,
                member_id: other,
            })
            .unwrap();
        assert!(removed.snapshot.members.is_empty());
        let saved = config::load_config(&path).unwrap();
        assert!(!saved.member(other).unwrap().send);
        assert!(saved.member(other).unwrap().receive);
        handle
            .command(Command::DirectoryUpsertContact {
                group_generation,
                member_id: other,
                address: "127.0.0.1:9".parse().unwrap(),
                name: None,
            })
            .unwrap();
        let switched = handle
            .command(Command::JoinGroup {
                name: "next group".into(),
            })
            .unwrap();
        assert!(switched.snapshot.members.is_empty());
        assert!(
            handle
                .command(Command::DirectoryUpsertContact {
                    group_generation,
                    member_id: other,
                    address: "127.0.0.1:9".parse().unwrap(),
                    name: None,
                })
                .is_err()
        );
        let reappeared = handle
            .command(Command::DirectoryUpsertContact {
                group_generation: switched.snapshot.group_generation,
                member_id: other,
                address: "127.0.0.1:9".parse().unwrap(),
                name: None,
            })
            .unwrap();
        assert!(!member(&reappeared.snapshot, other).send_allowed);
        running.stop().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_joins_the_worker_and_keeps_the_file() {
        let dir = scratch_dir();
        let (path, config) = write_new(&dir, "Ada", "127.0.0.1:0");
        let mut running = start(&path).unwrap();
        let handle = running.handle();
        running.stop().unwrap();
        let stopped = handle.snapshot();
        assert!(!stopped.running);
        assert!(stopped.local_address.is_none());
        assert!(stopped.run_id.is_none());
        assert_eq!(stopped.group_contact, GroupContact::Offline);
        let loaded = config::load_config(&path).unwrap();
        assert_eq!(loaded.member_id, config.member_id);
        assert!(!loaded.group.joined);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_remains_deliverable_when_the_command_queue_is_full() {
        let media_config =
            ParticipantConfig::create("Queue fixture", "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut media = MediaRuntime::start(media_config).unwrap();
        let (commands, queued) = sync_channel(COMMAND_CAPACITY);
        for _ in 0..COMMAND_CAPACITY {
            let (reply, _) = sync_channel(1);
            commands
                .try_send(Request {
                    command: Command::RefreshDevices,
                    room_epoch: media.handle.room_fence().epoch,
                    reply,
                })
                .unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            assert!(worker_stop.load(Ordering::Acquire));
            drop(queued);
        });
        let mut running = RunningParticipant {
            handle: ParticipantHandle {
                commands,
                snapshot: Arc::new(Mutex::new(placeholder_snapshot())),
                media: media.handle.clone(),
            },
            stop,
            worker: Some(worker),
            instance: None,
        };
        running.stop().unwrap();
        media.stop();
    }

    #[test]
    fn enabling_reconnection_resumes_missing_and_parked_contacts() {
        let dir = scratch_dir();
        let (path, mut config) = write_new(&dir, "Ada", "127.0.0.1:0");
        config.auto_reconnect = false;
        config.group.name = "ensemble".into();
        config.group.joined = true;
        config::save_config(&path, &config).unwrap();
        let mut worker = Worker::boot(&path).unwrap();
        let remote = MemberId::generate().unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        worker
            .apply(Command::UpsertContact {
                member_id: remote,
                address: receiver.local_addr().unwrap(),
                name: None,
            })
            .unwrap();
        assert!(!worker.media.handle.snapshot().peers.contains_key(&remote));
        assert!(worker.snapshot().members[0].parked);
        worker
            .apply(Command::SetAutoReconnect { enabled: true })
            .unwrap();
        assert!(worker.media.handle.snapshot().peers[&remote].allow_ticks);
        worker.media.handle.with_core(move |core| {
            core.peers.get_mut(&remote).unwrap().allow_ticks = false;
        });
        worker
            .apply(Command::SetAutoReconnect { enabled: true })
            .unwrap();
        assert!(worker.media.handle.snapshot().peers[&remote].allow_ticks);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn udp_peer_reachability_errors_remain_recoverable() {
        for code in [10050, 10051, 10052, 10053, 10054, 10060, 10064, 10065] {
            assert!(recoverable_udp_error(&io::Error::from_raw_os_error(code)));
        }
        assert!(recoverable_udp_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(!recoverable_udp_error(&io::Error::from_raw_os_error(10038)));
    }

    #[test]
    fn localhost_contacts_refuse_one_direction_and_restore_after_restart() {
        let dir = scratch_dir();
        let (path_a, config_a) = write_new(&dir, "Ada", &format!("127.0.0.1:{}", reserve_port()));
        let (path_b, config_b) = write_new(&dir, "Bea", &format!("127.0.0.1:{}", reserve_port()));
        let mut ada = start(&path_a).unwrap();
        let mut bea = start(&path_b).unwrap();
        let ada_address = ada.handle().snapshot().local_address.unwrap();
        let bea_address = bea.handle().snapshot().local_address.unwrap();
        ada.handle()
            .command(Command::UpsertContact {
                member_id: config_b.member_id,
                address: bea_address,
                name: Some("Bea".into()),
            })
            .unwrap();
        bea.handle()
            .command(Command::UpsertContact {
                member_id: config_a.member_id,
                address: ada_address,
                name: Some("Ada".into()),
            })
            .unwrap();
        ada.handle()
            .command(Command::SetPeerIntent {
                member_id: config_b.member_id,
                send: true,
                receive: true,
            })
            .unwrap();
        bea.handle()
            .command(Command::SetPeerIntent {
                member_id: config_a.member_id,
                send: true,
                receive: true,
            })
            .unwrap();
        ada.handle()
            .command(Command::JoinGroup {
                name: "ensemble".into(),
            })
            .unwrap();
        bea.handle()
            .command(Command::JoinGroup {
                name: "ensemble".into(),
            })
            .unwrap();
        assert!(
            wait_until(|| {
                let ada_snapshot = ada.handle().snapshot();
                let bea_snapshot = bea.handle().snapshot();
                let a = member(&ada_snapshot, config_b.member_id);
                let b = member(&bea_snapshot, config_a.member_id);
                a.send_ready && a.receive_ready && b.send_ready && b.receive_ready
            }),
            "contacts did not become ready\nAda: {:?}\nBea: {:?}",
            ada.handle().snapshot(),
            bea.handle().snapshot()
        );
        let original_generation = ada.handle().snapshot().group_generation;
        let catalogue_snapshot = ada.handle().snapshot();
        let remote_sources = &member(&catalogue_snapshot, config_b.member_id).sources;
        assert_eq!(remote_sources.len(), config_b.sources.len());
        for expected in &config_b.sources {
            let actual = remote_sources
                .iter()
                .find(|source| source.id == expected.id)
                .expect("installed source metadata");
            assert_eq!(actual.kind, expected.kind);
            let expected_publication = if expected.publish {
                PublicationState::Unavailable
            } else {
                PublicationState::Disabled
            };
            assert_eq!(actual.publication, expected_publication);
            assert!(actual.publication_epoch > 0);
            assert!(actual.format.is_none());
        }
        let enabled = ada
            .handle()
            .command(Command::SetAutoReconnect { enabled: true })
            .unwrap();
        assert!(member(&enabled.snapshot, config_b.member_id).send_ready);
        let repeated_join = ada
            .handle()
            .command(Command::JoinGroup {
                name: "ensemble".into(),
            })
            .unwrap();
        assert_eq!(repeated_join.snapshot.group_generation, original_generation);
        assert!(member(&repeated_join.snapshot, config_b.member_id).send_ready);
        ada.handle()
            .command(Command::SetPeerIntent {
                member_id: config_b.member_id,
                send: true,
                receive: false,
            })
            .unwrap();
        assert!(
            wait_until(|| {
                let ada_snapshot = ada.handle().snapshot();
                let bea_snapshot = bea.handle().snapshot();
                let a = member(&ada_snapshot, config_b.member_id);
                let b = member(&bea_snapshot, config_a.member_id);
                a.send_ready && !a.receive_allowed && !b.send_ready && b.receive_ready
            }),
            "refusal did not settle\nAda: {:?}\nBea: {:?}",
            ada.handle().snapshot(),
            bea.handle().snapshot()
        );
        ada.stop().unwrap();
        let mut ada = start(&path_a).unwrap();
        assert!(
            wait_until(|| {
                let snapshot = ada.handle().snapshot();
                let bea_snapshot = bea.handle().snapshot();
                let a = member(&snapshot, config_b.member_id);
                let b = member(&bea_snapshot, config_a.member_id);
                snapshot.joined
                    && snapshot.auto_reconnect
                    && a.send_allowed
                    && !a.receive_allowed
                    && a.send_ready
                    && !b.send_ready
                    && b.receive_ready
            }),
            "restart did not restore the refusal\nAda: {:?}\nBea: {:?}",
            ada.handle().snapshot(),
            bea.handle().snapshot()
        );
        let ada_handle = ada.handle();
        ada.stop().unwrap();
        let stopped = ada_handle.snapshot();
        assert!(!stopped.running);
        assert!(stopped.joined);
        assert_eq!(stopped.group_contact, GroupContact::Offline);
        assert!(!member(&stopped, config_b.member_id).send_ready);
        assert!(!member(&stopped, config_b.member_id).receive_ready);
        bea.stop().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_sixty_fifth_contact_is_rejected_before_the_file_changes() {
        let dir = scratch_dir();
        let (path, _) = write_new(&dir, "Ada", "127.0.0.1:0");
        let mut running = start(&path).unwrap();
        let handle = running.handle();
        for index in 1..=64 {
            let mut bytes = [0x5A; 16];
            bytes[0] = index;
            handle
                .command(Command::UpsertContact {
                    member_id: MemberId::from_bytes(bytes).unwrap(),
                    address: format!("127.0.0.1:{}", 1000 + u16::from(index))
                        .parse()
                        .unwrap(),
                    name: None,
                })
                .unwrap();
        }
        let mut overflow = [0x5A; 16];
        overflow[0] = 65;
        let rejected = handle.command(Command::UpsertContact {
            member_id: MemberId::from_bytes(overflow).unwrap(),
            address: "127.0.0.1:1065".parse().unwrap(),
            name: None,
        });
        assert!(rejected.is_err());
        assert_eq!(config::load_config(&path).unwrap().contacts.len(), 64);
        running.stop().unwrap();

        let mut saved = config::load_config(&path).unwrap();
        saved.group = config::SavedGroup {
            name: "Saved contacts".into(),
            joined: true,
        };
        config::save_config(&path, &saved).unwrap();
        let worker = Worker::boot(&path).unwrap();
        assert_eq!(
            worker.media.handle.snapshot().peers.len(),
            TRACKED_PEER_BUDGET
        );
        drop(worker);

        let overflow = MemberId::from_bytes(overflow).unwrap();
        saved.members.push(SavedMember {
            source_output_targets: Vec::new(),
            id: overflow,
            name: "Overflow".into(),
            source_policy: None,
            send: true,
            receive: true,
            playback: MixIntent::unity(),
            source_overrides: Vec::new(),
        });
        saved.contacts.push(SavedContact {
            member_id: overflow,
            address: "127.0.0.1:1065".parse().unwrap(),
            alternates: Vec::new(),
            resolved_for: None,
        });
        let oversized = serde_json::to_vec(&saved).unwrap();
        std::fs::write(&path, &oversized).unwrap();
        let error = match Worker::boot(&path) {
            Err(error) => error,
            Ok(_) => panic!("a saved configuration with 65 contacts must not start"),
        };
        assert!(error.contains("more than 64 contact members"));
        assert_eq!(std::fs::read(&path).unwrap(), oversized);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leave_rejoin_and_address_replacement_keep_the_pair_revision() {
        let dir = scratch_dir();
        let (path, _) = write_new(&dir, "Ada", "127.0.0.1:0");
        let mut worker = Worker::boot(&path).unwrap();
        let remote = MemberId::generate().unwrap();
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        worker
            .apply(Command::UpsertContact {
                member_id: remote,
                address: first.local_addr().unwrap(),
                name: None,
            })
            .unwrap();
        worker
            .apply(Command::JoinGroup {
                name: "ensemble".into(),
            })
            .unwrap();
        let joined_config = std::fs::read(&path).unwrap();
        worker
            .apply(Command::SetName { name: "Ada".into() })
            .unwrap();
        let rename_error = worker
            .apply(Command::SetName {
                name: "Ada renamed".into(),
            })
            .unwrap_err();
        assert!(rename_error.contains("离开房间"));
        assert_eq!(worker.config.display_name, "Ada");
        assert_eq!(std::fs::read(&path).unwrap(), joined_config);
        worker
            .apply(Command::SetPeerIntent {
                member_id: remote,
                send: false,
                receive: true,
            })
            .unwrap();
        let revision = worker.media.handle.snapshot().peers[&remote].local_revision;
        assert!(revision > 1);
        worker.apply(Command::LeaveGroup).unwrap();
        let left = worker.media.handle.snapshot();
        assert!(left.peers[&remote].paused);
        assert_eq!(left.peers[&remote].local_revision, revision);
        worker
            .apply(Command::SetName {
                name: "Ada renamed".into(),
            })
            .unwrap();
        assert_eq!(
            config::load_config(&path).unwrap().display_name,
            "Ada renamed"
        );
        worker
            .apply(Command::JoinGroup {
                name: "ensemble".into(),
            })
            .unwrap();
        let rejoined = worker.media.handle.snapshot();
        assert!(!rejoined.peers[&remote].paused);
        assert_eq!(rejoined.peers[&remote].local_revision, revision);
        let second = UdpSocket::bind("127.0.0.1:0").unwrap();
        worker
            .apply(Command::UpsertContact {
                member_id: remote,
                address: second.local_addr().unwrap(),
                name: None,
            })
            .unwrap();
        let replaced = worker.media.handle.snapshot();
        assert_eq!(replaced.peers[&remote].local_revision, revision);
        assert_eq!(
            replaced.peers[&remote].address,
            second.local_addr().unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn two_originals_cross_udp_and_complete_a_render_receipt() {
        let dir = scratch_dir();
        let (path_a, mut config_a) =
            write_new(&dir, "Ada", &format!("127.0.0.1:{}", reserve_port()));
        let (path_b, mut config_b) =
            write_new(&dir, "Bea", &format!("127.0.0.1:{}", reserve_port()));
        config_a.group.joined = true;
        config_a.group.name = "ensemble".into();
        config_a.sources[1].publish = true;
        config_b.group.joined = true;
        config_b.group.name = "ensemble".into();
        config::save_config(&path_a, &config_a).unwrap();
        config::save_config(&path_b, &config_b).unwrap();
        let mut ada = Worker::boot(&path_a).unwrap();
        let mut bea = Worker::boot(&path_b).unwrap();
        let ada_address = ada.local_address;
        let bea_address = bea.local_address;
        ada.apply(Command::UpsertContact {
            member_id: config_b.member_id,
            address: bea_address,
            name: Some("Bea".into()),
        })
        .unwrap();
        bea.apply(Command::UpsertContact {
            member_id: config_a.member_id,
            address: ada_address,
            name: Some("Ada".into()),
        })
        .unwrap();
        ada.apply(Command::SetPeerIntent {
            member_id: config_b.member_id,
            send: true,
            receive: false,
        })
        .unwrap();
        bea.apply(Command::SetPeerIntent {
            member_id: config_a.member_id,
            send: false,
            receive: true,
        })
        .unwrap();
        let ada_id = config_a.member_id;
        let bea_id = config_b.member_id;
        assert!(
            wait_until(|| {
                let ada_state = ada.media.handle.snapshot();
                let bea_state = bea.media.handle.snapshot();
                ada_state.peers[&bea_id].status.send_ready
                    && bea_state.peers[&ada_id].status.receive_ready
            }),
            "the direct pair did not become ready"
        );
        bea.media.handle.with_core(|core| {
            core.playback.select_software_route();
        });
        let system = config_a.sources[0].id;
        let microphone = config_a.sources[1].id;
        ada.media
            .handle
            .with_core(move |core| {
                bind_mono(core, system, bea_id)?;
                bind_mono(core, microphone, bea_id)
            })
            .unwrap();
        assert!(
            wait_until(|| {
                let state = bea.media.handle.snapshot();
                state.peers[&ada_id]
                    .catalogue
                    .as_ref()
                    .is_some_and(|catalogue| {
                        [system, microphone].iter().all(|id| {
                            catalogue.source(*id).is_some_and(|source| {
                                source.state == rlb_peer::PublicationState::Active
                            })
                        })
                    })
            }),
            "both source catalogues must arrive before publication"
        );
        let tone = vec![0.25_f32; 4_096];
        let (first, second) = ada
            .media
            .handle
            .with_core(move |core| {
                core.pump()?;
                let first = publish_mono(core, system, &tone, bea_id, 0)?;
                let second = publish_mono(core, microphone, &tone, bea_id, 0)?;
                Ok::<_, String>((first, second))
            })
            .unwrap();
        assert!(first > 0 && second > 0, "publication produced no datagrams");
        assert!(
            wait_until(|| bea.media.handle.with_core(|core| {
                for _ in 0..4 {
                    core.playback.service_software().unwrap();
                }
                let closed = core.playback.admitted_closed();
                closed.len() == 2
                    && closed.iter().all(|floor| *floor > 0)
                    && core.playback.submitted_frames() > 0
            })),
            "both sources must reach the software endpoint on its owner"
        );
        let (closed, timelines, playback_error, submitted) = bea.media.handle.with_core(|core| {
            (
                core.playback.admitted_closed(),
                core.playback.admitted_timelines().to_vec(),
                core.playback.last_error().map(str::to_owned),
                core.playback.submitted_frames(),
            )
        });
        let mut heard: Vec<SourceId> = timelines
            .iter()
            .filter_map(|timeline| match *timeline {
                crate::participant::audio::render::TimelineId::Remote {
                    member, source, ..
                } if member == ada_id => Some(source),
                _ => None,
            })
            .collect();
        heard.sort_by_key(|source| source.as_bytes().to_vec());
        let mut expected = vec![system, microphone];
        expected.sort_by_key(|source| source.as_bytes().to_vec());
        assert_eq!(
            heard, expected,
            "the two originals did not both reach playback"
        );
        assert!(
            closed.len() >= 2 && closed.iter().all(|floor| *floor > 0),
            "both originals were not retired: closed={closed:?} playback={:?} media_unavailable={} socket={:?}",
            playback_error,
            bea.media.handle.snapshot().counters.media_unavailable,
            bea.media.handle.snapshot().socket_error
        );
        assert!(
            ada.media
                .handle
                .with_core(|core| core.playback.admitted_closed().is_empty())
        );
        assert!(submitted > 0);

        let before_close = bea.media.handle.snapshot().admission_revision;
        bea.apply(Command::SetPeerIntent {
            member_id: config_a.member_id,
            send: false,
            receive: false,
        })
        .unwrap();
        assert!(bea.media.handle.snapshot().admission_revision > before_close);
        let closed_timelines = timelines.clone();
        assert!(
            bea.media.handle.with_core(move |core| closed_timelines
                .iter()
                .all(|timeline| { core.playback.source_active(*timeline) == Some(false) })),
            "receive closure must apply without another PCM packet"
        );

        bea.apply(Command::SetPeerIntent {
            member_id: config_a.member_id,
            send: false,
            receive: true,
        })
        .unwrap();
        assert!(wait_until(|| ada.media.handle.snapshot().peers[&bea_id]
            .status
            .send_ready));
        let open_timelines = timelines.clone();
        assert!(bea.media.handle.with_core(move |core| {
            open_timelines
                .iter()
                .all(|timeline| core.playback.source_active(*timeline) == Some(true))
        }));
        bea.apply(Command::LeaveGroup).unwrap();
        let left_state = bea.media.handle.snapshot();
        let left_revision = left_state.admission_revision;
        let left_timelines = timelines.clone();
        assert!(bea.media.handle.with_core(move |core| {
            left_timelines
                .iter()
                .all(|timeline| core.playback.source_active(*timeline) == Some(false))
        }));
        let dropped_before = left_state.counters.rejected_unconfigured;
        assert!(
            ada.media
                .handle
                .with_core(move |core| { publish_mono(core, system, &[0.25; 64], bea_id, 4_096) })
                .unwrap()
                > 0
        );
        assert!(wait_until(|| bea
            .media
            .handle
            .snapshot()
            .counters
            .rejected_unconfigured
            > dropped_before));
        assert_eq!(
            bea.media.handle.snapshot().admission_revision,
            left_revision
        );
        assert!(
            bea.media.handle.with_core(move |core| timelines
                .iter()
                .all(|timeline| { core.playback.source_active(*timeline) == Some(false) })),
            "PCM received after leave cannot restore the previous admission"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
