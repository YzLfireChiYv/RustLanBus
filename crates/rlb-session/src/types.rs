use rlb_audio::{AudioMetricsSnapshot, SourceRenderConfig};
use rlb_control::{
    CanonicalRevisions, CommandMessageType, CommandStatus, RoomCommandEnvelope,
    ValidatedDelegatedCommand, ValidatedRoomCommand, Version2,
};
use rlb_media::{
    AssemblerMetrics, DirectDatagram, EpochEndReason, SenderMetrics, SubscribeAck, SubscribeSet,
};
use rlb_model::{
    AdmissionState, Capability, CaptureMethod, CatalogRevision, DirectSessionId, Endpoint,
    ErrorCode, GrantRevision, IdentityRecord, MemberConfigRevision, MemberId, MemberMix,
    PresenceRevision, QualityProfile, ReceiveBufferConfig, RegistrySessionId, RequestId, RoomId,
    RoomRevision, RunId, ServerGeneration, SourceDescriptor, SourceFormat, SourceId, SourceMix,
    StreamEpoch, SubscriptionRevision,
};
use rlb_registry::{ClientMessage, RoomEvent, RoomSnapshot, ServerMessage, SnapshotReason};

use crate::MemberConfig;
use crate::config::{
    DeviceBindingConfig, JoinTarget, LoggingConfig, SourceMonitorConfig, StartupConfig,
};

/// Complete authority-owned viewer set for one local source.
///
/// The current registry room snapshot is permission-filtered for incoming
/// sources and does not carry the publisher's own viewer sets. The registry
/// client supplies these two complete sets alongside the snapshot so the member
/// reducer can enforce publisher-side visibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSourceVisibility {
    pub source_id: SourceId,
    pub viewer_member_ids: Vec<MemberId>,
}

/// Full room state required for one atomic member-side authority replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthoritySnapshot {
    pub room: RoomSnapshot,
    pub local_source_visibility: Vec<LocalSourceVisibility>,
}

/// Registry and direct-subscription state consumed by media ingress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaSourceProjection {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub source_id: SourceId,
    pub active_epoch: StreamEpoch,
    pub catalog_revision: CatalogRevision,
    pub media_endpoint: Endpoint,
    pub format: SourceFormat,
    pub subscription_revision: Option<SubscriptionRevision>,
    pub subscription_accepted: bool,
}

/// Complete room-scoped receive projection for one coordinator state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaReceivePlan {
    pub room_id: Option<RoomId>,
    pub sources: Vec<MediaSourceProjection>,
}

impl AuthoritySnapshot {
    /// Checks local scope, roster uniqueness, and the two complete visibility
    /// sets before any live state is replaced.
    pub fn validate(&self, identity: IdentityRecord) -> Result<(), SnapshotError> {
        if self.room.self_view.member_id != identity.member_id() {
            return Err(SnapshotError::MemberScope);
        }
        if self.room.self_view.admission != AdmissionState::Approved {
            return Err(SnapshotError::Admission);
        }
        if self.room.members.len() > 32 {
            return Err(SnapshotError::RosterBounds);
        }

        let mut members: Vec<_> = self
            .room
            .members
            .iter()
            .map(|member| member.member_id)
            .collect();
        members.sort_unstable();
        if members.windows(2).any(|pair| pair[0] == pair[1])
            || !members.contains(&identity.member_id())
        {
            return Err(SnapshotError::RosterIdentity);
        }
        for member in &self.room.members {
            if member.admission != AdmissionState::Approved || member.sources.len() > 2 {
                return Err(SnapshotError::RosterEntry);
            }
            let mut source_ids: Vec<_> = member
                .sources
                .iter()
                .map(SourceDescriptor::source_id)
                .collect();
            source_ids.sort_unstable();
            if source_ids.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(SnapshotError::RosterEntry);
            }
        }

