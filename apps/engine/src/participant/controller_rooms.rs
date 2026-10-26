//! Shared application room owner. Disk preferences, directory facts and
//! effective media routes have independent lifetimes.

use std::path::PathBuf;

use serde::Serialize;

use super::{
    Command, DirectoryStatus, ParticipantConfig, Resume, SavedContact, SavedMember, Worker,
};
use crate::participant::config::{MixIntent, RoomStartup, SourceKind, SourceMixOverride};
use crate::participant::output_routing::SourceOutputTargets;
use crate::participant::room_session::{
    OutputBody, Phase, RoomSession, SessionCommand, SessionConfig, SessionSnapshot,
};
use crate::participant::rooms::{
    JoinMode, MemberFacts, RoomCardView, RoomLibrary, RoomProfile, Rule, RuleMatch,
    evaluate_playback,
};
use crate::participant::routing::{PeerRoute, project_peers_on_paths};
#[cfg(test)]
use crate::participant::routing::project_peers;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RoomControlState {
    pub session: Option<SessionSnapshot>,
    pub cards: Vec<RoomCardView>,
    pub routes: Vec<PeerRoute>,
    pub global_rules_path: PathBuf,
    pub auto_enter: bool,
    pub error: Option<String>,
}

pub(super) struct RoomControl {
    pub(super) session: RoomSession,
    library: RoomLibrary,
    current: SessionSnapshot,
    expected_generation: u64,
    media_epoch: u64,
    pub(super) active: bool,
    cards: Vec<RoomCardView>,
    routes: Vec<PeerRoute>,
    error: Option<String>,
    catalogues: Vec<(rlb_model::MemberId, rlb_peer::Catalogue)>,
    joined_once: bool,
    output_loss_seen: u64,
    pending_startup: Option<RoomStartup>,
}

impl RoomControl {
    fn reset_room_view(&mut self, generation: u64, phase: Phase) {
        let mut next = SessionSnapshot::idle();
        next.generation = generation;
        next.phase = phase;
        next.global_rules = self.current.global_rules.clone();
        next.rule_files.global = self.current.rule_files.global.clone();
        next.rules_unsaved = next.rule_files.global.outcome.unsaved();
        next.rules_save_error = next.rule_files.global.outcome.detail().map(str::to_owned);
        self.current = next;
    }

    fn start(worker: &Worker) -> Result<Self, String> {
        let library = RoomLibrary::new(&worker.path);
        library.initialize()?;
        let transport = worker.media.handle.snapshot();
        let mut config = SessionConfig::for_preferences(
            worker.config.member_id,
            worker.run_id,
            &worker.config.display_name,
            worker.local_address,
            library.global_rules_path(),
        )?;
        config.requested_bind = worker.config.bind;
        config = config.with_network(worker.config.network.clone());
        config.media_addresses = transport.local_addresses.clone();
        config.interfaces = transport.network.interfaces.clone();
        let session = RoomSession::start(config)?;
        let current = session.snapshot();
        let cards = library.cards()?;
        Ok(Self {
            session,
            library,
            current,
            expected_generation: 0,
            media_epoch: worker.room_epoch,
            active: false,
            cards,
            routes: Vec::new(),
            error: None,
            catalogues: Vec::new(),
            joined_once: false,
            output_loss_seen: 0,
            pending_startup: None,
        })
    }

    fn refresh_cards(&mut self) {
        match self.library.cards() {
            Ok(cards) => self.cards = cards,
            Err(error) => self.error = Some(error),
        }
    }

    fn joined(&self) -> bool {
        self.active && self.current.generation == self.expected_generation && self.joined_once
    }

    fn project(&self, base: &ParticipantConfig, media: &super::MediaSnapshot) -> ParticipantConfig {
        if !self.active {
            return base.clone();
        }
        let mut result = base.clone();
        result.group.name = self.current.room_name.clone().unwrap_or_default();
        result.group.joined = self.joined();
        result.members.clear();
        result.contacts.clear();
        if result.group.joined {
            for route in &self.routes {
                let mut member = SavedMember::new(route.remote.member, &route.name);
                member.name.clone_from(&route.name);
                member.send = route.send;
                member.receive = route.listen;
                member.source_policy = Some(route.source_policy());
                member.source_output_targets = route
                    .sources
                    .iter()
                    .map(|source| SourceOutputTargets {
                        kind: source.kind,
                        targets: source.outputs.clone(),
                    })
                    .collect();
                if let (Some(address), Some(catalogue)) = (
                    route.address(),
                    media
                        .peers
                        .get(&route.remote.member)
                        .and_then(|peer| peer.catalogue.as_ref()),
                ) {
                    let facts = MemberFacts {
                        name: route.name.clone(),
                        ip: address.ip(),
                    };
                    for source in catalogue.as_slice() {
                        let kind = match source.kind {
                            rlb_peer::SourceKind::System => SourceKind::System,
                            rlb_peer::SourceKind::Microphone => SourceKind::Microphone,
                        };
                        if let Some(playback) =
                            evaluate_playback(&self.current.playback_rules, &facts, kind)
                        {
                            member.source_overrides.push(SourceMixOverride {
                                source_id: source.id,
                                playback,
                            });
                        }
                    }
                }
                result.members.push(member);
                if route.address().is_some() {
                    result.contacts.push(SavedContact {
                        member_id: route.remote.member,
                        address: route.preferred_address(),
                        alternates: route.alternate_addresses(),
                        resolved_for: route.resolved_for,
                    });
                }
            }
        }
        result
    }
}

impl Worker {
    /// Export the live startup intent even when the newly joined card has not
    /// reached disk. The room session still owns its pending card document.
    pub(super) fn effective_export_config(&self) -> ParticipantConfig {
        let mut config = self.config.clone();
        if let Some(room) = &self.rooms {
            if room.joined() {
                if let Some(startup) = &room.pending_startup {
                    config.room.selected = Some(startup.clone());
                    config.room.auto_enter = true;
                }
            }
        }
        config
    }

