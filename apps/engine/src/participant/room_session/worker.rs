//! One worker owns contact clients, the optional local service, and discovery.
//!
//! Command submission returns when the command is queued. TCP, files, and
//! discovery shutdown run on this thread.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rlb_contact::{
    ContactClient, ContactCode, ContactConfig, ContactError, ContactEvent, ContactService,
    JoinParams, ReturnAction, ReturnState, RoomInfo,
};
use rlb_model::RoomId;

#[cfg(test)]
use super::super::config::MixIntent;
use super::super::config::SourceKind;
use super::super::discovery::{AdvertisedRoom, RoomDiscovery};
use super::super::rooms::{
    DiscoveredRoom, GlobalRules, JoinMode, MixPatch, RoomPlaybackRule, RoomProfile, RoomTarget,
    Rule, RuleMatch, SaveFailure, apply_playback_edit, apply_rule_edit, choose_plan,
    save_global_rules_if_unchanged, save_room_profile_if_unchanged,
};
use super::apply::{
    CardWrite, DirectoryView, NextStep, Signal, apply_event, apply_ordered, mark_all_hidden,
    persist_new_card, recovery_step, step_from_plan, targets_differ, unknown_card,
};
#[cfg(test)]
use rlb_contact::MediaSocket;
#[cfg(test)]
use super::apply::usable_lan_media;
use super::types::{
    EntryFailureKind, OutputBody, Phase, RuleFileStatus, RuleFilesState, RuleSaveState,
    SessionCommand, SessionConfig, SessionOutput, SessionSnapshot,
};
use super::{QueuedCommand, Shared};

const TICK: Duration = Duration::from_millis(40);
const KEEPALIVE: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct PendingCard {
    path: PathBuf,
    rules: Vec<Rule>,
    entered_address: Option<IpAddr>,
    entered_port: Option<u16>,
    strict_auto_join: bool,
}

struct Loaded {
    /// None retains the owner's unsaved global document and baseline.
    global: Option<(GlobalRules, Vec<u8>)>,
    profile: RoomProfile,
    profile_bytes: Vec<u8>,
}

#[derive(Debug)]
struct EntryFailure {
    kind: EntryFailureKind,
    message: String,
}

struct Live {
    shared: std::sync::Arc<Shared>,
    config: SessionConfig,
    generation: u64,
    phase: Phase,
    global: Option<GlobalRules>,
    global_bytes: Option<Vec<u8>>,
    profile_document: Option<RoomProfile>,
    profile_bytes: Option<Vec<u8>>,
    room_save: RuleSaveState,
    global_save: RuleSaveState,
    rules: Vec<Rule>,
    room_name: Option<String>,
    playback_rules: Vec<RoomPlaybackRule>,
    saved_address: Option<IpAddr>,
    saved_port: Option<u16>,
    strict_auto_join: bool,
    password: String,
    profile_path: Option<PathBuf>,
    pending_card: Option<PendingCard>,
    instance_id: Option<RoomId>,
    endpoints: Vec<SocketAddr>,
    local_service: Option<SocketAddr>,
    service: Option<ContactService>,
    creator_bind: Option<SocketAddr>,
    client: Option<ContactClient>,
    discovery: Option<RoomDiscovery>,
    view: DirectoryView,
    choices: Vec<DiscoveredRoom>,
    discovered_rooms: Vec<DiscoveredRoom>,
    card_persisted: Option<bool>,
    pending_target_change: bool,
    failure: Option<EntryFailure>,
    discovery_failure: Option<String>,
    advertisement: Option<AdvertisedRoom>,
    last_keepalive: Instant,
    last_recovery: Instant,
    next_discovery_retry: Instant,
    entry_mode: JoinMode,
    joined_once: bool,
    virtual_return: Option<ReturnState>,
    operation_notice: Option<String>,
}

pub(crate) fn run(
    config: SessionConfig,
    commands: Receiver<QueuedCommand>,
    shared: std::sync::Arc<Shared>,
) {
    let mut live = Live::new(config, std::sync::Arc::clone(&shared));
    let initial_outputs = reload(&mut live);
    publish(&shared, &live, initial_outputs);
    maintain_discovery(&mut live);
    refresh_discovery(&mut live);
    publish(&shared, &live, Vec::new());
    loop {
        if live.phase == Phase::Stopped || shared.stop_requested.load(Ordering::Acquire) {
            break;
        }
        let pending = shared
            .pending_leave
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(generation) = pending {
            if shared.is_current(generation) {
                live.generation = generation;
                let outputs = leave(&mut live);
                publish(&shared, &live, outputs);
            }
        }
        match commands.recv_timeout(TICK) {
            Ok(queued) => {
                // Local preferences survive room switches. Room-scoped work
                // still belongs only to the generation that accepted it.
                let global_edit = matches!(
                    &queued.command,
                    SessionCommand::SetGlobalDefaults { .. }
                        | SessionCommand::EditGlobalRule { .. }
                        | SessionCommand::ReplaceGlobalRules { .. }
                        | SessionCommand::SetDisplayName { .. }
                        | SessionCommand::UpdateTransport { .. }
                        | SessionCommand::SnapshotSettings { .. }
                );
                if !shared.is_current(queued.generation) && !global_edit {
                    continue;
                }
                // A global edit cannot promote old room facts to a newer
                // generation. The queued room boundary advances that owner.
                if !global_edit {
                    live.generation = queued.generation;
                }
                let outputs = dispatch(&mut live, &commands, queued.command);
                refresh_discovery(&mut live);
                publish(&shared, &live, outputs);
            }
            Err(RecvTimeoutError::Timeout) => {
                let outputs = maintain(&mut live);
                let discovery_changed = refresh_discovery(&mut live);
                if !outputs.is_empty() || discovery_changed {
                    publish(&shared, &live, outputs);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    live.generation = shared.generation();
    let outputs = stop(&mut live);
    publish(&shared, &live, outputs);
}

fn publish(shared: &Shared, live: &Live, outputs: Vec<SessionOutput>) {
    if live.phase != Phase::Stopped && !shared.is_current(live.generation) {
        return;
    }
    for output in outputs {
        shared.push_output(output);
    }
    let mut snapshot = live.snapshot();
    snapshot.output_overflow = shared.overflow();
    snapshot.output_loss_revision = shared.loss_revision();
    shared.set_snapshot(snapshot);
}

impl Live {
    fn new(config: SessionConfig, shared: std::sync::Arc<Shared>) -> Self {
        let now = Instant::now();
        Self {
            shared,
            config,
            generation: 0,
            phase: Phase::Idle,
            global: None,
            global_bytes: None,
            profile_document: None,
            profile_bytes: None,
            room_save: RuleSaveState::Unloaded,
            global_save: RuleSaveState::Unloaded,
            rules: Vec::new(),
            playback_rules: Vec::new(),
            room_name: None,
            saved_address: None,
            saved_port: None,
            strict_auto_join: false,
            password: String::new(),
            profile_path: None,
            pending_card: None,
            instance_id: None,
            endpoints: Vec::new(),
            local_service: None,
            service: None,
            creator_bind: None,
            client: None,
            discovery: None,
            view: DirectoryView::new(),
            choices: Vec::new(),
            discovered_rooms: Vec::new(),
            card_persisted: None,
            pending_target_change: false,
            failure: None,
            discovery_failure: None,
            advertisement: None,
            last_keepalive: now,
            last_recovery: now,
            next_discovery_retry: now,
            entry_mode: JoinMode::Manual,
            joined_once: false,
            virtual_return: None,
            operation_notice: None,
        }
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            generation: self.generation,
            phase: self.phase,
            room_name: self.room_name.clone(),
            instance_id: self.instance_id,
            local_service: self.local_service,
            profile_path: self.profile_path.clone(),
            discovered_rooms: self.discovered_rooms.clone(),
            choices: self.choices.clone(),
            selected_endpoints: self.endpoints.clone(),
            members: self.view.members(),
            global_rules: self.global.clone(),
            rules: self.rules.clone(),
            playback_rules: self.playback_rules.clone(),
            rules_unsaved: self.room_save.unsaved() || self.global_save.unsaved(),
            rules_save_error: self.rules_save_error(),
            rule_files: RuleFilesState {
                room: RuleFileStatus {
                    path: self
                        .profile_path
                        .clone()
                        .or_else(|| self.pending_card.as_ref().map(|card| card.path.clone())),
                    outcome: self.room_save.clone(),
                },
                global: RuleFileStatus {
                    path: Some(self.config.global_rules_path.clone()),
                    outcome: self.global_save.clone(),
                },
            },
            saved_address: self.saved_address,
            card_persisted: self.card_persisted,
            pending_target_change: self.pending_target_change,
            failure: self.failure.as_ref().map(|failure| failure.message.clone()),
            failure_kind: self.failure.as_ref().map(|failure| failure.kind),
            output_overflow: false,
            output_loss_revision: 0,
            discovery_failure: self.discovery_failure.clone(),
            local_advertisement: self.advertisement.clone(),
            virtual_return: self.virtual_return.clone(),
            operation_notice: self.operation_notice.clone(),
        }
    }

    fn target(&self) -> Option<RoomTarget> {
        Some(RoomTarget {
            name: self.room_name.clone()?,
            address: self.saved_address,
            port: self.saved_port,
            password: self.password.clone(),
            strict_auto_join: self.strict_auto_join,
        })
    }

    fn rules_save_error(&self) -> Option<String> {
        match (self.room_save.detail(), self.global_save.detail()) {
            (Some(room), Some(global)) => Some(format!("{room} {global}")),
            (Some(error), None) | (None, Some(error)) => Some(error.to_owned()),
            (None, None) => None,
        }
    }
}

fn dispatch(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    command: SessionCommand,
) -> Vec<SessionOutput> {
    if live.phase == Phase::Stopped {
        return Vec::new();
    }
    match command {
        SessionCommand::UpdateTransport { bind, requested_bind, addresses, interfaces, network } => update_transport(live, bind, requested_bind, addresses, interfaces, network),
        SessionCommand::Create { profile_path, bind } => create(live, commands, profile_path, bind),
        SessionCommand::JoinSaved { mode, profile_path } => {
            join_saved(live, commands, mode, profile_path)
        }
        SessionCommand::JoinUnknown {
            mode,
            target,
            card_path,
            rules,
        } => join_unknown(live, commands, mode, target, card_path, rules),
        SessionCommand::Choose {
            generation,
            instance_id,
        } => choose(live, commands, generation, instance_id),
        SessionCommand::Leave | SessionCommand::Cancel => leave(live),
        SessionCommand::Reload => reload(live),
        SessionCommand::StartVirtualReturn => virtual_return_action(live, ReturnAction::Start),
        SessionCommand::StopVirtualReturn => virtual_return_action(live, ReturnAction::Stop),
        SessionCommand::EditRoomRule { rule } => edit_room_rule(live, rule),
        SessionCommand::EditGlobalRule { rule } => edit_global_rule(live, rule),
        SessionCommand::ReplaceGlobalRules { rules } => replace_global_rules(live, rules),
        SessionCommand::SetGlobalDefaults { send, listen } => {
            set_global_defaults(live, send, listen)
        }
        SessionCommand::SetDisplayName { name } => {
            // The supervisor permits renaming between rooms. An existing
            // contact registration remains immutable until it is retired.
            live.config.display_name = name;
            Vec::new()
        }
        SessionCommand::SetRoomSourceMix {
            condition,
            source_kind,
            playback,
        } => set_room_source_mix(live, condition, source_kind, playback),
        SessionCommand::UpdateCardTarget { path, target, expected_revision, reply } => {
            let (result, outputs) = update_card_target(live, &path, target, &expected_revision);
            let _ = reply.send(result);
            outputs
        }
        SessionCommand::SnapshotSettings { reply } => {
            let _ = reply.send(effective_settings(live));
            Vec::new()
        }
        SessionCommand::Stop => stop(live),
    }
}

fn create(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    profile_path: PathBuf,
    bind: SocketAddr,
) -> Vec<SessionOutput> {
    let generation = begin(live);
    let loaded = match load_for_entry(live, &profile_path) {
        Ok(loaded) => loaded,
        Err(message) => return fail_phase(live, generation, message),
    };
    remember_profile(live, &loaded.profile, Some(profile_path));
    remember_loaded(live, loaded);
    publish(&live.shared, live, Vec::new());
    live.creator_bind = Some(bind);
    if live.config.media_bind.port() == 0 {
        return wait_transport(live);
    }
    live.phase = Phase::Connecting;
    let info = match start_service(live, bind) {
        Ok(info) => info,
        Err(message) => {
            return fail_entry(live, generation, EntryFailureKind::ListenerFailed, message);
        }
    };
    if let Some(message) = if bind.ip().is_loopback() {
        None
    } else {
        advertise(live, &info)
    } {
        // Discovery is an optional convenience around an already usable TCP
        // listener. Keep the room available for explicit-address entry.
        live.discovery_failure = Some(message);
    }
    let control = control_address(
        live.local_service
            .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), info.port)),
    );
    live.endpoints = vec![control];
    live.instance_id = Some(info.instance_id);
    let dialed = dial(
        live,
        commands,
        generation,
        &[control],
        Some(info.instance_id),
    );
    finish_dial(live, generation, dialed, true)
}

