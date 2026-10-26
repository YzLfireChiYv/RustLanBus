//! Approved member-session state, replacement, binding, and command routing.

use std::collections::{BTreeMap, VecDeque};

use rlb_control::{
    AuthorityMutation, CanonicalCommandHash, CanonicalRevisions, DelegatedMutation, Field,
    RequestCacheKey, RoomCommandArgs, RoomCommandExpected, RoomCommandScope,
    ValidatedDelegatedCommand, ValidatedRoomCommand, canonical_command_hash,
};
use rlb_model::{
    AdmissionState, BindNonce, Capability, CapabilityGrants, CatalogRevision, Endpoint, ErrorCode,
    GrantRevision, MemberConfigRevision, MemberId, PresenceRevision, RegistrySessionId, RequestId,
    RoomRevision, RunId, ServerGeneration, SourceDescriptor,
};
use serde::Serialize;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::bind::{BindMedia, BindOk};
use crate::operator::{
    OperatorAuditContext, OperatorCommandResult, OperatorCommandStatus, OperatorLedgerError,
    OperatorOutcomeInput, OperatorPendingRequest, OperatorRequestDecision, OperatorState,
};
use crate::room::{PersistentRoomEvent, RoomState, RoomTransition, reduce_authority_command};
use crate::token::{MemberToken, SecretGenerationError, TokenDigest};
use crate::wire::{
    ClientMessage, ControlPresence, DecimalU64, MediaPresence, MemberCommandStatus,
    MemberProjection, RoomEvent, RoomEventValue, RoomSnapshot, RoomView, SelfView, ServerMessage,
    SnapshotMember,
};

/// Session lease duration.
pub const LEASE_MS: u64 = 30_000;
/// Required heartbeat interval.
pub const HEARTBEAT_MS: u32 = 10_000;
/// UDP binding challenge lifetime.
pub const BIND_TIMEOUT_MS: u64 = 10_000;
/// Successful bind replay window.
pub const BIND_REPLAY_MS: u64 = 2_000;
/// Maximum concurrently delivered member commands per session.
pub const MAX_IN_FLIGHT_COMMANDS: usize = 16;
/// Retained room-event ring size.
pub const EVENT_RING_CAPACITY: usize = 256;
/// Retained generation-local operator command event count.
pub const OPERATOR_COMMAND_EVENT_CAPACITY: usize = 256;

/// Normative member session states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberSessionState {
    Disconnected,
    TcpConnected,
    PrefaceOk,
    Authenticating,
    ApprovedUnbound,
    OnlineBound,
    Rebinding,
    Closing,
}

/// Explicit transition events for the normative state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTransition {
    TcpConnected,
    PrefaceAccepted,
    EngineHelloReceived,
    AuthenticationAccepted,
    MediaBound,
    RebindRequested,
    Closing,
    Closed,
}

impl MemberSessionState {
    /// Computes one allowed transition and rejects every other edge.
    pub const fn transition(self, event: SessionTransition) -> Result<Self, SessionStateError> {
        match (self, event) {
            (Self::Disconnected, SessionTransition::TcpConnected) => Ok(Self::TcpConnected),
            (Self::TcpConnected, SessionTransition::PrefaceAccepted) => Ok(Self::PrefaceOk),
            (Self::PrefaceOk, SessionTransition::EngineHelloReceived) => Ok(Self::Authenticating),
            (Self::Authenticating, SessionTransition::AuthenticationAccepted) => {
                Ok(Self::ApprovedUnbound)
            }
            (Self::ApprovedUnbound, SessionTransition::MediaBound)
            | (Self::Rebinding, SessionTransition::MediaBound) => Ok(Self::OnlineBound),
            (Self::OnlineBound, SessionTransition::RebindRequested) => Ok(Self::Rebinding),
            (
                Self::TcpConnected
                | Self::PrefaceOk
                | Self::Authenticating
                | Self::ApprovedUnbound
                | Self::OnlineBound
                | Self::Rebinding,
                SessionTransition::Closing,
            ) => Ok(Self::Closing),
            (Self::Closing, SessionTransition::Closed) => Ok(Self::Disconnected),
            _ => Err(SessionStateError),
        }
    }

    /// Whether an approved TCP lease is online for delegated control.
    #[must_use]
    pub const fn control_online(self) -> bool {
        matches!(
            self,
            Self::ApprovedUnbound | Self::OnlineBound | Self::Rebinding
        )
    }
}

/// Invalid normative state-machine edge.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid member session state transition")]
pub struct SessionStateError;

/// Parsed engine bootstrap retained after token validation.
#[derive(Clone, Debug)]
pub struct EngineHello {
    pub request_id: RequestId,
    pub member_token: MemberToken,
    pub token_generation: u64,
    pub token_sha256: TokenDigest,
    pub room_id: rlb_model::RoomId,
    pub member_id: MemberId,
    pub run_id: RunId,
    pub display_name: String,
    pub client_build: String,
    pub catalog_revision: CatalogRevision,
    pub sources: Vec<SourceDescriptor>,
    pub member_config_revision: MemberConfigRevision,
    pub member_projection: MemberProjection,
    pub grant_revision: GrantRevision,
    pub capability_grants: CapabilityGrants,
}

impl TryFrom<ClientMessage> for EngineHello {
    type Error = SessionError;

    fn try_from(message: ClientMessage) -> Result<Self, Self::Error> {
        let ClientMessage::EngineHello {
            request_id,
            member_token,
            token_generation,
            token_sha256,
            room_id,
            member_id,
            run_id,
            display_name,
            client_build,
            catalog_revision,
            sources,
            member_config_revision,
            member_projection,
            grant_revision,
            capability_grants,
            ..
        } = message
        else {
            return Err(SessionError::ExpectedEngineHello);
        };
        Ok(Self {
            request_id,
            member_token,
            token_generation: token_generation.get(),
            token_sha256,
            room_id,
            member_id,
            run_id,
            display_name,
            client_build,
            catalog_revision,
            sources,
            member_config_revision,
            member_projection,
            grant_revision,
            capability_grants,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct BindChallenge {
    nonce: BindNonce,
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug)]
struct CompletedBind {
    nonce: BindNonce,
    endpoint: Endpoint,
    replay_until_ms: u64,
}

/// One delivered, non-persistent member command.
#[derive(Clone, Debug)]
pub struct InFlightCommand {
    pub request_id: RequestId,
    pub capability: Capability,
    pub delivered_at_ms: u64,
    pub ack_deadline_ms: u64,
    pub deadline_ms: u64,
    pub acknowledged: bool,
    target_member_id: MemberId,
    request_key: RequestCacheKey,
    command_hash: CanonicalCommandHash,
    command: rlb_control::RoomCommandName,
    accepted_revisions: CanonicalRevisions,
    audit_context: OperatorAuditContext,
    before_revisions: CanonicalRevisions,
}

/// One approved member TCP session.
#[derive(Clone, Debug)]
pub struct MemberSession {
    pub registry_session_id: RegistrySessionId,
    pub member_id: MemberId,
    pub run_id: RunId,
    pub display_name: String,
    pub client_build: String,
    pub state: MemberSessionState,
    pub lease_deadline_ms: u64,
    pub endpoint: Option<Endpoint>,
    pub catalog_revision: CatalogRevision,
    pub sources: Vec<SourceDescriptor>,
    pub member_config_revision: MemberConfigRevision,
    pub member_projection: MemberProjection,
    pub grant_revision: GrantRevision,
    pub grants: CapabilityGrants,
    pub in_flight: BTreeMap<RequestId, InFlightCommand>,
    bind_challenge: BindChallenge,
    completed_bind: Option<CompletedBind>,
}

impl MemberSession {
    fn from_hello(
        hello: EngineHello,
        registry_session_id: RegistrySessionId,
        bind_nonce: BindNonce,
        now_ms: u64,
    ) -> Self {
        Self {
            registry_session_id,
            member_id: hello.member_id,
            run_id: hello.run_id,
            display_name: hello.display_name,
            client_build: hello.client_build,
            state: MemberSessionState::ApprovedUnbound,
            lease_deadline_ms: now_ms.saturating_add(LEASE_MS),
            endpoint: None,
            catalog_revision: hello.catalog_revision,
            sources: hello.sources,
            member_config_revision: hello.member_config_revision,
            member_projection: hello.member_projection,
            grant_revision: hello.grant_revision,
            grants: hello.capability_grants,
            in_flight: BTreeMap::new(),
            bind_challenge: BindChallenge {
                nonce: bind_nonce,
                expires_at_ms: now_ms.saturating_add(BIND_TIMEOUT_MS),
            },
            completed_bind: None,
        }
    }

    /// Returns whether the approved lease remains valid.
    #[must_use]
    pub fn control_online(&self, now_ms: u64) -> bool {
        self.state.control_online() && now_ms < self.lease_deadline_ms
    }
}

#[derive(Clone, Debug, Default)]
struct MemberSessions {
    active: Option<MemberSession>,
    replacement: Option<MemberSession>,
}

/// Session-ready values returned after a valid token bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionReady {
    pub request_id: RequestId,
    pub registry_session_id: RegistrySessionId,
    pub bind_nonce: BindNonce,
    pub room_revision: RoomRevision,
    pub presence_revision: PresenceRevision,
}

/// A superseded transport and all commands whose outcomes need classification.
#[derive(Clone, Debug)]
pub struct SupersededSession {
    pub registry_session_id: RegistrySessionId,
    pub run_id: RunId,
    pub interrupted_commands: Vec<InterruptedCommand>,
    interrupted_deliveries: Vec<InFlightCommand>,
}

/// Command terminal classification caused by disconnect or replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptedCommand {
    pub request_id: RequestId,
    pub code: ErrorCode,
}

/// Accepted terminal result metadata after removing an in-flight command.
#[derive(Clone, Debug)]
pub struct CompletedMemberCommand {
    pub delivered: InFlightCommand,
    pub status: MemberCommandStatus,
    pub code: Option<ErrorCode>,
}

/// Result of accepting a valid UDP binding packet.
#[derive(Clone, Debug)]
pub struct BindOutcome {
    pub reply: BindOk,
    pub member_id: MemberId,
    pub run_id: RunId,
    pub presence_revision: PresenceRevision,
    pub superseded: Option<SupersededSession>,
    pub event: RoomEventValue,
}

/// A member command paired with the exact current session.
#[derive(Clone, Debug)]
pub struct RoutedMemberCommand {
    pub session_id: RegistrySessionId,
    pub message: ServerMessage,
}

/// Delivery or cached state returned for an operator retry.
#[derive(Clone, Debug)]
pub enum MemberCommandRoute {
    Delivered(RoutedMemberCommand),
    Cached(OperatorCommandResult),
    InFlight { request_id: RequestId },
}

/// Synchronous authority-owned operator outcome.
#[derive(Clone, Debug)]
pub struct AuthorityCommandOutcome {
    pub result: OperatorCommandResult,
    pub interrupted_commands: Vec<InterruptedCommand>,
}

/// Failure before an authority-owned command can produce a terminal outcome.
#[derive(Debug, Error)]
pub enum AuthorityCommandError {
    #[error("member-owned command reached the authority command path")]
    CommandBoundary,
    #[error("authority command room scope does not match")]
    RoomScope,
    #[error("operator request ID conflicts with its canonical body")]
    RequestIdConflict,
    #[error("operator request ledger is full")]
    LedgerFull,
    #[error("authority command hash failed")]
    CanonicalHash,
    #[error(transparent)]
    Ledger(#[from] OperatorLedgerError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Event retained in the generation-local room ring.
#[derive(Clone, Debug)]
pub struct AuthorityEvent {
    pub event: RoomEvent,
}

/// Generation-local notification for a terminal operator command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorCommandEvent {
    pub event_seq: u64,
    pub server_generation: ServerGeneration,
    pub room_id: rlb_model::RoomId,
    pub result: OperatorCommandResult,
}

/// In-memory single-room authority runtime.
#[derive(Clone, Debug)]
pub struct RegistryAuthority {
    room: RoomState,
    server_generation: ServerGeneration,
    presence_revision: PresenceRevision,
    sessions: BTreeMap<MemberId, MemberSessions>,
    event_seq: u64,
    events: VecDeque<AuthorityEvent>,
    operator_state: OperatorState,
    operator_command_event_seq: u64,
    operator_command_events: VecDeque<OperatorCommandEvent>,
}

impl RegistryAuthority {
    /// Starts a new daemon generation with empty volatile presence.
    #[must_use]
    pub fn new(room: RoomState, server_generation: ServerGeneration) -> Self {
        let room_id = room.room.room_id;
        Self {
            room,
            server_generation,
            presence_revision: PresenceRevision::new(1),
            sessions: BTreeMap::new(),
            event_seq: 0,
            events: VecDeque::with_capacity(EVENT_RING_CAPACITY),
            operator_state: OperatorState::new(room_id),
            operator_command_event_seq: 0,
            operator_command_events: VecDeque::with_capacity(OPERATOR_COMMAND_EVENT_CAPACITY),
        }
    }