    pub(super) fn ensure_rooms(&mut self) -> Result<(), String> {
        if self.rooms.is_none() {
            self.rooms = Some(RoomControl::start(self)?);
        }
        self.room_error = None;
        Ok(())
    }

    pub(super) fn room_active(&self) -> bool {
        self.rooms.as_ref().is_some_and(|room| room.active)
    }

    pub(super) fn retry_creator_address(&self) -> bool {
        matches!(self.config.room.selected, Some(RoomStartup::Create { .. }))
            && self.rooms.as_ref().is_some_and(|room| {
                room.active
                    && !room.joined_once
                    && room.current.generation == room.expected_generation
                    && room.current.phase == Phase::Failed
                    && room.current.failure_kind
                        == Some(crate::participant::room_session::EntryFailureKind::AddressRequired)
            })
    }

    pub(super) fn media_config(&self) -> ParticipantConfig {
        let Some(room) = &self.rooms else {
            return self.config.clone();
        };
        if room.active
            && (room.media_epoch != self.room_epoch
                || self.room_epoch != self.media.handle.room_fence().epoch)
        {
            let mut closed = self.config.clone();
            closed.group.joined = false;
            closed.members.clear();
            closed.contacts.clear();
            return closed;
        }
        room.project(&self.config, &self.media.handle.snapshot())
    }

    pub(super) fn configure_media(&mut self, resume: Resume) -> Result<(), String> {
        self.media
            .handle
            .configure_in(&self.media_config(), resume, self.room_epoch)
    }

    pub(super) fn room_state(&self) -> RoomControlState {
        RoomControlState {
            session: self.rooms.as_ref().map(|room| room.current.clone()),
            cards: self
                .rooms
                .as_ref()
                .map(|room| room.cards.clone())
                .unwrap_or_else(|| self.room_cards.clone()),
            routes: self
                .rooms
                .as_ref()
                .map(|room| room.routes.clone())
                .unwrap_or_default(),
            global_rules_path: RoomLibrary::new(&self.path).global_rules_path(),
            auto_enter: self.config.room.auto_enter,
            error: self
                .rooms
                .as_ref()
                .and_then(|room| room.error.clone())
                .or_else(|| self.room_error.clone()),
        }
    }