fn join_saved(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    mode: JoinMode,
    profile_path: PathBuf,
) -> Vec<SessionOutput> {
    let generation = begin(live);
    live.entry_mode = mode;
    let loaded = match load_for_entry(live, &profile_path) {
        Ok(loaded) => loaded,
        Err(message) => return fail_phase(live, generation, message),
    };
    remember_profile(live, &loaded.profile, Some(profile_path));
    remember_loaded(live, loaded);
    publish(&live.shared, live, Vec::new());
    follow_plan(live, commands, generation, mode)
}

fn join_unknown(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    mode: JoinMode,
    target: RoomTarget,
    card_path: PathBuf,
    rules: Vec<Rule>,
) -> Vec<SessionOutput> {
    let generation = begin(live);
    live.entry_mode = mode;
    if let Err(message) = target.validate() {
        return fail_phase(live, generation, message);
    }
    if let Err(message) = rules.iter().try_for_each(Rule::validate) {
        return fail_phase(live, generation, message);
    }
    if live.config.media_bind.ip().is_loopback()
        && !target.address.is_some_and(|ip| ip.is_loopback())
    {
        return address_required(live, generation);
    }
    if !live.global_save.unsaved() {
        let (global, bytes) = match load_global_baseline(&live.config.global_rules_path) {
            Ok(value) => value,
            Err(message) => return fail_phase(live, generation, message),
        };
        live.global = Some(global);
        live.global_bytes = Some(bytes);
        live.global_save = RuleSaveState::Saved;
    }
    remember_target(live, &target);
    live.profile_path = None;
    live.rules = rules.clone();
    live.profile_document = None;
    live.profile_bytes = None;
    live.pending_card = Some(PendingCard {
        path: card_path,
        rules,
        entered_address: target.address,
        entered_port: target.port,
        strict_auto_join: target.strict_auto_join,
    });
    publish(&live.shared, live, Vec::new());
    follow_plan(live, commands, generation, mode)
}

fn follow_plan(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    generation: u64,
    mode: JoinMode,
) -> Vec<SessionOutput> {
    if live.config.media_bind.port() == 0 { return wait_transport(live); }
    live.phase = Phase::Resolving;
    let Some(target) = live.target() else {
        return fail_phase(live, generation, "The room target is missing.".to_owned());
    };
    let plan = match choose_plan(&target, &candidates(live), mode) {
        Ok(plan) => plan,
        Err(error) => {
            let super::super::rooms::SelectionError::InvalidTarget(message) = error;
            return fail_phase(live, generation, message);
        }
    };
    match step_from_plan(plan) {
        NextStep::Connect {
            instance_id,
            candidates,
        } => connect_step(live, commands, generation, candidates, instance_id),
        NextStep::Probe { address } => {
            connect_step(live, commands, generation, vec![address], None)
        }
        NextStep::WaitForChoice { rooms } => {
            live.phase = Phase::Choosing;
            live.choices = rooms.clone();
            live.failure = None;
            vec![output(generation, OutputBody::ChoiceRequired { rooms })]
        }
        NextStep::Blocked { message } => {
            fail_entry(live, generation, EntryFailureKind::AddressRequired, message)
        }
        NextStep::NotFound => entry_unavailable(
            live,
            generation,
            "No discovered room matches the name, and the card has no address to probe.".to_owned(),
        ),
    }
}

fn connect_step(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    generation: u64,
    candidates: Vec<SocketAddr>,
    instance_id: Option<RoomId>,
) -> Vec<SessionOutput> {
    live.phase = Phase::Connecting;
    live.endpoints = candidates.clone();
    let dialed = dial(live, commands, generation, &candidates, instance_id);
    finish_dial(live, generation, dialed, false)
}

fn finish_dial(
    live: &mut Live,
    generation: u64,
    result: Result<Vec<SessionOutput>, DialFailure>,
    release_on_failure: bool,
) -> Vec<SessionOutput> {
    match result {
        Ok(mut outputs) => {
            if !live.shared.is_current(generation) {
                return Vec::new();
            }
            if live.phase == Phase::Joined && live.client.is_some() {
                live.failure = None;
                outputs.extend(save_pending_card(live, generation));
            }
            outputs
        }
        Err(DialFailure::Superseded) => Vec::new(),
        Err(DialFailure::ScopeRequired) => {
            if release_on_failure {
                release_host(live);
            }
            control_scope_required(live, generation)
        }
        Err(DialFailure::Rejected(failure)) => {
            if release_on_failure {
                release_host(live);
            }
            fail_entry(live, generation, failure.kind, failure.message)
        }
        Err(DialFailure::Unavailable(message)) => {
            if release_on_failure {
                release_host(live);
                fail_entry(live, generation, EntryFailureKind::ListenerFailed, message)
            } else {
                entry_unavailable(live, generation, message)
            }
        }
    }
}

fn choose(
    live: &mut Live,
    commands: &Receiver<QueuedCommand>,
    generation: u64,
    instance_id: RoomId,
) -> Vec<SessionOutput> {
    if generation != live.generation || live.phase != Phase::Choosing {
        return Vec::new();
    }
    let Some(room) = live
        .choices
        .iter()
        .find(|room| room.instance_id == instance_id)
        .cloned()
    else {
        return fail_entry(
            live,
            generation,
            EntryFailureKind::SelectionExpired,
            "The selected room is no longer in the current choice.".to_owned(),
        );
    };
    live.room_name = Some(room.name);
    connect_step(
        live,
        commands,
        generation,
        room.candidates,
        Some(room.instance_id),
    )
}

fn leave(live: &mut Live) -> Vec<SessionOutput> {
    let generation = live.generation;
    live.phase = Phase::Leaving;
    live.joined_once = false;
    release_contact(live);
    release_host(live);
    live.creator_bind = None;
    live.view.clear();
    live.endpoints.clear();
    live.instance_id = None;
    live.choices.clear();
    live.pending_card = None;
    live.profile_path = None;
    live.profile_document = None;
    live.profile_bytes = None;
    live.room_save = RuleSaveState::Unloaded;
    live.card_persisted = None;
    live.rules.clear();
    live.playback_rules.clear();
    live.pending_target_change = false;
    live.room_name = None;
    live.saved_address = None;
    live.saved_port = None;
    live.password.clear();
    live.failure = None;
    live.virtual_return = None;
    live.operation_notice = None;
    live.phase = Phase::Idle;
    vec![output(generation, OutputBody::LocalSessionEnded)]
}

fn reload(live: &mut Live) -> Vec<SessionOutput> {
    let Some(path) = live.profile_path.clone() else {
        return match load_global_baseline(&live.config.global_rules_path) {
            Ok((global, bytes)) => {
                live.global = Some(global);
                live.global_bytes = Some(bytes);
                live.global_save = RuleSaveState::Saved;
                let mut outputs = vec![output(
                    live.generation,
                    OutputBody::Reloaded {
                        pending_target_change: live.pending_target_change,
                    },
                )];
                if live.pending_card.is_some() && live.joined_once {
                    outputs.extend(save_pending_card(live, live.generation));
                }
                outputs
            }
            Err(message) => vec![fail(live.generation, message)],
        };
    };
    let loaded = match load_entry(&live.config.global_rules_path, &path) {
        Ok(loaded) => loaded,
        Err(message) => return vec![fail(live.generation, message)],
    };
    let pending = live
        .target()
        .is_some_and(|current| targets_differ(&current, &loaded.profile.room));
    live.rules = loaded.profile.rules.clone();
    live.pending_target_change = pending;
    if !pending {
        remember_profile(live, &loaded.profile, Some(path));
    }
    remember_loaded(live, loaded);
    vec![output(
        live.generation,
        OutputBody::Reloaded {
            pending_target_change: pending,
        },
    )]
}

fn edit_room_rule(live: &mut Live, rule: Rule) -> Vec<SessionOutput> {
    if let Err(message) = rule.validate() {
        return vec![fail(live.generation, message)];
    }
    if live.profile_document.is_none() && live.pending_card.is_none() {
        return vec![fail(
            live.generation,
            "A room rule edit needs a loaded room profile or a pending joined card.".to_owned(),
        )];
    }
    let mut edited = live.rules.clone();
    if let Err(message) = apply_rule_edit(&mut edited, &rule) {
        return vec![fail(live.generation, message)];
    }
    live.rules = edited;
    save_active_room_rules(live)
}

fn effective_settings(live: &Live) -> Result<crate::participant::management::ActiveRoomSettings, String> {
    let profile = match live.profile_path.clone().or_else(|| live.pending_card.as_ref().map(|card| card.path.clone())) {
        Some(path) => {
            let mut profile = if let Some(document) = &live.profile_document {
                document.clone()
            } else if let Some(target) = live.target() {
                RoomProfile { schema: crate::participant::rooms::SCHEMA, room: target, rules: Vec::new(), playback: Vec::new() }
            } else { return Err("The active card has no complete room target to export.".into()); };
            profile.rules = live.rules.clone();
            profile.playback = live.playback_rules.clone();
            profile.validate()?;
            Some((path, profile))
        }
        None => None,
    };
    Ok(crate::participant::management::ActiveRoomSettings {
        global: live.global.clone(),
        profile,
        profile_revision: live.profile_bytes.as_deref().map(crate::participant::management::revision),
        has_unsaved: live.global_save.unsaved() || live.room_save.unsaved(),
        rule_files: live.snapshot().rule_files,
    })
}

fn update_card_target(
    live: &mut Live,
    path: &Path,
    target: RoomTarget,
    expected_revision: &str,
) -> (Result<crate::participant::management::CardDocument, String>, Vec<SessionOutput>) {
    let result = (|| -> Result<crate::participant::management::CardDocument, String> {
        target.validate()?;
        let active = live.profile_path.as_ref().ok_or("The active room has no saved card yet.")?;
        if !crate::participant::management::same_path(active, path) {
            return Err("This card is not the active room card.".into());
        }
        let baseline = live.profile_bytes.as_deref().ok_or("Reload the room card before editing its target.")?;
        if crate::participant::management::revision(baseline) != expected_revision {
            return Err("The card differs from the active room document. Reload it before editing the card target; current unsaved rules remain active.".into());
        }
        let current_target = live.target();
        let document = live.profile_document.as_mut().ok_or("The current card has not been loaded.")?;
        document.room = target;
        live.pending_target_change = current_target.as_ref().is_some_and(|active| targets_differ(active, &document.room));
        live.room_save = RuleSaveState::Pending;
        let saved = persist_room_rules(live);
        live.room_save = match &saved { Ok(()) => RuleSaveState::Saved, Err(error) => RuleSaveState::failed(error.clone()) };
        saved.map_err(|error| error.to_string())?;
        let profile = live.profile_document.clone();
        let revision = crate::participant::management::revision(live.profile_bytes.as_deref().unwrap_or_default());
        Ok(crate::participant::management::CardDocument { path: path.to_owned(), revision, profile, error: None })
    })();
    let outputs = vec![output(live.generation, OutputBody::RulesChanged { persisted: !live.room_save.unsaved() })];
    (result, outputs)
}