    /// Restores persistent operator request and audit ledgers for a new daemon
    /// generation while keeping all command delivery state empty.
    pub fn with_operator_state(
        room: RoomState,
        server_generation: ServerGeneration,
        mut operator_state: OperatorState,
        current_wall_time: &str,
    ) -> Result<Self, OperatorLedgerError> {
        operator_state.validate()?;
        if operator_state.requests.room_id() != room.room.room_id {
            return Err(OperatorLedgerError::RequestScope);
        }
        operator_state.interrupt_incomplete(server_generation, ErrorCode::CommandOutcomeUnknown)?;
        operator_state.purge_audit(current_wall_time)?;
        let mut authority = Self::new(room, server_generation);
        authority.operator_state = operator_state;
        Ok(authority)
    }

    /// Returns the durable canonical room state.
    #[must_use]
    pub const fn room(&self) -> &RoomState {
        &self.room
    }

    /// Returns the daemon generation.
    #[must_use]
    pub const fn server_generation(&self) -> ServerGeneration {
        self.server_generation
    }

    /// Returns the current generation-local presence revision.
    #[must_use]
    pub const fn presence_revision(&self) -> PresenceRevision {
        self.presence_revision
    }

    /// Returns persistent operator state for registryd's atomic state adapter.
    #[must_use]
    pub const fn operator_state(&self) -> &OperatorState {
        &self.operator_state
    }

    /// Returns a current session by its stable member identity.
    #[must_use]
    pub fn active_session(&self, member_id: MemberId) -> Option<&MemberSession> {
        self.sessions
            .get(&member_id)
            .and_then(|slots| slots.active.as_ref())
    }

    /// Authenticates an engine hello, allocates a session and challenge, and
    /// enforces one active plus one replacement session per identity.
    pub fn accept_engine_hello(
        &mut self,
        mut hello: EngineHello,
        now_ms: u64,
    ) -> Result<SessionReady, SessionError> {
        hello.display_name = hello.display_name.nfc().collect();
        self.room
            .verify_member_bootstrap(
                hello.room_id,
                hello.member_id,
                hello.token_generation,
                hello.token_sha256,
                &hello.member_token,
            )
            .map_err(SessionError::Credential)?;
        validate_session_display_name(&hello.display_name)?;
        normalize_session_source_names(&mut hello.sources)?;
        self.validate_hello_catalog(&hello)?;

        let session_id = RegistrySessionId::generate().map_err(SessionError::Random)?;
        let bind_nonce = BindNonce::generate().map_err(SessionError::Random)?;
        let request_id = hello.request_id;
        let member_id = hello.member_id;
        let session = MemberSession::from_hello(hello, session_id, bind_nonce, now_ms);
        let (became_online, active_run_id) = {
            let slots = self.sessions.entry(member_id).or_default();
            if slots.active.is_none() {
                slots.active = Some(session);
                (
                    true,
                    Some(
                        slots
                            .active
                            .as_ref()
                            .expect("active session was installed")
                            .run_id,
                    ),
                )
            } else if slots.replacement.is_none() {
                if let Some(active) = &slots.active
                    && active.run_id != session.run_id
                    && reuses_active_epoch(active, &session)
                {
                    return Err(SessionError::EpochReusedAcrossRun);
                }
                slots.replacement = Some(session);
                (false, None)
            } else {
                return Err(SessionError::DuplicateIdentity);
            }
        };
        if became_online {
            self.bump_presence()?;
            let run_id = active_run_id.expect("new active run was captured");
            self.push_event(RoomEventValue::MemberControlUp { member_id, run_id });
        }
        Ok(SessionReady {
            request_id,
            registry_session_id: session_id,
            bind_nonce,
            room_revision: self.room.room.room_revision,
            presence_revision: self.presence_revision,
        })
    }

    fn validate_hello_catalog(&self, hello: &EngineHello) -> Result<(), SessionError> {
        let record = self
            .room
            .member(hello.member_id)
            .ok_or(SessionError::MemberNotApproved)?;
        if record.admission != AdmissionState::Approved {
            return Err(SessionError::MemberNotApproved);
        }
        if hello.sources.len() != 2 {
            return Err(SessionError::InvalidCatalog);
        }
        for source in &hello.sources {
            if record.source_kind(source.source_id()) != Some(source.kind()) {
                return Err(SessionError::SourceIdentityChanged);
            }
            if source.state().effective_profile()
                != source
                    .state()
                    .selected_profile()
                    .capped_by(self.room.room.quality_cap)
            {
                return Err(SessionError::QualityPolicyOutOfSync);
            }
            if !source_send_ready_is_valid(
                source,
                hello.member_projection.local_emergency_stop,
                self.room.room.room_send_muted,
            ) {
                return Err(SessionError::InvalidCatalog);
            }
        }
        Ok(())
    }

    /// Extends an exact approved session lease.
    pub fn heartbeat(
        &mut self,
        session_id: RegistrySessionId,
        now_ms: u64,
    ) -> Result<u32, SessionError> {
        let session = self
            .session_mut(session_id)
            .ok_or(SessionError::UnknownSession)?;
        if !session.state.control_online() {
            return Err(SessionError::InvalidState);
        }
        session.lease_deadline_ms = now_ms.saturating_add(LEASE_MS);
        Ok(LEASE_MS as u32)
    }

    /// Begins a challenge rebind while retaining the old verified endpoint.
    pub fn request_rebind(
        &mut self,
        session_id: RegistrySessionId,
        now_ms: u64,
    ) -> Result<BindNonce, SessionError> {
        let nonce = BindNonce::generate().map_err(SessionError::Random)?;
        let session = self
            .session_mut(session_id)
            .ok_or(SessionError::UnknownSession)?;
        session.state = session
            .state
            .transition(SessionTransition::RebindRequested)
            .map_err(|_| SessionError::InvalidState)?;
        session.bind_challenge = BindChallenge {
            nonce,
            expires_at_ms: now_ms.saturating_add(BIND_TIMEOUT_MS),
        };
        session.completed_bind = None;
        Ok(nonce)
    }

    /// Applies a valid `BIND_MEDIA` using the datagram's actual source endpoint.
    pub fn bind_media(
        &mut self,
        bind: BindMedia,
        observed_endpoint: Endpoint,
        now_ms: u64,
    ) -> Result<BindOutcome, SessionError> {
        if bind.server_generation != self.server_generation {
            return Err(SessionError::StaleGeneration);
        }
        let server_generation = self.server_generation;
        let current_presence_revision = self.presence_revision;
        let (member_id, replacement) = self
            .locate_session(bind.registry_session_id)
            .ok_or(SessionError::UnknownSession)?;

        let was_rebinding = {
            let session = self
                .session_by_location_mut(member_id, replacement)
                .expect("located session exists");
            if let Some(completed) = session.completed_bind
                && completed.nonce == bind.bind_nonce
                && now_ms <= completed.replay_until_ms
                && completed.endpoint == observed_endpoint
            {
                return Ok(BindOutcome {
                    reply: BindOk::new(
                        server_generation,
                        bind.registry_session_id,
                        bind.bind_nonce,
                        observed_endpoint.ip(),
                        observed_endpoint.port(),
                    )
                    .map_err(|_| SessionError::InvalidBind)?,
                    member_id,
                    run_id: session.run_id,
                    presence_revision: current_presence_revision,
                    superseded: None,
                    event: RoomEventValue::MemberMediaBound {
                        member_id,
                        endpoint: observed_endpoint,
                    },
                });
            }
            if !matches!(
                session.state,
                MemberSessionState::ApprovedUnbound | MemberSessionState::Rebinding
            ) {
                return Err(SessionError::InvalidState);
            }
            if session.bind_challenge.nonce != bind.bind_nonce {
                return Err(SessionError::InvalidBind);
            }
            if now_ms > session.bind_challenge.expires_at_ms {
                return Err(SessionError::BindNonceExpired);
            }
            let was_rebinding = session.state == MemberSessionState::Rebinding;
            session.state = session
                .state
                .transition(SessionTransition::MediaBound)
                .map_err(|_| SessionError::InvalidState)?;
            session.endpoint = Some(observed_endpoint);
            session.completed_bind = Some(CompletedBind {
                nonce: bind.bind_nonce,
                endpoint: observed_endpoint,
                replay_until_ms: now_ms.saturating_add(BIND_REPLAY_MS),
            });
            was_rebinding
        };

        let (run_id, superseded, event) = if replacement {
            let slots = self
                .sessions
                .get_mut(&member_id)
                .expect("member slots exist");
            let new_session = slots
                .replacement
                .take()
                .expect("replacement session exists");
            let run_id = new_session.run_id;
            let old = slots.active.replace(new_session);
            let event = if let Some(old_session) = &old {
                if old_session.run_id != run_id {
                    RoomEventValue::MemberRunReplaced {
                        member_id,
                        old_run_id: old_session.run_id,
                        new_run_id: run_id,
                    }
                } else {
                    RoomEventValue::MediaEndpointChanged {
                        member_id,
                        endpoint: observed_endpoint,
                    }
                }
            } else {
                RoomEventValue::MemberMediaBound {
                    member_id,
                    endpoint: observed_endpoint,
                }
            };
            let superseded = old.map(interrupt_session);
            (run_id, superseded, event)
        } else {
            let session = self
                .sessions
                .get(&member_id)
                .and_then(|slots| slots.active.as_ref())
                .expect("bound active session exists");
            let event = if was_rebinding {
                RoomEventValue::MediaEndpointChanged {
                    member_id,
                    endpoint: observed_endpoint,
                }
            } else {
                RoomEventValue::MemberMediaBound {
                    member_id,
                    endpoint: observed_endpoint,
                }
            };
            (session.run_id, None, event)
        };
        if let Some(superseded) = &superseded {
            self.record_interrupted_session(superseded)?;
        }
        self.bump_presence()?;
        self.push_event(event.clone());
        Ok(BindOutcome {
            reply: BindOk::new(
                self.server_generation,
                bind.registry_session_id,
                bind.bind_nonce,
                observed_endpoint.ip(),
                observed_endpoint.port(),
            )
            .map_err(|_| SessionError::InvalidBind)?,
            member_id,
            run_id,
            presence_revision: self.presence_revision,
            superseded,
            event,
        })
    }