        if self.local_source_visibility.len() != 2 {
            return Err(SnapshotError::VisibilityCoverage);
        }
        let mut visibility_sources: Vec<_> = self
            .local_source_visibility
            .iter()
            .map(|entry| entry.source_id)
            .collect();
        visibility_sources.sort_unstable();
        let mut expected_sources = [
            identity.source_id(rlb_model::SourceKind::SystemAudio),
            identity.source_id(rlb_model::SourceKind::Microphone),
        ];
        expected_sources.sort_unstable();
        if visibility_sources != expected_sources {
            return Err(SnapshotError::VisibilityCoverage);
        }
        for visibility in &self.local_source_visibility {
            let mut viewers = visibility.viewer_member_ids.clone();
            viewers.sort_unstable();
            if viewers.windows(2).any(|pair| pair[0] == pair[1])
                || viewers
                    .iter()
                    .any(|viewer| *viewer == identity.member_id() || !members.contains(viewer))
            {
                return Err(SnapshotError::VisibilityViewer);
            }
        }
        Ok(())
    }
}

/// Invalid complete member-side authority replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    MemberScope,
    Admission,
    RosterBounds,
    RosterIdentity,
    RosterEntry,
    VisibilityCoverage,
    VisibilityViewer,
}

/// Member engine lifecycle owned by the coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineState {
    Starting,
    Running,
    ShuttingDown,
    Stopped,
}

/// Ordered shutdown operations acknowledged back into the reducer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownStage {
    CloseSendGate,
    LeaveRegistry,
    StopCapture,
    StopMedia,
    StopRender,
    ReleaseAudio,
    FlushPersistence,
    RemoveRuntimeState,
}

impl ShutdownStage {
    pub(crate) const fn next(self) -> Option<Self> {
        match self {
            Self::CloseSendGate => Some(Self::LeaveRegistry),
            Self::LeaveRegistry => Some(Self::StopCapture),
            Self::StopCapture => Some(Self::StopMedia),
            Self::StopMedia => Some(Self::StopRender),
            Self::StopRender => Some(Self::ReleaseAudio),
            Self::ReleaseAudio => Some(Self::FlushPersistence),
            Self::FlushPersistence => Some(Self::RemoveRuntimeState),
            Self::RemoveRuntimeState => None,
        }
    }
}

/// Registry control state. Existing direct peers can remain active in
/// [`RegistryState::Outage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryState {
    Disconnected,
    Connecting,
    AwaitingSnapshot,
    Online,
    Outage,
    Left,
}

/// Direct-media lifecycle for one stable remote member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerConnectionState {
    Disconnected,
    Connecting,
    Ready { direct_session_id: DirectSessionId },
}

/// Local source capture lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureState {
    Disabled,
    Starting,
    Running,
    Unavailable,
    Faulted,
}

/// The only reasons which can introduce a new source epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochChangeReason {
    EngineRun,
    EndpointChanged,
    CaptureMethodChanged,
    FormatChanged,
    TimelineDiscontinuity,
}

/// Why an active source stream ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceStopReason {
    CaptureDisabled,
    SourceUnavailable,
    Reconfigure(EpochChangeReason),
    EngineShutdown,
}

/// A supervisor-created source stream offered to the reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceStreamStarted {
    pub source_id: SourceId,
    pub stream_epoch: StreamEpoch,
    pub endpoint_id: String,
    pub capture_method: Option<CaptureMethod>,
    pub format: SourceFormat,
    pub reason: EpochChangeReason,
}

/// Low-rate receive health for one independently isolated remote source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteMediaState {
    Waiting,
    Preroll,
    Playing,
    Stalled,
    Faulted,
}

/// Media-worker counters accepted by the coordinator diagnostics projection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RemoteMediaMetrics {
    pub assembler: AssemblerMetrics,
    pub last_media_at_ms: Option<u64>,
    pub concealed_frames_total: u64,
}

/// Stable reason why one local source cannot currently send to one recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveSendBlock {
    SourceUnavailable,
    StreamInactive,
    MasterCaptureDisabled,
    PublishNotRequested,
    PublisherNotApproved,
    RecipientNotApproved,
    SourceHidden,
    SubscriptionNotAccepted,
    LocalEmergencyStop,
    RoomSendMuted,
    DirectSessionUnavailable,
}