/// The existing room worker owns return requests and their normal room rules.
/// Publication and output choices remain independent of this operation.
fn virtual_return_action(live: &mut Live, action: ReturnAction) -> Vec<SessionOutput> {
    if live.phase != Phase::Joined || live.client.is_none() {
        return vec![fail(
            live.generation,
            "Virtual return needs an active room contact connection.".into(),
        )];
    }
    let result = live
        .client
        .as_mut()
        .expect("joined contact")
        .return_action(action);
    let state = match result {
        Ok(state) => state,
        Err(error) => {
            let message = error.to_string();
            if let Some(state) = &mut live.virtual_return {
                state.error = Some(message.clone());
            }
            let mut outputs = vec![fail(live.generation, message)];
            if error.is_local_control_failure() {
                outputs.extend(lose_contact(live));
            }
            return outputs;
        }
    };
    if !live.shared.is_current(live.generation) {
        return Vec::new();
    }
    let member = state.member.clone();
    let error = state.error.clone();
    if let Some(message) = error {
        live.virtual_return = Some(state);
        return vec![fail(live.generation, message)];
    }
    live.virtual_return = if action == ReturnAction::Stop {
        None
    } else {
        Some(state)
    };
    match action {
        ReturnAction::Start => {
            let Some(member) = member else {
                return vec![fail(
                    live.generation,
                    "The contact service did not provide a virtual return member.".into(),
                )];
            };
            live.operation_notice = Some(format!(
                "Virtual return enables sending to and listening to \"{}\" and saves both choices in this room's name rule. Microphone publication and output selections are unchanged.",
                member.display_name
            ));
            edit_room_rule(
                live,
                Rule {
                    r#match: RuleMatch::Condition {
                        name: Some(member.display_name),
                        ip: None,
                    },
                    source_kind: None,
                    send: Some(true),
                    listen: Some(true),
                    outputs: None,
                },
            )
        }
        ReturnAction::Stop => {
            live.operation_notice = Some(
                "Virtual return stopped. Its saved room rules remain available for the next start."
                    .into(),
            );
            Vec::new()
        }
        ReturnAction::Status => Vec::new(),
    }
}

fn set_room_source_mix(
    live: &mut Live,
    condition: RuleMatch,
    source_kind: SourceKind,
    playback: MixPatch,
) -> Vec<SessionOutput> {
    let rule = RoomPlaybackRule {
        condition,
        source_kind,
        playback,
    };
    if let Err(message) = rule.validate() {
        return vec![fail(live.generation, message)];
    }
    if live.profile_document.is_none() && live.pending_card.is_none() {
        return vec![fail(
            live.generation,
            "A room source mix needs a loaded room profile or a pending joined card.".to_owned(),
        )];
    }
    // The rule has already been validated; compact only its assigned fields.
    if let Err(message) = apply_playback_edit(&mut live.playback_rules, rule) {
        return vec![fail(live.generation, message)];
    }
    save_active_room_rules(live)
}

fn save_active_room_rules(live: &mut Live) -> Vec<SessionOutput> {
    live.room_save = RuleSaveState::Pending;
    let generation = live.generation;
    if let Some(card) = &mut live.pending_card {
        card.rules = live.rules.clone();
        let mut outputs = if matches!(live.phase, Phase::Joined | Phase::ContactUnavailable) {
            save_pending_card(live, generation)
        } else {
            Vec::new()
        };
        outputs.insert(
            0,
            output(
                live.generation,
                OutputBody::RulesChanged {
                    persisted: !live.room_save.unsaved(),
                },
            ),
        );
        return outputs;
    }
    let result = persist_room_rules(live);
    live.room_save = match &result {
        Ok(()) => RuleSaveState::Saved,
        Err(error) => RuleSaveState::failed(error.clone()),
    };
    rule_change_outputs(live.generation, result)
}

fn persist_room_rules(live: &mut Live) -> Result<(), SaveFailure> {
    let path = live.profile_path.as_deref().ok_or_else(|| {
        "The current room has no saved profile path. Active rules remain applied.".to_owned()
    })?;
    let mut document = live.profile_document.clone().ok_or_else(|| {
        "The current room profile has not been loaded. Active rules remain applied.".to_owned()
    })?;
    document.rules = live.rules.clone();
    document.playback = live.playback_rules.clone();
    document.validate()?;
    let expected = required_baseline(live.profile_bytes.as_deref())?;
    let bytes = encoded_baseline(&document)?;
    save_room_profile_if_unchanged(path, &document, expected)?;
    live.profile_document = Some(document);
    live.profile_bytes = Some(bytes);
    Ok(())
}

fn replace_global_rules(live: &mut Live, rules: GlobalRules) -> Vec<SessionOutput> {
    if let Err(message) = rules.validate() {
        return vec![fail(live.generation, message)];
    }
    // Capture the current file before replacing active rules. An unreadable
    // baseline still permits the requested live mute/refusal to take effect.
    let baseline_error = if live.global_bytes.is_none() {
        match load_global_baseline(&live.config.global_rules_path) {
            Ok((_, bytes)) => {
                live.global_bytes = Some(bytes);
                None
            }
            Err(message) => Some(message),
        }
    } else {
        None
    };
    live.global = Some(rules);
    live.global_save = RuleSaveState::Pending;
    let result =
        baseline_error.map_or_else(|| persist_global_rules(live), |error| Err(error.into()));
    live.global_save = match &result {
        Ok(()) => RuleSaveState::Saved,
        Err(error) => RuleSaveState::failed(error.clone()),
    };
    rule_change_outputs(live.generation, result)
}