    /// Replaces a complete catalog for an exact session and strictly newer
    /// catalog revision.
    pub fn replace_catalog(
        &mut self,
        session_id: RegistrySessionId,
        run_id: RunId,
        catalog_revision: CatalogRevision,
        mut sources: Vec<SourceDescriptor>,
    ) -> Result<(), SessionError> {
        normalize_session_source_names(&mut sources)?;
        let room_send_muted = self.room.room.room_send_muted;
        let (member_id, _) = self
            .locate_session(session_id)
            .ok_or(SessionError::UnknownSession)?;
        let record = self
            .room
            .member(member_id)
            .ok_or(SessionError::MemberNotApproved)?;
        if sources.len() != 2
            || sources.iter().any(|source| {
                record.source_kind(source.source_id()) != Some(source.kind())
                    || source.state().effective_profile()
                        != source
                            .state()
                            .selected_profile()
                            .capped_by(self.room.room.quality_cap)
            })
        {
            return Err(SessionError::InvalidCatalog);
        }
        let session = self
            .session_mut(session_id)
            .ok_or(SessionError::UnknownSession)?;
        if session.run_id != run_id {
            return Err(SessionError::RunMismatch);
        }
        if catalog_revision <= session.catalog_revision {
            return Err(SessionError::StaleCatalogRevision);
        }
        if sources.iter().any(|source| {
            !source_send_ready_is_valid(
                source,
                session.member_projection.local_emergency_stop,
                room_send_muted,
            )
        }) {
            return Err(SessionError::InvalidCatalog);
        }
        if sources.iter().any(|new_source| {
            session.sources.iter().any(|old_source| {
                if old_source.source_id() != new_source.source_id() {
                    return false;
                }
                match (old_source.active(), new_source.active()) {
                    (Some((old_format, old_epoch)), Some((new_format, new_epoch))) => {
                        old_format != new_format && old_epoch == new_epoch
                    }
                    _ => false,
                }
            })
        }) {
            return Err(SessionError::EpochContinuity);
        }
        session.catalog_revision = catalog_revision;
        session.sources = sources;
        let event_sources = session.sources.clone();
        self.push_event(RoomEventValue::CatalogReplaced {
            member_id,
            catalog_revision,
            sources: event_sources,
        });
        Ok(())
    }

    /// Replaces the four-field complete grant projection.
    pub fn replace_grants(
        &mut self,
        session_id: RegistrySessionId,
        grant_revision: GrantRevision,
        grants: CapabilityGrants,
    ) -> Result<Vec<InterruptedCommand>, SessionError> {
        let member_id = self
            .locate_session(session_id)
            .map(|location| location.0)
            .ok_or(SessionError::UnknownSession)?;
        let revoked = {
            let session = self
                .session_mut(session_id)
                .ok_or(SessionError::UnknownSession)?;
            if grant_revision <= session.grant_revision {
                return Err(SessionError::StaleGrantRevision);
            }
            let old_grants = session.grants;
            let request_ids: Vec<_> = session
                .in_flight
                .values()
                .filter(|command| {
                    old_grants.get(command.capability) && !grants.get(command.capability)
                })
                .map(|command| command.request_id)
                .collect();
            session.grant_revision = grant_revision;
            session.grants = grants;
            request_ids
                .into_iter()
                .filter_map(|request_id| session.in_flight.remove(&request_id))
                .collect::<Vec<_>>()
        };
        self.push_event(RoomEventValue::CapabilityGrantsChanged {
            member_id,
            grant_revision,
            grants,
        });
        let mut interrupted = Vec::with_capacity(revoked.len());
        for command in revoked {
            interrupted.push(InterruptedCommand {
                request_id: command.request_id,
                code: ErrorCode::CapabilityRevoked,
            });
            let mut result = operator_interrupted_result(&command, ErrorCode::CapabilityRevoked);
            result.revisions.grant_revision = Some(grant_revision);
            self.record_operator_terminal(
                command.request_key.clone(),
                command.command_hash,
                command.audit_context.clone(),
                command.before_revisions,
                result,
            )?;
        }
        Ok(interrupted)
    }

    /// Replaces the online-only safe member projection.
    pub fn replace_member_projection(
        &mut self,
        session_id: RegistrySessionId,
        member_config_revision: MemberConfigRevision,
        projection: MemberProjection,
    ) -> Result<(), SessionError> {
        let member_id = self
            .locate_session(session_id)
            .map(|location| location.0)
            .ok_or(SessionError::UnknownSession)?;
        let session = self
            .session_mut(session_id)
            .ok_or(SessionError::UnknownSession)?;
        if member_config_revision < session.member_config_revision {
            return Err(SessionError::StaleMemberConfigRevision);
        }
        session.member_config_revision = member_config_revision;
        session.member_projection = projection;
        let event_projection = session.member_projection.clone();
        self.push_event(RoomEventValue::MemberProjectionChanged {
            member_id,
            member_config_revision,
            projection: event_projection,
        });
        Ok(())
    }

    /// Applies one authority-owned command and records its idempotency result
    /// and audit entry before returning the terminal outcome.
    pub fn apply_authority_command(
        &mut self,
        audit_context: OperatorAuditContext,
        command: &ValidatedRoomCommand,
        now_ms: u64,
    ) -> Result<AuthorityCommandOutcome, AuthorityCommandError> {
        let mut candidate = self.clone();
        let outcome = candidate.apply_authority_command_inner(audit_context, command, now_ms)?;
        *self = candidate;
        Ok(outcome)
    }

    fn apply_authority_command_inner(
        &mut self,
        audit_context: OperatorAuditContext,
        command: &ValidatedRoomCommand,
        now_ms: u64,
    ) -> Result<AuthorityCommandOutcome, AuthorityCommandError> {
        audit_context.validate()?;
        let ValidatedRoomCommand::Authority {
            request_id,
            room_id,
            expected_room_revision,
            mutation,
        } = command
        else {
            return Err(AuthorityCommandError::CommandBoundary);
        };
        if *room_id != self.room.room.room_id {
            return Err(AuthorityCommandError::RoomScope);
        }
        let command_hash = canonical_command_hash(&AuthorityCommandHashBody {
            request_id: *request_id,
            room_id: *room_id,
            expected_room_revision: *expected_room_revision,
            mutation,
        })
        .map_err(|_| AuthorityCommandError::CanonicalHash)?;
        let before_revisions = CanonicalRevisions {
            room_revision: Some(self.room.room.room_revision),
            presence_revision: Some(self.presence_revision),
            ..CanonicalRevisions::default()
        };
        let (request_key, decision) = self.operator_state.requests.begin(
            &audit_context.principal,
            *request_id,
            command_hash,
            now_ms,
        )?;
        match decision {
            OperatorRequestDecision::Replay(Some(result)) => {
                return Ok(AuthorityCommandOutcome {
                    result,
                    interrupted_commands: Vec::new(),
                });
            }
            OperatorRequestDecision::Replay(None) => {
                return Err(AuthorityCommandError::Ledger(
                    OperatorLedgerError::InvalidTransition,
                ));
            }
            OperatorRequestDecision::RequestIdConflict => {
                return Err(AuthorityCommandError::RequestIdConflict);
            }
            OperatorRequestDecision::Full => return Err(AuthorityCommandError::LedgerFull),
            OperatorRequestDecision::Execute => {}
        }

        let transition = match reduce_authority_command(&self.room, command) {
            Ok(transition) => transition,
            Err(error) => {
                let result = authority_command_result(
                    *request_id,
                    mutation,
                    OperatorCommandStatus::Rejected,
                    before_revisions,
                    Some(error.code()),
                );
                self.record_operator_terminal(
                    request_key,
                    command_hash,
                    audit_context,
                    before_revisions,
                    result.clone(),
                )?;
                return Ok(AuthorityCommandOutcome {
                    result,
                    interrupted_commands: Vec::new(),
                });
            }
        };
        let status = if transition.changed() {
            OperatorCommandStatus::Applied
        } else {
            OperatorCommandStatus::Unchanged
        };
        let after_revisions = CanonicalRevisions {
            room_revision: Some(transition.state.room.room_revision),
            presence_revision: Some(self.presence_revision),
            ..CanonicalRevisions::default()
        };
        let result = authority_command_result(*request_id, mutation, status, after_revisions, None);
        let interrupted_commands = self.install_persisted_transition(transition)?;
        self.record_operator_terminal(
            request_key,
            command_hash,
            audit_context,
            before_revisions,
            result.clone(),
        )?;
        Ok(AuthorityCommandOutcome {
            result,
            interrupted_commands,
        })
    }

