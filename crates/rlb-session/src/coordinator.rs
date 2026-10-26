use std::collections::{BTreeMap, BTreeSet};

use rlb_audio::{AudioMetricsSnapshot, SourceRenderConfig};
use rlb_control::{
    CanonicalRevisions, CommandStatus, DelegatedEffective, Field, RemoteMemberMix, RemoteSourceMix,
    RequestBoundary, RequestCache, RequestCacheAction, RequestCacheEffect, RequestCacheKey,
    RequestScope, WarningCode, canonical_command_hash, reduce_delegated_command,
    reduce_request_cache,
};
use rlb_media::{
    AckStatus as MediaAckStatus, BoundedRejections, BoundedSourceIds, DirectCommon, DirectDatagram,
    EpochEndReason, Rejection, RejectionCode, SubscribeAck, SubscribeSet,
};
use rlb_model::{
    AdmissionState, CatalogRevision, ErrorCode, IdentityRecord, MemberId, MemberMix,
    QualityProfile, RegistrySessionId, RoomRevision, RunId, ServerGeneration, SourceDescriptor,
    SourceFormat, SourceId, SourceKind, SourceState, StreamEpoch, SubscriptionRevision,
};
use rlb_registry::{
    ClientMessage, ControlPresence, MediaPresence, MemberCommandEffective, MemberCommandStatus,
    PublicationBlock, RoomEvent, RoomEventValue, RoomView, SnapshotMember, SnapshotReason,
};

use crate::config::{
    MEMBER_AUTHORITY_PRINCIPAL, MemberCommandResultRecord, MemberConfig, MemberConfigError,
};
use crate::types::{
    AudioMixPlan, AuthoritySnapshot, CaptureState, ConfigTransactionId, CoordinatorEffect,
    CoordinatorEvent, DiagnosticsSnapshot, EffectiveSendBlock, EngineState, EpochChangeReason,
    LocalConfigMutation, LocalConfigOutcome, LocalConfigRequest, LocalSourceDiagnostics,
    MediaReceivePlan, MediaSourceProjection, MemberCommandDelivery, PeerConnectPlan,
    PeerConnectionState, PeerDiagnostics, PersistenceResult, RecipientEligibility,
    RegistryProjectionUpdate, RegistryState, RemoteMediaMetrics, RemoteMediaState,
    RemoteSourceDiagnostics, SendPlan, SendRecipient, ShutdownStage, SourceEpochEndPlan,
    SourceEpochStartPlan, SourceRestartPlan, SourceSendPlan, SourceStopReason, SourceStreamStarted,
};