fn set_global_defaults(
    live: &mut Live,
    send: Option<bool>,
    listen: Option<bool>,
) -> Vec<SessionOutput> {
    let edit = Rule {
        outputs: None,
        r#match: RuleMatch::All,
        source_kind: None,
        send,
        listen,
    };
    if let Err(message) = edit.validate() {
        return vec![fail(live.generation, message)];
    }
    if let Err(message) = ensure_global_loaded(live) {
        return vec![fail(live.generation, message)];
    }
    let first = live
        .global
        .as_mut()
        .and_then(|rules| rules.rules.first_mut());
    let Some(first) =
        first.filter(|rule| rule.r#match == RuleMatch::All && rule.source_kind.is_none())
    else {
        return vec![fail(
            live.generation,
            "Global rule 1 must match all members and sources before editing the defaults."
                .to_owned(),
        )];
    };
    if send.is_some() {
        first.send = send;
    }
    if listen.is_some() {
        first.listen = listen;
    }
    save_active_global_rules(live)
}

fn ensure_global_loaded(live: &mut Live) -> Result<(), String> {
    if live.global.is_none() {
        let (rules, bytes) = load_global_baseline(&live.config.global_rules_path)?;
        live.global = Some(rules);
        live.global_bytes = Some(bytes);
        live.global_save = RuleSaveState::Saved;
    }
    Ok(())
}

fn edit_global_rule(live: &mut Live, rule: Rule) -> Vec<SessionOutput> {
    if let Err(message) = rule.validate().and_then(|()| ensure_global_loaded(live)) {
        return vec![fail(live.generation, message)];
    }
    let rules = &mut live.global.as_mut().expect("loaded global rules").rules;
    if let Err(message) = apply_rule_edit(rules, &rule) {
        return vec![fail(live.generation, message)];
    }
    save_active_global_rules(live)
}

fn save_active_global_rules(live: &mut Live) -> Vec<SessionOutput> {
    live.global_save = RuleSaveState::Pending;
    let result = persist_global_rules(live);
    live.global_save = match &result {
        Ok(()) => RuleSaveState::Saved,
        Err(error) => RuleSaveState::failed(error.clone()),
    };
    rule_change_outputs(live.generation, result)
}

fn persist_global_rules(live: &mut Live) -> Result<(), SaveFailure> {
    let rules = live.global.as_ref().ok_or_else(|| {
        "Global rules have not been loaded. Active rules remain applied.".to_owned()
    })?;
    let expected = required_baseline(live.global_bytes.as_deref())?;
    let bytes = encoded_baseline(rules)?;
    save_global_rules_if_unchanged(&live.config.global_rules_path, rules, expected)?;
    live.global_bytes = Some(bytes);
    Ok(())
}

fn rule_change_outputs(generation: u64, result: Result<(), SaveFailure>) -> Vec<SessionOutput> {
    vec![output(
        generation,
        OutputBody::RulesChanged {
            persisted: result.is_ok(),
        },
    )]
}

fn stop(live: &mut Live) -> Vec<SessionOutput> {
    let outputs = leave(live);
    release_discovery(live);
    live.phase = Phase::Stopped;
    outputs
}

fn begin(live: &mut Live) -> u64 {
    live.phase = Phase::Resolving;
    live.room_name = None;
    live.saved_address = None;
    live.saved_port = None;
    live.strict_auto_join = false;
    live.password.clear();
    live.entry_mode = JoinMode::Manual;
    live.joined_once = false;
    release_contact(live);
    release_host(live);
    live.creator_bind = None;
    live.view.clear();
    live.endpoints.clear();
    live.instance_id = None;
    live.choices.clear();
    live.failure = None;
    live.virtual_return = None;
    live.operation_notice = None;
    live.pending_target_change = false;
    live.card_persisted = None;
    live.pending_card = None;
    live.profile_path = None;
    live.profile_document = None;
    live.profile_bytes = None;
    live.rules.clear();
    live.playback_rules.clear();
    live.room_save = RuleSaveState::Unloaded;
    live.generation
}

fn maintain(live: &mut Live) -> Vec<SessionOutput> {
    if !live.shared.is_current(live.generation) {
        return Vec::new();
    }
    let mut outputs = Vec::new();
    if live.phase == Phase::Joined {
        outputs.extend(poll_client(live));
        if live.phase == Phase::Joined && live.last_keepalive.elapsed() >= KEEPALIVE {
            live.last_keepalive = Instant::now();
            let failed = live
                .client
                .as_mut()
                .map(|client| client.keepalive())
                .is_some_and(|result| {
                    result
                        .err()
                        .is_some_and(|error| error.is_local_control_failure())
                });
            if failed {
                outputs.extend(lose_contact(live));
            } else if live.virtual_return.is_some() {
                let before = live.virtual_return.clone();
                outputs.extend(virtual_return_action(live, ReturnAction::Status));
                if live.virtual_return != before {
                    publish(&live.shared, live, Vec::new());
                }
            }
        }
    }
    if live.phase == Phase::ContactUnavailable
        && live.last_recovery.elapsed() >= live.config.recovery_interval
    {
        live.last_recovery = Instant::now();
        outputs.extend(recover(live));
    }
    outputs
}

fn poll_client(live: &mut Live) -> Vec<SessionOutput> {
    let polled = match live.client.as_mut() {
        Some(client) => client.poll(Duration::from_millis(15)),
        None => return Vec::new(),
    };
    match polled {
        Ok(Some(event)) => absorb(live, &event),
        Ok(None) => Vec::new(),
        Err(error) if error.is_local_control_failure() => lose_contact(live),
        Err(_) => Vec::new(),
    }
}

fn recover(live: &mut Live) -> Vec<SessionOutput> {
    let generation = live.generation;
    if live.config.media_bind.port() == 0 { return Vec::new(); }
    if let Some(requested) = live.creator_bind {
        return resume_creator(live, requested);
    }
    let Some(target) = live.target() else {
        return Vec::new();
    };
    let discovered = candidates(live);
    let step = match recovery_step(&target, &discovered, &live.endpoints) {
        Ok(step) => step,
        Err(_) => return Vec::new(),
    };
    match step {
        NextStep::WaitForChoice { rooms } => {
            live.phase = Phase::Choosing;
            live.choices = rooms.clone();
            vec![output(generation, OutputBody::ChoiceRequired { rooms })]
        }
        NextStep::Connect {
            instance_id,
            candidates,
        } => dial_quiet(live, generation, &candidates, instance_id),
        NextStep::Probe { address } => dial_quiet(live, generation, &[address], None),
        NextStep::Blocked { message } => {
            live.phase = Phase::Choosing;
            live.failure = Some(EntryFailure {
                kind: EntryFailureKind::AddressRequired,
                message,
            });
            live.choices = discovered
                .into_iter()
                .filter(|room| room.name == target.name)
                .collect();
            let mut outputs = vec![output(generation, OutputBody::AddressRequired)];
            if !live.choices.is_empty() {
                outputs.push(output(
                    generation,
                    OutputBody::ChoiceRequired {
                        rooms: live.choices.clone(),
                    },
                ));
            }
            outputs
        }
        NextStep::NotFound => Vec::new(),
    }
}

fn dial_quiet(
    live: &mut Live,
    generation: u64,
    candidates: &[SocketAddr],
    instance_id: Option<RoomId>,
) -> Vec<SessionOutput> {
    if candidates.is_empty() {
        return Vec::new();
    }
    match dial_one_loop(live, generation, candidates, instance_id) {
        Ok(mut outputs) => {
            if live.shared.is_current(generation)
                && live.phase == Phase::Joined
                && live.client.is_some()
            {
                live.failure = None;
                outputs.extend(save_pending_card(live, generation));
            }
            outputs
        }
        Err(DialFailure::ScopeRequired) => control_scope_required(live, generation),
        Err(DialFailure::Rejected(failure)) => {
            fail_entry(live, generation, failure.kind, failure.message)
        }
        Err(DialFailure::Unavailable(_) | DialFailure::Superseded) => Vec::new(),
    }
}

fn wait_transport(live: &mut Live) -> Vec<SessionOutput> {
    live.phase = Phase::ContactUnavailable;
    let message = "Waiting for the selected media network; local settings and monitoring remain available.".to_owned();
    live.failure = Some(EntryFailure { kind: EntryFailureKind::Unavailable, message: message.clone() });
    vec![output(live.generation, OutputBody::EntryFailed { kind: EntryFailureKind::Unavailable, message })]
}

fn update_transport(
    live: &mut Live,
    bind: SocketAddr,
    requested_bind: SocketAddr,
    addresses: Vec<SocketAddr>,
    interfaces: Vec<crate::participant::network::NetworkInterface>,
    network: crate::participant::network::NetworkPolicy,
) -> Vec<SessionOutput> {
    if live.config.media_bind == bind && live.config.requested_bind == requested_bind && live.config.media_addresses == addresses && live.config.network == network && live.config.interfaces == interfaces { return Vec::new(); }
    // Address maintenance is not an explicit departure from this room.
    // Existing peer audio elsewhere continues across directory visibility loss.
    live.client = None;
    release_host(live);
    release_discovery(live);
    live.config.media_bind = bind;
    live.config.requested_bind = requested_bind;
    live.config.media_addresses = addresses;
    live.config.interfaces = interfaces;
    live.config.network = network;
    live.config.discovery = live.config.network.for_discovery(requested_bind).unwrap_or_else(|_| {
        super::super::discovery::DiscoveryConfig {
            enable_interfaces: Vec::new(),
            disable_interfaces: vec![super::super::discovery::InterfaceSelector::Ipv4, super::super::discovery::InterfaceSelector::Ipv6],
        }
    });
    live.next_discovery_retry = Instant::now();
    if live.room_name.is_none() { return Vec::new(); }
    let mut outputs = signals_to_outputs(live.generation, mark_all_hidden(&mut live.view));
    outputs.extend(wait_transport(live));
    live.last_recovery = Instant::now().checked_sub(live.config.recovery_interval).unwrap_or_else(Instant::now);
    outputs
}

fn resume_creator(live: &mut Live, requested: SocketAddr) -> Vec<SessionOutput> {
    let generation = live.generation;
    live.entry_mode = JoinMode::Automatic;
    if live.service.is_none() {
        let info = match start_service(live, requested) {
            Ok(info) => info,
            Err(message) => return entry_unavailable(live, generation, message),
        };
        live.instance_id = Some(info.instance_id);
        if !live.local_service.is_some_and(|address| address.ip().is_loopback()) {
            if let Some(error) = advertise(live, &info) { live.discovery_failure = Some(error); }
        }
    }
    let Some(local) = live.local_service else { return Vec::new(); };
    let address = control_address(local);
    live.endpoints = vec![address];
    dial_quiet(live, generation, &[address], live.instance_id)
}

fn dial_one_loop(
    live: &mut Live,
    generation: u64,
    candidates: &[SocketAddr],
    instance_id: Option<RoomId>,
) -> Result<Vec<SessionOutput>, DialFailure> {
    if candidates.is_empty() {
        return Err(DialFailure::Unavailable(
            "The selected room has no contact address.".to_owned(),
        ));
    }
    let mut last = "The contact service did not answer.".to_owned();
    let mut missing_scope = false;
    for address in candidates {
        if !live.shared.is_current(generation) {
            return Err(DialFailure::Superseded);
        }
        match dial_one(live, generation, *address, instance_id) {
            Ok(outputs) => return Ok(outputs),
            Err(Dial::Superseded) => return Err(DialFailure::Superseded),
            Err(Dial::Stop(failure)) => return Err(DialFailure::Rejected(failure)),
            Err(Dial::Skip(message)) => last = message,
            Err(Dial::ScopeRequired) => missing_scope = true,
        }
    }
    if missing_scope {
        Err(DialFailure::ScopeRequired)
    } else {
        Err(DialFailure::Unavailable(last))
    }
}

fn dial(
    live: &mut Live,
    _commands: &Receiver<QueuedCommand>,
    generation: u64,
    candidates: &[SocketAddr],
    instance_id: Option<RoomId>,
) -> Result<Vec<SessionOutput>, DialFailure> {
    dial_one_loop(live, generation, candidates, instance_id)
}

enum Dial {
    Superseded,
    Stop(EntryFailure),
    Skip(String),
    ScopeRequired,
}

/// Terminal policy and temporary transport absence remain distinct through
/// the complete dial path; retry behavior must not depend on error strings.
#[derive(Debug)]
enum DialFailure {
    Superseded,
    ScopeRequired,
    Rejected(EntryFailure),
    Unavailable(String),
}

fn entry_unavailable(live: &mut Live, generation: u64, message: String) -> Vec<SessionOutput> {
    if live.entry_mode == JoinMode::Automatic {
        live.phase = Phase::ContactUnavailable;
        live.failure = Some(EntryFailure {
            kind: EntryFailureKind::Unavailable,
            message: message.clone(),
        });
        live.last_recovery = Instant::now();
        vec![output(
            generation,
            OutputBody::EntryFailed {
                kind: EntryFailureKind::Unavailable,
                message,
            },
        )]
    } else {
        fail_entry(live, generation, EntryFailureKind::Unavailable, message)
    }
}

fn dial_one(
    live: &mut Live,
    generation: u64,
    address: SocketAddr,
    instance_id: Option<RoomId>,
) -> Result<Vec<SessionOutput>, Dial> {
    if !live.shared.is_current(generation) {
        return Err(Dial::Superseded);
    }
    let address = scoped_control_address(address, live.config.local_scope).unwrap_or(address);
    if live.config.media_bind.port() == 0 {
        return Err(Dial::Skip("The media socket is waiting for its selected network.".into()));
    }
    let own_service = live.local_service.is_some_and(|local| local.port() == address.port())
        && live.service.is_some();
    let mut client = if own_service {
        ContactClient::connect(address, live.config.io_timeout)
    } else {
        let stream = live.config.network.connect_tcp(address, live.config.io_timeout)
            .map_err(Dial::Skip)?;
        ContactClient::from_stream(stream, live.config.io_timeout)
    }.map_err(|error| Dial::Skip(error.to_string()))?;
    if let SocketAddr::V6(address) = address {
        if address.scope_id() != 0 { live.config.local_scope = address.scope_id(); }
    }
    if !live.shared.is_current(generation) {
        return Err(Dial::Superseded);
    }
    let info = match client.inspect() {
        Ok(info) => info,
        Err(error) => return Err(classify(error)),
    };
    if !live.shared.is_current(generation) {
        return Err(Dial::Superseded);
    }
    let expected_name = live.room_name.as_deref().unwrap_or_default();
    if info.room_name != expected_name {
        let message = format!(
            "The service at {address} reported room {:?}; expected {:?}.",
            info.room_name, expected_name
        );
        // A direct probe has one explicitly entered address. Waiting cannot
        // correct that target. A discovered instance can have other usable
        // addresses after an old advertisement, so keep its candidate loop.
        return Err(if instance_id.is_none() {
            Dial::Stop(EntryFailure {
                kind: EntryFailureKind::NameMismatch,
                message,
            })
        } else {
            Dial::Skip(message)
        });
    }
    if let Some(expected_instance) = instance_id {
        if info.instance_id != expected_instance {
            return Err(Dial::Skip(
                "The contact service at this address has a different run identity.".to_owned(),
            ));
        }
    }
    let local_ip = client.local_addr().ok().map(|address| address.ip());
    let reply = match client.join(join_params(live, local_ip)) {
        Ok(reply) => reply,
        Err(error) => return Err(classify(error)),
    };
    if !live.shared.is_current(generation) {
        return Err(Dial::Superseded);
    }
    live.instance_id = Some(reply.value.room.instance_id);
    live.endpoints = vec![address];
    live.last_keepalive = Instant::now();
    let mut outputs = signals_to_outputs(
        generation,
        apply_ordered(
            &mut live.view,
            &reply.before,
            &reply.value.members,
            live.config.local_scope,
        ),
    );
    live.client = Some(client);
    live.joined_once = true;
    live.phase = Phase::Joined;
    loop {
        if !live.shared.is_current(generation) {
            live.client = None;
            return Err(Dial::Superseded);
        }
        let polled = match live.client.as_mut() {
            Some(client) => client.poll(Duration::from_millis(1)),
            None => break,
        };
        match polled {
            Ok(Some(event)) => outputs.extend(absorb(live, &event)),
            Ok(None) => break,
            Err(error) if error.is_local_control_failure() => {
                outputs.extend(lose_contact(live));
                break;
            }
            Err(_) => break,
        }
    }
    Ok(outputs)
}

fn classify(error: ContactError) -> Dial {
    match error {
        ContactError::Rejected {
            code: ContactCode::PasswordRejected,
            message,
        } => Dial::Stop(EntryFailure {
            kind: EntryFailureKind::PasswordRejected,
            message,
        }),
        ContactError::Rejected { code: ContactCode::UnsupportedVersion, message } => Dial::Stop(EntryFailure {
            kind: EntryFailureKind::ProtocolMismatch,
            message,
        }),
        ContactError::Rejected { message, .. } => Dial::Stop(EntryFailure {
            kind: EntryFailureKind::ContactRejected,
            message,
        }),
        ContactError::InvalidConfig(message) | ContactError::InvalidRequest(message) => {
            Dial::Stop(EntryFailure {
                kind: EntryFailureKind::Configuration,
                message: message.to_owned(),
            })
        }
        other if other.is_local_control_failure() => Dial::Skip(other.to_string()),
        other => Dial::Skip(other.to_string()),
    }
}

fn absorb(live: &mut Live, event: &ContactEvent) -> Vec<SessionOutput> {
    if !live.shared.is_current(live.generation) {
        return Vec::new();
    }
    if matches!(event, ContactEvent::ServiceStopped) {
        return lose_contact(live);
    }
    if let ContactEvent::Return(state) = event {
        live.virtual_return = Some(state.clone());
        publish(&live.shared, live, Vec::new());
        return state.error.as_ref().map_or_else(Vec::new, |message| {
            vec![fail(live.generation, message.clone())]
        });
    }
    let generation = live.generation;
    let epoch = live.view.epoch();
    let signal = apply_event(&mut live.view, event, epoch, live.config.local_scope);
    match signal {
        Some(Signal::DirectoryLost) => {
            live.phase = Phase::ContactUnavailable;
            live.client = None;
            signals_to_outputs(generation, mark_all_hidden(&mut live.view))
        }
        Some(signal) => signals_to_outputs(generation, vec![signal]),
        None => Vec::new(),
    }
}

fn lose_contact(live: &mut Live) -> Vec<SessionOutput> {
    live.client = None;
    live.phase = Phase::ContactUnavailable;
    live.last_recovery = Instant::now();
    signals_to_outputs(live.generation, mark_all_hidden(&mut live.view))
}

fn save_pending_card(live: &mut Live, generation: u64) -> Vec<SessionOutput> {
    if !live.joined_once {
        return Vec::new();
    }
    let Some(card) = live.pending_card.clone() else {
        return Vec::new();
    };
    if !live.shared.is_current(generation) {
        return Vec::new();
    }
    let runtime = live
        .endpoints
        .first()
        .copied()
        .unwrap_or_else(|| SocketAddr::from((Ipv4Addr::LOCALHOST, 1)));
    let mut profile = match unknown_card(
        live.room_name.as_deref().unwrap_or(""),
        &live.password,
        card.entered_address,
        card.entered_port,
        card.strict_auto_join,
        card.rules.clone(),
        runtime,
    ) {
        Ok(profile) => profile,
        Err(message) => {
            live.card_persisted = Some(false);
            live.room_save = RuleSaveState::WriteFailed {
                detail: message.clone(),
            };
            return vec![output(
                generation,
                OutputBody::CardUnsaved {
                    path: card.path,
                    message,
                },
            )];
        }
    };
    profile.playback = live.playback_rules.clone();
    let bytes = match encoded_baseline(&profile) {
        Ok(bytes) => bytes,
        Err(message) => {
            live.card_persisted = Some(false);
            live.room_save = RuleSaveState::WriteFailed {
                detail: message.clone(),
            };
            return vec![output(
                generation,
                OutputBody::CardUnsaved {
                    path: card.path,
                    message,
                },
            )];
        }
    };
    match persist_new_card(&card.path, &profile, generation, live.shared.generation()) {
        CardWrite::Saved => {
            live.card_persisted = Some(true);
            live.profile_path = Some(card.path.clone());
            live.profile_document = Some(profile);
            live.profile_bytes = Some(bytes);
            live.pending_card = None;
            live.room_save = RuleSaveState::Saved;
            live.rules = card.rules;
            vec![output(
                generation,
                OutputBody::CardSaved { path: card.path },
            )]
        }
        CardWrite::Superseded => Vec::new(),
        CardWrite::Unchanged(message) => {
            live.card_persisted = Some(false);
            live.room_save = RuleSaveState::WriteFailed {
                detail: message.clone(),
            };
            vec![output(
                generation,
                OutputBody::CardUnsaved {
                    path: card.path,
                    message,
                },
            )]
        }
    }
}

fn start_service(live: &mut Live, bind: SocketAddr) -> Result<RoomInfo, String> {
    let config = ContactConfig::try_new(
        bind,
        live.room_name.clone().unwrap_or_default(),
        live.password.clone(),
    )
    .map_err(|error| error.to_string())?;
    let config = config.with_embedded_media(live.config.member_id);
    let listeners = live.config.network.listen_tcp_all(bind)?;
    let service = ContactService::from_listeners(config, listeners).map_err(|error| error.to_string())?;
    let info = service.room_info();
    live.local_service = Some(service.local_addr());
    live.service = Some(service);
    Ok(info)
}

fn advertise(live: &mut Live, info: &RoomInfo) -> Option<String> {
    let room = AdvertisedRoom {
        instance_id: info.instance_id,
        display_name: info.room_name.clone(),
        port: info.port,
    };
    let Some(discovery) = live.discovery.as_ref() else {
        return Some(
            live.discovery_failure
                .clone()
                .unwrap_or_else(|| "Room discovery is not running.".to_owned()),
        );
    };
    let bind_ips: Vec<_> = live.service.as_ref().map(|service| service.local_addrs().iter().map(|address|address.ip()).collect()).unwrap_or_default();
    if let Err(error) = discovery.advertise_bound_all(room.clone(), &bind_ips) {
        return Some(error.to_string());
    }
    live.advertisement = Some(room);
    None
}

fn release_contact(live: &mut Live) {
    if let Some(mut client) = live.client.take() {
        let _ = client.leave();
    }
}

fn release_host(live: &mut Live) {
    if let Some(discovery) = live.discovery.as_ref() {
        let _ = discovery.withdraw();
    }
    live.advertisement = None;
    if let Some(service) = live.service.take() {
        let _ = service.stop();
    }
    live.local_service = None;
}

fn release_discovery(live: &mut Live) {
    if let Some(discovery) = live.discovery.take() {
        let _ = discovery.shutdown();
    }
    live.advertisement = None;
}

fn load_entry(global_path: &Path, profile_path: &Path) -> Result<Loaded, String> {
    let (global, global_bytes) = load_global_baseline(global_path)?;
    let (profile, profile_bytes) = load_baseline(profile_path, RoomProfile::validate)?;
    Ok(Loaded {
        global: Some((global, global_bytes)),
        profile,
        profile_bytes,
    })
}

fn load_for_entry(live: &Live, profile_path: &Path) -> Result<Loaded, String> {
    if !live.global_save.unsaved() {
        return load_entry(&live.config.global_rules_path, profile_path);
    }
    let (profile, profile_bytes) = load_baseline(profile_path, RoomProfile::validate)?;
    Ok(Loaded {
        global: None,
        profile,
        profile_bytes,
    })
}

fn load_global_baseline(path: &Path) -> Result<(GlobalRules, Vec<u8>), String> {
    load_baseline(path, GlobalRules::validate)
}

/// Parse the exact bytes retained for the next save comparison. A separate
/// read after parsing could bless an unrelated external replacement.
fn load_baseline<T: serde::de::DeserializeOwned>(
    path: &Path,
    validate: impl FnOnce(&T) -> Result<(), String>,
) -> Result<(T, Vec<u8>), String> {
    let bytes = std::fs::read(path).map_err(|error| {
        format!(
            "The rules file {} could not be read: {error}",
            path.display()
        )
    })?;
    let value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "The rules file {} contains invalid JSON at line {}, column {}.",
            path.display(),
            error.line(),
            error.column()
        )
    })?;
    validate(&value)?;
    Ok((value, bytes))
}