/// Full effective publication decision for one `(source, recipient)` edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecipientEligibility {
    pub source_id: SourceId,
    pub recipient_member_id: MemberId,
    pub product_eligible: bool,
    pub media_emitter_eligible: bool,
    pub blocking_reasons: Vec<EffectiveSendBlock>,
}

/// One current UDP media target in a source send plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendRecipient {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub endpoint: Endpoint,
}

/// Encode-once fanout targets for one local source and epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSendPlan {
    pub source_id: SourceId,
    pub stream_epoch: StreamEpoch,
    pub recipients: Vec<SendRecipient>,
}

/// Complete current media-emitter plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendPlan {
    pub catalog_revision: CatalogRevision,
    pub sources: Vec<SourceSendPlan>,
}

/// Complete prepared mix controls expressed with the `rlb-audio` contract.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioMixPlan {
    pub generation: u64,
    pub sources: Vec<SourceRenderConfig>,
}

/// Source supervisor request. The supervisor returns a
/// [`CoordinatorEvent::SourceStreamStarted`] carrying the fresh epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceRestartPlan {
    pub source_id: SourceId,
    pub reason: EpochChangeReason,
}

/// Fresh epoch configuration installed in the source media worker after the
/// supervisor confirms a stream start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceEpochStartPlan {
    pub source_id: SourceId,
    pub stream_epoch: StreamEpoch,
    pub format: SourceFormat,
}

/// Exact active epoch which a source media worker must terminate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceEpochEndPlan {
    pub source_id: SourceId,
    pub stream_epoch: StreamEpoch,
    pub reason: EpochEndReason,
}

/// Direct peer construction request for the engine media owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerConnectPlan {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub endpoint: Endpoint,
    pub server_generation: ServerGeneration,
}

/// Member-owned projection that must be sent on the current registry session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryProjectionUpdate {
    Catalog {
        catalog_revision: CatalogRevision,
        sources: Vec<SourceDescriptor>,
    },
    Grants {
        grant_revision: GrantRevision,
        grants: rlb_model::CapabilityGrants,
    },
    MemberConfigRevision {
        member_config_revision: MemberConfigRevision,
    },
}

/// Typed local member mutation understood by the pure reducer.
#[derive(Clone, Debug, PartialEq)]
pub enum LocalConfigMutation {
    SetMasterCapture {
        source_id: SourceId,
        enabled: bool,
    },
    SetSourcePublication {
        room_id: RoomId,
        source_id: SourceId,
        publish_requested: bool,
    },
    SetSendQuality {
        room_id: RoomId,
        source_id: SourceId,
        selected_profile: QualityProfile,
    },
    SetCaptureMethod {
        source_id: SourceId,
        method: CaptureMethod,
    },
    SetSourceBinding {
        source_id: SourceId,
        capture_method: CaptureMethod,
        binding: DeviceBindingConfig,
    },
    SetPlaybackBinding {
        binding: DeviceBindingConfig,
    },
    SetReceiveBuffer {
        room_id: RoomId,
        buffer: ReceiveBufferConfig,
    },
    SetEmergencyStop {
        enabled: bool,
    },
    SetCapabilityGrant {
        room_id: RoomId,
        capability: Capability,
        enabled: bool,
    },
    SetMemberMix {
        room_id: RoomId,
        remote_member_id: MemberId,
        mix: MemberMix,
    },
    SetSourceMix {
        room_id: RoomId,
        remote_member_id: MemberId,
        source_id: SourceId,
        mix: SourceMix,
    },
    SetAutoReconnect {
        room_id: RoomId,
        enabled: bool,
    },
    SetJoinTarget {
        room_id: RoomId,
        target: JoinTarget,
    },
    ClearJoin,
    SetSourceMonitor {
        source_id: SourceId,
        monitor: SourceMonitorConfig,
    },
    SetStartup {
        startup: StartupConfig,
    },
    SetLogging {
        logging: LoggingConfig,
    },
    SetDriverSelection {
        package_id: Option<String>,
        enabled: bool,
    },
}