    pub(super) fn room_phase(&self) -> &'static str {
        let Some(room) = self.rooms.as_ref().filter(|room| room.active) else {
            return if self.config.group.joined {
                "direct-contacts"
            } else {
                "idle"
            };
        };
        match room.current.phase {
            Phase::Idle => "idle",
            Phase::Resolving => "resolving",
            Phase::Choosing => "choosing",
            Phase::Connecting => "connecting",
            Phase::Joined => "joined",
            Phase::ContactUnavailable => "contact_unavailable",
            Phase::Leaving => "leaving",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
        }
    }

    pub(super) fn restore_room(&mut self) -> Result<(), String> {
        if !self.config.room.auto_enter {
            return Ok(());
        }
        let Some(selected) = self.config.room.selected.clone() else {
            return Ok(());
        };
        let command = match selected {
            RoomStartup::Join { profile_path } => Command::JoinSavedRoom {
                profile_path,
                mode: JoinMode::Automatic,
            },
            RoomStartup::Create { profile_path, bind } => {
                Command::CreateSavedRoom { profile_path, bind }
            }
        };
        self.room_epoch = self.media.handle.submit_scope_if_current(
            self.room_epoch,
            super::RoomTransition::Enter,
            Ok,
        )?;
        self.apply_room_command(command)?;
        Ok(())
    }

    pub(super) fn apply_room_command(
        &mut self,
        command: Command,
    ) -> Result<super::CommandOutcome, String> {
        let name = command.name().to_owned();
        if matches!(command, Command::RefreshRooms) {
            let library = RoomLibrary::new(&self.path);
            library.initialize()?;
            self.room_cards = library.cards()?;
            if let Some(room) = &mut self.rooms {
                room.cards.clone_from(&self.room_cards);
            }
            return Ok(self.outcome(
                &name,
                true,
                super::AudioEffect::NotApplicable,
                "Saved cards were refreshed independently of network availability.",
            ));
        }
        let entering = matches!(
            command,
            Command::CreateRoom { .. }
                | Command::CreateSavedRoom { .. }
                | Command::JoinSavedRoom { .. }
                | Command::JoinUnknownRoom { .. }
        );
        if entering {
            // Enter already changed the immediate media fence. Invalidate the
            // old projection before file I/O or any other fallible operation.
            self.config.group.joined = false;
            if let Some(room) = &mut self.rooms {
                room.active = true;
                room.joined_once = false;
                room.expected_generation = u64::MAX;
                room.routes.clear();
            }
        }
        self.ensure_rooms()?;
        let room = self.rooms.as_mut().expect("room owner");
        let mut startup = None;
        let mut entry = false;
        let unknown_entry = matches!(command, Command::JoinUnknownRoom { .. });
        let session_command = match command {
            Command::CreateRoom { profile, bind } => {
                let path = room.library.create_card(&profile)?;
                startup = Some(RoomStartup::Create {
                    profile_path: path.clone(),
                    bind,
                });
                entry = true;
                SessionCommand::Create {
                    profile_path: path,
                    bind,
                }
            }
            Command::CreateSavedRoom { profile_path, bind } => {
                startup = Some(RoomStartup::Create {
                    profile_path: profile_path.clone(),
                    bind,
                });
                entry = true;
                SessionCommand::Create { profile_path, bind }
            }
            Command::JoinSavedRoom { profile_path, mode } => {
                startup = Some(RoomStartup::Join {
                    profile_path: profile_path.clone(),
                });
                entry = true;
                SessionCommand::JoinSaved { profile_path, mode }
            }
            Command::JoinUnknownRoom { target } => {
                let path = room.library.allocate_card_path()?;
                startup = Some(RoomStartup::Join {
                    profile_path: path.clone(),
                });
                entry = true;
                SessionCommand::JoinUnknown {
                    mode: JoinMode::Manual,
                    target,
                    card_path: path,
                    rules: Vec::new(),
                }
            }
            Command::ChooseRoom {
                generation,
                instance_id,
            } => SessionCommand::Choose {
                generation,
                instance_id,
            },
            Command::LeaveRoom | Command::LeaveGroup => SessionCommand::Leave,
            Command::CancelRoom => SessionCommand::Cancel,
            Command::ReloadRoomRules => SessionCommand::Reload,
            Command::StartVirtualReturn => SessionCommand::StartVirtualReturn,
            Command::StopVirtualReturn => SessionCommand::StopVirtualReturn,
            Command::EditRoomRule { rule } => SessionCommand::EditRoomRule { rule },
            Command::EditGlobalRule { rule } => SessionCommand::EditGlobalRule { rule },
            Command::SetRoomSourceMix {
                condition,
                source_kind,
                playback,
            } => SessionCommand::SetRoomSourceMix {
                condition,
                source_kind,
                playback,
            },
            Command::ReplaceGlobalRules { rules } => SessionCommand::ReplaceGlobalRules { rules },
            Command::SetGlobalDefaults { send, listen } => {
                SessionCommand::SetGlobalDefaults { send, listen }
            }
            _ => return Err("This command does not belong to room control.".into()),
        };
        let leaving = matches!(
            session_command,
            SessionCommand::Leave | SessionCommand::Cancel
        );
        let generation = room
            .session
            .submit_generation(session_command)
            .map_err(|error| error.to_string())?;
        room.expected_generation = generation;
        room.media_epoch = self.room_epoch;
        room.error = None;
        if entry || leaving {
            room.active = true;
            room.joined_once = false;
            room.routes.clear();
            room.catalogues.clear();
            room.pending_startup = None;
            room.reset_room_view(
                generation,
                if entry { Phase::Resolving } else { Phase::Idle },
            );
            // Runtime facts from the direct-contact diagnostic mode cannot
            // become persistent room rules or implicit permissions.
            self.config.group.joined = false;
            self.config.contacts.clear();
            self.config.members.clear();
            self.config.room.auto_enter = entry && !unknown_entry;
            if unknown_entry {
                room.pending_startup = startup;
            } else if let Some(selected) = startup {
                self.config.room.selected = Some(selected);
            }
            self.advance_group_generation()?;
            self.directory = DirectoryStatus::Unavailable;
            self.configure_media(Resume::Automatic)?;
            self.persist();
        }
        if let Some(room) = &mut self.rooms {
            room.refresh_cards();
        }
        Ok(self.outcome(
            &name,
            true,
            super::AudioEffect::Requested,
            "The room command was queued. Observe room phase and rule-save state for completion.",
        ))
    }

    pub(super) fn observe_rooms(&mut self) -> Result<(), String> {
        let media = self.media.handle.snapshot();
        let mut catalogues = media
            .peers
            .iter()
            .filter_map(|(id, peer)| peer.catalogue.clone().map(|catalogue| (*id, catalogue)))
            .collect::<Vec<_>>();
        catalogues.sort_by_key(|(id, _)| *id);
        let Some(room) = &mut self.rooms else {
            return Ok(());
        };
        let mut departed = Vec::new();
        let mut refresh_cards = false;
        while let Some(output) = room.session.poll_output() {
            if output.generation != room.expected_generation {
                continue;
            }
            match output.body {
                OutputBody::ExplicitDeparture {
                    member_id, run_id, ..
                } => {
                    if room
                        .routes
                        .iter()
                        .any(|route| route.remote.member == member_id && route.remote.run == run_id)
                    {
                        departed.push(member_id);
                    }
                }
                OutputBody::LocalSessionEnded => room.joined_once = false,
                // Entry failure/recovery is projected from the latest session;
                // do not retain its old message as a separate operation error.
                OutputBody::EntryFailed { .. } => {}
                OutputBody::Failure { message } => room.error = Some(message),
                OutputBody::Reloaded { .. } => {
                    room.error = None;
                    refresh_cards = true;
                }
                OutputBody::RulesChanged { persisted } => {
                    room.error = None;
                    refresh_cards |= persisted;
                }
                _ => {}
            }
        }
        if refresh_cards {
            room.refresh_cards();
        }
        let next = room.session.snapshot();
        if next.generation != room.expected_generation
            || (room.active && room.media_epoch != self.room_epoch)
        {
            return Ok(());
        }
        if next.output_loss_revision != room.output_loss_seen {
            // A missed terminal edge cannot preserve an old pair interval.
            departed.extend(room.routes.iter().map(|route| route.remote.member));
            room.output_loss_seen = next.output_loss_revision;
        }
        let catalogue_changed = room.catalogues != catalogues;
        let playback_changed = room.current.playback_rules != next.playback_rules;
        let return_changed = room.current.virtual_return != next.virtual_return;
        let return_state = next.virtual_return.clone();
        let selected_path = |remote: rlb_peer::MemberRun| {
            let peer = media.peers.get(&remote.member)?;
            let status = peer.status;
            if !status.connected || status.remote_run != Some(remote.run) { return None; }
            Some(super::config::ResolvedPath { address: peer.address, session: status.session?, remote_run: remote.run })
        };
        let paths_changed = room.routes.iter().any(|route| route.resolved_for != selected_path(route.remote));
        if next == room.current && departed.is_empty() && !catalogue_changed && !paths_changed {
            return Ok(());
        }
        room.catalogues = catalogues;
        let old_routes = room.routes.clone();
        let was_joined = room.joined();
        let old_card = room.current.card_persisted;
        if !departed.is_empty() {
            room.routes
                .retain(|route| !departed.contains(&route.remote.member));
            self.configure_media(Resume::Automatic)?;
        }
        let room = self.rooms.as_mut().expect("room owner");
        room.current = next;
        if room.current.phase == Phase::Joined {
            room.joined_once = true;
        }
        let saved_startup = if room.current.card_persisted == Some(true) {
            room.pending_startup.take()
        } else {
            None
        };
        room.routes = project_peers_on_paths(self.config.member_id, &room.current, selected_path);
        if old_card != room.current.card_persisted {
            room.refresh_cards();
        }
        self.directory = match room.current.phase {
            Phase::Joined => DirectoryStatus::Available,
            Phase::ContactUnavailable => DirectoryStatus::Stale,
            _ => DirectoryStatus::Unavailable,
        };
        let routes_changed = old_routes.len() != room.routes.len()
            || old_routes
                .iter()
                .zip(&room.routes)
                .any(|(old, new)| !old.same_media_settings(new));
        let changed = paths_changed || routes_changed
            || was_joined != room.joined()
            || !departed.is_empty()
            || catalogue_changed
            || playback_changed;
        let reset = room
            .routes
            .iter()
            .filter(|route| {
                departed.contains(&route.remote.member)
                    || !old_routes
                        .iter()
                        .any(|old| old.remote.member == route.remote.member)
                    || old_routes.iter().any(|old| {
                        old.remote.member == route.remote.member
                            && (old.remote.run != route.remote.run
                                || old.endpoint != route.endpoint)
                    })
            })
            .map(|route| {
                (
                    route.remote.member,
                    departed.contains(&route.remote.member)
                        || old_routes
                            .iter()
                            .any(|old| old.remote.member == route.remote.member),
                )
            })
            .collect::<Vec<_>>();
        if changed {
            self.configure_media(Resume::Automatic)?;
            for (id, reset) in reset {
                self.configure_media(Resume::Member(id, reset))?;
            }
        }
        if return_changed {
            self.media
                .handle
                .configure_return_in(return_state, self.room_epoch)?;
        }
        if let Some(startup) = saved_startup {
            self.config.room.selected = Some(startup);
            self.config.room.auto_enter = true;
            self.persist();
        }
        Ok(())
    }

    pub(super) fn edit_member_rule(
        &mut self,
        member_id: rlb_model::MemberId,
        send: bool,
        listen: bool,
    ) -> Result<super::CommandOutcome, String> {
        let room = self
            .rooms
            .as_ref()
            .ok_or("Join a room before changing its member rules.")?;
        let member = room
            .routes
            .iter()
            .find(|route| route.remote.member == member_id)
            .ok_or("The member is no longer in the current room.")?;
        self.apply_room_command(Command::EditRoomRule {
            rule: Rule {
                r#match: RuleMatch::Condition {
                    ip: None,
                    name: Some(member.name.clone()),
                },
                source_kind: None,
                send: Some(send),
                listen: Some(listen),
                outputs: None,
            },
        })
    }

    pub(super) fn stop_room(&mut self) {
        if let Some(room) = self.rooms.take() {
            self.room_cards.clone_from(&room.cards);
            let _ = room.session.stop();
        }
    }

    pub(super) fn edit_room_source_mix(
        &mut self,
        member: rlb_model::MemberId,
        source: rlb_model::SourceId,
        playback: MixIntent,
    ) -> Result<super::CommandOutcome, String> {
        let room = self
            .rooms
            .as_ref()
            .ok_or("Join a room before changing its source mix.")?;
        let route = room
            .routes
            .iter()
            .find(|route| route.remote.member == member)
            .ok_or("The member is no longer in this room.")?;
        let media = self.media.handle.snapshot();
        let entry = media
            .peers
            .get(&member)
            .and_then(|peer| peer.catalogue.as_ref())
            .and_then(|catalogue| catalogue.source(source))
            .ok_or("The selected source is not in this member's current catalogue.")?;
        let source_kind = match entry.kind {
            rlb_peer::SourceKind::System => SourceKind::System,
            rlb_peer::SourceKind::Microphone => SourceKind::Microphone,
        };
        self.apply_room_command(Command::SetRoomSourceMix {
            condition: RuleMatch::Condition {
                ip: None,
                name: Some(route.name.clone()),
            },
            source_kind,
            playback: playback.into(),
        })
    }

    pub(super) fn close_room_from_fence(&mut self) {
        self.config.room.auto_enter = false;
        if let Some(room) = &mut self.rooms {
            if let Ok(generation) = room.session.force_leave() {
                room.expected_generation = generation;
            }
            room.media_epoch = self.room_epoch;
            room.routes.clear();
            room.joined_once = false;
            room.pending_startup = None;
            room.reset_room_view(room.expected_generation, Phase::Idle);
        }
    }
}