    /// Routes a command only to the current online approved session. Failure
    /// creates no delayed job or persistent queue entry.
    pub fn route_member_command(
        &mut self,
        audit_context: OperatorAuditContext,
        command: ValidatedDelegatedCommand,
        now_ms: u64,
    ) -> Result<MemberCommandRoute, RouteError> {
        audit_context
            .validate()
            .map_err(|_| RouteError::InvalidAuditContext)?;
        if command.room_id != self.room.room.room_id {
            return Err(RouteError::RoomScope);
        }
        let before_revisions = self.current_operator_revisions(command.member_id);
        let command_hash =
            canonical_command_hash(&command).map_err(|_| RouteError::LedgerFailure)?;
        let (request_key, decision) = self
            .operator_state
            .requests
            .begin(
                &audit_context.principal,
                command.request_id,
                command_hash,
                now_ms,
            )
            .map_err(|error| match error {
                OperatorLedgerError::Principal => RouteError::InvalidPrincipal,
                _ => RouteError::LedgerFailure,
            })?;
        match decision {
            OperatorRequestDecision::Replay(Some(result)) => {
                return Ok(MemberCommandRoute::Cached(result));
            }
            OperatorRequestDecision::Replay(None) => {
                return Ok(MemberCommandRoute::InFlight {
                    request_id: command.request_id,
                });
            }
            OperatorRequestDecision::RequestIdConflict => {
                return Err(RouteError::RequestIdConflict);
            }
            OperatorRequestDecision::Full => return Err(RouteError::ServerBusy),
            OperatorRequestDecision::Execute => {}
        }
        if command.expected.room_revision != self.room.room.room_revision {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::StaleRoomRevision,
            );
        }
        let record = self
            .room
            .member(command.member_id)
            .ok_or(RouteError::MemberNotFound);
        let record = match record {
            Ok(record) => record,
            Err(error) => {
                return self.reject_new_route(
                    &command,
                    &audit_context,
                    before_revisions,
                    request_key,
                    command_hash,
                    error,
                );
            }
        };
        if record.admission != AdmissionState::Approved {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::MemberNotApproved,
            );
        }
        let session = self
            .active_session(command.member_id)
            .filter(|session| session.control_online(now_ms))
            .map(|session| {
                (
                    session.grants,
                    session.member_config_revision,
                    session.grant_revision,
                    session.catalog_revision,
                    session.in_flight.len(),
                )
            });
        let Some((
            grants,
            member_config_revision,
            grant_revision,
            catalog_revision,
            in_flight_count,
        )) = session
        else {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::MemberOffline,
            );
        };
        if !grants.get(command.capability()) {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::CapabilityDenied,
            );
        }
        if command.expected.member_config_revision != member_config_revision {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::StaleMemberConfigRevision,
            );
        }
        if command.expected.grant_revision != grant_revision {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::StaleGrantRevision,
            );
        }
        if let Some(expected) = command.expected.catalog_revision
            && expected != catalog_revision
        {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::StaleCatalogRevision,
            );
        }
        if let Err(error) = self.validate_command_target(&command) {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                error,
            );
        }
        if in_flight_count >= MAX_IN_FLIGHT_COMMANDS {
            return self.reject_new_route(
                &command,
                &audit_context,
                before_revisions,
                request_key,
                command_hash,
                RouteError::ServerBusy,
            );
        }

        let accepted_revisions = CanonicalRevisions {
            room_revision: Some(self.room.room.room_revision),
            presence_revision: Some(self.presence_revision),
            catalog_revision: Some(catalog_revision),
            member_config_revision: Some(member_config_revision),
            grant_revision: Some(grant_revision),
            subscription_revision: None,
        };
        self.operator_state
            .requests
            .mark_in_flight(OperatorPendingRequest {
                request_key: request_key.clone(),
                command_hash,
                audit_context: audit_context.clone(),
                target_member_id: command.member_id,
                capability: command.capability(),
                command: command.mutation.command_name(),
                before_revisions,
                accepted_revisions,
            })
            .map_err(|_| RouteError::LedgerFailure)?;
        let session = self
            .sessions
            .get_mut(&command.member_id)
            .and_then(|slots| slots.active.as_mut())
            .expect("online active session was checked");
        let message = delegated_server_message(session.registry_session_id, &command);
        session.in_flight.insert(
            command.request_id,
            InFlightCommand {
                request_id: command.request_id,
                capability: command.capability(),
                delivered_at_ms: now_ms,
                ack_deadline_ms: now_ms.saturating_add(1_000),
                deadline_ms: now_ms.saturating_add(5_000),
                acknowledged: false,
                target_member_id: command.member_id,
                request_key,
                command_hash,
                command: command.mutation.command_name(),
                accepted_revisions,
                audit_context,
                before_revisions,
            },
        );
        Ok(MemberCommandRoute::Delivered(RoutedMemberCommand {
            session_id: session.registry_session_id,
            message,
        }))
    }

    fn validate_command_target(
        &self,
        command: &ValidatedDelegatedCommand,
    ) -> Result<(), RouteError> {
        match command.mutation {
            DelegatedMutation::SetSourcePublication { source_id, .. }
            | DelegatedMutation::SetSendQuality { source_id, .. } => {
                let source_exists = self
                    .active_session(command.member_id)
                    .is_some_and(|session| {
                        session
                            .sources
                            .iter()
                            .any(|source| source.source_id() == source_id)
                    });
                if !source_exists {
                    return Err(RouteError::SourceNotFound);
                }
            }
            DelegatedMutation::SetMemberMix {
                remote_member_id, ..
            } => {
                if remote_member_id == command.member_id
                    || self
                        .room
                        .member(remote_member_id)
                        .is_none_or(|record| record.admission != AdmissionState::Approved)
                {
                    return Err(RouteError::TargetNotVisible);
                }
            }
            DelegatedMutation::SetSourceMix {
                remote_member_id,
                source_id,
                ..
            } => {
                let Some(remote) = self.room.member(remote_member_id) else {
                    return Err(RouteError::MemberNotFound);
                };
                if remote.admission != AdmissionState::Approved
                    || remote_member_id == command.member_id
                {
                    return Err(RouteError::TargetNotVisible);
                }
                if remote.source_kind(source_id).is_none() {
                    return Err(RouteError::SourceNotFound);
                }
                if !self
                    .room
                    .visible_viewers(remote_member_id, source_id)
                    .is_ok_and(|viewers| viewers.contains(&command.member_id))
                {
                    return Err(RouteError::TargetNotVisible);
                }
            }
            DelegatedMutation::SetAutoReconnect { .. } => {}
        }
        Ok(())
    }

    fn reject_new_route(
        &mut self,
        command: &ValidatedDelegatedCommand,
        audit_context: &OperatorAuditContext,
        before_revisions: CanonicalRevisions,
        request_key: RequestCacheKey,
        command_hash: CanonicalCommandHash,
        error: RouteError,
    ) -> Result<MemberCommandRoute, RouteError> {
        let result = OperatorCommandResult {
            request_id: command.request_id,
            target_member_id: Some(command.member_id),
            capability: Some(command.capability()),
            command: command.mutation.command_name(),
            status: OperatorCommandStatus::Rejected,
            revisions: self.current_operator_revisions(command.member_id),
            code: Some(error.code()),
        };
        self.record_operator_terminal(
            request_key,
            command_hash,
            audit_context.clone(),
            before_revisions,
            result,
        )
        .map_err(|_| RouteError::LedgerFailure)?;
        Err(error)
    }

    fn current_operator_revisions(&self, member_id: MemberId) -> CanonicalRevisions {
        let session = self.active_session(member_id);
        CanonicalRevisions {
            room_revision: Some(self.room.room.room_revision),
            presence_revision: Some(self.presence_revision),
            catalog_revision: session.map(|session| session.catalog_revision),
            member_config_revision: session.map(|session| session.member_config_revision),
            grant_revision: session.map(|session| session.grant_revision),
            subscription_revision: None,
        }
    }

    fn record_operator_terminal(
        &mut self,
        request_key: RequestCacheKey,
        command_hash: CanonicalCommandHash,
        audit_context: OperatorAuditContext,
        before_revisions: CanonicalRevisions,
        result: OperatorCommandResult,
    ) -> Result<(), OperatorLedgerError> {
        let mut candidate = self.operator_state.clone();
        candidate.record_outcome(OperatorOutcomeInput {
            request_key,
            canonical_command_hash: command_hash,
            audit_context,
            server_generation: self.server_generation,
            room_id: self.room.room.room_id,
            result: result.clone(),
            before_revisions,
        })?;
        self.operator_state = candidate;
        self.operator_command_event_seq = self.operator_command_event_seq.saturating_add(1);
        if self.operator_command_events.len() == OPERATOR_COMMAND_EVENT_CAPACITY {
            self.operator_command_events.pop_front();
        }
        self.operator_command_events
            .push_back(OperatorCommandEvent {
                event_seq: self.operator_command_event_seq,
                server_generation: self.server_generation,
                room_id: self.room.room.room_id,
                result,
            });
        Ok(())
    }

    fn record_interrupted_session(
        &mut self,
        session: &SupersededSession,
    ) -> Result<(), OperatorLedgerError> {
        for command in &session.interrupted_deliveries {
            let code = if command.acknowledged {
                ErrorCode::CommandOutcomeUnknown
            } else {
                ErrorCode::MemberDisconnected
            };
            let result = operator_interrupted_result(command, code);
            self.record_operator_terminal(
                command.request_key.clone(),
                command.command_hash,
                command.audit_context.clone(),
                command.before_revisions,
                result,
            )?;
        }
        Ok(())
    }

    /// Marks transport-level command acknowledgement for disconnect semantics.
    pub fn acknowledge_command(
        &mut self,
        session_id: RegistrySessionId,
        request_id: RequestId,
    ) -> Result<(), SessionError> {
        let session = self
            .session_mut(session_id)
            .ok_or(SessionError::UnknownSession)?;
        let command = session
            .in_flight
            .get_mut(&request_id)
            .ok_or(SessionError::UnknownCommand)?;
        command.acknowledged = true;
        Ok(())
    }

    /// Completes and removes one volatile in-flight command.
    pub fn complete_command(
        &mut self,
        session_id: RegistrySessionId,
        request_id: RequestId,
    ) -> Result<InFlightCommand, SessionError> {
        let delivered = self
            .session_mut(session_id)
            .ok_or(SessionError::UnknownSession)?
            .in_flight
            .remove(&request_id)
            .ok_or(SessionError::UnknownCommand)?;
        let result = operator_interrupted_result(&delivered, ErrorCode::CommandOutcomeUnknown);
        self.record_operator_terminal(
            delivered.request_key.clone(),
            delivered.command_hash,
            delivered.audit_context.clone(),
            delivered.before_revisions,
            result,
        )?;
        Ok(delivered)
    }

    /// Validates a terminal member result against its exact session and updates
    /// the volatile member revision projection.
    pub fn apply_member_command_result(
        &mut self,
        message: ClientMessage,
    ) -> Result<CompletedMemberCommand, SessionError> {
        let ClientMessage::MemberCommandResult {
            request_id,
            registry_session_id,
            capability,
            status,
            revisions,
            code,
            ..
        } = message
        else {
            return Err(SessionError::ExpectedCommandResult);
        };
        if revisions.room_revision != Some(self.room.room.room_revision) {
            return Err(SessionError::StaleRoomRevision);
        }
        let session = self
            .session_mut(registry_session_id)
            .ok_or(SessionError::UnknownSession)?;
        let delivered = session
            .in_flight
            .get(&request_id)
            .ok_or(SessionError::UnknownCommand)?;
        if delivered.capability != capability {
            return Err(SessionError::CommandCapabilityMismatch);
        }
        let member_config_revision = revisions
            .member_config_revision
            .ok_or(SessionError::MissingCommandRevision)?;
        let catalog_revision = revisions
            .catalog_revision
            .ok_or(SessionError::MissingCommandRevision)?;
        let grant_revision = revisions
            .grant_revision
            .ok_or(SessionError::MissingCommandRevision)?;
        if member_config_revision < session.member_config_revision
            || catalog_revision < session.catalog_revision
            || grant_revision != session.grant_revision
        {
            return Err(SessionError::StaleCommandResult);
        }
        session.member_config_revision = member_config_revision;
        session.catalog_revision = catalog_revision;
        let delivered = session
            .in_flight
            .remove(&request_id)
            .expect("in-flight command was checked");
        let operator_result = OperatorCommandResult {
            request_id,
            target_member_id: Some(session.member_id),
            capability: Some(capability),
            command: delivered.command,
            status: status.into(),
            revisions,
            code,
        };
        self.record_operator_terminal(
            delivered.request_key.clone(),
            delivered.command_hash,
            delivered.audit_context.clone(),
            delivered.before_revisions,
            operator_result,
        )?;
        Ok(CompletedMemberCommand {
            delivered,
            status,
            code,
        })
    }

    /// Removes commands whose five-second terminal deadline elapsed. The
    /// returned failures are terminal and are never replayed on reconnect.
    pub fn expire_member_commands(&mut self, now_ms: u64) -> Vec<InterruptedCommand> {
        let mut expired = Vec::new();
        let mut terminal = Vec::new();
        for slots in self.sessions.values_mut() {
            for session in slots.active.iter_mut().chain(slots.replacement.iter_mut()) {
                let request_ids: Vec<_> = session
                    .in_flight
                    .values()
                    .filter(|command| {
                        now_ms >= command.deadline_ms
                            || (!command.acknowledged && now_ms >= command.ack_deadline_ms)
                    })
                    .map(|command| command.request_id)
                    .collect();
                for request_id in request_ids {
                    if let Some(command) = session.in_flight.remove(&request_id) {
                        expired.push(InterruptedCommand {
                            request_id,
                            code: ErrorCode::MemberCommandTimeout,
                        });
                        terminal.push(command);
                    }
                }
            }
        }
        for command in terminal {
            let result = operator_interrupted_result(&command, ErrorCode::MemberCommandTimeout);
            self.record_operator_terminal(
                command.request_key.clone(),
                command.command_hash,
                command.audit_context.clone(),
                command.before_revisions,
                result,
            )
            .expect("in-flight request has a matching operator ledger record");
        }
        expired
    }

    /// Removes an exact transport. Active loss never replays an in-flight
    /// command; a binding replacement is promoted without an old endpoint.
    pub fn disconnect(
        &mut self,
        session_id: RegistrySessionId,
    ) -> Result<Vec<InterruptedCommand>, SessionError> {
        let (member_id, replacement) = self
            .locate_session(session_id)
            .ok_or(SessionError::UnknownSession)?;
        if replacement {
            let session = self
                .sessions
                .get_mut(&member_id)
                .and_then(|slots| slots.replacement.take())
                .expect("located replacement exists");
            let interrupted = interrupt_session(session);
            self.record_interrupted_session(&interrupted)?;
            return Ok(interrupted.interrupted_commands);
        }

        let slots = self
            .sessions
            .get_mut(&member_id)
            .expect("located member slots exist");
        let old = slots.active.take().expect("located active exists");
        let old_run_id = old.run_id;
        let interrupted_session = interrupt_session(old);
        let interrupted = interrupted_session.interrupted_commands.clone();
        let event = if let Some(mut replacement_session) = slots.replacement.take() {
            let replacement_run_id = replacement_session.run_id;
            replacement_session.endpoint = None;
            replacement_session.state = MemberSessionState::ApprovedUnbound;
            slots.active = Some(replacement_session);
            if replacement_run_id == old_run_id {
                RoomEventValue::MemberMediaUnbound { member_id }
            } else {
                RoomEventValue::MemberRunReplaced {
                    member_id,
                    old_run_id,
                    new_run_id: replacement_run_id,
                }
            }
        } else {
            RoomEventValue::MemberControlDown { member_id }
        };
        let went_offline = slots.active.is_none();
        if went_offline {
            self.sessions.remove(&member_id);
        }
        self.record_interrupted_session(&interrupted_session)?;
        self.bump_presence()?;
        self.push_event(event);
        Ok(interrupted)
    }

    /// Expires all sessions whose monotonic lease deadline has passed.
    pub fn expire_leases(&mut self, now_ms: u64) -> Vec<InterruptedCommand> {
        let expired: Vec<_> = self
            .sessions
            .values()
            .flat_map(|slots| {
                slots
                    .active
                    .iter()
                    .chain(slots.replacement.iter())
                    .filter(move |session| now_ms >= session.lease_deadline_ms)
                    .map(|session| session.registry_session_id)
            })
            .collect();
        let mut interrupted = Vec::new();
        for session_id in expired {
            if let Ok(mut commands) = self.disconnect(session_id) {
                interrupted.append(&mut commands);
            }
        }
        interrupted
    }

    /// Installs a room state only after the caller has persisted the candidate.
    /// Removal synchronously revokes all volatile sessions for that identity.
    pub fn install_persisted_transition(
        &mut self,
        transition: RoomTransition,
    ) -> Result<Vec<InterruptedCommand>, SessionError> {
        if transition.state.room.room_id != self.room.room.room_id {
            return Err(SessionError::RoomScope);
        }
        let event = transition.event.clone();
        self.room = transition.state;
        let mut interrupted = Vec::new();
        if let Some(PersistentRoomEvent::AdmissionChanged {
            member_id,
            admission: AdmissionState::Removed,
            ..
        }) = event
            && let Some(slots) = self.sessions.remove(&member_id)
        {
            for session in slots.active.into_iter().chain(slots.replacement) {
                let interrupted_session = interrupt_session(session);
                self.record_interrupted_session(&interrupted_session)?;
                interrupted.extend(interrupted_session.interrupted_commands);
            }
            self.bump_presence()?;
        }
        if let Some(event) = event.map(map_persistent_event) {
            self.push_event(event);
        }
        Ok(interrupted)
    }

    /// Builds a complete permission-filtered snapshot for an approved member.
    pub fn snapshot_for(&self, viewer_id: MemberId) -> Result<RoomSnapshot, SessionError> {
        let viewer = self
            .room
            .member(viewer_id)
            .ok_or(SessionError::MemberNotApproved)?;
        if viewer.admission != AdmissionState::Approved {
            return Err(SessionError::MemberNotApproved);
        }
        let mut members = Vec::new();
        for record in self
            .room
            .members
            .iter()
            .filter(|record| record.admission == AdmissionState::Approved)
        {
            let session = self.active_session(record.member_id);
            let sources = match session {
                Some(session) => session
                    .sources
                    .iter()
                    .filter(|source| {
                        record.member_id == viewer_id
                            || (source.state().effective_profile()
                                == source
                                    .state()
                                    .selected_profile()
                                    .capped_by(self.room.room.quality_cap)
                                && self
                                    .room
                                    .visible_viewers(record.member_id, source.source_id())
                                    .is_ok_and(|viewers| viewers.contains(&viewer_id)))
                    })
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            members.push(SnapshotMember {
                member_id: record.member_id,
                display_name: session.map_or_else(
                    || record.last_display_name.clone(),
                    |session| session.display_name.clone(),
                ),
                admission: AdmissionState::Approved,
                control_presence: if session.is_some() {
                    ControlPresence::Online
                } else {
                    ControlPresence::Offline
                },
                media_presence: if session.and_then(|session| session.endpoint).is_some() {
                    MediaPresence::Bound
                } else {
                    MediaPresence::Unbound
                },
                run_id: session.map(|session| session.run_id),
                media_endpoint: session.and_then(|session| session.endpoint),
                catalog_revision: session.map(|session| session.catalog_revision),
                sources,
            });
        }
        Ok(RoomSnapshot {
            server_generation: self.server_generation,
            room_id: self.room.room.room_id,
            room_revision: self.room.room.room_revision,
            presence_revision: self.presence_revision,
            event_cursor: DecimalU64::new(self.event_seq),
            room: RoomView {
                name: self.room.room.name.clone(),
                quality_cap: self.room.room.quality_cap,
                qos_policy: self.room.room.qos_policy,
                room_send_muted: self.room.room.room_send_muted,
                max_approved_members: 32,
            },
            self_view: SelfView {
                member_id: viewer_id,
                admission: AdmissionState::Approved,
            },
            members,
        })
    }

    /// Returns retained events strictly after a cursor, or `None` when the ring
    /// can no longer provide a gap-free sequence.
    #[must_use]
    pub fn events_after(&self, cursor: u64) -> Option<Vec<&AuthorityEvent>> {
        if cursor == self.event_seq {
            return Some(Vec::new());
        }
        let first = self.events.front()?.event.event_seq.get();
        if cursor.saturating_add(1) < first {
            return None;
        }
        Some(
            self.events
                .iter()
                .filter(|event| event.event.event_seq.get() > cursor)
                .collect(),
        )
    }

    /// Returns generation-local terminal operator events strictly after a
    /// cursor, or `None` after an event-ring miss.
    #[must_use]
    pub fn operator_command_events_after(&self, cursor: u64) -> Option<Vec<&OperatorCommandEvent>> {
        if cursor == self.operator_command_event_seq {
            return Some(Vec::new());
        }
        let first = self.operator_command_events.front()?.event_seq;
        if cursor.saturating_add(1) < first {
            return None;
        }
        Some(
            self.operator_command_events
                .iter()
                .filter(|event| event.event_seq > cursor)
                .collect(),
        )
    }

    fn locate_session(&self, session_id: RegistrySessionId) -> Option<(MemberId, bool)> {
        self.sessions.iter().find_map(|(member_id, slots)| {
            if slots
                .active
                .as_ref()
                .is_some_and(|session| session.registry_session_id == session_id)
            {
                Some((*member_id, false))
            } else if slots
                .replacement
                .as_ref()
                .is_some_and(|session| session.registry_session_id == session_id)
            {
                Some((*member_id, true))
            } else {
                None
            }
        })
    }

    fn session_mut(&mut self, session_id: RegistrySessionId) -> Option<&mut MemberSession> {
        let (member_id, replacement) = self.locate_session(session_id)?;
        self.session_by_location_mut(member_id, replacement)
    }

    fn session_by_location_mut(
        &mut self,
        member_id: MemberId,
        replacement: bool,
    ) -> Option<&mut MemberSession> {
        let slots = self.sessions.get_mut(&member_id)?;
        if replacement {
            slots.replacement.as_mut()
        } else {
            slots.active.as_mut()
        }
    }

    fn bump_presence(&mut self) -> Result<(), SessionError> {
        self.presence_revision = self
            .presence_revision
            .checked_next()
            .ok_or(SessionError::RevisionExhausted)?;
        Ok(())
    }

    fn push_event(&mut self, event: RoomEventValue) {
        self.event_seq = self.event_seq.saturating_add(1);
        if self.events.len() == EVENT_RING_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(AuthorityEvent {
            event: RoomEvent {
                event_seq: DecimalU64::new(self.event_seq),
                server_generation: self.server_generation,
                room_id: self.room.room.room_id,
                room_revision: self.room.room.room_revision,
                presence_revision: self.presence_revision,
                event,
            },
        });
    }
}