/// Compare-and-commit request originating from local control A.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalConfigRequest {
    pub request_id: RequestId,
    pub expected_member_config_revision: MemberConfigRevision,
    pub expected_grant_revision: Option<GrantRevision>,
    pub mutation: LocalConfigMutation,
}

/// Terminal local control projection emitted after reduction or persistence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalConfigOutcome {
    pub request_id: RequestId,
    pub status: CommandStatus,
    pub code: Option<ErrorCode>,
    pub revisions: CanonicalRevisions,
}

/// Opaque in-process persistence transaction identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConfigTransactionId(pub u64);

/// Persistence adapter completion reported back into the reducer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceResult {
    Committed,
    Failed,
}

/// A validated, deadline-bound command delivered on one exact registry
/// session.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberCommandDelivery {
    pub registry_session_id: RegistrySessionId,
    pub command: ValidatedDelegatedCommand,
    pub received_at_ms: u64,
    pub expires_at_ms: u64,
}

impl MemberCommandDelivery {
    /// Converts the current RLBREG2 wire union through the closed
    /// `rlb-control` command validator.
    pub fn from_server_message(
        message: ServerMessage,
        received_at_ms: u64,
    ) -> Result<Self, MemberCommandDeliveryError> {
        message
            .validate()
            .map_err(|_| MemberCommandDeliveryError::Invalid)?;
        let ServerMessage::MemberCommand {
            request_id,
            registry_session_id,
            capability,
            command,
            scope,
            expected,
            args,
            delivery_ttl_ms,
            ..
        } = message
        else {
            return Err(MemberCommandDeliveryError::WrongMessage);
        };
        let envelope = RoomCommandEnvelope {
            v: Version2,
            message_type: CommandMessageType::Command,
            request_id,
            command,
            scope,
            expected,
            args,
        };
        let ValidatedRoomCommand::Delegated(command) = envelope
            .validate()
            .map_err(|_| MemberCommandDeliveryError::Invalid)?
        else {
            return Err(MemberCommandDeliveryError::Invalid);
        };
        if command.capability() != capability {
            return Err(MemberCommandDeliveryError::Capability);
        }
        Ok(Self {
            registry_session_id,
            command,
            received_at_ms,
            expires_at_ms: received_at_ms.saturating_add(u64::from(delivery_ttl_ms)),
        })
    }
}

/// Wire-to-reducer delivery rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberCommandDeliveryError {
    WrongMessage,
    Invalid,
    Capability,
}

impl MemberCommandDeliveryError {
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::WrongMessage | Self::Invalid => ErrorCode::InvalidValue,
            Self::Capability => ErrorCode::CommandScopeMismatch,
        }
    }
}

/// Every input capable of changing member-session effective state.
#[derive(Clone, Debug)]
pub enum CoordinatorEvent {
    EngineStarted,
    RegistryConnectRequested,
    RegistrySessionReady {
        registry_session_id: RegistrySessionId,
    },
    AuthoritySnapshotReceived(AuthoritySnapshot),
    AuthorityEventReceived(RoomEvent),
    RegistryDisconnected,
    ReconnectTimerElapsed,
    DirectSessionReady {
        member_id: MemberId,
        run_id: RunId,
        direct_session_id: DirectSessionId,
    },
    DirectSessionLost {
        member_id: MemberId,
        direct_session_id: DirectSessionId,
    },
    SubscribeSetReceived(SubscribeSet),
    SubscribeAckReceived(SubscribeAck),
    SourceAvailabilityChanged {
        source_id: SourceId,
        available: bool,
    },
    SourceStreamStarted(SourceStreamStarted),
    SourceStreamStopped {
        source_id: SourceId,
        reason: SourceStopReason,
    },
    LocalConfigRequested(LocalConfigRequest),
    MemberCommandReceived {
        delivery: MemberCommandDelivery,
        now_ms: u64,
    },
    ConfigPersistenceCompleted {
        transaction_id: ConfigTransactionId,
        result: PersistenceResult,
    },
    RemoteMediaHealth {
        member_id: MemberId,
        source_id: SourceId,
        state: RemoteMediaState,
        metrics: RemoteMediaMetrics,
    },
    LocalSenderMetrics {
        source_id: SourceId,
        metrics: SenderMetrics,
    },
    AudioMetricsUpdated(AudioMetricsSnapshot),
    ShutdownRequested,
    ShutdownStageCompleted(ShutdownStage),
}