fn encoded_baseline(value: &impl serde::Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| "The active rules could not be encoded.".to_owned())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn required_baseline(baseline: Option<&[u8]>) -> Result<&[u8], String> {
    baseline.ok_or_else(|| {
        "The rules file has no loaded baseline. Reload it before saving; active rules remain applied."
            .to_owned()
    })
}

fn remember_loaded(live: &mut Live, loaded: Loaded) {
    if let Some((global, bytes)) = loaded.global {
        live.global = Some(global);
        live.global_bytes = Some(bytes);
        live.global_save = RuleSaveState::Saved;
    }
    live.playback_rules = loaded.profile.playback.clone();
    live.profile_document = Some(loaded.profile);
    live.profile_bytes = Some(loaded.profile_bytes);
    live.room_save = RuleSaveState::Saved;
}

fn remember_profile(live: &mut Live, profile: &RoomProfile, path: Option<PathBuf>) {
    remember_target(live, &profile.room);
    live.rules = profile.rules.clone();
    live.playback_rules = profile.playback.clone();
    live.profile_path = path;
}

fn remember_target(live: &mut Live, target: &RoomTarget) {
    live.room_name = Some(target.name.clone());
    live.saved_address = target.address;
    live.saved_port = target.port;
    live.strict_auto_join = target.strict_auto_join;
    live.password = target.password.clone();
}

fn candidates(live: &Live) -> Vec<DiscoveredRoom> {
    live.discovery
        .as_ref()
        .map(|discovery| discovery.snapshot().candidates)
        .unwrap_or_default()
}

fn refresh_discovery(live: &mut Live) -> bool {
    let restarted = maintain_discovery(live);
    let Some(discovery) = live.discovery.as_ref() else {
        return restarted;
    };
    let snapshot = discovery.snapshot();
    let changed =
        live.discovered_rooms != snapshot.candidates || live.discovery_failure != snapshot.failure;
    live.discovered_rooms = snapshot.candidates;
    live.discovery_failure = snapshot.failure;
    changed || restarted
}

fn maintain_discovery(live: &mut Live) -> bool {
    if !live.config.discovery_enabled()
        || live.shared.stop_requested.load(Ordering::Acquire)
        || Instant::now() < live.next_discovery_retry
    {
        return false;
    }
    let failed = live.discovery.as_ref().is_some_and(|discovery| {
        let state = discovery.snapshot();
        state.stopped || !state.browsing
    });
    if live.discovery.is_some() && !failed {
        if live.service.is_some()
            && live.local_service.is_some_and(|address| !address.ip().is_loopback())
            && live.advertisement.is_none()
        {
            live.next_discovery_retry = Instant::now() + live.config.recovery_interval;
            let info = live.service.as_ref().expect("a running service").room_info();
            if let Some(message) = advertise(live, &info) {
                live.discovery_failure = Some(message);
            }
            return true;
        }
        return false;
    }
    live.next_discovery_retry = Instant::now() + live.config.recovery_interval;
    if let Some(discovery) = live.discovery.take() {
        let _ = discovery.shutdown();
    }
    live.advertisement = None;
    match RoomDiscovery::start(live.config.discovery.clone()) {
        Ok(discovery) => {
            live.discovery = Some(discovery);
            live.discovery_failure = None;
            if let Some(service) = live.service.as_ref().filter(|_| {
                live.local_service.is_some_and(|address| !address.ip().is_loopback())
            }) {
                let info = service.room_info();
                if let Some(message) = advertise(live, &info) {
                    live.discovery_failure = Some(message);
                }
            }
        }
        Err(error) => live.discovery_failure = Some(error.to_string()),
    }
    true
}

fn join_params(live: &Live, local_ip: Option<IpAddr>) -> JoinParams {
    // Actual bound sockets are the address authority. The control connection
    // is only a preference among valid media candidates, never a fake binding.
    let offered = crate::participant::network::media_candidates(
        &live.config.network, &live.config.media_addresses, &live.config.interfaces, local_ip);
    let selected = offered.first().copied();
    JoinParams {
        room_name: live.room_name.clone().unwrap_or_default(),
        password: live.password.clone(),
        member_id: live.config.member_id,
        run_id: live.config.run_id,
        display_name: live.config.display_name.clone(),
        media_port: live.config.media_bind.port(),
        media_ip: selected.map(|media| media.ip()),
        media_scope_id: selected.map(|media| media.scope_id()).unwrap_or(0),
        media_alternates: offered.into_iter().skip(1).collect(),
    }
}

// Historical selector fixture. Production uses the actual socket set.
#[cfg(test)]
fn select_creator_media(bound: SocketAddr, contact: SocketAddr) -> Result<MediaSocket, String> {
    let (ip, scope) = if !bound.ip().is_unspecified() {
        (
            bound.ip(),
            match bound {
                SocketAddr::V6(value) => value.scope_id(),
                _ => 0,
            },
        )
    } else if contact.ip().is_loopback() {
        (
            if bound.is_ipv4() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            },
            0,
        )
    } else {
        let mut interfaces = if_addrs::get_if_addrs()
            .map_err(|error| format!("Local network addresses could not be read: {error}"))?;
        interfaces.retain(|item| {
            item.is_oper_up()
                && !item.is_loopback()
                && !item.is_link_local()
                && item.ip().is_ipv4() == bound.is_ipv4()
                && !item.ip().is_unspecified()
        });
        interfaces.sort_by_key(|item| (item.is_p2p(), item.ip()));
        let selected = if contact.ip().is_unspecified() {
            interfaces.first()
        } else {
            interfaces.iter().find(|item| item.ip() == contact.ip())
        }
        .ok_or(
            "No usable local network address matches this room. Select a local interface address.",
        )?;
        (
            selected.ip(),
            if selected.ip().is_ipv6() {
                selected.index.unwrap_or(0)
            } else {
                0
            },
        )
    };
    let media =
        MediaSocket::explicit(ip, bound.port(), scope).map_err(|error| error.to_string())?;
    if !contact.ip().is_loopback() && !usable_lan_media(media) {
        return Err("A LAN contact service requires a non-loopback media address.".into());
    }
    Ok(media)
}