fn interrupt_session(session: MemberSession) -> SupersededSession {
    let interrupted_deliveries: Vec<_> = session.in_flight.into_values().collect();
    let interrupted_commands = interrupted_deliveries
        .iter()
        .map(|command| InterruptedCommand {
            request_id: command.request_id,
            code: if command.acknowledged {
                ErrorCode::CommandOutcomeUnknown
            } else {
                ErrorCode::MemberDisconnected
            },
        })
        .collect();
    SupersededSession {
        registry_session_id: session.registry_session_id,
        run_id: session.run_id,
        interrupted_commands,
        interrupted_deliveries,
    }
}

fn operator_interrupted_result(
    command: &InFlightCommand,
    code: ErrorCode,
) -> OperatorCommandResult {
    OperatorCommandResult {
        request_id: command.request_id,
        target_member_id: Some(command.target_member_id),
        capability: Some(command.capability),
        command: command.command,
        status: if code == ErrorCode::CommandOutcomeUnknown {
            OperatorCommandStatus::OutcomeUnknown
        } else {
            OperatorCommandStatus::Rejected
        },
        revisions: command.accepted_revisions,
        code: Some(code),
    }
}

fn reuses_active_epoch(active: &MemberSession, replacement: &MemberSession) -> bool {
    replacement.sources.iter().any(|new_source| {
        let Some((_, new_epoch)) = new_source.active() else {
            return false;
        };
        active.sources.iter().any(|old_source| {
            old_source.source_id() == new_source.source_id()
                && old_source
                    .active()
                    .is_some_and(|(_, old_epoch)| old_epoch == new_epoch)
        })
    })
}

fn source_send_ready_is_valid(
    source: &SourceDescriptor,
    local_emergency_stop: bool,
    room_send_muted: bool,
) -> bool {
    let state = source.state();
    state.send_ready()
        == (state.available()
            && state.master_capture_enabled()
            && state.publish_requested()
            && !local_emergency_stop
            && !room_send_muted)
}