/// Side effects executed by `apps/engine` owners and acknowledged through
/// typed events where completion changes state.
#[derive(Debug)]
pub enum CoordinatorEffect {
    ConnectRegistry,
    RequestFullRoomSnapshot {
        reason: SnapshotReason,
    },
    ScheduleRegistryReconnect {
        base_delay_ms: u32,
    },
    PersistMemberConfig {
        transaction_id: ConfigTransactionId,
        candidate: MemberConfig,
    },
    LocalConfigCompleted(LocalConfigOutcome),
    SendRegistryMessage(ClientMessage),
    PublishRegistryProjection(RegistryProjectionUpdate),
    ConnectPeer(PeerConnectPlan),
    DisconnectPeer {
        member_id: MemberId,
    },
    LeaveRegistry,
    SendDirect(DirectDatagram),
    ApplySendPlan(SendPlan),
    ApplyAudioMix(AudioMixPlan),
    StartOrRestartSource(SourceRestartPlan),
    StartSourceEpoch(SourceEpochStartPlan),
    EndSourceEpoch(SourceEpochEndPlan),
    StopSource {
        source_id: SourceId,
        reason: SourceStopReason,
    },
    SetEmergencySendGate {
        stopped: bool,
    },
    RunShutdownStage(ShutdownStage),
}

/// Diagnostics for one remote source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteSourceDiagnostics {
    pub descriptor: SourceDescriptor,
    pub subscription_accepted: bool,
    pub media_state: RemoteMediaState,
    pub metrics: RemoteMediaMetrics,
}

/// Diagnostics for one stable remote member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerDiagnostics {
    pub member_id: MemberId,
    pub run_id: Option<RunId>,
    pub endpoint: Option<Endpoint>,
    pub connection: PeerConnectionState,
    pub inbound_subscription_revision: Option<SubscriptionRevision>,
    pub sources: Vec<RemoteSourceDiagnostics>,
}

/// Diagnostics for one local source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSourceDiagnostics {
    pub source_id: SourceId,
    pub capture_state: CaptureState,
    pub available: bool,
    pub master_capture_enabled: bool,
    pub publish_requested: bool,
    pub selected_profile: QualityProfile,
    pub effective_profile: QualityProfile,
    pub stream_epoch: Option<StreamEpoch>,
    pub epoch_changes_total: u64,
    pub effective_recipients: Vec<MemberId>,
    pub sender_metrics: SenderMetrics,
}

/// Immutable low-rate coordinator diagnostics projection.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticsSnapshot {
    pub engine_state: EngineState,
    pub registry_state: RegistryState,
    pub server_generation: Option<ServerGeneration>,
    pub room_revision: Option<RoomRevision>,
    pub presence_revision: Option<PresenceRevision>,
    pub member_config_revision: MemberConfigRevision,
    pub catalog_revision: CatalogRevision,
    pub grant_revision: GrantRevision,
    pub local_emergency_stop: bool,
    pub room_send_muted: bool,
    pub registry_outages_total: u64,
    pub registry_reconnects_total: u64,
    pub rejected_events_total: u64,
    pub persistence_failures_total: u64,
    pub commands_dropped_without_session_total: u64,
    pub subscription_rejections_total: u64,
    pub peers: Vec<PeerDiagnostics>,
    pub sources: Vec<LocalSourceDiagnostics>,
    pub audio: AudioMetricsSnapshot,
}