fn control_address(bound: SocketAddr) -> SocketAddr {
    match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv4Addr::LOCALHOST, bound.port()))
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv6Addr::LOCALHOST, bound.port()))
        }
        _ => bound,
    }
}

fn scoped_control_address(address: SocketAddr, local_scope: u32) -> Option<SocketAddr> {
    match address {
        SocketAddr::V6(mut address) if (address.ip().segments()[0] & 0xffc0) == 0xfe80 => {
            if address.scope_id() == 0 {
                if local_scope == 0 {
                    return None;
                }
                address.set_scope_id(local_scope);
            }
            Some(SocketAddr::V6(address))
        }
        _ => Some(address),
    }
}

fn control_scope_required(live: &mut Live, generation: u64) -> Vec<SessionOutput> {
    let message =
        "The IPv6 link-local contact address requires a local interface selection.".to_owned();
    let mut outputs = fail_entry(live, generation, EntryFailureKind::AddressRequired, message);
    outputs.insert(0, output(generation, OutputBody::AddressRequired));
    outputs
}

fn address_required(live: &mut Live, generation: u64) -> Vec<SessionOutput> {
    let message =
        "The local media address must be a reachable address. Loopback is not used as the LAN endpoint."
            .to_owned();
    let mut outputs = fail_entry(live, generation, EntryFailureKind::AddressRequired, message);
    outputs.insert(0, output(generation, OutputBody::AddressRequired));
    outputs
}

fn fail_phase(live: &mut Live, generation: u64, message: String) -> Vec<SessionOutput> {
    fail_entry(live, generation, EntryFailureKind::Configuration, message)
}

fn fail_entry(
    live: &mut Live,
    generation: u64,
    kind: EntryFailureKind,
    message: String,
) -> Vec<SessionOutput> {
    live.phase = Phase::Failed;
    live.failure = Some(EntryFailure {
        kind,
        message: message.clone(),
    });
    vec![output(
        generation,
        OutputBody::EntryFailed { kind, message },
    )]
}

fn fail(generation: u64, message: String) -> SessionOutput {
    output(generation, OutputBody::Failure { message })
}

fn output(generation: u64, body: OutputBody) -> SessionOutput {
    SessionOutput { generation, body }
}

fn signals_to_outputs(generation: u64, signals: Vec<Signal>) -> Vec<SessionOutput> {
    signals
        .into_iter()
        .map(|signal| output(generation, body_from_signal(signal)))
        .collect()
}