fn validate_session_display_name(value: &str) -> Result<(), SessionError> {
    if value.is_empty()
        || value.len() > 64
        || value.nfc().ne(value.chars())
        || value
            .chars()
            .any(|character| matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'))
    {
        Err(SessionError::InvalidDisplayName)
    } else {
        Ok(())
    }
}

fn normalize_session_source_names(sources: &mut [SourceDescriptor]) -> Result<(), SessionError> {
    for source in sources {
        let display_name: String = source.display_name().nfc().collect();
        if display_name != source.display_name() {
            *source = SourceDescriptor::new(
                source.source_id(),
                source.kind(),
                display_name,
                source.state(),
                source.active(),
            )
            .map_err(|_| SessionError::InvalidCatalog)?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct AuthorityCommandHashBody<'a> {
    request_id: RequestId,
    room_id: rlb_model::RoomId,
    expected_room_revision: RoomRevision,
    mutation: &'a AuthorityMutation,
}

fn authority_command_result(
    request_id: RequestId,
    mutation: &AuthorityMutation,
    status: OperatorCommandStatus,
    revisions: CanonicalRevisions,
    code: Option<ErrorCode>,
) -> OperatorCommandResult {
    let (command, target_member_id) = match mutation {
        AuthorityMutation::SetName { .. } => (rlb_control::RoomCommandName::RoomNameSet, None),
        AuthorityMutation::Approve { member_id } => (
            rlb_control::RoomCommandName::AdmissionApprove,
            Some(*member_id),
        ),
        AuthorityMutation::Reject { member_id } => (
            rlb_control::RoomCommandName::AdmissionReject,
            Some(*member_id),
        ),
        AuthorityMutation::Remove { member_id, .. } => (
            rlb_control::RoomCommandName::AdmissionRemove,
            Some(*member_id),
        ),
        AuthorityMutation::ReplaceVisibility {
            publisher_member_id,
            ..
        } => (
            rlb_control::RoomCommandName::SourceVisibilityReplace,
            Some(*publisher_member_id),
        ),
        AuthorityMutation::SetQualityCap { .. } => {
            (rlb_control::RoomCommandName::RoomQualityCapSet, None)
        }
        AuthorityMutation::SetQos { .. } => (rlb_control::RoomCommandName::RoomQosSet, None),
        AuthorityMutation::SetSendMute { .. } => {
            (rlb_control::RoomCommandName::RoomSendMuteSet, None)
        }
    };
    OperatorCommandResult {
        request_id,
        target_member_id,
        capability: None,
        command,
        status,
        revisions,
        code,
    }
}

fn delegated_server_message(
    session_id: RegistrySessionId,
    command: &ValidatedDelegatedCommand,
) -> ServerMessage {
    let mut scope = RoomCommandScope {
        room_id: Field::Present(command.room_id),
        member_id: Field::Present(command.member_id),
        ..RoomCommandScope::default()
    };
    let mut args = RoomCommandArgs::default();
    match command.mutation {
        DelegatedMutation::SetSourcePublication {
            source_id,
            publish_requested,
        } => {
            scope.source_id = Field::Present(source_id);
            args.publish_requested = Field::Present(publish_requested);
        }
        DelegatedMutation::SetSendQuality {
            source_id,
            selected_profile,
        } => {
            scope.source_id = Field::Present(source_id);
            args.selected_profile = Field::Present(selected_profile);
        }
        DelegatedMutation::SetMemberMix {
            remote_member_id,
            gain_db,
            mute,
            solo,
        } => {
            scope.remote_member_id = Field::Present(remote_member_id);
            args.gain_db = Field::Present(gain_db);
            args.mute = Field::Present(mute);
            args.solo = Field::Present(solo);
        }
        DelegatedMutation::SetSourceMix {
            remote_member_id,
            source_id,
            gain_db,
            pan,
            mute,
        } => {
            scope.remote_member_id = Field::Present(remote_member_id);
            scope.source_id = Field::Present(source_id);
            args.gain_db = Field::Present(gain_db);
            args.pan = Field::Present(pan);
            args.mute = Field::Present(mute);
        }
        DelegatedMutation::SetAutoReconnect { enabled } => {
            args.enabled = Field::Present(enabled);
        }
    }
    ServerMessage::MemberCommand {
        v: rlb_control::Version2,
        request_id: command.request_id,
        registry_session_id: session_id,
        capability: command.capability(),
        command: command.mutation.command_name(),
        scope,
        expected: RoomCommandExpected {
            room_revision: Field::Present(command.expected.room_revision),
            member_config_revision: Field::Present(command.expected.member_config_revision),
            catalog_revision: command
                .expected
                .catalog_revision
                .map_or(Field::Missing, Field::Present),
            grant_revision: Field::Present(command.expected.grant_revision),
        },
        args,
        delivery_ttl_ms: 5_000,
    }
}

fn map_persistent_event(event: PersistentRoomEvent) -> RoomEventValue {
    match event {
        PersistentRoomEvent::AdmissionChanged {
            member_id,
            admission,
            ..
        } => RoomEventValue::AdmissionChanged {
            member_id,
            admission,
        },
        PersistentRoomEvent::CredentialIssued { member_id, .. } => {
            RoomEventValue::AdmissionChanged {
                member_id,
                admission: AdmissionState::Approved,
            }
        }
        PersistentRoomEvent::RoomNameChanged { name } => RoomEventValue::RoomNameChanged { name },
        PersistentRoomEvent::SourceVisibilityChanged {
            publisher_member_id,
            source_id,
            viewer_member_ids,
        } => RoomEventValue::SourceVisibilityChanged {
            publisher_member_id,
            source_id,
            viewer_member_ids,
        },
        PersistentRoomEvent::QualityCapChanged { profile } => RoomEventValue::QualityCapChanged {
            quality_cap: profile,
        },
        PersistentRoomEvent::QosChanged { mode } => {
            RoomEventValue::QosPolicyChanged { qos_policy: mode }
        }
        PersistentRoomEvent::SendMuteChanged { enabled } => RoomEventValue::RoomSendMutedChanged {
            room_send_muted: enabled,
        },
    }
}

/// Session bootstrap, lease, catalog, or binding failure.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("first session message must be engine_hello")]
    ExpectedEngineHello,
    #[error("message must be member_command_result")]
    ExpectedCommandResult,
    #[error(transparent)]
    Credential(#[from] crate::token::CredentialError),
    #[error("member is not approved")]
    MemberNotApproved,
    #[error("member display name violates its bound")]
    InvalidDisplayName,
    #[error("member already has active and replacement sessions")]
    DuplicateIdentity,
    #[error("catalog does not match fixed source slots")]
    InvalidCatalog,
    #[error("source identity changed")]
    SourceIdentityChanged,
    #[error("catalog effective profile conflicts with room policy")]
    QualityPolicyOutOfSync,
    #[error("new run reused an active source epoch")]
    EpochReusedAcrossRun,
    #[error("source format changed without a new epoch")]
    EpochContinuity,
    #[error("session identity is unknown")]
    UnknownSession,
    #[error("session state does not allow this operation")]
    InvalidState,
    #[error("server generation is stale")]
    StaleGeneration,
    #[error("binding packet does not match the current challenge")]
    InvalidBind,
    #[error("binding nonce expired")]
    BindNonceExpired,
    #[error("catalog run does not match the session")]
    RunMismatch,
    #[error("catalog revision is stale")]
    StaleCatalogRevision,
    #[error("grant revision is stale")]
    StaleGrantRevision,
    #[error("member config revision is stale")]
    StaleMemberConfigRevision,
    #[error("command identity is unknown")]
    UnknownCommand,
    #[error("command result capability does not match delivery")]
    CommandCapabilityMismatch,
    #[error("command result omits a canonical revision")]
    MissingCommandRevision,
    #[error("command result revision is stale")]
    StaleCommandResult,
    #[error("command result room revision is stale")]
    StaleRoomRevision,
    #[error("room scope does not match")]
    RoomScope,
    #[error("revision counter is exhausted")]
    RevisionExhausted,
    #[error("operating-system random source is unavailable")]
    Random(#[from] rlb_model::IdGenerationError),
    #[error("secret random source is unavailable")]
    SecretRandom(#[from] SecretGenerationError),
    #[error(transparent)]
    OperatorLedger(#[from] OperatorLedgerError),
}

impl SessionError {
    /// Returns the stable protocol error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::ExpectedEngineHello | Self::ExpectedCommandResult | Self::InvalidState => {
                ErrorCode::InvalidStateTransition
            }
            Self::Credential(error) => error.code(),
            Self::MemberNotApproved => ErrorCode::AdmissionDenied,
            Self::InvalidDisplayName => ErrorCode::InvalidValue,
            Self::DuplicateIdentity => ErrorCode::DuplicateIdentity,
            Self::InvalidCatalog => ErrorCode::InvalidCatalog,
            Self::SourceIdentityChanged => ErrorCode::SourceIdentityChanged,
            Self::QualityPolicyOutOfSync => ErrorCode::QualityPolicyOutOfSync,
            Self::EpochReusedAcrossRun => ErrorCode::MediaEpochConflict,
            Self::EpochContinuity => ErrorCode::MediaEpochConflict,
            Self::UnknownSession => ErrorCode::UnknownSession,
            Self::StaleGeneration => ErrorCode::StaleGeneration,
            Self::InvalidBind => ErrorCode::MediaBindInvalid,
            Self::BindNonceExpired => ErrorCode::MediaBindNonceExpired,
            Self::RunMismatch => ErrorCode::RunMismatch,
            Self::StaleCatalogRevision => ErrorCode::StaleCatalogRevision,
            Self::StaleGrantRevision => ErrorCode::StaleGrantRevision,
            Self::StaleMemberConfigRevision => ErrorCode::StaleMemberConfigRevision,
            Self::UnknownCommand => ErrorCode::UnknownCommand,
            Self::CommandCapabilityMismatch => ErrorCode::CommandScopeMismatch,
            Self::MissingCommandRevision => ErrorCode::InvalidValue,
            Self::StaleCommandResult => ErrorCode::StaleMemberConfigRevision,
            Self::StaleRoomRevision => ErrorCode::StaleRoomRevision,
            Self::RoomScope => ErrorCode::RoomScopeMismatch,
            Self::RevisionExhausted
            | Self::Random(_)
            | Self::SecretRandom(_)
            | Self::OperatorLedger(_) => ErrorCode::UnexpectedError,
        }
    }
}

/// Immediate command-route rejection. No command is queued for any variant.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RouteError {
    #[error("room scope does not match")]
    RoomScope,
    #[error("room revision is stale")]
    StaleRoomRevision,
    #[error("member does not exist")]
    MemberNotFound,
    #[error("member is not approved")]
    MemberNotApproved,
    #[error("member has no current online session")]
    MemberOffline,
    #[error("capability is not granted")]
    CapabilityDenied,
    #[error("member config revision is stale")]
    StaleMemberConfigRevision,
    #[error("grant revision is stale")]
    StaleGrantRevision,
    #[error("catalog revision is stale")]
    StaleCatalogRevision,
    #[error("source does not exist")]
    SourceNotFound,
    #[error("target is outside the member's visible room projection")]
    TargetNotVisible,
    #[error("request ID is already in flight")]
    RequestIdConflict,
    #[error("authenticated operator principal is invalid")]
    InvalidPrincipal,
    #[error("operator audit context is invalid")]
    InvalidAuditContext,
    #[error("operator request ledger failed")]
    LedgerFailure,
    #[error("session command bound is full")]
    ServerBusy,
}

impl RouteError {
    /// Returns the stable protocol error code.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::RoomScope => ErrorCode::RoomScopeMismatch,
            Self::StaleRoomRevision => ErrorCode::StaleRoomRevision,
            Self::MemberNotFound => ErrorCode::MemberNotFound,
            Self::MemberNotApproved => ErrorCode::AdmissionDenied,
            Self::MemberOffline => ErrorCode::MemberOffline,
            Self::CapabilityDenied => ErrorCode::CapabilityDenied,
            Self::StaleMemberConfigRevision => ErrorCode::StaleMemberConfigRevision,
            Self::StaleGrantRevision => ErrorCode::StaleGrantRevision,
            Self::StaleCatalogRevision => ErrorCode::StaleCatalogRevision,
            Self::SourceNotFound => ErrorCode::SourceNotFound,
            Self::TargetNotVisible => ErrorCode::TargetNotVisible,
            Self::RequestIdConflict => ErrorCode::RequestIdConflict,
            Self::InvalidPrincipal => ErrorCode::InvalidValue,
            Self::InvalidAuditContext => ErrorCode::InvalidValue,
            Self::LedgerFailure => ErrorCode::UnexpectedError,
            Self::ServerBusy => ErrorCode::ServerBusy,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use rlb_control::{DelegatedExpected, DelegatedMutation};
    use rlb_model::{
        CaptureMethod, EndpointMode, QualityProfile, RoomId, SourceFormat, SourceKind, SourceState,
        StreamEpoch,
    };

    use super::*;
    use crate::room::{RoomAction, reduce_room};
    use crate::token::{ClaimSecret, CredentialError, MemberToken};
    use crate::wire::{DriverSigningState, SafeLocalFields, SourceSlot};

    fn room_id() -> RoomId {
        RoomId::from_bytes(1_u128.to_be_bytes()).unwrap()
    }

    fn member_id() -> MemberId {
        MemberId::from_bytes(2_u128.to_be_bytes()).unwrap()
    }

    fn source_id(value: u128) -> rlb_model::SourceId {
        rlb_model::SourceId::from_bytes(value.to_be_bytes()).unwrap()
    }

    fn run_id(value: u128) -> RunId {
        RunId::from_bytes(value.to_be_bytes()).unwrap()
    }

    fn request_id(value: u128) -> RequestId {
        RequestId::from_bytes(value.to_be_bytes()).unwrap()
    }

    fn audit_context() -> OperatorAuditContext {
        OperatorAuditContext {
            principal: "operator-1".to_owned(),
            principal_kind: crate::operator::OperatorPrincipalKind::Operator,
            wall_time: "2026-09-18T05:42:00+08:00".to_owned(),
        }
    }

    fn projection() -> MemberProjection {
        MemberProjection {
            local_emergency_stop: false,
            auto_reconnect: true,
            effective_room_send_muted: false,
            effective_qos_policy: rlb_model::QosPolicy::AudioPriority,
            safe_local_fields: SafeLocalFields {
                capture_method: CaptureMethod::WasapiLoopback,
                capture_endpoint_mode: EndpointMode::FollowDefault,
                capture_friendly_label: "Default".to_owned(),
                capture_available: true,
                playback_endpoint_mode: EndpointMode::FollowDefault,
                playback_friendly_label: "Default".to_owned(),
                playback_available: true,
                driver_version: None,
                driver_signing_state: DriverSigningState::NotInstalled,
                driver_available: false,
                master_capture_enabled: false,
                local_emergency_stop: false,
                startup_enabled: false,
                file_logging_enabled: false,
            },
        }
    }

    fn sources() -> Vec<SourceDescriptor> {
        [
            (source_id(3), SourceKind::SystemAudio, "System audio"),
            (source_id(4), SourceKind::Microphone, "Microphone"),
        ]
        .into_iter()
        .map(|(source_id, kind, name)| {
            SourceDescriptor::new(
                source_id,
                kind,
                name,
                SourceState::new(
                    false,
                    false,
                    false,
                    QualityProfile::PcmF32V1,
                    QualityProfile::PcmF32V1,
                    false,
                )
                .unwrap(),
                None::<(SourceFormat, StreamEpoch)>,
            )
            .unwrap()
        })
        .collect()
    }

    fn approved_room() -> (RoomState, MemberToken) {
        let claim = ClaimSecret::generate().unwrap();
        let token = MemberToken::from_bytes(vec![7; 32]).unwrap();
        let state = RoomState::new(room_id(), "room").unwrap();
        let state = reduce_room(
            &state,
            RoomAction::Enroll {
                member_id: member_id(),
                display_name: "member".to_owned(),
                source_slots: [
                    SourceSlot {
                        source_id: source_id(3),
                        kind: SourceKind::SystemAudio,
                    },
                    SourceSlot {
                        source_id: source_id(4),
                        kind: SourceKind::Microphone,
                    },
                ],
                claim_sha256: claim.digest(),
                request_readmission: false,
            },
        )
        .unwrap()
        .state;
        let state = reduce_room(
            &state,
            RoomAction::Approve {
                member_id: member_id(),
            },
        )
        .unwrap()
        .state;
        let state = reduce_room(
            &state,
            RoomAction::Claim {
                member_id: member_id(),
                claim_secret: claim,
                issued_token: token.clone(),
            },
        )
        .unwrap()
        .state;
        (state, token)
    }

    fn hello(token: MemberToken, run: RunId, grants: CapabilityGrants) -> EngineHello {
        let token_sha256 = TokenDigest::of(token.expose_bytes());
        EngineHello {
            request_id: request_id(10),
            member_token: token,
            token_generation: 1,
            token_sha256,
            room_id: room_id(),
            member_id: member_id(),
            run_id: run,
            display_name: "member".to_owned(),
            client_build: "2.0.0-test".to_owned(),
            catalog_revision: CatalogRevision::new(1),
            sources: sources(),
            member_config_revision: MemberConfigRevision::new(1),
            member_projection: projection(),
            grant_revision: GrantRevision::new(1),
            capability_grants: grants,
        }
    }

    fn publication_command(
        request: u128,
        room_revision: RoomRevision,
    ) -> ValidatedDelegatedCommand {
        ValidatedDelegatedCommand {
            request_id: request_id(request),
            room_id: room_id(),
            member_id: member_id(),
            expected: DelegatedExpected {
                room_revision,
                member_config_revision: MemberConfigRevision::new(1),
                catalog_revision: Some(CatalogRevision::new(1)),
                grant_revision: GrantRevision::new(1),
            },
            mutation: DelegatedMutation::SetSourcePublication {
                source_id: source_id(3),
                publish_requested: true,
            },
        }
    }

    #[test]
    fn normative_state_machine_rejects_skipped_edges() {
        assert!(
            MemberSessionState::Disconnected
                .transition(SessionTransition::PrefaceAccepted)
                .is_err()
        );
        let state = MemberSessionState::Disconnected
            .transition(SessionTransition::TcpConnected)
            .unwrap()
            .transition(SessionTransition::PrefaceAccepted)
            .unwrap()
            .transition(SessionTransition::EngineHelloReceived)
            .unwrap()
            .transition(SessionTransition::AuthenticationAccepted)
            .unwrap();
        assert_eq!(state, MemberSessionState::ApprovedUnbound);
    }

    #[test]
    fn commands_require_online_approved_grant_and_are_never_queued_offline() {
        let (room, token) = approved_room();
        let room_revision = room.room.room_revision;
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let grants = CapabilityGrants {
            source_publication: true,
            ..CapabilityGrants::default()
        };
        let ready = authority
            .accept_engine_hello(hello(token, run_id(20), grants), 0)
            .unwrap();
        let routed = authority
            .route_member_command(audit_context(), publication_command(30, room_revision), 1)
            .unwrap();
        let MemberCommandRoute::Delivered(routed) = routed else {
            panic!("first request must be delivered");
        };
        assert_eq!(routed.session_id, ready.registry_session_id);
        authority
            .acknowledge_command(ready.registry_session_id, request_id(30))
            .unwrap();
        let interrupted = authority.disconnect(ready.registry_session_id).unwrap();
        assert_eq!(
            interrupted,
            vec![InterruptedCommand {
                request_id: request_id(30),
                code: ErrorCode::CommandOutcomeUnknown,
            }]
        );
        let replayed = authority
            .route_member_command(audit_context(), publication_command(30, room_revision), 2)
            .unwrap();
        let MemberCommandRoute::Cached(replayed) = replayed else {
            panic!("interrupted request must be cached");
        };
        assert_eq!(replayed.status, OperatorCommandStatus::OutcomeUnknown);
        assert_eq!(replayed.code, Some(ErrorCode::CommandOutcomeUnknown));
        assert_eq!(
            authority
                .route_member_command(audit_context(), publication_command(31, room_revision), 2)
                .unwrap_err(),
            RouteError::MemberOffline
        );
        assert!(authority.active_session(member_id()).is_none());
    }

    #[test]
    fn revoked_summary_stops_new_delivery() {
        let (room, token) = approved_room();
        let room_revision = room.room.room_revision;
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let grants = CapabilityGrants {
            source_publication: true,
            ..CapabilityGrants::default()
        };
        let ready = authority
            .accept_engine_hello(hello(token, run_id(20), grants), 0)
            .unwrap();
        let command = publication_command(32, room_revision);
        assert!(matches!(
            authority
                .route_member_command(audit_context(), command.clone(), 1)
                .unwrap(),
            MemberCommandRoute::Delivered(_)
        ));
        let interrupted = authority
            .replace_grants(
                ready.registry_session_id,
                GrantRevision::new(2),
                CapabilityGrants::default(),
            )
            .unwrap();
        assert_eq!(
            interrupted,
            vec![InterruptedCommand {
                request_id: request_id(32),
                code: ErrorCode::CapabilityRevoked,
            }]
        );
        assert!(
            authority
                .active_session(member_id())
                .unwrap()
                .in_flight
                .is_empty()
        );
        let replay = authority
            .route_member_command(audit_context(), command, 2)
            .expect("revoked terminal result is retained");
        let MemberCommandRoute::Cached(replay) = replay else {
            panic!("revoked request must not be delivered again");
        };
        assert_eq!(replay.code, Some(ErrorCode::CapabilityRevoked));
        assert_eq!(replay.revisions.grant_revision, Some(GrantRevision::new(2)));
        let events = authority
            .operator_command_events_after(0)
            .expect("operator event retained");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].result.code, Some(ErrorCode::CapabilityRevoked));

        let mut new_command = publication_command(33, room_revision);
        new_command.expected.grant_revision = GrantRevision::new(2);
        assert_eq!(
            authority
                .route_member_command(audit_context(), new_command, 3)
                .unwrap_err(),
            RouteError::CapabilityDenied
        );
    }

    #[test]
    fn operator_request_ledger_caches_in_flight_timeout_and_restart() {
        let (room, token) = approved_room();
        let persisted_room = room.clone();
        let room_revision = room.room.room_revision;
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let grants = CapabilityGrants {
            source_publication: true,
            ..CapabilityGrants::default()
        };
        authority
            .accept_engine_hello(hello(token, run_id(20), grants), 0)
            .unwrap();
        let command = publication_command(40, room_revision);
        assert!(matches!(
            authority
                .route_member_command(audit_context(), command.clone(), 1)
                .unwrap(),
            MemberCommandRoute::Delivered(_)
        ));
        assert!(matches!(
            authority
                .route_member_command(audit_context(), command.clone(), 2)
                .unwrap(),
            MemberCommandRoute::InFlight { request_id: id } if id == request_id(40)
        ));
        let mut interrupted_restart = RegistryAuthority::with_operator_state(
            persisted_room.clone(),
            ServerGeneration::from_bytes(11_u128.to_be_bytes()).unwrap(),
            authority.operator_state().clone(),
            "2026-09-18T05:43:00+08:00",
        )
        .unwrap();
        assert!(matches!(
            interrupted_restart
                .route_member_command(audit_context(), command.clone(), 0)
                .unwrap(),
            MemberCommandRoute::Cached(OperatorCommandResult {
                status: OperatorCommandStatus::OutcomeUnknown,
                code: Some(ErrorCode::CommandOutcomeUnknown),
                ..
            })
        ));
        assert!(interrupted_restart.active_session(member_id()).is_none());
        let mut conflict = command.clone();
        conflict.mutation = DelegatedMutation::SetSourcePublication {
            source_id: source_id(3),
            publish_requested: false,
        };
        assert_eq!(
            authority
                .route_member_command(audit_context(), conflict, 2)
                .unwrap_err(),
            RouteError::RequestIdConflict
        );
        assert_eq!(
            authority.expire_member_commands(5_001),
            vec![InterruptedCommand {
                request_id: request_id(40),
                code: ErrorCode::MemberCommandTimeout,
            }]
        );
        let replay = authority
            .route_member_command(audit_context(), command.clone(), 5_002)
            .unwrap();
        let MemberCommandRoute::Cached(replay) = replay else {
            panic!("timed-out request must be cached");
        };
        assert_eq!(replay.code, Some(ErrorCode::MemberCommandTimeout));

        let persisted_operator_state = authority.operator_state().clone();
        let restarted = RegistryAuthority::with_operator_state(
            persisted_room,
            ServerGeneration::from_bytes(10_u128.to_be_bytes()).unwrap(),
            persisted_operator_state,
            "2026-09-18T05:43:00+08:00",
        )
        .unwrap();
        let mut restarted = restarted;
        assert!(matches!(
            restarted
                .route_member_command(audit_context(), command, 0)
                .unwrap(),
            MemberCommandRoute::Cached(OperatorCommandResult {
                code: Some(ErrorCode::MemberCommandTimeout),
                ..
            })
        ));
        assert!(restarted.active_session(member_id()).is_none());
    }

    #[test]
    fn engine_bootstrap_checks_generation_digest_and_normalizes_name() {
        let (room, token) = approved_room();
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let mut old_generation = hello(token.clone(), run_id(20), CapabilityGrants::default());
        old_generation.token_generation = 0;
        assert!(matches!(
            authority.accept_engine_hello(old_generation, 0),
            Err(SessionError::Credential(CredentialError::StaleGeneration))
        ));

        let mut wrong_digest = hello(token.clone(), run_id(20), CapabilityGrants::default());
        wrong_digest.token_sha256 = TokenDigest::of(&[9; 32]);
        assert!(matches!(
            authority.accept_engine_hello(wrong_digest, 0),
            Err(SessionError::Credential(CredentialError::InvalidToken))
        ));

        let mut normalized = hello(token, run_id(20), CapabilityGrants::default());
        normalized.display_name = "Cafe\u{301}".to_owned();
        authority.accept_engine_hello(normalized, 0).unwrap();
        assert_eq!(
            authority.active_session(member_id()).unwrap().display_name,
            "Café"
        );
    }

    #[test]
    fn completed_member_result_is_cached_without_redelivery() {
        let (room, token) = approved_room();
        let room_revision = room.room.room_revision;
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let grants = CapabilityGrants {
            source_publication: true,
            ..CapabilityGrants::default()
        };
        let ready = authority
            .accept_engine_hello(hello(token, run_id(20), grants), 0)
            .unwrap();
        let command = publication_command(50, room_revision);
        assert!(matches!(
            authority
                .route_member_command(audit_context(), command.clone(), 1)
                .unwrap(),
            MemberCommandRoute::Delivered(_)
        ));
        let completed = authority
            .apply_member_command_result(ClientMessage::MemberCommandResult {
                v: rlb_control::Version2,
                request_id: request_id(50),
                registry_session_id: ready.registry_session_id,
                capability: Capability::SourcePublication,
                status: MemberCommandStatus::Applied,
                revisions: CanonicalRevisions {
                    room_revision: Some(room_revision),
                    presence_revision: Some(authority.presence_revision()),
                    catalog_revision: Some(CatalogRevision::new(2)),
                    member_config_revision: Some(MemberConfigRevision::new(2)),
                    grant_revision: Some(GrantRevision::new(1)),
                    subscription_revision: None,
                },
                code: None,
                effective: crate::wire::MemberCommandEffective::default(),
                warnings: Vec::new(),
            })
            .unwrap();
        assert_eq!(completed.status, MemberCommandStatus::Applied);
        let audit = authority.operator_state().audit.records();
        assert_eq!(audit.len(), 1);
        let record = &audit[0];
        assert_eq!(record.principal, "operator-1");
        assert_eq!(record.wall_time, "2026-09-18T05:42:00+08:00");
        assert_eq!(record.room_id, room_id());
        assert_eq!(record.target_member_id, Some(member_id()));
        assert_eq!(record.capability, Some(Capability::SourcePublication));
        assert_eq!(
            record.command,
            rlb_control::RoomCommandName::MemberSourcePublicationSet
        );
        assert_eq!(record.request_id, request_id(50));
        assert_eq!(
            record.canonical_command_hash,
            canonical_command_hash(&command).unwrap()
        );
        assert_eq!(
            record.before_revisions.member_config_revision,
            Some(MemberConfigRevision::new(1))
        );
        assert_eq!(
            record.after_revisions.member_config_revision,
            Some(MemberConfigRevision::new(2))
        );
        assert_eq!(record.outcome, OperatorCommandStatus::Applied);
        assert_eq!(record.error_code, None);

        let encoded = serde_json::to_vec(authority.operator_state()).unwrap();
        let restored: OperatorState = serde_json::from_slice(&encoded).unwrap();
        let restored_authority = RegistryAuthority::with_operator_state(
            authority.room().clone(),
            ServerGeneration::from_bytes(12_u128.to_be_bytes()).unwrap(),
            restored,
            "2026-09-19T05:42:00+08:00",
        )
        .unwrap();
        assert_eq!(restored_authority.operator_state().audit.records(), audit);
        let replay = authority
            .route_member_command(audit_context(), command, 2)
            .unwrap();
        let MemberCommandRoute::Cached(replay) = replay else {
            panic!("completed command must be cached");
        };
        assert_eq!(replay.status, OperatorCommandStatus::Applied);
        assert_eq!(replay.code, None);
        assert_eq!(
            authority
                .active_session(member_id())
                .unwrap()
                .in_flight
                .len(),
            0
        );
    }

    #[test]
    fn authority_room_mutation_appends_audit_and_survives_reload() {
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let room = RoomState::new(room_id(), "room").unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let command = ValidatedRoomCommand::Authority {
            request_id: request_id(60),
            room_id: room_id(),
            expected_room_revision: RoomRevision::new(1),
            mutation: AuthorityMutation::SetName {
                name: "家庭房间".to_owned(),
            },
        };
        let outcome = authority
            .apply_authority_command(audit_context(), &command, 0)
            .unwrap();
        assert_eq!(outcome.result.status, OperatorCommandStatus::Applied);
        assert_eq!(authority.room().room.name, "家庭房间");
        assert_eq!(authority.room().room.room_revision, RoomRevision::new(2));
        let records = authority.operator_state().audit.records().to_vec();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.principal, "operator-1");
        assert_eq!(record.wall_time, "2026-09-18T05:42:00+08:00");
        assert_eq!(record.server_generation, generation);
        assert_eq!(record.room_id, room_id());
        assert_eq!(record.target_member_id, None);
        assert_eq!(record.capability, None);
        assert_eq!(record.command, rlb_control::RoomCommandName::RoomNameSet);
        assert_eq!(record.request_id, request_id(60));
        let ValidatedRoomCommand::Authority {
            request_id: command_request_id,
            room_id: command_room_id,
            expected_room_revision,
            mutation,
        } = &command
        else {
            panic!("authority command");
        };
        assert_eq!(
            record.canonical_command_hash,
            canonical_command_hash(&AuthorityCommandHashBody {
                request_id: *command_request_id,
                room_id: *command_room_id,
                expected_room_revision: *expected_room_revision,
                mutation,
            })
            .unwrap()
        );
        assert_eq!(record.outcome, OperatorCommandStatus::Applied);
        assert_eq!(
            record.before_revisions.room_revision,
            Some(RoomRevision::new(1))
        );
        assert_eq!(
            record.after_revisions.room_revision,
            Some(RoomRevision::new(2))
        );

        let replay = authority
            .apply_authority_command(audit_context(), &command, 1)
            .unwrap();
        assert_eq!(replay.result, outcome.result);
        assert_eq!(authority.operator_state().audit.records().len(), 1);

        let conflict = ValidatedRoomCommand::Authority {
            request_id: request_id(60),
            room_id: room_id(),
            expected_room_revision: RoomRevision::new(1),
            mutation: AuthorityMutation::SetName {
                name: "other".to_owned(),
            },
        };
        assert!(matches!(
            authority.apply_authority_command(audit_context(), &conflict, 2),
            Err(AuthorityCommandError::RequestIdConflict)
        ));
        assert_eq!(authority.operator_state().audit.records().len(), 1);

        let room_json = authority.room().to_json().unwrap();
        let operator_json = serde_json::to_vec(authority.operator_state()).unwrap();
        let restored_room = RoomState::from_json(&room_json).unwrap();
        let restored_operator: OperatorState = serde_json::from_slice(&operator_json).unwrap();
        let restored = RegistryAuthority::with_operator_state(
            restored_room,
            ServerGeneration::from_bytes(10_u128.to_be_bytes()).unwrap(),
            restored_operator,
            "2026-09-19T05:42:00+08:00",
        )
        .unwrap();
        assert_eq!(restored.room().room.name, "家庭房间");
        assert_eq!(restored.room().room.room_revision, RoomRevision::new(2));
        assert_eq!(restored.operator_state().audit.records(), records);
    }

    #[test]
    fn bound_replacement_supersedes_one_active_run_and_third_is_rejected() {
        let (room, token) = approved_room();
        let generation = ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap();
        let mut authority = RegistryAuthority::new(room, generation);
        let first = authority
            .accept_engine_hello(
                hello(token.clone(), run_id(20), CapabilityGrants::default()),
                0,
            )
            .unwrap();
        let first_endpoint = Endpoint::new(Ipv4Addr::new(192, 0, 2, 10), 17_001).unwrap();
        authority
            .bind_media(
                BindMedia {
                    server_generation: generation,
                    registry_session_id: first.registry_session_id,
                    bind_nonce: first.bind_nonce,
                },
                first_endpoint,
                1,
            )
            .unwrap();
        let replacement = authority
            .accept_engine_hello(
                hello(token.clone(), run_id(21), CapabilityGrants::default()),
                2,
            )
            .unwrap();
        assert!(matches!(
            authority.accept_engine_hello(hello(token, run_id(22), CapabilityGrants::default()), 3),
            Err(SessionError::DuplicateIdentity)
        ));
        assert_eq!(
            authority.active_session(member_id()).unwrap().run_id,
            run_id(20)
        );
        let outcome = authority
            .bind_media(
                BindMedia {
                    server_generation: generation,
                    registry_session_id: replacement.registry_session_id,
                    bind_nonce: replacement.bind_nonce,
                },
                Endpoint::new(Ipv4Addr::new(192, 0, 2, 11), 17_002).unwrap(),
                4,
            )
            .unwrap();
        assert_eq!(outcome.run_id, run_id(21));
        assert_eq!(outcome.superseded.unwrap().run_id, run_id(20));
        assert_eq!(
            authority.active_session(member_id()).unwrap().run_id,
            run_id(21)
        );
    }
}