const RECONNECT_DELAYS_MS: [u32; 6] = [1_000, 2_000, 4_000, 8_000, 15_000, 30_000];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveSourceStream {
    stream_epoch: StreamEpoch,
    endpoint_id: String,
    capture_method: Option<rlb_model::CaptureMethod>,
    format: SourceFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalSourceRuntime {
    source_id: SourceId,
    kind: SourceKind,
    capture_state: CaptureState,
    available: bool,
    active: Option<ActiveSourceStream>,
    last_stream: Option<ActiveSourceStream>,
    required_epoch_reason: Option<EpochChangeReason>,
    epoch_changes_total: u64,
    sender_metrics: rlb_media::SenderMetrics,
}

#[derive(Clone, Debug)]
struct AuthorityState {
    server_generation: ServerGeneration,
    room_id: rlb_model::RoomId,
    room_revision: RoomRevision,
    presence_revision: rlb_model::PresenceRevision,
    event_cursor: u64,
    room: RoomView,
    self_admission: AdmissionState,
    members: BTreeMap<MemberId, SnapshotMember>,
    local_visibility: BTreeMap<SourceId, BTreeSet<MemberId>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemoteHealth {
    state: RemoteMediaState,
    metrics: RemoteMediaMetrics,
}

impl Default for RemoteHealth {
    fn default() -> Self {
        Self {
            state: RemoteMediaState::Waiting,
            metrics: RemoteMediaMetrics::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InboundSubscription {
    request: SubscribeSet,
    ack: SubscribeAck,
}

#[derive(Clone, Debug)]
struct PeerState {
    member: SnapshotMember,
    connection: PeerConnectionState,
    inbound_subscription: Option<InboundSubscription>,
    inbound_invalidated: BTreeSet<SourceId>,
    outbound_pending: Option<SubscribeSet>,
    outbound_accepted: BoundedSourceIds,
    next_outbound_revision: SubscriptionRevision,
    health: BTreeMap<SourceId, RemoteHealth>,
}

impl PeerState {
    fn new(member: SnapshotMember) -> Self {
        Self {
            member,
            connection: PeerConnectionState::Disconnected,
            inbound_subscription: None,
            inbound_invalidated: BTreeSet::new(),
            outbound_pending: None,
            outbound_accepted: BoundedSourceIds::empty(),
            next_outbound_revision: SubscriptionRevision::new(0),
            health: BTreeMap::new(),
        }
    }

    fn direct_session_id(&self) -> Option<rlb_model::DirectSessionId> {
        match self.connection {
            PeerConnectionState::Ready { direct_session_id } => Some(direct_session_id),
            PeerConnectionState::Disconnected | PeerConnectionState::Connecting => None,
        }
    }

    fn transport(&self) -> Option<(RunId, rlb_model::Endpoint)> {
        Some((self.member.run_id?, self.member.media_endpoint?))
    }

    fn authority_online(&self) -> bool {
        self.member.admission == AdmissionState::Approved
            && self.member.control_presence == ControlPresence::Online
            && self.member.media_presence == MediaPresence::Bound
            && self.transport().is_some()
    }

    fn remote_subscription_accepted(&self, source_id: SourceId) -> bool {
        let Some(pending) = self.outbound_pending else {
            return false;
        };
        self.member.catalog_revision == Some(pending.catalog_revision)
            && self.outbound_accepted.contains(source_id)
    }
}

#[derive(Clone, Debug)]
enum CommitOrigin {
    Local { request_id: rlb_model::RequestId },
    Delegated(Box<DelegatedCommit>),
}

#[derive(Clone, Debug)]
struct DelegatedCommit {
    delivery: MemberCommandDelivery,
    result: MemberCommandResultRecord,
}

#[derive(Clone, Debug)]
struct PendingCommit {
    transaction_id: ConfigTransactionId,
    candidate: MemberConfig,
    target_catalog_revision: CatalogRevision,
    publish_catalog_after_commit: bool,
    origin: CommitOrigin,
}

#[derive(Clone, Copy, Debug, Default)]
struct DiagnosticCounters {
    registry_outages: u64,
    registry_reconnects: u64,
    rejected_events: u64,
    persistence_failures: u64,
    commands_dropped_without_session: u64,
    subscription_rejections: u64,
}

/// Pure, serial member-engine coordinator.
///
/// The type owns no sockets, files, device handles, or threads. Callers feed
/// typed events from their I/O owners and execute the returned effects in
/// order. Configuration changes only become canonical after a matching
/// persistence-completion event.
#[derive(Clone, Debug)]
pub struct MemberSessionCoordinator {
    identity: IdentityRecord,
    run_id: RunId,
    config: MemberConfig,
    catalog_revision: CatalogRevision,
    engine_state: EngineState,
    registry_state: RegistryState,
    registry_session_id: Option<RegistrySessionId>,
    reconnect_attempt: usize,
    authority: Option<AuthorityState>,
    sources: BTreeMap<SourceId, LocalSourceRuntime>,
    peers: BTreeMap<MemberId, PeerState>,
    pending_commit: Option<PendingCommit>,
    next_transaction_id: u64,
    volatile_emergency_stop: bool,
    shutdown_stage: Option<ShutdownStage>,
    audio_metrics: AudioMetricsSnapshot,
    audio_mix_generation: u64,
    last_audio_sources: Vec<SourceRenderConfig>,
    last_send_plan: Option<SendPlan>,
    diagnostics: DiagnosticCounters,
}

impl MemberSessionCoordinator {
    /// Restores a validated member configuration for one fresh engine run.
    pub fn new(
        identity: IdentityRecord,
        run_id: RunId,
        config: MemberConfig,
        catalog_revision: CatalogRevision,
    ) -> Result<Self, CoordinatorError> {
        config
            .validate(identity)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        if catalog_revision.get() == 0 {
            return Err(CoordinatorError::new(ErrorCode::InvalidCatalog));
        }
        let sources = config
            .sources
            .iter()
            .map(|source| {
                let capture_state = if source.config.master_capture_enabled {
                    CaptureState::Starting
                } else {
                    CaptureState::Disabled
                };
                (
                    source.source_id,
                    LocalSourceRuntime {
                        source_id: source.source_id,
                        kind: source.kind,
                        capture_state,
                        available: false,
                        active: None,
                        last_stream: None,
                        required_epoch_reason: source
                            .config
                            .master_capture_enabled
                            .then_some(EpochChangeReason::EngineRun),
                        epoch_changes_total: 0,
                        sender_metrics: rlb_media::SenderMetrics::default(),
                    },
                )
            })
            .collect();
        Ok(Self {
            identity,
            run_id,
            volatile_emergency_stop: config.local_emergency_stop,
            config,
            catalog_revision,
            engine_state: EngineState::Starting,
            registry_state: RegistryState::Disconnected,
            registry_session_id: None,
            reconnect_attempt: 0,
            authority: None,
            sources,
            peers: BTreeMap::new(),
            pending_commit: None,
            next_transaction_id: 1,
            shutdown_stage: None,
            audio_metrics: AudioMetricsSnapshot::default(),
            audio_mix_generation: 0,
            last_audio_sources: Vec::new(),
            last_send_plan: None,
            diagnostics: DiagnosticCounters::default(),
        })
    }

    /// Returns the last durably committed member configuration.
    #[must_use]
    pub const fn config(&self) -> &MemberConfig {
        &self.config
    }

    /// Returns the separate stable identity record used by engine bootstrap.
    #[must_use]
    pub const fn identity(&self) -> IdentityRecord {
        self.identity
    }

    /// Returns the current process-run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Builds the complete current source catalog for `ENGINE_HELLO` or an
    /// explicit registry catalog replacement.
    pub fn source_catalog(&self) -> Result<Vec<SourceDescriptor>, CoordinatorError> {
        self.catalog()
    }

    /// Returns the current run-scoped complete catalog revision.
    #[must_use]
    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }

    /// Projects visible active sources together with matching direct ACK state.
    #[must_use]
    pub fn media_receive_plan(&self) -> MediaReceivePlan {
        let room_id = self.config.attached_room_id();
        let Some(authority) = self
            .authority
            .as_ref()
            .filter(|authority| Some(authority.room_id) == room_id)
        else {
            return MediaReceivePlan {
                room_id,
                sources: Vec::new(),
            };
        };
        if authority.self_admission != AdmissionState::Approved {
            return MediaReceivePlan {
                room_id,
                sources: Vec::new(),
            };
        }

        let mut sources = Vec::new();
        for peer in self.peers.values() {
            let Some((run_id, media_endpoint)) = peer.transport() else {
                continue;
            };
            let Some(catalog_revision) = peer.member.catalog_revision else {
                continue;
            };
            for descriptor in &peer.member.sources {
                let Some((format, active_epoch)) = descriptor.active() else {
                    continue;
                };
                let subscription_accepted =
                    peer.remote_subscription_accepted(descriptor.source_id());
                let subscription_revision = peer
                    .outbound_pending
                    .map(|pending| pending.subscription_revision);
                sources.push(MediaSourceProjection {
                    member_id: peer.member.member_id,
                    run_id,
                    source_id: descriptor.source_id(),
                    active_epoch,
                    catalog_revision,
                    media_endpoint,
                    format,
                    subscription_revision,
                    subscription_accepted,
                });
            }
        }
        sources.sort_by_key(|source| (source.member_id, source.source_id));
        MediaReceivePlan { room_id, sources }
    }

    /// Returns the member engine lifecycle.
    #[must_use]
    pub const fn engine_state(&self) -> EngineState {
        self.engine_state
    }

    /// Returns the current registry-control lifecycle.
    #[must_use]
    pub const fn registry_state(&self) -> RegistryState {
        self.registry_state
    }

    /// Authority admission for this member, if a room snapshot is installed.
    #[must_use]
    pub fn local_admission(&self) -> Option<AdmissionState> {
        self.authority
            .as_ref()
            .map(|authority| authority.self_admission)
    }

    /// Authority policy for the attached room, if a snapshot is installed.
    #[must_use]
    pub fn room_policy(&self) -> Option<rlb_model::RoomPolicy> {
        self.authority
            .as_ref()
            .map(|authority| rlb_model::RoomPolicy {
                quality_cap: authority.room.quality_cap,
                qos_policy: authority.room.qos_policy,
                room_send_muted: authority.room.room_send_muted,
            })
    }

    /// Authority room revision, if a snapshot is installed.
    #[must_use]
    pub fn room_revision(&self) -> Option<RoomRevision> {
        self.authority
            .as_ref()
            .map(|authority| authority.room_revision)
    }

    /// Authority presence revision, if a snapshot is installed.
    #[must_use]
    pub fn presence_revision(&self) -> Option<rlb_model::PresenceRevision> {
        self.authority
            .as_ref()
            .map(|authority| authority.presence_revision)
    }

    /// Inbound subscription revision when exactly one peer has one.
    #[must_use]
    pub fn snapshot_subscription_revision(&self) -> Option<SubscriptionRevision> {
        let mut found = None;
        for peer in self.peers.values() {
            if let Some(revision) = peer
                .inbound_subscription
                .map(|subscription| subscription.request.subscription_revision)
            {
                if found.is_some() {
                    return None;
                }
                found = Some(revision);
            }
        }
        found
    }

    /// Reports whether one atomic configuration write is outstanding.
    #[must_use]
    pub const fn has_pending_persistence(&self) -> bool {
        self.pending_commit.is_some()
    }

    /// Applies one event atomically and returns ordered external work.
    pub fn handle(
        &mut self,
        event: CoordinatorEvent,
    ) -> Result<Vec<CoordinatorEffect>, CoordinatorError> {
        let mut candidate = self.clone();
        match candidate.apply(event) {
            Ok(effects) => {
                *self = candidate;
                Ok(effects)
            }
            Err(error) => {
                self.diagnostics.rejected_events =
                    self.diagnostics.rejected_events.saturating_add(1);
                Err(error)
            }
        }
    }

    /// Computes all recipient decisions, including closed and transport-only
    /// gates, from the current immutable state.
    #[must_use]
    pub fn effective_send_eligibility(&self) -> Vec<RecipientEligibility> {
        let mut output = Vec::new();
        for source in &self.config.sources {
            for member_id in self.peers.keys().copied() {
                output.push(self.eligibility(source.source_id, member_id));
            }
        }
        output.sort_by_key(|entry| (entry.source_id, entry.recipient_member_id));
        output
    }

    /// Builds a bounded low-rate diagnostics snapshot.
    #[must_use]
    pub fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        let mut peers = Vec::with_capacity(self.peers.len());
        for peer in self.peers.values() {
            let mut sources = Vec::with_capacity(peer.member.sources.len());
            for descriptor in &peer.member.sources {
                let health = peer
                    .health
                    .get(&descriptor.source_id())
                    .copied()
                    .unwrap_or_default();
                sources.push(RemoteSourceDiagnostics {
                    descriptor: descriptor.clone(),
                    subscription_accepted: peer
                        .remote_subscription_accepted(descriptor.source_id()),
                    media_state: health.state,
                    metrics: health.metrics,
                });
            }
            sources.sort_by_key(|source| source.descriptor.source_id());
            peers.push(PeerDiagnostics {
                member_id: peer.member.member_id,
                run_id: peer.member.run_id,
                endpoint: peer.member.media_endpoint,
                connection: peer.connection,
                inbound_subscription_revision: peer
                    .inbound_subscription
                    .map(|subscription| subscription.request.subscription_revision),
                sources,
            });
        }
        peers.sort_by_key(|peer| peer.member_id);

        let eligibility = self.effective_send_eligibility();
        let mut sources = Vec::with_capacity(self.config.sources.len());
        for source in &self.config.sources {
            let runtime = self
                .sources
                .get(&source.source_id)
                .expect("validated fixed source runtime");
            let effective_recipients = eligibility
                .iter()
                .filter(|entry| entry.source_id == source.source_id && entry.media_emitter_eligible)
                .map(|entry| entry.recipient_member_id)
                .collect();
            sources.push(LocalSourceDiagnostics {
                source_id: source.source_id,
                capture_state: runtime.capture_state,
                available: runtime.available,
                master_capture_enabled: source.config.master_capture_enabled,
                publish_requested: source.config.publish_requested,
                selected_profile: source.config.selected_profile,
                effective_profile: self.effective_profile(source.source_id),
                stream_epoch: runtime.active.as_ref().map(|stream| stream.stream_epoch),
                epoch_changes_total: runtime.epoch_changes_total,
                effective_recipients,
                sender_metrics: runtime.sender_metrics,
            });
        }
        sources.sort_by_key(|source| source.source_id);

        DiagnosticsSnapshot {
            engine_state: self.engine_state,
            registry_state: self.registry_state,
            server_generation: self
                .authority
                .as_ref()
                .map(|authority| authority.server_generation),
            room_revision: self
                .authority
                .as_ref()
                .map(|authority| authority.room_revision),
            presence_revision: self
                .authority
                .as_ref()
                .map(|authority| authority.presence_revision),
            member_config_revision: self.config.member_config_revision,
            catalog_revision: self.catalog_revision,
            grant_revision: self.config.room.grant_revision,
            local_emergency_stop: self.local_emergency_stop(),
            room_send_muted: self.room_send_muted(),
            registry_outages_total: self.diagnostics.registry_outages,
            registry_reconnects_total: self.diagnostics.registry_reconnects,
            rejected_events_total: self.diagnostics.rejected_events,
            persistence_failures_total: self.diagnostics.persistence_failures,
            commands_dropped_without_session_total: self
                .diagnostics
                .commands_dropped_without_session,
            subscription_rejections_total: self.diagnostics.subscription_rejections,
            peers,
            sources,
            audio: self.audio_metrics,
        }
    }

    fn apply(
        &mut self,
        event: CoordinatorEvent,
    ) -> Result<Vec<CoordinatorEffect>, CoordinatorError> {
        let mut effects = Vec::new();
        match event {
            CoordinatorEvent::EngineStarted => self.handle_engine_started(&mut effects)?,
            CoordinatorEvent::RegistryConnectRequested => {
                self.ensure_running()?;
                if !matches!(
                    self.registry_state,
                    RegistryState::Disconnected | RegistryState::Outage | RegistryState::Left
                ) {
                    return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
                }
                self.registry_state = RegistryState::Connecting;
                effects.push(CoordinatorEffect::ConnectRegistry);
            }
            CoordinatorEvent::RegistrySessionReady {
                registry_session_id,
            } => {
                self.ensure_running()?;
                if self.registry_state != RegistryState::Connecting {
                    return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
                }
                self.registry_session_id = Some(registry_session_id);
                self.registry_state = RegistryState::AwaitingSnapshot;
                effects.push(CoordinatorEffect::RequestFullRoomSnapshot {
                    reason: SnapshotReason::Initial,
                });
            }
            CoordinatorEvent::AuthoritySnapshotReceived(snapshot) => {
                self.handle_authority_snapshot(snapshot, &mut effects)?;
            }
            CoordinatorEvent::AuthorityEventReceived(event) => {
                self.handle_authority_event(event, &mut effects)?;
            }
            CoordinatorEvent::RegistryDisconnected => {
                self.handle_registry_disconnected(&mut effects)?;
            }
            CoordinatorEvent::ReconnectTimerElapsed => {
                self.ensure_running()?;
                if self.registry_state != RegistryState::Outage {
                    return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
                }
                self.registry_state = RegistryState::Connecting;
                effects.push(CoordinatorEffect::ConnectRegistry);
            }
            CoordinatorEvent::DirectSessionReady {
                member_id,
                run_id,
                direct_session_id,
            } => {
                self.handle_direct_ready(member_id, run_id, direct_session_id, &mut effects)?;
            }
            CoordinatorEvent::DirectSessionLost {
                member_id,
                direct_session_id,
            } => {
                self.handle_direct_lost(member_id, direct_session_id, &mut effects)?;
            }
            CoordinatorEvent::SubscribeSetReceived(request) => {
                self.handle_subscribe_set(request, &mut effects)?;
            }
            CoordinatorEvent::SubscribeAckReceived(ack) => {
                self.handle_subscribe_ack(ack, &mut effects)?;
            }
            CoordinatorEvent::SourceAvailabilityChanged {
                source_id,
                available,
            } => {
                self.handle_source_availability(source_id, available, &mut effects)?;
            }
            CoordinatorEvent::SourceStreamStarted(stream) => {
                self.handle_source_started(stream, &mut effects)?;
            }
            CoordinatorEvent::SourceStreamStopped { source_id, reason } => {
                self.handle_source_stopped(source_id, reason, &mut effects)?;
            }
            CoordinatorEvent::LocalConfigRequested(request) => {
                self.handle_local_config(request, &mut effects)?;
            }
            CoordinatorEvent::MemberCommandReceived { delivery, now_ms } => {
                self.handle_member_command(delivery, now_ms, &mut effects)?;
            }
            CoordinatorEvent::ConfigPersistenceCompleted {
                transaction_id,
                result,
            } => {
                self.handle_persistence_completed(transaction_id, result, &mut effects)?;
            }
            CoordinatorEvent::RemoteMediaHealth {
                member_id,
                source_id,
                state,
                metrics,
            } => {
                let peer = self
                    .peers
                    .get_mut(&member_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
                if !peer
                    .member
                    .sources
                    .iter()
                    .any(|source| source.source_id() == source_id)
                {
                    return Err(CoordinatorError::new(ErrorCode::SourceNotFound));
                }
                peer.health
                    .insert(source_id, RemoteHealth { state, metrics });
            }
            CoordinatorEvent::LocalSenderMetrics { source_id, metrics } => {
                self.sources
                    .get_mut(&source_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?
                    .sender_metrics = metrics;
            }
            CoordinatorEvent::AudioMetricsUpdated(metrics) => {
                self.audio_metrics = metrics;
            }
            CoordinatorEvent::ShutdownRequested => {
                self.handle_shutdown_requested(&mut effects)?;
            }
            CoordinatorEvent::ShutdownStageCompleted(stage) => {
                self.handle_shutdown_stage_completed(stage, &mut effects)?;
            }
        }
        Ok(effects)
    }

    fn handle_engine_started(
        &mut self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.engine_state != EngineState::Starting {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        }
        self.engine_state = EngineState::Running;
        effects.push(CoordinatorEffect::SetEmergencySendGate {
            stopped: self.local_emergency_stop(),
        });
        for source in &self.config.sources {
            if source.config.master_capture_enabled {
                effects.push(CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
                    source_id: source.source_id,
                    reason: EpochChangeReason::EngineRun,
                }));
            }
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn handle_registry_disconnected(
        &mut self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        if !matches!(
            self.registry_state,
            RegistryState::Connecting | RegistryState::AwaitingSnapshot | RegistryState::Online
        ) {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        }
        self.registry_state = RegistryState::Outage;
        self.registry_session_id = None;
        self.diagnostics.registry_outages = self.diagnostics.registry_outages.saturating_add(1);
        if self.config.room.settings.auto_reconnect {
            let index = self
                .reconnect_attempt
                .min(RECONNECT_DELAYS_MS.len().saturating_sub(1));
            effects.push(CoordinatorEffect::ScheduleRegistryReconnect {
                base_delay_ms: RECONNECT_DELAYS_MS[index],
            });
            self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        }
        // Authority, direct sessions, subscriptions, send plans, and mix plans
        // deliberately remain unchanged during an authority outage.
        Ok(())
    }

    fn handle_authority_snapshot(
        &mut self,
        snapshot: AuthoritySnapshot,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        if !matches!(
            self.registry_state,
            RegistryState::AwaitingSnapshot | RegistryState::Online
        ) || self.registry_session_id.is_none()
        {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        }
        snapshot
            .validate(self.identity)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidCatalog))?;
        if self.config.room.room_id != Some(snapshot.room.room_id) {
            return Err(CoordinatorError::new(ErrorCode::RoomScopeMismatch));
        }
        if let Some(current) = &self.authority {
            if snapshot.room.room_revision < current.room_revision {
                return Err(CoordinatorError::new(ErrorCode::StaleRoomRevision));
            }
            if snapshot.room.server_generation == current.server_generation
                && snapshot.room.presence_revision < current.presence_revision
            {
                return Err(CoordinatorError::new(ErrorCode::StaleGeneration));
            }
        }

        let old_catalog = self.catalog()?;
        let old_visibility = self
            .authority
            .as_ref()
            .map(|authority| authority.local_visibility.clone());
        let reconnect = self.authority.is_some();
        let mut members = BTreeMap::new();
        for member in snapshot.room.members {
            members.insert(member.member_id, member);
        }
        let local_visibility = snapshot
            .local_source_visibility
            .into_iter()
            .map(|entry| {
                (
                    entry.source_id,
                    entry.viewer_member_ids.into_iter().collect(),
                )
            })
            .collect();
        self.authority = Some(AuthorityState {
            server_generation: snapshot.room.server_generation,
            room_id: snapshot.room.room_id,
            room_revision: snapshot.room.room_revision,
            presence_revision: snapshot.room.presence_revision,
            event_cursor: snapshot.room.event_cursor.get(),
            room: snapshot.room.room,
            self_admission: snapshot.room.self_view.admission,
            members,
            local_visibility,
        });
        if let Some(old_visibility) = old_visibility {
            let new_visibility = &self
                .authority
                .as_ref()
                .expect("authority was installed")
                .local_visibility;
            for peer in self.peers.values_mut() {
                for source in &self.config.sources {
                    let was_visible = old_visibility
                        .get(&source.source_id)
                        .is_some_and(|viewers| viewers.contains(&peer.member.member_id));
                    let is_visible = new_visibility
                        .get(&source.source_id)
                        .is_some_and(|viewers| viewers.contains(&peer.member.member_id));
                    if was_visible != is_visible {
                        peer.inbound_invalidated.insert(source.source_id);
                    }
                }
            }
        }
        self.registry_state = RegistryState::Online;
        self.reconnect_attempt = 0;
        if reconnect {
            self.diagnostics.registry_reconnects =
                self.diagnostics.registry_reconnects.saturating_add(1);
        }

        self.reconcile_source_policy(effects)?;
        self.reconcile_all_peers(effects)?;
        let new_catalog = self.catalog()?;
        if new_catalog != old_catalog {
            self.bump_catalog()?;
        }
        self.publish_all_registry_projections(effects)?;
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn handle_authority_event(
        &mut self,
        event: RoomEvent,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        if self.registry_state != RegistryState::Online {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        }
        let Some(authority) = self.authority.as_ref() else {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        };
        let expected_seq = authority.event_cursor.saturating_add(1);
        let room_revision_gap =
            event.room_revision.get() > authority.room_revision.get().saturating_add(1);
        let presence_revision_gap =
            event.presence_revision.get() > authority.presence_revision.get().saturating_add(1);
        if self.config.room.room_id != Some(event.room_id)
            || event.server_generation != authority.server_generation
            || event.event_seq.get() != expected_seq
            || event.room_revision < authority.room_revision
            || event.presence_revision < authority.presence_revision
            || room_revision_gap
            || presence_revision_gap
        {
            self.registry_state = RegistryState::AwaitingSnapshot;
            effects.push(CoordinatorEffect::RequestFullRoomSnapshot {
                reason: SnapshotReason::RevisionGap,
            });
            return Ok(());
        }

        let old_catalog = self.catalog()?;
        {
            let authority = self
                .authority
                .as_mut()
                .expect("authority was checked above");
            authority.event_cursor = event.event_seq.get();
            authority.room_revision = event.room_revision;
            authority.presence_revision = event.presence_revision;
        }

        match event.event {
            RoomEventValue::RoomNameChanged { name } => {
                self.authority_mut()?.room.name = name;
            }
            RoomEventValue::QualityCapChanged { quality_cap } => {
                self.authority_mut()?.room.quality_cap = quality_cap;
                self.reconcile_source_policy(effects)?;
            }
            RoomEventValue::QosPolicyChanged { qos_policy } => {
                self.authority_mut()?.room.qos_policy = qos_policy;
            }
            RoomEventValue::RoomSendMutedChanged { room_send_muted } => {
                self.authority_mut()?.room.room_send_muted = room_send_muted;
            }
            RoomEventValue::SourceVisibilityChanged {
                publisher_member_id,
                source_id,
                viewer_member_ids,
            } => {
                self.apply_visibility_event(
                    publisher_member_id,
                    source_id,
                    viewer_member_ids,
                    effects,
                )?;
            }
            RoomEventValue::CatalogReplaced {
                member_id,
                catalog_revision,
                sources,
            } => {
                if member_id != self.identity.member_id() {
                    let authority = self.authority_mut()?;
                    let member = authority
                        .members
                        .get_mut(&member_id)
                        .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
                    member.catalog_revision = Some(catalog_revision);
                    member.sources = sources;
                    self.reconcile_peer(member_id, effects)?;
                }
            }
            RoomEventValue::MemberControlDown { member_id } => {
                if member_id != self.identity.member_id() {
                    let authority = self.authority_mut()?;
                    let member = authority
                        .members
                        .get_mut(&member_id)
                        .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
                    member.control_presence = ControlPresence::Offline;
                    member.media_presence = MediaPresence::Unbound;
                    member.media_endpoint = None;
                    self.reconcile_peer(member_id, effects)?;
                }
            }
            RoomEventValue::MemberControlUp { member_id, run_id } => {
                if member_id != self.identity.member_id() {
                    let authority = self.authority_mut()?;
                    let Some(member) = authority.members.get_mut(&member_id) else {
                        self.request_snapshot(SnapshotReason::RevisionGap, effects);
                        return Ok(());
                    };
                    member.control_presence = ControlPresence::Online;
                    member.run_id = Some(run_id);
                    self.reconcile_peer(member_id, effects)?;
                }
            }
            RoomEventValue::MemberMediaBound {
                member_id,
                endpoint,
            }
            | RoomEventValue::MediaEndpointChanged {
                member_id,
                endpoint,
            } => {
                if member_id != self.identity.member_id() {
                    let authority = self.authority_mut()?;
                    let Some(member) = authority.members.get_mut(&member_id) else {
                        self.request_snapshot(SnapshotReason::RevisionGap, effects);
                        return Ok(());
                    };
                    member.media_presence = MediaPresence::Bound;
                    member.media_endpoint = Some(endpoint);
                    self.reconcile_peer(member_id, effects)?;
                }
            }
            RoomEventValue::MemberMediaUnbound { member_id } => {
                if member_id != self.identity.member_id() {
                    let authority = self.authority_mut()?;
                    let member = authority
                        .members
                        .get_mut(&member_id)
                        .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
                    member.media_presence = MediaPresence::Unbound;
                    member.media_endpoint = None;
                    self.reconcile_peer(member_id, effects)?;
                }
            }
            RoomEventValue::MemberRunReplaced {
                member_id,
                new_run_id,
                ..
            } => {
                if member_id != self.identity.member_id() {
                    let authority = self.authority_mut()?;
                    let member = authority
                        .members
                        .get_mut(&member_id)
                        .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
                    member.run_id = Some(new_run_id);
                    self.reconcile_peer(member_id, effects)?;
                }
            }
            RoomEventValue::AdmissionChanged {
                member_id,
                admission,
            } => {
                self.apply_admission_event(member_id, admission, effects)?;
            }
            RoomEventValue::CapabilityGrantsChanged { .. }
            | RoomEventValue::MemberProjectionChanged { .. } => {}
            RoomEventValue::ResyncRequired => {
                self.request_snapshot(SnapshotReason::RevisionGap, effects);
                return Ok(());
            }
        }

        let new_catalog = self.catalog()?;
        if new_catalog != old_catalog {
            self.bump_catalog()?;
            self.publish_catalog(effects)?;
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn apply_visibility_event(
        &mut self,
        publisher_member_id: MemberId,
        source_id: SourceId,
        viewer_member_ids: Vec<MemberId>,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if publisher_member_id == self.identity.member_id() {
            if self.config.source(source_id).is_none() {
                return Err(CoordinatorError::new(ErrorCode::SourceNotFound));
            }
            let viewers: BTreeSet<_> = viewer_member_ids.into_iter().collect();
            let old_viewers = self
                .authority_ref()?
                .local_visibility
                .get(&source_id)
                .cloned()
                .unwrap_or_default();
            self.authority_mut()?
                .local_visibility
                .insert(source_id, viewers);
            for peer in self.peers.values_mut() {
                let member_id = peer.member.member_id;
                let was_visible = old_viewers.contains(&member_id);
                let is_visible = self
                    .authority
                    .as_ref()
                    .and_then(|authority| authority.local_visibility.get(&source_id))
                    .is_some_and(|viewers| viewers.contains(&member_id));
                if was_visible != is_visible {
                    peer.inbound_invalidated.insert(source_id);
                }
            }
            return Ok(());
        }

        let visible = viewer_member_ids.contains(&self.identity.member_id());
        let authority = self.authority_mut()?;
        let Some(member) = authority.members.get_mut(&publisher_member_id) else {
            self.request_snapshot(SnapshotReason::RevisionGap, effects);
            return Ok(());
        };
        if visible {
            if !member
                .sources
                .iter()
                .any(|source| source.source_id() == source_id)
            {
                self.request_snapshot(SnapshotReason::RevisionGap, effects);
                return Ok(());
            }
        } else {
            member
                .sources
                .retain(|source| source.source_id() != source_id);
            self.reconcile_peer(publisher_member_id, effects)?;
        }
        Ok(())
    }

    fn apply_admission_event(
        &mut self,
        member_id: MemberId,
        admission: AdmissionState,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if member_id == self.identity.member_id() {
            self.authority_mut()?.self_admission = admission;
            if admission != AdmissionState::Approved {
                for peer in self.peers.values() {
                    if peer.connection != PeerConnectionState::Disconnected {
                        effects.push(CoordinatorEffect::DisconnectPeer {
                            member_id: peer.member.member_id,
                        });
                    }
                }
                self.peers.clear();
            }
            return Ok(());
        }
        if admission != AdmissionState::Approved {
            self.authority_mut()?.members.remove(&member_id);
            if let Some(peer) = self.peers.remove(&member_id)
                && peer.connection != PeerConnectionState::Disconnected
            {
                effects.push(CoordinatorEffect::DisconnectPeer { member_id });
            }
        } else if !self.authority_ref()?.members.contains_key(&member_id) {
            self.request_snapshot(SnapshotReason::RevisionGap, effects);
        }
        Ok(())
    }

    fn reconcile_all_peers(
        &mut self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let members: BTreeSet<_> = self
            .authority_ref()?
            .members
            .keys()
            .copied()
            .filter(|member_id| *member_id != self.identity.member_id())
            .collect();
        let removed: Vec<_> = self
            .peers
            .keys()
            .copied()
            .filter(|member_id| !members.contains(member_id))
            .collect();
        for member_id in removed {
            if let Some(peer) = self.peers.remove(&member_id)
                && peer.connection != PeerConnectionState::Disconnected
            {
                effects.push(CoordinatorEffect::DisconnectPeer { member_id });
            }
        }
        for member_id in members {
            self.reconcile_peer(member_id, effects)?;
        }
        Ok(())
    }

    fn reconcile_peer(
        &mut self,
        member_id: MemberId,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let member = self
            .authority_ref()?
            .members
            .get(&member_id)
            .cloned()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
        let generation = self.authority_ref()?.server_generation;
        let peer = self
            .peers
            .entry(member_id)
            .or_insert_with(|| PeerState::new(member.clone()));
        let old_transport = peer.transport();
        let old_catalog = peer.member.catalog_revision;
        let old_sources = peer.member.sources.clone();
        peer.member = member;
        let new_transport = peer.transport();

        if !peer.authority_online() {
            if peer.connection != PeerConnectionState::Disconnected {
                effects.push(CoordinatorEffect::DisconnectPeer { member_id });
            }
            peer.connection = PeerConnectionState::Disconnected;
            peer.inbound_subscription = None;
            peer.inbound_invalidated.clear();
            peer.outbound_pending = None;
            peer.outbound_accepted = BoundedSourceIds::empty();
            return Ok(());
        }

        if old_transport != new_transport && peer.connection != PeerConnectionState::Disconnected {
            effects.push(CoordinatorEffect::DisconnectPeer { member_id });
            peer.connection = PeerConnectionState::Disconnected;
            peer.inbound_subscription = None;
            peer.inbound_invalidated.clear();
            peer.outbound_pending = None;
            peer.outbound_accepted = BoundedSourceIds::empty();
        }
        if peer.connection == PeerConnectionState::Disconnected {
            let (run_id, endpoint) = new_transport.expect("online peer has transport");
            peer.connection = PeerConnectionState::Connecting;
            effects.push(CoordinatorEffect::ConnectPeer(PeerConnectPlan {
                member_id,
                run_id,
                endpoint,
                server_generation: generation,
            }));
        } else if (old_catalog != peer.member.catalog_revision
            || old_sources != peer.member.sources)
            && matches!(peer.connection, PeerConnectionState::Ready { .. })
        {
            self.send_desired_subscription(member_id, effects, true)?;
        }
        Ok(())
    }

    fn handle_direct_ready(
        &mut self,
        member_id: MemberId,
        run_id: RunId,
        direct_session_id: rlb_model::DirectSessionId,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        let peer = self
            .peers
            .get_mut(&member_id)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
        if !peer.authority_online()
            || peer.member.run_id != Some(run_id)
            || !matches!(
                peer.connection,
                PeerConnectionState::Connecting | PeerConnectionState::Disconnected
            )
        {
            return Err(CoordinatorError::new(ErrorCode::RunMismatch));
        }
        peer.connection = PeerConnectionState::Ready { direct_session_id };
        peer.inbound_subscription = None;
        peer.inbound_invalidated.clear();
        peer.outbound_pending = None;
        peer.outbound_accepted = BoundedSourceIds::empty();
        self.send_desired_subscription(member_id, effects, true)?;
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn handle_direct_lost(
        &mut self,
        member_id: MemberId,
        direct_session_id: rlb_model::DirectSessionId,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let authority_online = {
            let peer = self
                .peers
                .get_mut(&member_id)
                .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
            if peer.direct_session_id() != Some(direct_session_id) {
                return Err(CoordinatorError::new(ErrorCode::RunMismatch));
            }
            peer.connection = PeerConnectionState::Disconnected;
            peer.inbound_subscription = None;
            peer.inbound_invalidated.clear();
            peer.outbound_pending = None;
            peer.outbound_accepted = BoundedSourceIds::empty();
            peer.authority_online()
        };
        if authority_online && self.registry_state == RegistryState::Online {
            self.reconcile_peer(member_id, effects)?;
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn send_desired_subscription(
        &mut self,
        member_id: MemberId,
        effects: &mut Vec<CoordinatorEffect>,
        force: bool,
    ) -> Result<(), CoordinatorError> {
        let peer = self
            .peers
            .get(&member_id)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
        let Some(direct_session_id) = peer.direct_session_id() else {
            return Ok(());
        };
        let Some(remote_run_id) = peer.member.run_id else {
            return Ok(());
        };
        let Some(catalog_revision) = peer.member.catalog_revision else {
            return Ok(());
        };
        let mut source_ids: Vec<_> = peer
            .member
            .sources
            .iter()
            .filter(|source| source.active().is_some())
            .map(SourceDescriptor::source_id)
            .collect();
        source_ids.sort_unstable();
        let source_ids = BoundedSourceIds::new(&source_ids)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidCatalog))?;
        if !force
            && peer.outbound_pending.is_some_and(|pending| {
                pending.catalog_revision == catalog_revision && pending.source_ids == source_ids
            })
        {
            return Ok(());
        }
        let next_revision = peer
            .next_outbound_revision
            .checked_next()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::UnexpectedError))?;
        let request = SubscribeSet::new(
            DirectCommon {
                room_id: self.require_room()?,
                sender_member_id: self.identity.member_id(),
                sender_run_id: self.run_id,
                receiver_member_id: member_id,
                receiver_run_id: remote_run_id,
                direct_session_id,
            },
            next_revision,
            catalog_revision,
            source_ids,
        )
        .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        let peer = self
            .peers
            .get_mut(&member_id)
            .expect("peer was checked above");
        peer.next_outbound_revision = next_revision;
        peer.outbound_pending = Some(request);
        peer.outbound_accepted = BoundedSourceIds::empty();
        effects.push(CoordinatorEffect::SendDirect(DirectDatagram::SubscribeSet(
            request,
        )));
        Ok(())
    }

    fn handle_subscribe_set(
        &mut self,
        request: SubscribeSet,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let member_id = request.common.sender_member_id;
        self.validate_direct_common_from_peer(request.common)?;
        let existing = self
            .peers
            .get(&member_id)
            .and_then(|peer| peer.inbound_subscription);
        if let Some(existing) = existing {
            if request.subscription_revision < existing.request.subscription_revision
                || (request.subscription_revision == existing.request.subscription_revision
                    && request != existing.request)
            {
                let ack =
                    self.subscription_status_ack(&request, MediaAckStatus::RevisionConflict)?;
                effects.push(CoordinatorEffect::SendDirect(DirectDatagram::SubscribeAck(
                    ack,
                )));
                return Ok(());
            }
            if request == existing.request {
                effects.push(CoordinatorEffect::SendDirect(DirectDatagram::SubscribeAck(
                    existing.ack,
                )));
                return Ok(());
            }
        }

        if request.catalog_revision != self.catalog_revision {
            let ack = self.subscription_status_ack(&request, MediaAckStatus::CatalogStale)?;
            effects.push(CoordinatorEffect::SendDirect(DirectDatagram::SubscribeAck(
                ack,
            )));
            return Ok(());
        }

        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        for source_id in request.source_ids.iter() {
            match self.subscription_rejection(source_id, member_id) {
                None => accepted.push(source_id),
                Some(code) => rejected.push(Rejection { source_id, code }),
            }
        }
        let accepted = BoundedSourceIds::new(&accepted)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        let rejections = BoundedRejections::new(&rejected)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        self.diagnostics.subscription_rejections = self
            .diagnostics
            .subscription_rejections
            .saturating_add(rejected.len() as u64);
        let ack = SubscribeAck::new(
            self.reverse_direct_common(request.common),
            request.subscription_revision,
            self.catalog_revision,
            MediaAckStatus::Ok,
            accepted,
            rejections,
        )
        .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        let peer = self
            .peers
            .get_mut(&member_id)
            .expect("direct peer was validated");
        peer.inbound_subscription = Some(InboundSubscription { request, ack });
        peer.inbound_invalidated.clear();
        effects.push(CoordinatorEffect::SendDirect(DirectDatagram::SubscribeAck(
            ack,
        )));
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn handle_subscribe_ack(
        &mut self,
        ack: SubscribeAck,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let member_id = ack.common.sender_member_id;
        self.validate_direct_common_from_peer(ack.common)?;
        let request = self
            .peers
            .get(&member_id)
            .and_then(|peer| peer.outbound_pending)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::InvalidStateTransition))?;
        if ack.subscription_revision != request.subscription_revision {
            return Err(CoordinatorError::new(ErrorCode::MediaSubscriptionDenied));
        }
        match ack.status {
            MediaAckStatus::Ok => {
                ack.validate_against(&request)
                    .map_err(|_| CoordinatorError::new(ErrorCode::MediaSubscriptionDenied))?;
                self.peers
                    .get_mut(&member_id)
                    .expect("direct peer was validated")
                    .outbound_accepted = ack.accepted_source_ids;
            }
            MediaAckStatus::CatalogStale => {
                self.peers
                    .get_mut(&member_id)
                    .expect("direct peer was validated")
                    .outbound_accepted = BoundedSourceIds::empty();
                self.request_snapshot(SnapshotReason::RevisionGap, effects);
            }
            MediaAckStatus::RevisionConflict => {
                self.peers
                    .get_mut(&member_id)
                    .expect("direct peer was validated")
                    .outbound_accepted = BoundedSourceIds::empty();
                self.send_desired_subscription(member_id, effects, true)?;
            }
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn subscription_status_ack(
        &self,
        request: &SubscribeSet,
        status: MediaAckStatus,
    ) -> Result<SubscribeAck, CoordinatorError> {
        SubscribeAck::new(
            self.reverse_direct_common(request.common),
            request.subscription_revision,
            self.catalog_revision,
            status,
            BoundedSourceIds::empty(),
            BoundedRejections::empty(),
        )
        .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))
    }

    fn subscription_rejection(
        &self,
        source_id: SourceId,
        member_id: MemberId,
    ) -> Option<RejectionCode> {
        let Some(source) = self.config.source(source_id) else {
            return Some(RejectionCode::SourceUnknown);
        };
        let Some(runtime) = self.sources.get(&source_id) else {
            return Some(RejectionCode::SourceUnknown);
        };
        if !self.local_approved() || !self.recipient_approved(member_id) {
            return Some(RejectionCode::MemberNotApproved);
        }
        if !self.visible_to(source_id, member_id) {
            return Some(RejectionCode::SourceHidden);
        }
        if !runtime.available || runtime.active.is_none() {
            return Some(RejectionCode::SourceUnavailable);
        }
        if !source.config.master_capture_enabled || !source.config.publish_requested {
            return Some(RejectionCode::SourceUnpublished);
        }
        if self.room_send_muted() {
            return Some(RejectionCode::RoomSendMuted);
        }
        if self.local_emergency_stop() {
            return Some(RejectionCode::LocalEmergencyStop);
        }
        None
    }

    fn validate_direct_common_from_peer(
        &self,
        common: DirectCommon,
    ) -> Result<(), CoordinatorError> {
        if self.config.room.room_id != Some(common.room_id)
            || common.receiver_member_id != self.identity.member_id()
            || common.receiver_run_id != self.run_id
        {
            return Err(CoordinatorError::new(ErrorCode::MediaIdentityMismatch));
        }
        let peer = self
            .peers
            .get(&common.sender_member_id)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::MemberNotFound))?;
        if peer.member.run_id != Some(common.sender_run_id)
            || peer.direct_session_id() != Some(common.direct_session_id)
        {
            return Err(CoordinatorError::new(ErrorCode::MediaStaleRun));
        }
        Ok(())
    }

    fn reverse_direct_common(&self, common: DirectCommon) -> DirectCommon {
        DirectCommon {
            room_id: common.room_id,
            sender_member_id: self.identity.member_id(),
            sender_run_id: self.run_id,
            receiver_member_id: common.sender_member_id,
            receiver_run_id: common.sender_run_id,
            direct_session_id: common.direct_session_id,
        }
    }

    fn handle_source_availability(
        &mut self,
        source_id: SourceId,
        available: bool,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        let source_config = *self
            .config
            .source(source_id)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
        let old_catalog = self.catalog()?;
        let runtime = self
            .sources
            .get_mut(&source_id)
            .expect("validated source runtime");
        if available {
            if source_config.config.master_capture_enabled {
                runtime.available = false;
                runtime.capture_state = CaptureState::Starting;
                let reason = if runtime.last_stream.is_none() {
                    EpochChangeReason::EngineRun
                } else {
                    EpochChangeReason::TimelineDiscontinuity
                };
                runtime.required_epoch_reason = Some(reason);
                effects.push(CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
                    source_id,
                    reason,
                }));
            } else {
                runtime.available = true;
                runtime.capture_state = CaptureState::Disabled;
            }
        } else {
            runtime.available = false;
            runtime.capture_state = CaptureState::Unavailable;
            let ended = runtime.active.take();
            if let Some(ended) = ended {
                effects.push(CoordinatorEffect::EndSourceEpoch(SourceEpochEndPlan {
                    source_id,
                    stream_epoch: ended.stream_epoch,
                    reason: EpochEndReason::SourceUnavailable,
                }));
            }
            runtime.required_epoch_reason = Some(EpochChangeReason::TimelineDiscontinuity);
        }
        if self.catalog()? != old_catalog {
            self.bump_catalog()?;
            self.publish_catalog(effects)?;
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn handle_source_started(
        &mut self,
        stream: SourceStreamStarted,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        if stream.endpoint_id.is_empty() {
            return Err(CoordinatorError::new(ErrorCode::InvalidValue));
        }
        let source_config = *self
            .config
            .source(stream.source_id)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
        if !source_config.config.master_capture_enabled
            || stream.format.channels() != source_config.kind.channels()
            || stream.format.channel_layout() != source_config.kind.channel_layout()
            || stream.format.profile() != self.effective_profile(stream.source_id)
        {
            return Err(CoordinatorError::new(ErrorCode::InvalidCatalog));
        }
        let expected_method = match source_config.kind {
            SourceKind::SystemAudio => Some(self.config.system_capture_method),
            SourceKind::Microphone => None,
        };
        if stream.capture_method != expected_method {
            return Err(CoordinatorError::new(ErrorCode::InvalidValue));
        }

        let old_catalog = self.catalog()?;
        let runtime = self
            .sources
            .get_mut(&stream.source_id)
            .expect("validated source runtime");
        if runtime
            .last_stream
            .as_ref()
            .is_some_and(|last| last.stream_epoch == stream.stream_epoch)
        {
            return Err(CoordinatorError::new(ErrorCode::MediaEpochConflict));
        }
        if runtime
            .required_epoch_reason
            .is_some_and(|required| required != stream.reason)
            || !Self::epoch_reason_matches(runtime.last_stream.as_ref(), &stream)
        {
            return Err(CoordinatorError::new(ErrorCode::MediaEpochConflict));
        }
        let active = ActiveSourceStream {
            stream_epoch: stream.stream_epoch,
            endpoint_id: stream.endpoint_id,
            capture_method: stream.capture_method,
            format: stream.format,
        };
        runtime.available = true;
        runtime.capture_state = CaptureState::Running;
        runtime.active = Some(active.clone());
        runtime.last_stream = Some(active);
        runtime.required_epoch_reason = None;
        runtime.epoch_changes_total = runtime.epoch_changes_total.saturating_add(1);
        effects.push(CoordinatorEffect::StartSourceEpoch(SourceEpochStartPlan {
            source_id: stream.source_id,
            stream_epoch: stream.stream_epoch,
            format: stream.format,
        }));
        if self.catalog()? != old_catalog {
            self.bump_catalog()?;
            self.publish_catalog(effects)?;
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    fn epoch_reason_matches(
        previous: Option<&ActiveSourceStream>,
        next: &SourceStreamStarted,
    ) -> bool {
        match next.reason {
            EpochChangeReason::EngineRun => previous.is_none(),
            EpochChangeReason::EndpointChanged => {
                previous.is_some_and(|previous| previous.endpoint_id != next.endpoint_id)
            }
            EpochChangeReason::CaptureMethodChanged => {
                previous.is_some_and(|previous| previous.capture_method != next.capture_method)
            }
            EpochChangeReason::FormatChanged => {
                previous.is_some_and(|previous| previous.format != next.format)
            }
            EpochChangeReason::TimelineDiscontinuity => previous.is_some(),
        }
    }

    fn handle_source_stopped(
        &mut self,
        source_id: SourceId,
        reason: SourceStopReason,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let old_catalog = self.catalog()?;
        let runtime = self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
        let ended = runtime.active.take();
        if let Some(ended) = ended {
            effects.push(CoordinatorEffect::EndSourceEpoch(SourceEpochEndPlan {
                source_id,
                stream_epoch: ended.stream_epoch,
                reason: Self::epoch_end_reason(reason),
            }));
        }
        match reason {
            SourceStopReason::CaptureDisabled => {
                runtime.capture_state = CaptureState::Disabled;
            }
            SourceStopReason::SourceUnavailable => {
                runtime.available = false;
                runtime.capture_state = CaptureState::Unavailable;
                runtime.required_epoch_reason = Some(EpochChangeReason::TimelineDiscontinuity);
            }
            SourceStopReason::Reconfigure(epoch_reason) => {
                runtime.available = false;
                runtime.capture_state = CaptureState::Starting;
                runtime.required_epoch_reason = Some(epoch_reason);
            }
            SourceStopReason::EngineShutdown => {
                runtime.available = false;
                runtime.capture_state = CaptureState::Disabled;
            }
        }
        if self.catalog()? != old_catalog {
            self.bump_catalog()?;
            self.publish_catalog(effects)?;
        }
        self.refresh_plans(effects)?;
        Ok(())
    }

    const fn epoch_end_reason(reason: SourceStopReason) -> EpochEndReason {
        match reason {
            SourceStopReason::CaptureDisabled => EpochEndReason::SourceStopped,
            SourceStopReason::SourceUnavailable => EpochEndReason::SourceUnavailable,
            SourceStopReason::Reconfigure(_) => EpochEndReason::EpochReplaced,
            SourceStopReason::EngineShutdown => EpochEndReason::EngineShutdown,
        }
    }

    fn handle_local_config(
        &mut self,
        request: LocalConfigRequest,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        if request.expected_member_config_revision != self.config.member_config_revision {
            effects.push(CoordinatorEffect::LocalConfigCompleted(self.local_outcome(
                request.request_id,
                CommandStatus::Rejected,
                Some(ErrorCode::StaleMemberConfigRevision),
            )));
            return Ok(());
        }
        if self.pending_commit.is_some() {
            effects.push(CoordinatorEffect::LocalConfigCompleted(self.local_outcome(
                request.request_id,
                CommandStatus::Rejected,
                Some(ErrorCode::OperationInProgress),
            )));
            return Ok(());
        }
        if matches!(
            request.mutation,
            LocalConfigMutation::SetCapabilityGrant { .. }
        ) && request.expected_grant_revision != Some(self.config.room.grant_revision)
        {
            effects.push(CoordinatorEffect::LocalConfigCompleted(self.local_outcome(
                request.request_id,
                CommandStatus::Rejected,
                Some(ErrorCode::StaleGrantRevision),
            )));
            return Ok(());
        }

        let mut candidate = self.config.clone();
        let mut catalog_changes = false;
        let changed;
        let mut safety_gate_preapplied = false;
        match request.mutation {
            LocalConfigMutation::SetMasterCapture { source_id, enabled } => {
                let source = candidate
                    .source_mut(source_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
                changed = source.config.master_capture_enabled != enabled;
                source.config.master_capture_enabled = enabled;
                catalog_changes = changed;
            }
            LocalConfigMutation::SetSourcePublication {
                room_id,
                source_id,
                publish_requested,
            } => {
                self.ensure_room(room_id)?;
                let source = candidate
                    .source_mut(source_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
                changed = source.config.publish_requested != publish_requested;
                source.config.publish_requested = publish_requested;
                catalog_changes = changed;
            }
            LocalConfigMutation::SetSendQuality {
                room_id,
                source_id,
                selected_profile,
            } => {
                self.ensure_room(room_id)?;
                let source = candidate
                    .source_mut(source_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
                changed = source.config.selected_profile != selected_profile;
                source.config.selected_profile = selected_profile;
                catalog_changes = changed;
            }
            LocalConfigMutation::SetCaptureMethod { source_id, method } => {
                if source_id != self.identity.source_id(SourceKind::SystemAudio) {
                    return Err(CoordinatorError::new(ErrorCode::FieldNotAllowed));
                }
                changed = candidate.system_capture_method != method;
                candidate.system_capture_method = method;
                catalog_changes = changed
                    && self
                        .sources
                        .get(&source_id)
                        .is_some_and(|runtime| runtime.active.is_some());
            }
            LocalConfigMutation::SetSourceBinding {
                source_id,
                capture_method,
                binding,
            } => {
                let source = candidate
                    .source(source_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
                let kind = source.kind;
                let active = self
                    .sources
                    .get(&source_id)
                    .is_some_and(|runtime| runtime.active.is_some());
                match kind {
                    SourceKind::SystemAudio => {
                        changed = candidate.system_capture_method != capture_method
                            || candidate.system_audio_binding != binding;
                        candidate.system_capture_method = capture_method;
                        candidate.system_audio_binding = binding;
                    }
                    SourceKind::Microphone => {
                        changed = candidate.microphone_binding != binding;
                        candidate.microphone_binding = binding;
                    }
                }
                catalog_changes = changed && active;
            }
            LocalConfigMutation::SetPlaybackBinding { binding } => {
                changed = candidate.playback_binding != binding;
                candidate.playback_binding = binding;
            }
            LocalConfigMutation::SetReceiveBuffer { room_id, buffer } => {
                self.ensure_room(room_id)?;
                changed = candidate.room.settings.receive_buffer != buffer;
                candidate.room.settings.receive_buffer = buffer;
            }
            LocalConfigMutation::SetEmergencyStop { enabled } => {
                changed = candidate.local_emergency_stop != enabled
                    || self.volatile_emergency_stop != enabled;
                candidate.local_emergency_stop = enabled;
                if enabled && !self.local_emergency_stop() {
                    self.volatile_emergency_stop = true;
                    self.bump_catalog()?;
                    safety_gate_preapplied = true;
                    effects.push(CoordinatorEffect::SetEmergencySendGate { stopped: true });
                    self.publish_catalog(effects)?;
                    self.refresh_plans(effects)?;
                }
                catalog_changes = changed && !safety_gate_preapplied;
            }
            LocalConfigMutation::SetCapabilityGrant {
                room_id,
                capability,
                enabled,
            } => {
                self.ensure_room(room_id)?;
                changed = candidate.room.settings.grants.get(capability) != enabled;
                candidate.room.settings.grants.set(capability, enabled);
                if changed {
                    candidate.room.grant_revision = candidate
                        .room
                        .grant_revision
                        .checked_next()
                        .ok_or_else(|| CoordinatorError::new(ErrorCode::UnexpectedError))?;
                }
            }
            LocalConfigMutation::SetMemberMix {
                room_id,
                remote_member_id,
                mix,
            } => {
                self.ensure_room(room_id)?;
                self.ensure_remote_member(remote_member_id)?;
                changed = Self::set_member_mix(&mut candidate, remote_member_id, mix);
            }
            LocalConfigMutation::SetSourceMix {
                room_id,
                remote_member_id,
                source_id,
                mix,
            } => {
                self.ensure_room(room_id)?;
                self.ensure_remote_source(remote_member_id, source_id)?;
                changed = Self::set_source_mix(&mut candidate, remote_member_id, source_id, mix);
            }
            LocalConfigMutation::SetAutoReconnect { room_id, enabled } => {
                self.ensure_room(room_id)?;
                changed = candidate.room.settings.auto_reconnect != enabled;
                candidate.room.settings.auto_reconnect = enabled;
            }
            LocalConfigMutation::SetJoinTarget { room_id, target } => {
                if let Some(existing) = candidate.room.room_id
                    && existing != room_id
                {
                    return Err(CoordinatorError::new(ErrorCode::RoomScopeMismatch));
                }
                changed = candidate.room.room_id != Some(room_id)
                    || candidate.room.join.as_ref() != Some(&target);
                candidate.room.room_id = Some(room_id);
                candidate.room.join = Some(target);
            }
            LocalConfigMutation::ClearJoin => {
                changed = candidate.has_room_scoped_state();
                if changed {
                    catalog_changes = candidate
                        .sources
                        .iter()
                        .any(|source| source.config.publish_requested);
                    candidate
                        .detach_room()
                        .map_err(|_| CoordinatorError::new(ErrorCode::UnexpectedError))?;
                }
            }
            LocalConfigMutation::SetSourceMonitor { source_id, monitor } => {
                let source = candidate
                    .source_mut(source_id)
                    .ok_or_else(|| CoordinatorError::new(ErrorCode::SourceNotFound))?;
                changed = source.monitor != monitor;
                source.monitor = monitor;
            }
            LocalConfigMutation::SetStartup { startup } => {
                changed = candidate.startup != startup;
                candidate.startup = startup;
            }
            LocalConfigMutation::SetLogging { logging } => {
                changed = candidate.logging != logging;
                candidate.logging = logging;
            }
            LocalConfigMutation::SetDriverSelection {
                package_id,
                enabled,
            } => {
                changed = candidate.driver.package_id != package_id
                    || candidate.driver.enabled != enabled;
                candidate.driver.package_id = package_id;
                candidate.driver.enabled = enabled;
            }
        }

        if !changed {
            effects.push(CoordinatorEffect::LocalConfigCompleted(self.local_outcome(
                request.request_id,
                CommandStatus::Unchanged,
                None,
            )));
            return Ok(());
        }
        candidate.audit_seq = candidate.audit_seq.saturating_add(1);
        candidate.member_config_revision = candidate
            .member_config_revision
            .checked_next()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::UnexpectedError))?;
        candidate
            .validate(self.identity)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        let target_catalog_revision = if catalog_changes {
            self.catalog_revision
                .checked_next()
                .ok_or_else(|| CoordinatorError::new(ErrorCode::UnexpectedError))?
        } else {
            self.catalog_revision
        };
        self.begin_commit(
            candidate,
            target_catalog_revision,
            safety_gate_preapplied,
            CommitOrigin::Local {
                request_id: request.request_id,
            },
            effects,
        )
    }

    fn handle_member_command(
        &mut self,
        delivery: MemberCommandDelivery,
        now_ms: u64,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        self.ensure_running()?;
        if self.registry_state != RegistryState::Online
            || self.registry_session_id != Some(delivery.registry_session_id)
        {
            self.diagnostics.commands_dropped_without_session = self
                .diagnostics
                .commands_dropped_without_session
                .saturating_add(1);
            return Ok(());
        }

        let command_hash = canonical_command_hash(&delivery.command)
            .map_err(|_| CoordinatorError::new(ErrorCode::UnexpectedError))?;
        let cache_key = RequestCacheKey::new(
            MEMBER_AUTHORITY_PRINCIPAL,
            RequestBoundary::Member,
            RequestScope::Room {
                room_id: self.require_room()?,
            },
            delivery.command.request_id,
        )
        .map_err(|_| CoordinatorError::new(ErrorCode::UnexpectedError))?;
        let (started_cache, cache_effect) = reduce_request_cache(
            &self.config.room.member_command_cache,
            RequestCacheAction::Begin {
                key: cache_key.clone(),
                command_hash,
                now_ms,
            },
        );
        match cache_effect {
            RequestCacheEffect::Replay(Some(result)) => {
                effects.push(CoordinatorEffect::SendRegistryMessage(
                    self.member_command_result(&delivery, &result),
                ));
                return Ok(());
            }
            RequestCacheEffect::Replay(None) => {
                return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
            }
            RequestCacheEffect::RequestIdConflict => {
                self.reject_member_command(&delivery, ErrorCode::RequestIdConflict, effects);
                return Ok(());
            }
            RequestCacheEffect::Full => {
                self.reject_member_command(&delivery, ErrorCode::ServerBusy, effects);
                return Ok(());
            }
            RequestCacheEffect::Execute => {}
            RequestCacheEffect::Stored
            | RequestCacheEffect::Missing
            | RequestCacheEffect::Purged(_) => {
                return Err(CoordinatorError::new(ErrorCode::UnexpectedError));
            }
        }
        if self.pending_commit.is_some() {
            self.reject_member_command(&delivery, ErrorCode::OperationInProgress, effects);
            return Ok(());
        }
        if now_ms > delivery.expires_at_ms {
            return self.persist_member_rejection(
                delivery,
                ErrorCode::MemberCommandTimeout,
                started_cache,
                cache_key,
                command_hash,
                effects,
            );
        }
        if let Err(code) = self.validate_delegated_target(&delivery.command) {
            return self.persist_member_rejection(
                delivery,
                code,
                started_cache,
                cache_key,
                command_hash,
                effects,
            );
        }
        let authority = self.authority_ref()?;
        let delegated = self
            .config
            .delegated_state(
                self.identity,
                authority.room_revision,
                self.catalog_revision,
                authority.room.quality_cap,
            )
            .map_err(|_| CoordinatorError::new(ErrorCode::RoomScopeMismatch))?;
        let transition = match reduce_delegated_command(&delegated, &delivery.command) {
            Ok(transition) => transition,
            Err(error) => {
                return self.persist_member_rejection(
                    delivery,
                    error.code,
                    started_cache,
                    cache_key,
                    command_hash,
                    effects,
                );
            }
        };
        let target_catalog_revision = transition.state.catalog_revision;
        let mut candidate = self.config.clone();
        candidate
            .apply_delegated_state(self.identity, &transition.state)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        let effective = self.member_command_effective(
            &candidate,
            &transition.effective,
            &delivery.command.mutation,
        );
        if transition.status == CommandStatus::Unchanged {
            let result = self.member_result_record(
                &candidate,
                target_catalog_revision,
                delivery.command.capability(),
                MemberCommandStatus::Unchanged,
                None,
                effective,
                transition.warnings,
            );
            return self.begin_member_result_commit(
                candidate,
                target_catalog_revision,
                delivery,
                result,
                started_cache,
                cache_key,
                command_hash,
                effects,
            );
        }

        let result = self.member_result_record(
            &candidate,
            target_catalog_revision,
            delivery.command.capability(),
            MemberCommandStatus::Applied,
            None,
            effective,
            transition.warnings,
        );
        self.begin_member_result_commit(
            candidate,
            target_catalog_revision,
            delivery,
            result,
            started_cache,
            cache_key,
            command_hash,
            effects,
        )
    }

    fn validate_delegated_target(
        &self,
        command: &rlb_control::ValidatedDelegatedCommand,
    ) -> Result<(), ErrorCode> {
        match command.mutation {
            rlb_control::DelegatedMutation::SetSourcePublication { source_id, .. }
            | rlb_control::DelegatedMutation::SetSendQuality { source_id, .. } => {
                if self.config.source(source_id).is_none() {
                    return Err(ErrorCode::SourceNotFound);
                }
            }
            rlb_control::DelegatedMutation::SetMemberMix {
                remote_member_id, ..
            } => {
                if self.ensure_remote_member(remote_member_id).is_err() {
                    return Err(ErrorCode::TargetNotVisible);
                }
            }
            rlb_control::DelegatedMutation::SetSourceMix {
                remote_member_id,
                source_id,
                ..
            } => {
                if self
                    .ensure_remote_source(remote_member_id, source_id)
                    .is_err()
                {
                    return Err(ErrorCode::TargetNotVisible);
                }
            }
            rlb_control::DelegatedMutation::SetAutoReconnect { .. } => {}
        }
        Ok(())
    }

    fn persist_member_rejection(
        &mut self,
        delivery: MemberCommandDelivery,
        code: ErrorCode,
        started_cache: RequestCache<MemberCommandResultRecord>,
        cache_key: RequestCacheKey,
        command_hash: rlb_control::CanonicalCommandHash,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let result = self.member_result_record(
            &self.config,
            self.catalog_revision,
            delivery.command.capability(),
            MemberCommandStatus::Rejected,
            Some(code),
            MemberCommandEffective::default(),
            Vec::new(),
        );
        self.begin_member_result_commit(
            self.config.clone(),
            self.catalog_revision,
            delivery,
            result,
            started_cache,
            cache_key,
            command_hash,
            effects,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_member_result_commit(
        &mut self,
        mut candidate: MemberConfig,
        target_catalog_revision: CatalogRevision,
        delivery: MemberCommandDelivery,
        result: MemberCommandResultRecord,
        started_cache: RequestCache<MemberCommandResultRecord>,
        cache_key: RequestCacheKey,
        command_hash: rlb_control::CanonicalCommandHash,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let (completed_cache, effect) = reduce_request_cache(
            &started_cache,
            RequestCacheAction::StoreResponse {
                key: cache_key,
                command_hash,
                response: result.clone(),
            },
        );
        if effect != RequestCacheEffect::Stored {
            return Err(CoordinatorError::new(ErrorCode::UnexpectedError));
        }
        candidate.room.member_command_cache = completed_cache;
        candidate
            .validate(self.identity)
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidValue))?;
        self.begin_commit(
            candidate,
            target_catalog_revision,
            false,
            CommitOrigin::Delegated(Box::new(DelegatedCommit { delivery, result })),
            effects,
        )
    }

    fn begin_commit(
        &mut self,
        candidate: MemberConfig,
        target_catalog_revision: CatalogRevision,
        catalog_projection_preapplied: bool,
        origin: CommitOrigin,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.pending_commit.is_some() {
            return Err(CoordinatorError::new(ErrorCode::OperationInProgress));
        }
        let transaction_id = ConfigTransactionId(self.next_transaction_id);
        self.next_transaction_id = self
            .next_transaction_id
            .checked_add(1)
            .ok_or_else(|| CoordinatorError::new(ErrorCode::UnexpectedError))?;
        self.pending_commit = Some(PendingCommit {
            transaction_id,
            candidate: candidate.clone(),
            target_catalog_revision,
            publish_catalog_after_commit: target_catalog_revision != self.catalog_revision
                && !catalog_projection_preapplied,
            origin,
        });
        effects.push(CoordinatorEffect::PersistMemberConfig {
            transaction_id,
            candidate,
        });
        Ok(())
    }

    fn handle_persistence_completed(
        &mut self,
        transaction_id: ConfigTransactionId,
        result: PersistenceResult,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let pending = self
            .pending_commit
            .take()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::InvalidStateTransition))?;
        if pending.transaction_id != transaction_id {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        }
        match result {
            PersistenceResult::Failed => {
                self.diagnostics.persistence_failures =
                    self.diagnostics.persistence_failures.saturating_add(1);
                match pending.origin {
                    CommitOrigin::Local { request_id } => {
                        effects.push(CoordinatorEffect::LocalConfigCompleted(self.local_outcome(
                            request_id,
                            CommandStatus::Rejected,
                            Some(ErrorCode::MemberConfigPersistFailed),
                        )));
                    }
                    CommitOrigin::Delegated(commit) => {
                        let delivery = commit.delivery;
                        if self.registry_state == RegistryState::Online
                            && self.registry_session_id == Some(delivery.registry_session_id)
                        {
                            self.reject_member_command(
                                &delivery,
                                ErrorCode::MemberConfigPersistFailed,
                                effects,
                            );
                        } else {
                            self.diagnostics.commands_dropped_without_session = self
                                .diagnostics
                                .commands_dropped_without_session
                                .saturating_add(1);
                        }
                    }
                }
                return Ok(());
            }
            PersistenceResult::Committed => {}
        }

        let old = self.config.clone();
        self.config = pending.candidate;
        self.catalog_revision = pending.target_catalog_revision;
        self.apply_committed_config_runtime(&old, effects)?;
        self.publish_changed_registry_projections(
            &old,
            pending.publish_catalog_after_commit,
            effects,
        )?;
        self.refresh_plans(effects)?;

        match pending.origin {
            CommitOrigin::Local { request_id } => {
                effects.push(CoordinatorEffect::LocalConfigCompleted(self.local_outcome(
                    request_id,
                    CommandStatus::Applied,
                    None,
                )));
            }
            CommitOrigin::Delegated(commit) => {
                let DelegatedCommit { delivery, result } = *commit;
                if self.registry_state == RegistryState::Online
                    && self.registry_session_id == Some(delivery.registry_session_id)
                {
                    effects.push(CoordinatorEffect::SendRegistryMessage(
                        self.member_command_result(&delivery, &result),
                    ));
                } else {
                    self.diagnostics.commands_dropped_without_session = self
                        .diagnostics
                        .commands_dropped_without_session
                        .saturating_add(1);
                }
            }
        }
        Ok(())
    }

    fn apply_committed_config_runtime(
        &mut self,
        old: &MemberConfig,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if old.room.join != self.config.room.join {
            match &self.config.room.join {
                Some(_)
                    if matches!(
                        self.registry_state,
                        RegistryState::Disconnected | RegistryState::Outage | RegistryState::Left
                    ) =>
                {
                    self.registry_state = RegistryState::Connecting;
                    effects.push(CoordinatorEffect::ConnectRegistry);
                }
                None => {
                    self.registry_state = RegistryState::Left;
                    self.registry_session_id = None;
                    effects.push(CoordinatorEffect::LeaveRegistry);
                    for peer in self.peers.values() {
                        if peer.connection != PeerConnectionState::Disconnected {
                            effects.push(CoordinatorEffect::DisconnectPeer {
                                member_id: peer.member.member_id,
                            });
                        }
                    }
                    self.peers.clear();
                    self.authority = None;
                }
                Some(_) => {}
            }
        }
        if old.local_emergency_stop != self.config.local_emergency_stop
            || self.volatile_emergency_stop != self.config.local_emergency_stop
        {
            self.volatile_emergency_stop = self.config.local_emergency_stop;
            effects.push(CoordinatorEffect::SetEmergencySendGate {
                stopped: self.local_emergency_stop(),
            });
        }
        for source in self.config.sources {
            let old_source = old
                .source(source.source_id)
                .ok_or_else(|| CoordinatorError::new(ErrorCode::InvalidCatalog))?;
            let method_changed = source.kind == SourceKind::SystemAudio
                && old.system_capture_method != self.config.system_capture_method;
            let binding_changed =
                old.source_binding(source.kind) != self.config.source_binding(source.kind);
            let profile_changed =
                old_source.config.selected_profile != source.config.selected_profile;
            let master_changed =
                old_source.config.master_capture_enabled != source.config.master_capture_enabled;
            let runtime = self
                .sources
                .get_mut(&source.source_id)
                .expect("validated source runtime");

            if master_changed && !source.config.master_capture_enabled {
                if let Some(ended) = runtime.active.take() {
                    effects.push(CoordinatorEffect::EndSourceEpoch(SourceEpochEndPlan {
                        source_id: source.source_id,
                        stream_epoch: ended.stream_epoch,
                        reason: EpochEndReason::SourceStopped,
                    }));
                }
                runtime.capture_state = CaptureState::Disabled;
                runtime.required_epoch_reason = None;
                effects.push(CoordinatorEffect::StopSource {
                    source_id: source.source_id,
                    reason: SourceStopReason::CaptureDisabled,
                });
                continue;
            }
            if !source.config.master_capture_enabled {
                continue;
            }

            let restart_reason = if method_changed {
                Some(EpochChangeReason::CaptureMethodChanged)
            } else if binding_changed {
                Some(EpochChangeReason::EndpointChanged)
            } else if profile_changed {
                Some(EpochChangeReason::FormatChanged)
            } else if master_changed {
                Some(if runtime.last_stream.is_none() {
                    EpochChangeReason::EngineRun
                } else {
                    EpochChangeReason::TimelineDiscontinuity
                })
            } else {
                None
            };
            if let Some(reason) = restart_reason {
                if let Some(ended) = runtime.active.take() {
                    effects.push(CoordinatorEffect::EndSourceEpoch(SourceEpochEndPlan {
                        source_id: source.source_id,
                        stream_epoch: ended.stream_epoch,
                        reason: EpochEndReason::EpochReplaced,
                    }));
                    effects.push(CoordinatorEffect::StopSource {
                        source_id: source.source_id,
                        reason: SourceStopReason::Reconfigure(reason),
                    });
                }
                runtime.available = false;
                runtime.capture_state = CaptureState::Starting;
                runtime.required_epoch_reason = Some(reason);
                effects.push(CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
                    source_id: source.source_id,
                    reason,
                }));
            }
        }
        Ok(())
    }

    fn publish_changed_registry_projections(
        &self,
        old: &MemberConfig,
        publish_catalog: bool,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.registry_state != RegistryState::Online {
            return Ok(());
        }
        if publish_catalog {
            self.publish_catalog(effects)?;
        }
        if old.room.grant_revision != self.config.room.grant_revision {
            effects.push(CoordinatorEffect::PublishRegistryProjection(
                RegistryProjectionUpdate::Grants {
                    grant_revision: self.config.room.grant_revision,
                    grants: self.config.room.settings.grants,
                },
            ));
        }
        if old.member_config_revision != self.config.member_config_revision {
            effects.push(CoordinatorEffect::PublishRegistryProjection(
                RegistryProjectionUpdate::MemberConfigRevision {
                    member_config_revision: self.config.member_config_revision,
                },
            ));
        }
        Ok(())
    }

    fn reconcile_source_policy(
        &mut self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        for source in self.config.sources {
            if !source.config.master_capture_enabled {
                continue;
            }
            let effective_profile = self.effective_profile(source.source_id);
            let runtime = self
                .sources
                .get_mut(&source.source_id)
                .expect("validated source runtime");
            if runtime
                .last_stream
                .as_ref()
                .is_some_and(|stream| stream.format.profile() != effective_profile)
            {
                if let Some(ended) = runtime.active.take() {
                    effects.push(CoordinatorEffect::EndSourceEpoch(SourceEpochEndPlan {
                        source_id: source.source_id,
                        stream_epoch: ended.stream_epoch,
                        reason: EpochEndReason::EpochReplaced,
                    }));
                    effects.push(CoordinatorEffect::StopSource {
                        source_id: source.source_id,
                        reason: SourceStopReason::Reconfigure(EpochChangeReason::FormatChanged),
                    });
                }
                runtime.available = false;
                runtime.capture_state = CaptureState::Starting;
                runtime.required_epoch_reason = Some(EpochChangeReason::FormatChanged);
                effects.push(CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
                    source_id: source.source_id,
                    reason: EpochChangeReason::FormatChanged,
                }));
            }
        }
        Ok(())
    }

    fn handle_shutdown_requested(
        &mut self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.engine_state != EngineState::Running || self.pending_commit.is_some() {
            return Err(CoordinatorError::new(if self.pending_commit.is_some() {
                ErrorCode::OperationInProgress
            } else {
                ErrorCode::InvalidStateTransition
            }));
        }
        self.engine_state = EngineState::ShuttingDown;
        self.shutdown_stage = Some(ShutdownStage::CloseSendGate);
        self.volatile_emergency_stop = true;
        effects.push(CoordinatorEffect::SetEmergencySendGate { stopped: true });
        effects.push(CoordinatorEffect::RunShutdownStage(
            ShutdownStage::CloseSendGate,
        ));
        Ok(())
    }

    fn handle_shutdown_stage_completed(
        &mut self,
        stage: ShutdownStage,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.engine_state != EngineState::ShuttingDown || self.shutdown_stage != Some(stage) {
            return Err(CoordinatorError::new(ErrorCode::InvalidStateTransition));
        }
        if let Some(next) = stage.next() {
            self.shutdown_stage = Some(next);
            effects.push(CoordinatorEffect::RunShutdownStage(next));
        } else {
            self.shutdown_stage = None;
            self.engine_state = EngineState::Stopped;
            self.registry_state = RegistryState::Left;
            self.registry_session_id = None;
            self.peers.clear();
        }
        Ok(())
    }

    fn set_member_mix(
        config: &mut MemberConfig,
        remote_member_id: MemberId,
        mix: MemberMix,
    ) -> bool {
        if let Some(entry) = config
            .room
            .member_mix
            .iter_mut()
            .find(|entry| entry.remote_member_id == remote_member_id)
        {
            let changed = entry.mix != mix;
            entry.mix = mix;
            changed
        } else {
            config.room.member_mix.push(RemoteMemberMix {
                remote_member_id,
                mix,
            });
            true
        }
    }

    fn set_source_mix(
        config: &mut MemberConfig,
        remote_member_id: MemberId,
        source_id: SourceId,
        mix: rlb_model::SourceMix,
    ) -> bool {
        if let Some(entry) = config.room.source_mix.iter_mut().find(|entry| {
            entry.remote_member_id == remote_member_id && entry.source_id == source_id
        }) {
            let changed = entry.mix != mix;
            entry.mix = mix;
            changed
        } else {
            config.room.source_mix.push(RemoteSourceMix {
                remote_member_id,
                source_id,
                mix,
            });
            true
        }
    }

    fn member_command_effective(
        &self,
        config: &MemberConfig,
        effective: &DelegatedEffective,
        mutation: &rlb_control::DelegatedMutation,
    ) -> MemberCommandEffective {
        match *effective {
            DelegatedEffective::SourcePublication {
                publish_requested,
                master_capture_enabled: _,
            } => {
                let source_id = match mutation {
                    rlb_control::DelegatedMutation::SetSourcePublication { source_id, .. } => {
                        Some(*source_id)
                    }
                    _ => None,
                };
                let blocks = source_id.map_or_else(Vec::new, |source_id| {
                    self.local_publication_blocks(config, source_id)
                });
                MemberCommandEffective {
                    publish_requested: Field::Present(publish_requested),
                    effective_publishing: Field::Present(publish_requested && blocks.is_empty()),
                    blocking_reasons: Field::Present(blocks),
                    ..MemberCommandEffective::default()
                }
            }
            DelegatedEffective::SendQuality {
                selected_profile,
                effective_profile,
            } => MemberCommandEffective {
                selected_profile: Field::Present(selected_profile),
                effective_profile: Field::Present(effective_profile),
                ..MemberCommandEffective::default()
            },
            DelegatedEffective::MemberMix { mix, .. } => MemberCommandEffective {
                gain_db: Field::Present(mix.gain_db),
                mute: Field::Present(mix.mute),
                solo: Field::Present(mix.solo),
                ..MemberCommandEffective::default()
            },
            DelegatedEffective::SourceMix { mix, .. } => MemberCommandEffective {
                gain_db: Field::Present(mix.gain_db),
                pan: Field::Present(mix.pan),
                mute: Field::Present(mix.mute),
                ..MemberCommandEffective::default()
            },
            DelegatedEffective::AutoReconnect { enabled } => MemberCommandEffective {
                enabled: Field::Present(enabled),
                ..MemberCommandEffective::default()
            },
        }
    }

    fn local_publication_blocks(
        &self,
        config: &MemberConfig,
        source_id: SourceId,
    ) -> Vec<PublicationBlock> {
        let mut blocks = Vec::new();
        let Some(source) = config.source(source_id) else {
            return blocks;
        };
        let Some(runtime) = self.sources.get(&source_id) else {
            return blocks;
        };
        if !source.config.master_capture_enabled {
            blocks.push(PublicationBlock::MasterCaptureDisabled);
        }
        if !runtime.available || runtime.active.is_none() {
            blocks.push(PublicationBlock::SourceUnavailable);
        }
        if self.room_send_muted() {
            blocks.push(PublicationBlock::RoomSendMuted);
        }
        if self.local_emergency_stop() {
            blocks.push(PublicationBlock::LocalEmergencyStop);
        }
        blocks
    }

    #[allow(clippy::too_many_arguments)]
    fn member_result_record(
        &self,
        config: &MemberConfig,
        catalog_revision: CatalogRevision,
        capability: rlb_model::Capability,
        status: MemberCommandStatus,
        code: Option<ErrorCode>,
        effective: MemberCommandEffective,
        warnings: Vec<WarningCode>,
    ) -> MemberCommandResultRecord {
        MemberCommandResultRecord {
            capability,
            status,
            revisions: self.canonical_revisions_for(config, catalog_revision),
            code,
            effective,
            warnings,
        }
    }

    fn member_command_result(
        &self,
        delivery: &MemberCommandDelivery,
        result: &MemberCommandResultRecord,
    ) -> ClientMessage {
        ClientMessage::MemberCommandResult {
            v: rlb_control::Version2,
            request_id: delivery.command.request_id,
            registry_session_id: delivery.registry_session_id,
            capability: result.capability,
            status: result.status,
            revisions: result.revisions,
            code: result.code,
            effective: result.effective.clone(),
            warnings: result.warnings.clone(),
        }
    }

    fn reject_member_command(
        &mut self,
        delivery: &MemberCommandDelivery,
        code: ErrorCode,
        effects: &mut Vec<CoordinatorEffect>,
    ) {
        if self.registry_state == RegistryState::Online
            && self.registry_session_id == Some(delivery.registry_session_id)
        {
            let result = self.member_result_record(
                &self.config,
                self.catalog_revision,
                delivery.command.capability(),
                MemberCommandStatus::Rejected,
                Some(code),
                MemberCommandEffective::default(),
                Vec::new(),
            );
            effects.push(CoordinatorEffect::SendRegistryMessage(
                self.member_command_result(delivery, &result),
            ));
        } else {
            self.diagnostics.commands_dropped_without_session = self
                .diagnostics
                .commands_dropped_without_session
                .saturating_add(1);
        }
    }

    fn local_outcome(
        &self,
        request_id: rlb_model::RequestId,
        status: CommandStatus,
        code: Option<ErrorCode>,
    ) -> LocalConfigOutcome {
        LocalConfigOutcome {
            request_id,
            status,
            code,
            revisions: self.canonical_revisions(),
        }
    }

    fn canonical_revisions(&self) -> CanonicalRevisions {
        self.canonical_revisions_for(&self.config, self.catalog_revision)
    }

    fn canonical_revisions_for(
        &self,
        config: &MemberConfig,
        catalog_revision: CatalogRevision,
    ) -> CanonicalRevisions {
        CanonicalRevisions {
            room_revision: self
                .authority
                .as_ref()
                .map(|authority| authority.room_revision),
            presence_revision: self
                .authority
                .as_ref()
                .map(|authority| authority.presence_revision),
            catalog_revision: Some(catalog_revision),
            member_config_revision: Some(config.member_config_revision),
            grant_revision: Some(config.room.grant_revision),
            subscription_revision: None,
        }
    }

    fn catalog(&self) -> Result<Vec<SourceDescriptor>, CoordinatorError> {
        let mut output = Vec::with_capacity(2);
        for source in &self.config.sources {
            let runtime = self
                .sources
                .get(&source.source_id)
                .ok_or_else(|| CoordinatorError::new(ErrorCode::InvalidCatalog))?;
            let effective_profile = self.effective_profile(source.source_id);
            let send_ready = runtime.available
                && source.config.master_capture_enabled
                && source.config.publish_requested
                && !self.local_emergency_stop()
                && !self.room_send_muted();
            let state = SourceState::new(
                runtime.available,
                source.config.master_capture_enabled,
                source.config.publish_requested,
                source.config.selected_profile,
                effective_profile,
                send_ready,
            )
            .map_err(|_| CoordinatorError::new(ErrorCode::InvalidCatalog))?;
            let active = runtime
                .active
                .as_ref()
                .map(|stream| (stream.format, stream.stream_epoch));
            let name = match source.kind {
                SourceKind::SystemAudio => "System audio",
                SourceKind::Microphone => "Microphone",
            };
            output.push(
                SourceDescriptor::new(source.source_id, source.kind, name, state, active)
                    .map_err(|_| CoordinatorError::new(ErrorCode::InvalidCatalog))?,
            );
        }
        output.sort_by_key(SourceDescriptor::source_id);
        Ok(output)
    }

    fn publish_catalog(
        &self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.registry_state == RegistryState::Online {
            effects.push(CoordinatorEffect::PublishRegistryProjection(
                RegistryProjectionUpdate::Catalog {
                    catalog_revision: self.catalog_revision,
                    sources: self.catalog()?,
                },
            ));
        }
        Ok(())
    }

    fn publish_all_registry_projections(
        &self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        if self.registry_state != RegistryState::Online {
            return Ok(());
        }
        self.publish_catalog(effects)?;
        effects.push(CoordinatorEffect::PublishRegistryProjection(
            RegistryProjectionUpdate::Grants {
                grant_revision: self.config.room.grant_revision,
                grants: self.config.room.settings.grants,
            },
        ));
        effects.push(CoordinatorEffect::PublishRegistryProjection(
            RegistryProjectionUpdate::MemberConfigRevision {
                member_config_revision: self.config.member_config_revision,
            },
        ));
        Ok(())
    }

    fn bump_catalog(&mut self) -> Result<(), CoordinatorError> {
        self.catalog_revision = self
            .catalog_revision
            .checked_next()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::UnexpectedError))?;
        Ok(())
    }

    fn refresh_plans(
        &mut self,
        effects: &mut Vec<CoordinatorEffect>,
    ) -> Result<(), CoordinatorError> {
        let send_plan = self.build_send_plan();
        if self.last_send_plan.as_ref() != Some(&send_plan) {
            self.last_send_plan = Some(send_plan.clone());
            effects.push(CoordinatorEffect::ApplySendPlan(send_plan));
        }

        let audio_sources = self.build_audio_sources();
        if self.last_audio_sources != audio_sources {
            self.audio_mix_generation = self.audio_mix_generation.saturating_add(1);
            self.last_audio_sources.clone_from(&audio_sources);
            effects.push(CoordinatorEffect::ApplyAudioMix(AudioMixPlan {
                generation: self.audio_mix_generation,
                sources: audio_sources,
            }));
        }
        Ok(())
    }

    fn build_send_plan(&self) -> SendPlan {
        let eligibility = self.effective_send_eligibility();
        let mut sources = Vec::new();
        for source in &self.config.sources {
            let Some(stream_epoch) = self
                .sources
                .get(&source.source_id)
                .and_then(|runtime| runtime.active.as_ref())
                .map(|stream| stream.stream_epoch)
            else {
                continue;
            };
            let mut recipients = Vec::new();
            for edge in eligibility
                .iter()
                .filter(|edge| edge.source_id == source.source_id && edge.media_emitter_eligible)
            {
                let peer = self
                    .peers
                    .get(&edge.recipient_member_id)
                    .expect("eligibility peer exists");
                let (run_id, endpoint) = peer
                    .transport()
                    .expect("emitter-eligible peer has transport");
                recipients.push(SendRecipient {
                    member_id: edge.recipient_member_id,
                    run_id,
                    endpoint,
                });
            }
            recipients.sort_by_key(|recipient| recipient.member_id);
            sources.push(SourceSendPlan {
                source_id: source.source_id,
                stream_epoch,
                recipients,
            });
        }
        sources.sort_by_key(|source| source.source_id);
        SendPlan {
            catalog_revision: self.catalog_revision,
            sources,
        }
    }

    fn build_audio_sources(&self) -> Vec<SourceRenderConfig> {
        let mut output = Vec::new();
        for peer in self.peers.values() {
            for descriptor in &peer.member.sources {
                let Some((format, _)) = descriptor.active() else {
                    continue;
                };
                if !peer.remote_subscription_accepted(descriptor.source_id()) {
                    continue;
                }
                output.push(SourceRenderConfig {
                    member_id: peer.member.member_id,
                    source_id: descriptor.source_id(),
                    channels: format.channels(),
                    routed: true,
                    member_solo_eligible: matches!(
                        peer.connection,
                        PeerConnectionState::Ready { .. }
                    ),
                    source_mix: self
                        .config
                        .source_mix(peer.member.member_id, descriptor.source_id()),
                    member_mix: self.config.member_mix(peer.member.member_id),
                });
            }
        }
        output.sort_by_key(|source| (source.member_id, source.source_id));
        output
    }

    fn eligibility(
        &self,
        source_id: SourceId,
        recipient_member_id: MemberId,
    ) -> RecipientEligibility {
        let mut blocking_reasons = Vec::new();
        let source = self
            .config
            .source(source_id)
            .expect("fixed source requested");
        let runtime = self
            .sources
            .get(&source_id)
            .expect("fixed source runtime requested");
        let peer = self
            .peers
            .get(&recipient_member_id)
            .expect("known peer requested");
        if !runtime.available {
            blocking_reasons.push(EffectiveSendBlock::SourceUnavailable);
        }
        if runtime.active.is_none() {
            blocking_reasons.push(EffectiveSendBlock::StreamInactive);
        }
        if !source.config.master_capture_enabled {
            blocking_reasons.push(EffectiveSendBlock::MasterCaptureDisabled);
        }
        if !source.config.publish_requested {
            blocking_reasons.push(EffectiveSendBlock::PublishNotRequested);
        }
        if !self.local_approved() {
            blocking_reasons.push(EffectiveSendBlock::PublisherNotApproved);
        }
        if !self.recipient_approved(recipient_member_id) {
            blocking_reasons.push(EffectiveSendBlock::RecipientNotApproved);
        }
        if !self.visible_to(source_id, recipient_member_id) {
            blocking_reasons.push(EffectiveSendBlock::SourceHidden);
        }
        let subscription_accepted = peer.inbound_subscription.is_some_and(|subscription| {
            subscription.ack.catalog_revision == self.catalog_revision
                && subscription.ack.accepted_source_ids.contains(source_id)
        }) && !peer.inbound_invalidated.contains(&source_id);
        if !subscription_accepted {
            blocking_reasons.push(EffectiveSendBlock::SubscriptionNotAccepted);
        }
        if self.local_emergency_stop() {
            blocking_reasons.push(EffectiveSendBlock::LocalEmergencyStop);
        }
        if self.room_send_muted() {
            blocking_reasons.push(EffectiveSendBlock::RoomSendMuted);
        }
        let product_eligible = blocking_reasons.is_empty();
        if !matches!(peer.connection, PeerConnectionState::Ready { .. }) {
            blocking_reasons.push(EffectiveSendBlock::DirectSessionUnavailable);
        }
        RecipientEligibility {
            source_id,
            recipient_member_id,
            product_eligible,
            media_emitter_eligible: blocking_reasons.is_empty(),
            blocking_reasons,
        }
    }

    fn local_emergency_stop(&self) -> bool {
        self.config.local_emergency_stop || self.volatile_emergency_stop
    }

    fn room_send_muted(&self) -> bool {
        self.authority
            .as_ref()
            .is_some_and(|authority| authority.room.room_send_muted)
    }

    fn effective_profile(&self, source_id: SourceId) -> QualityProfile {
        let selected = self
            .config
            .source(source_id)
            .map_or(QualityProfile::PcmF32V1, |source| {
                source.config.selected_profile
            });
        self.authority.as_ref().map_or(selected, |authority| {
            selected.capped_by(authority.room.quality_cap)
        })
    }

    fn local_approved(&self) -> bool {
        self.authority
            .as_ref()
            .is_some_and(|authority| authority.self_admission == AdmissionState::Approved)
    }

    fn recipient_approved(&self, member_id: MemberId) -> bool {
        self.peers
            .get(&member_id)
            .is_some_and(|peer| peer.member.admission == AdmissionState::Approved)
    }

    fn visible_to(&self, source_id: SourceId, member_id: MemberId) -> bool {
        self.authority
            .as_ref()
            .and_then(|authority| authority.local_visibility.get(&source_id))
            .is_some_and(|viewers| viewers.contains(&member_id))
    }

    fn ensure_running(&self) -> Result<(), CoordinatorError> {
        if self.engine_state == EngineState::Running {
            Ok(())
        } else {
            Err(CoordinatorError::new(ErrorCode::InvalidStateTransition))
        }
    }

    fn require_room(&self) -> Result<rlb_model::RoomId, CoordinatorError> {
        self.config
            .room
            .room_id
            .ok_or_else(|| CoordinatorError::new(ErrorCode::RoomScopeMismatch))
    }

    fn ensure_room(&self, room_id: rlb_model::RoomId) -> Result<(), CoordinatorError> {
        if self.config.room.room_id == Some(room_id) {
            Ok(())
        } else {
            Err(CoordinatorError::new(ErrorCode::RoomScopeMismatch))
        }
    }

    fn ensure_remote_member(&self, member_id: MemberId) -> Result<(), CoordinatorError> {
        if self
            .authority
            .as_ref()
            .and_then(|authority| authority.members.get(&member_id))
            .is_some_and(|member| {
                member_id != self.identity.member_id()
                    && member.admission == AdmissionState::Approved
            })
        {
            Ok(())
        } else {
            Err(CoordinatorError::new(ErrorCode::TargetNotVisible))
        }
    }

    fn ensure_remote_source(
        &self,
        member_id: MemberId,
        source_id: SourceId,
    ) -> Result<(), CoordinatorError> {
        self.ensure_remote_member(member_id)?;
        if self
            .authority
            .as_ref()
            .and_then(|authority| authority.members.get(&member_id))
            .is_some_and(|member| {
                member
                    .sources
                    .iter()
                    .any(|source| source.source_id() == source_id)
            })
        {
            Ok(())
        } else {
            Err(CoordinatorError::new(ErrorCode::TargetNotVisible))
        }
    }

    fn authority_ref(&self) -> Result<&AuthorityState, CoordinatorError> {
        self.authority
            .as_ref()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::RoomServiceUnreachable))
    }

    fn authority_mut(&mut self) -> Result<&mut AuthorityState, CoordinatorError> {
        self.authority
            .as_mut()
            .ok_or_else(|| CoordinatorError::new(ErrorCode::RoomServiceUnreachable))
    }

    fn request_snapshot(&mut self, reason: SnapshotReason, effects: &mut Vec<CoordinatorEffect>) {
        self.registry_state = RegistryState::AwaitingSnapshot;
        effects.push(CoordinatorEffect::RequestFullRoomSnapshot { reason });
    }
}

/// Stable reducer rejection. Invalid events leave the complete coordinator
/// state unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoordinatorError {
    pub code: ErrorCode,
}

impl CoordinatorError {
    const fn new(code: ErrorCode) -> Self {
        Self { code }
    }
}

impl From<MemberConfigError> for CoordinatorError {
    fn from(_: MemberConfigError) -> Self {
        Self::new(ErrorCode::InvalidValue)
    }
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "member session event rejected with {:?}",
            self.code
        )
    }
}

impl std::error::Error for CoordinatorError {}