fn body_from_signal(signal: Signal) -> OutputBody {
    match signal {
        Signal::Visible(member) => OutputBody::MemberVisible(member),
        Signal::Visibility {
            member_id,
            run_id,
            display_name,
            cause,
        } => OutputBody::ContactVisibility {
            member_id,
            run_id,
            display_name,
            cause,
        },
        Signal::Departure {
            member_id,
            run_id,
            display_name,
        } => OutputBody::ExplicitDeparture {
            member_id,
            run_id,
            display_name,
        },
        Signal::DirectoryLost => OutputBody::DirectoryLost,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::rooms::{
        RuleMatch, create_global_rules_file, create_room_profile_file, load_room_profile,
    };
    use super::*;
    use std::net::SocketAddrV6;

    fn fixture() -> Live {
        let config = SessionConfig::try_new(
            rlb_model::MemberId::from_bytes([1; 16]).unwrap(),
            rlb_model::RunId::from_bytes([2; 16]).unwrap(),
            "member",
            rlb_contact::MediaSocket::explicit("198.51.100.20".parse().unwrap(), 5004, 0).unwrap(),
            PathBuf::from("unused-global.json"),
        )
        .unwrap();
        Live::new(config, std::sync::Arc::new(Shared::new()))
    }

    fn saved_fixture() -> (Live, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("rlb-session-rules-{}", RoomId::generate().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let global = dir.join("global.json");
        let path = dir.join("room.json");
        create_global_rules_file(&global, &GlobalRules::baseline()).unwrap();
        create_room_profile_file(&path, &RoomProfile::create("Headphones").unwrap()).unwrap();
        let mut live = fixture();
        live.config.global_rules_path = global;
        let loaded = load_entry(&live.config.global_rules_path, &path).unwrap();
        remember_profile(&mut live, &loaded.profile, Some(path));
        remember_loaded(&mut live, loaded);
        live.phase = Phase::Joined;
        live.joined_once = true;
        (live, dir)
    }

    fn listen_rule(listen: bool) -> Rule {
        Rule {
            outputs: None,
            r#match: RuleMatch::Condition {
                ip: None,
                name: Some("Ada".to_owned()),
            },
            source_kind: None,
            send: None,
            listen: Some(listen),
        }
    }

    #[test]
    fn output_only_edits_save_reload_and_leave_direction_assignments_untouched() {
        use crate::participant::output_routing::OutputTarget;
        use crate::participant::rooms::{MemberFacts, evaluate_source, load_global_rules};

        let (mut live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        let member = MemberFacts {
            name: "Ada".into(),
            ip: "192.0.2.31".parse().unwrap(),
        };
        edit_room_rule(&mut live, listen_rule(true));
        let output = Rule {
            source_kind: Some(SourceKind::Microphone),
            send: None,
            listen: None,
            outputs: Some(vec![OutputTarget::Endpoint("headphones".into())]),
            ..listen_rule(true)
        };
        assert!(matches!(
            edit_room_rule(&mut live, output.clone())[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        assert_eq!(
            load_room_profile(&path).unwrap().rules.last(),
            Some(&output)
        );
        let room_rule = evaluate_source(&[], &live.rules, &member, SourceKind::Microphone);
        assert!(room_rule.send && room_rule.listen);
        assert_eq!(room_rule.outputs, output.outputs.clone().unwrap());

        let global_output = Rule {
            source_kind: None,
            outputs: Some(vec![
                OutputTarget::Main,
                OutputTarget::Endpoint("cable".into()),
            ]),
            ..output.clone()
        };
        assert!(matches!(
            edit_global_rule(&mut live, global_output.clone())[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        let saved_global = load_global_rules(&live.config.global_rules_path).unwrap();
        assert_eq!(saved_global.rules.last(), Some(&global_output));
        let room_then_global = evaluate_source(
            &saved_global.rules,
            &live.rules,
            &member,
            SourceKind::Microphone,
        );
        assert!(room_then_global.send && room_then_global.listen);
        assert_eq!(room_then_global.outputs, room_rule.outputs);
        assert_eq!(
            evaluate_source(
                &saved_global.rules,
                &live.rules,
                &member,
                SourceKind::System
            )
            .outputs,
            global_output.outputs.clone().unwrap()
        );

        let baseline = std::fs::read(&path).unwrap();
        let mut external = baseline.clone();
        external.push(b' ');
        std::fs::write(&path, &external).unwrap();
        let none = Rule {
            outputs: Some(vec![]),
            ..output.clone()
        };
        assert!(matches!(
            edit_room_rule(&mut live, none)[0].body,
            OutputBody::RulesChanged { persisted: false }
        ));
        assert!(
            evaluate_source(
                &saved_global.rules,
                &live.rules,
                &member,
                SourceKind::Microphone
            )
            .outputs
            .is_empty()
        );
        assert_eq!(std::fs::read(&path).unwrap(), external);
        assert!(matches!(
            reload(&mut live)[0].body,
            OutputBody::Reloaded { .. }
        ));
        assert_eq!(live.rules.last(), Some(&output));
        assert_eq!(live.global.as_ref(), Some(&saved_global));
        assert!(!live.snapshot().rules_unsaved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_rule_edits_save_the_selector_and_reload_after_external_change() {
        use crate::participant::rooms::{MemberFacts, evaluate_source, load_global_rules};

        let (mut live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        edit_room_rule(&mut live, listen_rule(true));
        let mut microphone = listen_rule(false);
        microphone.source_kind = Some(SourceKind::Microphone);
        let outputs = edit_room_rule(&mut live, microphone.clone());
        assert!(matches!(
            outputs[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        let saved = load_room_profile(&path).unwrap();
        assert_eq!(saved.rules, vec![listen_rule(true), microphone]);
        assert_eq!(live.snapshot().rules, saved.rules);

        let mut send = listen_rule(false);
        send.source_kind = Some(SourceKind::Microphone);
        send.send = Some(false);
        send.listen = None;
        assert!(matches!(
            edit_global_rule(&mut live, send.clone())[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        let global = load_global_rules(&live.config.global_rules_path).unwrap();
        assert_eq!(global.rules.last(), Some(&send));
        let member = MemberFacts {
            name: "Ada".into(),
            ip: "192.0.2.31".parse().unwrap(),
        };
        let system = evaluate_source(&global.rules, &live.rules, &member, SourceKind::System);
        assert!(system.send && system.listen);
        let mic = evaluate_source(&global.rules, &live.rules, &member, SourceKind::Microphone);
        assert!(!mic.send && !mic.listen);

        let baseline = std::fs::read(&path).unwrap();
        let mut external = baseline.clone();
        external.push(b' ');
        std::fs::write(&path, &external).unwrap();
        let mut system_closed = listen_rule(false);
        system_closed.source_kind = Some(SourceKind::System);
        assert!(matches!(
            edit_room_rule(&mut live, system_closed)[0].body,
            OutputBody::RulesChanged { persisted: false }
        ));
        assert!(!evaluate_source(&global.rules, &live.rules, &member, SourceKind::System).listen);
        assert_eq!(std::fs::read(&path).unwrap(), external);
        assert_eq!(live.profile_bytes.as_ref(), Some(&baseline));
        assert!(live.snapshot().rule_files.room.outcome.unsaved());

        assert!(matches!(
            reload(&mut live)[0].body,
            OutputBody::Reloaded { .. }
        ));
        assert_eq!(live.rules, saved.rules);
        assert_eq!(live.global.as_ref(), Some(&global));
        assert!(!live.snapshot().rules_unsaved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn room_source_mix_persists_scope_and_role_with_directional_rules() {
        let (mut live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        let condition = listen_rule(true).r#match;
        edit_room_rule(&mut live, listen_rule(true));
        let quiet: MixPatch = MixIntent {
            gain_db: -6.0,
            ..MixIntent::unity()
        }
        .into();
        set_room_source_mix(&mut live, condition.clone(), SourceKind::System, quiet);
        set_room_source_mix(
            &mut live,
            RuleMatch::All,
            SourceKind::System,
            MixIntent::unity().into(),
        );
        let muted: MixPatch = MixIntent {
            mute: true,
            ..MixIntent::unity()
        }
        .into();
        let outputs = set_room_source_mix(&mut live, condition.clone(), SourceKind::System, muted);
        assert!(matches!(
            outputs[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        assert_eq!(live.playback_rules.len(), 2);
        assert_eq!(live.playback_rules[0].condition, RuleMatch::All);
        assert_eq!(live.playback_rules[1].condition, condition);
        assert_eq!(live.playback_rules[1].playback, muted);
        let saved = load_room_profile(&path).unwrap();
        assert_eq!(saved.rules, vec![listen_rule(true)]);
        assert_eq!(saved.playback, live.playback_rules);
        assert_eq!(live.snapshot().playback_rules, saved.playback);

        let mut external = std::fs::read(&path).unwrap();
        external.push(b' ');
        std::fs::write(&path, &external).unwrap();
        let unsaved =
            set_room_source_mix(&mut live, condition.clone(), SourceKind::Microphone, quiet);
        assert!(matches!(
            unsaved[0].body,
            OutputBody::RulesChanged { persisted: false }
        ));
        assert_eq!(live.playback_rules.len(), 3);
        assert_eq!(std::fs::read(&path).unwrap(), external);
        let invalid: MixPatch = MixIntent {
            gain_db: f32::NAN,
            ..MixIntent::unity()
        }
        .into();
        let refused = set_room_source_mix(&mut live, condition, SourceKind::System, invalid);
        assert!(matches!(refused[0].body, OutputBody::Failure { .. }));
        assert_eq!(live.playback_rules.len(), 3);
        reload(&mut live);
        assert_eq!(live.playback_rules, saved.playback);
        assert_eq!(live.rules, saved.rules);
        assert!(!live.snapshot().rules_unsaved);
        assert_eq!(live.generation, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rule_edits_preserve_external_bytes_and_apply_until_explicit_reload() {
        let (mut live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut external = original.clone();
        external.push(b' ');
        std::fs::write(&path, &external).unwrap();
        let outputs = edit_room_rule(&mut live, listen_rule(true));
        assert!(matches!(
            outputs[0].body,
            OutputBody::RulesChanged { persisted: false }
        ));
        assert_eq!(live.rules, vec![listen_rule(true)]);
        assert_eq!(live.phase, Phase::Joined);
        assert!(live.snapshot().rules_unsaved);
        assert!(
            live.snapshot()
                .rules_save_error
                .unwrap()
                .contains("changed outside")
        );
        assert_eq!(std::fs::read(&path).unwrap(), external);
        assert_eq!(live.profile_bytes.as_ref().unwrap(), &original);

        std::fs::write(&path, b"{").unwrap();
        let invalid = reload(&mut live);
        assert!(matches!(invalid[0].body, OutputBody::Failure { .. }));
        assert_eq!(live.rules, vec![listen_rule(true)]);
        assert!(live.snapshot().rules_unsaved);

        std::fs::write(&path, &external).unwrap();
        let reloaded = reload(&mut live);
        assert!(matches!(reloaded[0].body, OutputBody::Reloaded { .. }));
        assert!(live.rules.is_empty());
        assert!(!live.snapshot().rules_unsaved);
        assert_eq!(live.profile_bytes.as_ref().unwrap(), &external);
        let saved = edit_room_rule(&mut live, listen_rule(false));
        assert!(matches!(
            saved[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        assert_eq!(
            load_room_profile(&path).unwrap().rules,
            vec![listen_rule(false)]
        );
        assert_eq!(live.generation, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn typed_file_outcomes_are_independent_and_reload_preserves_active_state_on_failure() {
        let (mut live, dir) = saved_fixture();
        let room_path = live.profile_path.clone().unwrap();
        let original_room = live.profile_bytes.clone().unwrap();
        let original_global = live.global_bytes.clone().unwrap();
        let mut external = original_global.clone();
        external.push(b' ');
        std::fs::write(&live.config.global_rules_path, &external).unwrap();
        edit_global_rule(&mut live, listen_rule(true));
        std::fs::remove_file(&room_path).unwrap();
        edit_room_rule(&mut live, listen_rule(true));
        let files = live.snapshot().rule_files;
        assert!(matches!(
            files.global.outcome,
            RuleSaveState::ExternalChange { .. }
        ));
        assert!(matches!(
            files.room.outcome,
            RuleSaveState::WriteFailed { .. }
        ));
        assert_eq!(files.room.path, Some(room_path.clone()));
        assert_eq!(
            files.global.path,
            Some(live.config.global_rules_path.clone())
        );
        std::fs::write(&room_path, &original_room).unwrap();
        edit_room_rule(&mut live, listen_rule(false));
        assert_eq!(live.room_save, RuleSaveState::Saved);
        assert!(matches!(
            live.global_save,
            RuleSaveState::ExternalChange { .. }
        ));
        std::fs::write(&live.config.global_rules_path, b"{").unwrap();
        let before = live.snapshot();
        assert!(matches!(
            reload(&mut live)[0].body,
            OutputBody::Failure { .. }
        ));
        assert_eq!(live.snapshot(), before);
        std::fs::write(&live.config.global_rules_path, &external).unwrap();
        reload(&mut live);
        assert_eq!(live.room_save, RuleSaveState::Saved);
        assert_eq!(live.global_save, RuleSaveState::Saved);
        assert!(!live.snapshot().rules_unsaved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsaved_globals_survive_failed_entries_until_explicit_reload() {
        let (mut live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        let original = live.global_bytes.clone().unwrap();
        let mut external = original.clone();
        external.push(b' ');
        std::fs::write(&live.config.global_rules_path, &external).unwrap();
        edit_global_rule(&mut live, listen_rule(true));
        let wanted = live.global.clone();
        let baseline = live.global_bytes.clone();
        let failure = live.global_save.clone();
        std::fs::write(&live.config.global_rules_path, b"{").unwrap();
        live.config.media_bind = "0.0.0.0:5004".parse().unwrap();
        let (_sender, receiver) = std::sync::mpsc::channel();
        // An explicit IPv6 contact IP cannot match the IPv4 media candidate
        // set, so preparation fails independently of this PC's interfaces.
        create(
            &mut live,
            &receiver,
            path.clone(),
            "[2001:db8::1]:47101".parse().unwrap(),
        );
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::AddressRequired)
        );
        assert_eq!(live.global, wanted);
        assert_eq!(live.global_bytes, baseline);
        assert_eq!(live.global_save, failure);
        join_saved(&mut live, &receiver, JoinMode::Manual, path.clone());
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::Unavailable)
        );
        assert_eq!(live.global, wanted);
        join_unknown(
            &mut live,
            &receiver,
            JoinMode::Manual,
            RoomTarget {
                name: "Other".into(),
                address: None,
                port: None,
                password: String::new(),
                strict_auto_join: false,
            },
            dir.join("unknown.json"),
            Vec::new(),
        );
        assert_eq!(live.global, wanted);
        leave(&mut live);
        assert_eq!(live.global_save, failure);
        std::fs::write(&live.config.global_rules_path, &original).unwrap();
        reload(&mut live);
        assert_eq!(live.global, Some(GlobalRules::baseline()));
        assert_eq!(live.global_save, RuleSaveState::Saved);
        let mut edited = GlobalRules::baseline();
        edited.rules[0].listen = Some(true);
        crate::participant::rooms::save_global_rules(&live.config.global_rules_path, &edited)
            .unwrap();
        join_saved(&mut live, &receiver, JoinMode::Manual, path);
        assert_eq!(
            live.global,
            Some(edited),
            "clean entry adopts the external file"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn leaving_discards_failed_room_document_and_pending_card_but_not_global_edits() {
        let (mut live, dir) = saved_fixture();
        let path = dir.join("occupied-card.json");
        std::fs::write(&path, b"external file").unwrap();
        live.profile_path = None;
        live.profile_document = None;
        live.profile_bytes = None;
        live.pending_card = Some(PendingCard {
            path: path.clone(),
            rules: vec![listen_rule(true)],
            entered_address: None,
            entered_port: None,
            strict_auto_join: false,
        });
        live.global_save = RuleSaveState::WriteFailed {
            detail: "global write failed".into(),
        };
        save_pending_card(&mut live, 0);
        assert!(live.snapshot().rule_files.room.outcome.unsaved());
        assert_eq!(live.snapshot().rule_files.room.path, Some(path.clone()));
        leave(&mut live);
        let state = live.snapshot();
        assert_eq!(state.rule_files.room.outcome, RuleSaveState::Unloaded);
        assert!(state.rule_files.room.path.is_none());
        assert!(state.card_persisted.is_none());
        assert!(state.rules.is_empty());
        assert!(live.pending_card.is_none());
        assert!(state.rule_files.global.outcome.unsaved());
        assert_eq!(std::fs::read(&path).unwrap(), b"external file");
        reload(&mut live);
        assert_eq!(std::fs::read(&path).unwrap(), b"external file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creator_selection_respects_real_udp_scope_and_recovery_keeps_registration() {
        let (mut live, dir) = saved_fixture();
        let loopback = select_creator_media(
            "0.0.0.0:5004".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(loopback.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(loopback.port(), 5004);
        let bound: SocketAddr = "[fe80::1%7]:5005".parse().unwrap();
        let selected = select_creator_media(bound, "[::]:0".parse().unwrap()).unwrap();
        assert_eq!(selected.scope_id(), 7);
        live.config.media_bind = bound;
        live.config.media_addresses = vec![bound];
        live.local_service = Some("[::]:47101".parse().unwrap());
        let params = join_params(&live, None);
        assert_eq!(params.media_ip, Some(bound.ip()));
        assert_eq!(params.media_port, 5005);
        assert_eq!(params.media_scope_id, 7);
        leave(&mut live);
        // Acquisition/transport bindings have an independent lifetime from a room.
        assert_eq!(join_params(&live, None).media_ip, Some(bound.ip()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_rule_edits_use_current_owner_fields_and_preserve_external_bytes() {
        let (mut live, dir) = saved_fixture();
        let condition = listen_rule(true).r#match;
        edit_global_rule(
            &mut live,
            Rule {
                outputs: None,
                r#match: condition.clone(),
                source_kind: None,
                send: Some(false),
                listen: None,
            },
        );
        let original = std::fs::read(&live.config.global_rules_path).unwrap();
        let mut external = original.clone();
        external.push(b' ');
        std::fs::write(&live.config.global_rules_path, &external).unwrap();
        let outputs = edit_global_rule(&mut live, listen_rule(true));
        assert!(matches!(
            outputs[0].body,
            OutputBody::RulesChanged { persisted: false }
        ));
        let active = live.global.as_ref().unwrap();
        let resolved = crate::participant::rooms::evaluate(
            &active.rules,
            &[],
            &crate::participant::rooms::MemberFacts {
                name: "Ada".into(),
                ip: "192.0.2.1".parse().unwrap(),
            },
        );
        assert!(!resolved.send);
        assert!(resolved.listen);
        assert_eq!(
            std::fs::read(&live.config.global_rules_path).unwrap(),
            external
        );
        assert_eq!(live.global_bytes.as_ref(), Some(&original));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_defaults_patch_current_fields_without_moving_specific_rules() {
        let (mut live, dir) = saved_fixture();
        let mut global = GlobalRules::baseline();
        global.rules[0].send = None;
        global.rules[0].outputs = Some(vec![
            crate::participant::output_routing::OutputTarget::Endpoint("headphones".into()),
        ]);
        let specific = Rule {
            outputs: None,
            r#match: RuleMatch::Condition {
                ip: Some("192.0.2.31".parse().unwrap()),
                name: None,
            },
            source_kind: None,
            send: Some(true),
            listen: Some(false),
        };
        global.rules.push(specific.clone());
        super::super::super::rooms::save_global_rules(&live.config.global_rules_path, &global)
            .unwrap();
        live.global = None;
        live.global_bytes = None;

        let first = set_global_defaults(&mut live, None, Some(true));
        assert!(matches!(
            first[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        assert_eq!(live.global.as_ref().unwrap().rules[0].send, None);
        let second = set_global_defaults(&mut live, Some(false), None);
        assert!(matches!(
            second[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        let saved =
            super::super::super::rooms::load_global_rules(&live.config.global_rules_path).unwrap();
        assert_eq!(saved.rules[0].send, Some(false));
        assert_eq!(saved.rules[0].listen, Some(true));
        assert_eq!(saved.rules[0].outputs, global.rules[0].outputs);
        assert_eq!(saved.rules[1], specific);
        assert_eq!(live.global.as_ref(), Some(&saved));
        let resolved = super::super::super::rooms::evaluate(
            &saved.rules,
            &[],
            &super::super::super::rooms::MemberFacts {
                name: "Music".to_owned(),
                ip: "192.0.2.31".parse().unwrap(),
            },
        );
        assert!(resolved.send);
        assert!(!resolved.listen);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_defaults_reject_bad_initial_document_and_custom_first_rule() {
        let (mut live, dir) = saved_fixture();
        live.global = None;
        live.global_bytes = None;
        std::fs::write(&live.config.global_rules_path, b"{").unwrap();
        assert!(matches!(
            set_global_defaults(&mut live, Some(false), None)[0].body,
            OutputBody::Failure { .. }
        ));
        assert!(live.global.is_none());
        assert!(live.global_bytes.is_none());
        assert_eq!(std::fs::read(&live.config.global_rules_path).unwrap(), b"{");
        let global = GlobalRules {
            schema: 1,
            rules: vec![listen_rule(true)],
        };
        super::super::super::rooms::save_global_rules(&live.config.global_rules_path, &global)
            .unwrap();
        let original = std::fs::read(&live.config.global_rules_path).unwrap();
        assert!(matches!(
            set_global_defaults(&mut live, Some(false), None)[0].body,
            OutputBody::Failure { .. }
        ));
        assert_eq!(live.global.as_ref(), Some(&global));
        assert_eq!(
            std::fs::read(&live.config.global_rules_path).unwrap(),
            original
        );
        assert!(matches!(
            set_global_defaults(&mut live, None, None)[0].body,
            OutputBody::Failure { .. }
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_defaults_refuse_a_source_qualified_all_member_first_rule() {
        let (mut live, dir) = saved_fixture();
        let mut global = GlobalRules::baseline();
        global.rules[0].source_kind = Some(SourceKind::Microphone);
        super::super::super::rooms::save_global_rules(&live.config.global_rules_path, &global)
            .unwrap();
        let original = std::fs::read(&live.config.global_rules_path).unwrap();
        live.global = None;
        live.global_bytes = None;
        assert!(matches!(
            set_global_defaults(&mut live, Some(false), None)[0].body,
            OutputBody::Failure { .. }
        ));
        assert_eq!(live.global.as_ref(), Some(&global));
        assert_eq!(
            std::fs::read(&live.config.global_rules_path).unwrap(),
            original
        );
        assert_eq!(live.global_save, RuleSaveState::Saved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_defaults_keep_successive_unsaved_edits_and_external_bytes() {
        let (mut live, dir) = saved_fixture();
        let original = live.global_bytes.clone().unwrap();
        let mut external = original.clone();
        external.push(b' ');
        std::fs::write(&live.config.global_rules_path, &external).unwrap();
        for (send, listen) in [(Some(false), None), (None, Some(true))] {
            assert!(matches!(
                set_global_defaults(&mut live, send, listen)[0].body,
                OutputBody::RulesChanged { persisted: false }
            ));
        }
        let active = live.global.as_ref().unwrap();
        assert_eq!(
            (active.rules[0].send, active.rules[0].listen),
            (Some(false), Some(true))
        );
        assert!(live.snapshot().rules_unsaved);
        assert_eq!(live.global_bytes.as_ref().unwrap(), &original);
        assert_eq!(
            std::fs::read(&live.config.global_rules_path).unwrap(),
            external
        );
        assert!(matches!(
            reload(&mut live)[0].body,
            OutputBody::Reloaded { .. }
        ));
        assert_eq!(live.global.as_ref(), Some(&GlobalRules::baseline()));
        assert!(!live.snapshot().rules_unsaved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queued_global_defaults_survive_a_room_leave_without_applying_old_room_edits() {
        let (live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        let mut config = live.config;
        // Fail discovery validation before any network work; drive the real
        // worker queue with a room transition already accepted by its handle.
        config.discovery.enable_interfaces.push(
            crate::participant::discovery::InterfaceSelector::Name(String::new()),
        );
        let shared = std::sync::Arc::new(Shared::new());
        *shared.lock_generation() = 1;
        let (sender, receiver) = std::sync::mpsc::channel();
        for (generation, command) in [
            (
                0,
                SessionCommand::SetGlobalDefaults {
                    send: Some(false),
                    listen: None,
                },
            ),
            (
                0,
                SessionCommand::EditRoomRule {
                    rule: listen_rule(true),
                },
            ),
            (1, SessionCommand::Leave),
            (
                1,
                SessionCommand::SetGlobalDefaults {
                    send: None,
                    listen: Some(true),
                },
            ),
        ] {
            sender
                .send(QueuedCommand {
                    generation,
                    command,
                })
                .unwrap();
        }
        drop(sender);
        let global_path = config.global_rules_path.clone();
        run(config, receiver, std::sync::Arc::clone(&shared));
        let global = super::super::super::rooms::load_global_rules(&global_path).unwrap();
        assert_eq!(
            (global.rules[0].send, global.rules[0].listen),
            (Some(false), Some(true))
        );
        assert!(load_room_profile(&path).unwrap().rules.is_empty());
        assert_eq!(shared.snapshot().generation, 1);
        // The earlier global edit may save, but cannot relabel old room
        // admission as the new generation before Leave is processed.
        assert!(matches!(
            shared.pop_output().unwrap().body,
            OutputBody::LocalSessionEnded
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_edit_failure_and_invalid_input_leave_live_state_truthful() {
        let (mut live, dir) = saved_fixture();
        let original = std::fs::read(&live.config.global_rules_path).unwrap();
        let mut external = original.clone();
        external.push(b' ');
        std::fs::write(&live.config.global_rules_path, &external).unwrap();
        let mut global = GlobalRules::baseline();
        global.rules[0].send = Some(false);
        let outputs = replace_global_rules(&mut live, global.clone());
        assert!(matches!(
            outputs[0].body,
            OutputBody::RulesChanged { persisted: false }
        ));
        assert_eq!(live.global, Some(global.clone()));
        assert_eq!(live.global_bytes.as_ref().unwrap(), &original);
        assert_eq!(
            std::fs::read(&live.config.global_rules_path).unwrap(),
            external
        );

        let mut invalid = global.clone();
        invalid.schema = 99;
        assert!(matches!(
            replace_global_rules(&mut live, invalid)[0].body,
            OutputBody::Failure { .. }
        ));
        assert_eq!(live.global, Some(global.clone()));
        std::fs::write(&live.config.global_rules_path, &original).unwrap();
        let retried = replace_global_rules(&mut live, global);
        assert!(matches!(
            retried[0].body,
            OutputBody::RulesChanged { persisted: true }
        ));
        assert!(!live.snapshot().rules_unsaved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_discovery_retains_only_automatic_entry_intent() {
        let (mut live, dir) = saved_fixture();
        let (_sender, receiver) = std::sync::mpsc::channel();
        live.entry_mode = JoinMode::Automatic;
        follow_plan(&mut live, &receiver, 0, JoinMode::Automatic);
        assert_eq!(live.phase, Phase::ContactUnavailable);
        assert!(live.target().is_some());
        live.entry_mode = JoinMode::Manual;
        follow_plan(&mut live, &receiver, 0, JoinMode::Manual);
        assert_eq!(live.phase, Phase::Failed);
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::Unavailable)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn entry_failure_facts_distinguish_listener_configuration_and_required_address() {
        let (mut live, dir) = saved_fixture();
        let path = live.profile_path.clone().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (_sender, receiver) = std::sync::mpsc::channel();
        create(&mut live, &receiver, path, address);
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::ListenerFailed)
        );
        assert!(live.snapshot().failure.unwrap().contains("bind failed"));
        create(&mut live, &receiver, dir.join("absent.json"), address);
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::Configuration)
        );
        assert!(
            live.snapshot().room_name.is_none(),
            "a failed new entry must not name the retired room"
        );
        control_scope_required(&mut live, 0);
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::AddressRequired)
        );
        let Dial::Stop(failure) = classify(ContactError::InvalidRequest("invalid local join"))
        else {
            panic!("a local invalid request must not become a temporary absence");
        };
        live.entry_mode = JoinMode::Automatic;
        finish_dial(&mut live, 0, Err(DialFailure::Rejected(failure)), false);
        assert_eq!(live.phase, Phase::Failed);
        assert_eq!(
            live.snapshot().failure_kind,
            Some(EntryFailureKind::Configuration)
        );
        leave(&mut live);
        assert!(live.snapshot().failure_kind.is_none());
        assert!(live.snapshot().failure.is_none());
        drop(listener);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_card_failure_retains_retry_and_current_rules() {
        let (mut live, dir) = saved_fixture();
        let path = dir.join("unknown.json");
        std::fs::write(&path, b"external document").unwrap();
        live.profile_path = None;
        live.profile_document = None;
        live.profile_bytes = None;
        live.pending_card = Some(PendingCard {
            path: path.clone(),
            rules: Vec::new(),
            entered_address: None,
            entered_port: None,
            strict_auto_join: false,
        });
        let unsaved = save_pending_card(&mut live, 0);
        assert!(matches!(unsaved[0].body, OutputBody::CardUnsaved { .. }));
        assert!(live.pending_card.is_some());
        edit_room_rule(&mut live, listen_rule(true));
        assert_eq!(std::fs::read(&path).unwrap(), b"external document");
        assert_eq!(live.rules, vec![listen_rule(true)]);
        std::fs::remove_file(&path).unwrap();
        let saved = reload(&mut live);
        assert!(
            saved
                .iter()
                .any(|notice| matches!(notice.body, OutputBody::CardSaved { .. }))
        );
        assert!(live.pending_card.is_none());
        assert_eq!(live.profile_path, Some(path.clone()));
        assert_eq!(
            live.profile_bytes.as_ref().unwrap(),
            &std::fs::read(&path).unwrap()
        );
        let profile = load_room_profile(&path).unwrap();
        assert_eq!(profile.rules, vec![listen_rule(true)]);
        assert!(profile.room.address.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_late_success_does_not_publish_or_save_a_card() {
        let mut live = fixture();
        live.generation = 1;
        *live.shared.lock_generation() = 2;
        let path = std::env::temp_dir().join(format!(
            "rlb-c16-cancelled-{}.json",
            rlb_model::RoomId::generate().unwrap()
        ));
        live.pending_card = Some(PendingCard {
            path: path.clone(),
            rules: Vec::new(),
            entered_address: None,
            entered_port: None,
            strict_auto_join: false,
        });
        live.room_name = Some("Headphones".to_owned());
        let outputs = finish_dial(
            &mut live,
            1,
            Ok(vec![output(1, OutputBody::DirectoryLost)]),
            false,
        );
        assert!(outputs.is_empty());
        assert!(save_pending_card(&mut live, 1).is_empty());
        assert!(!path.exists());
        assert!(live.client.is_none());
    }

    #[test]
    fn lost_join_reply_is_not_reported_as_joined() {
        let mut live = fixture();
        live.phase = Phase::ContactUnavailable;
        let _ = finish_dial(&mut live, 0, Ok(Vec::new()), false);
        assert_eq!(live.phase, Phase::ContactUnavailable);
        assert!(live.client.is_none());
    }

    #[test]
    fn control_ipv6_scope_is_local_and_explicit_scope_is_preserved() {
        let ip = "fe80::1234".parse::<Ipv6Addr>().unwrap();
        let explicit = SocketAddr::V6(SocketAddrV6::new(ip, 47101, 0, 3));
        assert_eq!(control_address(explicit), explicit);
        assert_eq!(scoped_control_address(explicit, 7), Some(explicit));
        let unspecified_scope = SocketAddr::new(ip.into(), 47101);
        assert_eq!(scoped_control_address(unspecified_scope, 0), None);
        assert_eq!(
            scoped_control_address(unspecified_scope, 7),
            Some(SocketAddr::V6(SocketAddrV6::new(ip, 47101, 0, 7)))
        );
        for address in ["192.0.2.10:47101", "[2001:db8::1]:47101", "[::1]:47101"] {
            let address = address.parse::<SocketAddr>().unwrap();
            assert_eq!(scoped_control_address(address, 0), Some(address));
        }
        assert_eq!(
            control_address("[::]:47101".parse().unwrap()),
            "[::1]:47101".parse::<SocketAddr>().unwrap()
        );
    }
}