pub(super) fn validate_command(command: &Command) -> Result<(), String> {
    match command {
        Command::CreateRoom { profile, .. } => profile.validate(),
        Command::JoinUnknownRoom { target } => target.validate(),
        Command::CreateSavedRoom { profile_path, .. }
        | Command::JoinSavedRoom { profile_path, .. } => {
            crate::participant::rooms::load_room_profile(profile_path).map(|_: RoomProfile| ())
        }
        Command::EditRoomRule { rule } => rule.validate(),
        Command::EditGlobalRule { rule } => rule.validate(),
        Command::ReplaceGlobalRules { rules } => rules.validate(),
        Command::SetGlobalDefaults { send, listen } => Rule {
            r#match: RuleMatch::All,
            source_kind: None,
            send: *send,
            listen: *listen,
            outputs: None,
        }
        .validate(),
        Command::SetRoomSourceMix {
            condition,
            playback,
            ..
        } => {
            condition.validate()?;
            playback.validate("room source mix")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant::rooms::{RoomTarget, load_room_profile};
    use crate::participant::{SourceKind, config};
    use std::time::{Duration, Instant};

    fn make_worker(root: &std::path::Path, file: &str, name: &str) -> Worker {
        let path = root.join(file);
        let mut config = ParticipantConfig::create(name, "127.0.0.1:0".parse().unwrap()).unwrap();
        config.auto_reconnect = false;
        config::create_config_file(&path, &config).unwrap();
        Worker::boot(&path).unwrap()
    }

    #[test]
    fn projected_output_choices_apply_without_enabling_receive_or_waiting_for_catalogue() {
        use crate::participant::output_routing::OutputTarget;
        use crate::participant::room_session::{MemberView, ProjectedEndpoint};
        use rlb_model::{MemberId, RunId};

        let root = std::env::temp_dir().join(format!(
            "rlb-route-projection-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut worker = make_worker(&root, "local.json", "Local");
        worker.ensure_rooms().unwrap();
        let room = worker.rooms.as_mut().unwrap();
        room.active = true;
        room.joined_once = true;
        room.current.generation = room.expected_generation;
        room.current.room_name = Some("Headphones".into());
        room.current.members = vec![MemberView {
            member_id: MemberId::from_bytes([3; 16]).unwrap(),
            run_id: RunId::from_bytes([4; 16]).unwrap(),
            display_name: "Ada".into(),
            endpoint: ProjectedEndpoint::Ready("192.0.2.31:47001".parse().unwrap()),
            visible: true,
            alternatives: Vec::new(),
        }];
        room.current.global_rules = Some(crate::participant::rooms::GlobalRules::baseline());
        room.current.rules = vec![Rule {
            r#match: RuleMatch::Condition {
                name: Some("Ada".into()),
                ip: None,
            },
            source_kind: Some(SourceKind::System),
            send: None,
            listen: None,
            outputs: Some(vec![OutputTarget::Endpoint("cable".into())]),
        }];
        room.routes = project_peers(worker.config.member_id, &room.current);
        let config = room.project(&worker.config, &worker.media.handle.snapshot());
        let member = &config.members[0];
        assert!(member.send && !member.receive);
        assert_eq!(
            member.source_output_targets,
            vec![
                SourceOutputTargets {
                    kind: SourceKind::System,
                    targets: vec![OutputTarget::Endpoint("cable".into())]
                },
                SourceOutputTargets {
                    kind: SourceKind::Microphone,
                    targets: vec![OutputTarget::Main]
                },
            ]
        );
        assert!(member.source_overrides.is_empty());
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn until(workers: &mut [&mut Worker], predicate: impl Fn(&[&mut Worker]) -> bool) {
        let start = Instant::now();
        loop {
            for worker in workers.iter_mut() {
                worker.pump().unwrap();
            }
            if predicate(workers) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(6),
                "room assembly timeout: {:?}",
                workers
                    .iter()
                    .map(|worker| worker.room_state())
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    #[test]
    fn name_and_failed_entry_keep_unsaved_globals_on_the_same_owner() {
        let root = std::env::temp_dir().join(format!(
            "rlb-local-lifetime-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut worker = make_worker(&root, "local.json", "Local");
        worker.ensure_rooms().unwrap();
        until(&mut [&mut worker], |workers| {
            workers[0]
                .room_state()
                .session
                .as_ref()
                .unwrap()
                .global_rules
                .is_some()
        });
        let library = RoomLibrary::new(&worker.path);
        let global_path = library.global_rules_path();
        let mut external = std::fs::read(&global_path).unwrap();
        external.push(b' ');
        std::fs::write(&global_path, &external).unwrap();
        worker
            .apply(Command::SetGlobalDefaults {
                send: None,
                listen: Some(true),
            })
            .unwrap();
        until(&mut [&mut worker], |workers| {
            workers[0]
                .room_state()
                .session
                .as_ref()
                .unwrap()
                .rule_files
                .global
                .outcome
                .unsaved()
        });
        worker
            .apply(Command::SetName {
                name: "Renamed".into(),
            })
            .unwrap();
        let cards = library
            .allocate_card_path()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(cards.starts_with(&root));
        std::fs::remove_dir(&cards).unwrap();
        std::fs::write(&cards, b"blocked directory").unwrap();
        assert!(
            worker
                .apply(Command::CreateRoom {
                    profile: RoomProfile::create("Fail").unwrap(),
                    bind: "127.0.0.1:0".parse().unwrap()
                })
                .is_err()
        );
        assert!(
            worker
                .room_state()
                .session
                .unwrap()
                .rule_files
                .global
                .outcome
                .unsaved()
        );
        std::fs::remove_file(&cards).unwrap();
        std::fs::create_dir(&cards).unwrap();
        worker
            .apply(Command::CreateRoom {
                profile: RoomProfile::create("After failure").unwrap(),
                bind: "127.0.0.1:0".parse().unwrap(),
            })
            .unwrap();
        until(&mut [&mut worker], |workers| workers[0].snapshot().joined);
        let state = worker.room_state().session.unwrap();
        assert!(state.rule_files.global.outcome.unsaved());
        assert_eq!(state.global_rules.unwrap().rules[0].listen, Some(true));
        assert_eq!(
            state
                .members
                .iter()
                .find(|member| member.member_id == worker.config.member_id)
                .unwrap()
                .display_name,
            "Renamed"
        );
        assert_eq!(std::fs::read(&global_path).unwrap(), external);
        worker.close_runtime();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_creator_address_recovery_reuses_the_local_rule_owner() {
        let root = std::env::temp_dir().join(format!(
            "rlb-address-retry-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut worker = make_worker(&root, "local.json", "Local");
        worker.ensure_rooms().unwrap();
        until(&mut [&mut worker], |workers| {
            workers[0]
                .room_state()
                .session
                .as_ref()
                .unwrap()
                .global_rules
                .is_some()
        });
        let global_path = RoomLibrary::new(&worker.path).global_rules_path();
        let mut external = std::fs::read(&global_path).unwrap();
        external.push(b' ');
        std::fs::write(&global_path, &external).unwrap();
        worker
            .apply(Command::EditGlobalRule {
                rule: Rule {
                    outputs: None,
                    r#match: RuleMatch::Condition {
                        ip: None,
                        name: Some("Keep".into()),
                    },
                    source_kind: None,
                    send: None,
                    listen: Some(true),
                },
            })
            .unwrap();
        until(&mut [&mut worker], |workers| {
            workers[0]
                .room_state()
                .session
                .as_ref()
                .unwrap()
                .rule_files
                .global
                .outcome
                .unsaved()
        });
        let path = RoomLibrary::new(&worker.path)
            .create_card(&RoomProfile::create("Retry creator").unwrap())
            .unwrap();
        worker.config.room.selected = Some(RoomStartup::Create {
            profile_path: path,
            bind: "127.0.0.1:0".parse().unwrap(),
        });
        worker.config.room.auto_enter = true;
        let room = worker.rooms.as_mut().unwrap();
        room.active = true;
        room.current.phase = Phase::Failed;
        room.current.failure_kind =
            Some(crate::participant::room_session::EntryFailureKind::AddressRequired);
        worker.next_room_retry = Instant::now();
        worker.pump().unwrap();
        assert!(
            worker
                .room_state()
                .session
                .unwrap()
                .rule_files
                .global
                .outcome
                .unsaved()
        );
        until(&mut [&mut worker], |workers| workers[0].snapshot().joined);
        assert_eq!(
            worker.room_state().session.unwrap().room_name.as_deref(),
            Some("Retry creator")
        );
        assert!(
            worker
                .room_state()
                .session
                .unwrap()
                .rule_files
                .global
                .outcome
                .unsaved(),
            "creator retry must retain the original active global document"
        );
        assert_eq!(std::fs::read(&global_path).unwrap(), external);
        worker.close_runtime();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_same_configuration_has_one_instance_until_owners_are_joined() {
        let root = std::env::temp_dir().join(format!(
            "rlb-instance-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("local.json");
        let config = ParticipantConfig::create("Local", "127.0.0.1:0".parse().unwrap()).unwrap();
        config::create_config_file(&path, &config).unwrap();
        let mut first = super::super::start(&path).unwrap();
        let error = match super::super::start(&root.join(".").join("local.json")) {
            Ok(_) => panic!("a second owner must not start for this config"),
            Err(error) => error,
        };
        assert!(error.contains("already running"));
        first.stop().unwrap();
        let mut second = super::super::start(&path).unwrap();
        second.stop().unwrap();
        drop(first);
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_startup_room_replaces_saved_intent_before_any_owner_restores() {
        let root = std::env::temp_dir().join(format!(
            "rlb-start-override-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("local.json");
        let mut config =
            ParticipantConfig::create("Local", "127.0.0.1:0".parse().unwrap()).unwrap();
        config.group.joined = true;
        config.group.name = "Old diagnostic".into();
        config.room.auto_enter = true;
        config.room.selected = Some(RoomStartup::Join {
            profile_path: root.join("must-not-load.json"),
        });
        config::create_config_file(&path, &config).unwrap();
        RoomLibrary::new(&path).initialize().unwrap();
        let card = RoomLibrary::new(&path)
            .create_card(&RoomProfile::create("Chosen").unwrap())
            .unwrap();
        let chosen = RoomStartup::Create {
            profile_path: card,
            bind: "127.0.0.1:0".parse().unwrap(),
        };
        let mut participant = super::super::start_with_room(&path, Some(chosen.clone())).unwrap();
        let handle = participant.handle();
        let start = Instant::now();
        while !handle.snapshot().joined {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "{:?}",
                handle.snapshot().room
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let state = handle.snapshot();
        assert_eq!(
            state.room.session.unwrap().room_name.as_deref(),
            Some("Chosen")
        );
        assert_eq!(
            config::load_config(&path).unwrap().room.selected,
            Some(chosen)
        );
        assert!(!root.join("must-not-load.json").exists());
        participant.stop().unwrap();
        drop(participant);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn waiting_for_a_saved_room_clears_entry_failure_after_recovery() {
        let root = std::env::temp_dir().join(format!(
            "rlb-entry-wait-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut worker = make_worker(&root, "local.json", "Local");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut profile = RoomProfile::create("Delayed").unwrap();
        profile.room.address = Some(address.ip());
        profile.room.port = Some(address.port());
        let path = RoomLibrary::new(&worker.path)
            .create_card(&profile)
            .unwrap();
        worker
            .apply(Command::JoinSavedRoom {
                profile_path: path.clone(),
                mode: JoinMode::Manual,
            })
            .unwrap();
        until(&mut [&mut worker], |workers| {
            workers[0].room_phase() == "failed"
        });
        assert_eq!(
            worker.room_state().session.unwrap().failure_kind,
            Some(crate::participant::room_session::EntryFailureKind::Unavailable)
        );
        assert!(worker.room_state().error.is_none());
        worker
            .apply(Command::JoinSavedRoom {
                profile_path: path,
                mode: JoinMode::Automatic,
            })
            .unwrap();
        until(&mut [&mut worker], |workers| {
            workers[0].room_phase() == "contact_unavailable"
        });
        let service = rlb_contact::ContactService::start(
            rlb_contact::ContactConfig::try_new(address, "Delayed", "").unwrap(),
        )
        .unwrap();
        until(&mut [&mut worker], |workers| workers[0].snapshot().joined);
        let state = worker.room_state();
        assert!(state.error.is_none());
        assert!(state.session.unwrap().failure.is_none());
        worker.close_runtime();
        service.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn card_refresh_works_without_starting_a_network_session() {
        let root = std::env::temp_dir().join(format!(
            "rlb-offline-cards-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut worker = make_worker(&root, "local.json", "Local");
        let library = RoomLibrary::new(&worker.path);
        library
            .create_card(&RoomProfile::create("Saved offline").unwrap())
            .unwrap();
        assert!(worker.rooms.is_none());
        worker.apply(Command::RefreshRooms).unwrap();
        assert!(worker.rooms.is_none());
        assert_eq!(
            worker.room_state().cards[0].name.as_deref(),
            Some("Saved offline")
        );
        worker.close_runtime();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_room_rules_drive_peers_and_leave_preserves_local_monitor() {
        let root = std::env::temp_dir().join(format!(
            "rlb-common-room-{}",
            rlb_model::SourceId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut host = make_worker(&root, "host.json", "Headphones");
        let mut first = make_worker(&root, "first.json", "Music");
        let mut second = make_worker(&root, "second.json", "Music");
        let mut profile = RoomProfile::create("Common room").unwrap();
        profile.room.password = "local-fixture".into();
        host.apply(Command::CreateRoom {
            profile,
            bind: "127.0.0.1:0".parse().unwrap(),
        })
        .unwrap();
        until(&mut [&mut host], |workers| workers[0].snapshot().joined);
        let service = host.room_state().session.unwrap().local_service.unwrap();
        let target = RoomTarget {
            name: "Common room".into(),
            address: Some(service.ip()),
            port: Some(service.port()),
            password: "local-fixture".into(),
            strict_auto_join: false,
        };
        first
            .apply(Command::JoinUnknownRoom {
                target: target.clone(),
            })
            .unwrap();
        second.apply(Command::JoinUnknownRoom { target }).unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers
                .iter()
                .all(|worker| worker.snapshot().joined && worker.snapshot().members.len() == 2)
                && workers[0]
                    .snapshot()
                    .members
                    .iter()
                    .all(|member| member.contact == super::super::MemberContact::Connected)
        });
        assert!(
            host.snapshot()
                .members
                .iter()
                .all(|member| !member.receive_allowed)
        );
        let id = first.config.member_id;
        host.apply(Command::SetPeerIntent {
            member_id: id,
            send: true,
            receive: true,
        })
        .unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0]
                .snapshot()
                .members
                .iter()
                .all(|member| member.receive_allowed)
        });
        let path = host.room_state().session.unwrap().profile_path.unwrap();
        let saved = load_room_profile(&path).unwrap();
        assert_eq!(
            host.room_state()
                .cards
                .iter()
                .find(|card| card.path == path)
                .unwrap()
                .rule_count,
            saved.rules.len()
        );
        assert!(
            matches!(&saved.rules.last().unwrap().r#match, RuleMatch::Condition { ip: None, name: Some(name) } if name == "Music")
        );
        let local = config::load_config(&host.path).unwrap();
        assert!(local.members.is_empty() && local.contacts.is_empty());
        assert!(local.room.auto_enter);
        // Declare actual publications through the existing test input seam;
        // room policy and all peer/media handshakes use their production owners.
        let host_id = host.config.member_id;
        let first_system = first.config.sources[0].id;
        let first_mic = first.config.sources[1].id;
        let second_system = second.config.sources[0].id;
        let second_mic = second.config.sources[1].id;
        for (sender, microphone, system) in [
            (&mut first, first_mic, first_system),
            (&mut second, second_mic, second_system),
        ] {
            sender
                .apply(Command::SetLocalSource {
                    source_id: microphone,
                    publish: Some(true),
                    monitor: None,
                })
                .unwrap();
            sender
                .media
                .handle
                .with_core(move |core| {
                    super::super::bind_mono(core, system, host_id)?;
                    super::super::bind_mono(core, microphone, host_id)
                })
                .unwrap();
        }
        host.media
            .handle
            .with_core(|core| core.playback.select_software_route());
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0].snapshot().members.iter().all(|member| {
                member.sources.len() == 2
                    && member.sources.iter().all(|source| source.receive_ready)
            })
        });
        let neighbour_fence = host.media.handle.snapshot().peers[&id]
            .admission
            .sources
            .iter()
            .flatten()
            .find(|source| source.entry.id == first_system)
            .unwrap()
            .fence;
        host.apply(Command::EditRoomRule {
            rule: Rule {
                outputs: None,
                r#match: RuleMatch::Condition {
                    name: Some("Music".into()),
                    ip: None,
                },
                source_kind: Some(SourceKind::Microphone),
                send: None,
                listen: Some(false),
            },
        })
        .unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0].snapshot().members.iter().all(|member| {
                member.sources.iter().all(|source| {
                    if source.kind == SourceKind::Microphone {
                        !source.receive_allowed && !source.receive_ready
                    } else {
                        source.receive_allowed && source.receive_ready
                    }
                })
            }) && workers[1].media.handle.with_core(move |core| {
                core.peers[&host_id].peer.source_send_ready(first_system)
                    && !core.peers[&host_id].peer.source_send_ready(first_mic)
            }) && workers[2]
                .media
                .handle
                .with_core(move |core| !core.peers[&host_id].peer.source_send_ready(second_mic))
                && workers[1].snapshot().members.iter().any(|member| {
                    member.id == host_id && member.sending_sources == vec![first_system]
                })
        });
        assert!(load_room_profile(&path).unwrap().rules.iter().any(|rule| {
            rule.source_kind == Some(SourceKind::Microphone) && rule.listen == Some(false)
        }));
        assert_eq!(
            host.media.handle.snapshot().peers[&id]
                .admission
                .sources
                .iter()
                .flatten()
                .find(|source| source.entry.id == first_system)
                .unwrap()
                .fence,
            neighbour_fence
        );
        let sent = first
            .media
            .handle
            .with_core(move |core| {
                let samples = [0.25; 4096];
                Ok::<_, String>((
                    super::super::publish_mono(core, first_system, &samples, host_id, 0)?,
                    super::super::publish_mono(core, first_mic, &samples, host_id, 0)?,
                ))
            })
            .unwrap();
        assert!(sent.0 > 0);
        assert_eq!(
            sent.1, 0,
            "an unselected microphone sends no audio datagram"
        );
        // A later member action is the latest all-source assignment. It covers
        // both same-name members and overrides the older microphone-only rule.
        host.apply(Command::SetPeerIntent {
            member_id: id,
            send: true,
            receive: true,
        })
        .unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0]
                .snapshot()
                .members
                .iter()
                .all(|member| member.sources.iter().all(|source| source.receive_ready))
                && workers[1]
                    .media
                    .handle
                    .with_core(move |core| core.peers[&host_id].peer.source_send_ready(first_mic))
        });
        let sent = first
            .media
            .handle
            .with_core(move |core| {
                super::super::publish_mono(core, first_mic, &[0.5; 4096], host_id, 4096)
            })
            .unwrap();
        assert!(sent > 0);
        let extra_output = "room-stereo-44100";
        host.apply(Command::EditRoomRule {
            rule: Rule {
                r#match: RuleMatch::Condition {
                    name: Some("Music".into()),
                    ip: None,
                },
                source_kind: Some(SourceKind::System),
                send: None,
                listen: None,
                outputs: Some(vec![
                    crate::participant::OutputTarget::Main,
                    crate::participant::OutputTarget::Endpoint(extra_output.into()),
                ]),
            },
        })
        .unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0].snapshot().output_devices.iter().any(|device| {
                device.endpoint_id == extra_output
                    && device.state.phase == "running"
                    && device.state.sample_rate_hz == Some(44_100)
            })
        });
        assert_eq!(
            host.media.handle.snapshot().peers[&id]
                .admission
                .sources
                .iter()
                .flatten()
                .find(|source| source.entry.id == first_system)
                .unwrap()
                .fence,
            neighbour_fence,
            "an output-only assignment must preserve network admission"
        );
        assert!(load_room_profile(&path).unwrap().rules.iter().any(|rule| {
            rule.outputs.as_ref().is_some_and(|targets| {
                targets.contains(&crate::participant::OutputTarget::Endpoint(
                    extra_output.into(),
                ))
            })
        }));
        let before = host
            .snapshot()
            .output_devices
            .iter()
            .find(|device| device.endpoint_id == extra_output)
            .unwrap()
            .state
            .generation;
        first
            .media
            .handle
            .with_core(move |core| {
                super::super::publish_mono(core, first_system, &[0.25; 4096], host_id, 4096)
            })
            .unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0]
                .media
                .handle
                .snapshot()
                .output
                .outputs
                .iter()
                .any(|(key, state)| {
                    key == extra_output && state.generation == before && state.submitted_frames > 0
                })
        });
        let remote_source = first.config.sources[0].id;
        host.apply(Command::SetSourceMix {
            member_id: Some(id),
            source_id: remote_source,
            gain_db: -6.0,
            pan: 0.0,
            mute: false,
            solo: false,
        })
        .unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            workers[0].snapshot().members.iter().all(|member| {
                member
                    .source_mix_overrides
                    .iter()
                    .any(|choice| choice.playback.gain_db == -6.0)
            })
        });
        assert_eq!(load_room_profile(&path).unwrap().playback.len(), 1);
        let mut external = load_room_profile(&path).unwrap();
        let expected_count = external.rules.len() + external.playback.len();
        assert_eq!(
            host.room_state()
                .cards
                .iter()
                .find(|card| card.path == path)
                .unwrap()
                .rule_count,
            expected_count
        );
        external.room.name = "External saved name".into();
        crate::participant::rooms::save_room_profile(&path, &external).unwrap();
        host.apply(Command::ReloadRoomRules).unwrap();
        until(&mut [&mut host], |workers| {
            let state = workers[0].room_state();
            state
                .session
                .as_ref()
                .is_some_and(|session| session.pending_target_change)
                && state.cards.iter().any(|card| {
                    card.path == path && card.name.as_deref() == Some("External saved name")
                })
        });
        assert_eq!(host.snapshot().group_name, "Common room");
        // Invalid reload is visible and leaves the current directions in effect.
        std::fs::write(&path, "invalid replacement").unwrap();
        host.apply(Command::ReloadRoomRules).unwrap();
        until(&mut [&mut host], |workers| {
            workers[0].room_state().error.is_some()
        });
        assert!(
            host.snapshot()
                .members
                .iter()
                .all(|member| member.receive_allowed)
        );
        let system = host
            .config
            .sources
            .iter()
            .find(|source| source.kind == SourceKind::System)
            .unwrap()
            .id;
        host.apply(Command::SetLocalSource {
            source_id: system,
            publish: Some(true),
            monitor: Some(true),
        })
        .unwrap();
        host.apply(Command::LeaveRoom).unwrap();
        until(&mut [&mut host, &mut first, &mut second], |workers| {
            !workers[0].snapshot().joined
        });
        assert!(host.config.source(system).unwrap().monitor);
        assert!(!config::load_config(&host.path).unwrap().room.auto_enter);
        let second_id = second.config.member_id;
        until(&mut [&mut first, &mut second], |workers| {
            workers.iter().all(|worker| {
                worker.room_phase() == "contact_unavailable" && worker.snapshot().joined
            }) && workers[0].snapshot().members.iter().any(|member| {
                member.id == second_id && member.contact == super::super::MemberContact::Connected
            })
        });
        // A later ambiguous recovery retains this generation's established
        // direct directions. An accepted Enter fence still closes them.
        first.rooms.as_mut().unwrap().current.phase = Phase::Choosing;
        assert!(first.media_config().group.joined);
        first.room_epoch = first
            .media
            .handle
            .submit_scope(super::super::RoomTransition::Enter, Ok)
            .unwrap();
        assert!(!first.media_config().group.joined);
        let leave_epoch = first
            .media
            .handle
            .submit_scope(super::super::RoomTransition::Leave, Ok)
            .unwrap();
        assert!(first.config.room.auto_enter);
        assert!(first.restore_room().is_err());
        assert_eq!(first.media.handle.room_fence().epoch, leave_epoch);
        assert!(first.media.handle.room_fence().closed);
        host.close_runtime();
        first.close_runtime();
        second.close_runtime();
        std::fs::remove_dir_all(root).unwrap();
    }
}
