use serde::{Deserialize, Serialize};

use crate::{Capability, DiagnosticId, MemberId, RoomId, RunId, SourceId};

/// Stable machine-readable V2 error code.
///
/// UI text is localized separately. Protocol handlers match this closed enum and
/// never infer behavior from human-readable messages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolVersionUnsupported,
    ProtocolVersion,
    PrefaceInvalid,
    MalformedMessage,
    MalformedJson,
    InvalidUtf8,
    FrameTooLarge,
    MessageTooLarge,
    JsonDepthExceeded,
    DuplicateJsonKey,
    UnsupportedBinaryMessage,
    UnknownType,
    UnknownCommand,
    UnknownField,
    InvalidValue,
    RequestIdConflict,
    RequestLimit,
    RateLimited,
    SlowConsumer,
    ServerBusy,
    UnexpectedError,
    IpcAuthFailed,
    StaleEngineRun,
    EngineControlUnreachable,
    RevisionConflict,
    ObjectNotFound,
    ConfirmationRequired,
    ConfirmationExpired,
    ConfirmationMismatch,
    OperationInProgress,
    OperationNotSupported,
    ConfigPersistFailed,
    DeviceUnavailable,
    EndpointConflict,
    TestDriverUnavailable,
    LocalLogWriteFailed,
    OperatorAuthRequired,
    OperatorCsrfFailed,
    OriginDenied,
    MemberTokenInvalid,
    MemberTokenRevoked,
    ClaimInvalid,
    ClaimExpired,
    AdmissionPending,
    AdmissionDenied,
    MemberRemoved,
    RoomFull,
    PendingCapacity,
    RemovedCapacity,
    RoomScopeMismatch,
    MemberScopeMismatch,
    MemberNotFound,
    MemberOffline,
    MemberDisconnected,
    CommandOutcomeUnknown,
    SourceNotFound,
    TargetNotVisible,
    DuplicateIdentity,
    UnknownSession,
    StaleGeneration,
    LeaseExpired,
    Superseded,
    InvalidStateTransition,
    CapabilityDenied,
    CapabilityRevoked,
    FieldNotAllowed,
    StaleRoomRevision,
    StaleMemberConfigRevision,
    StaleGrantRevision,
    QualityCapped,
    QualityPolicyOutOfSync,
    MediaBindTimeout,
    MediaBindInvalid,
    MediaBindNonceExpired,
    EndpointFamilyUnsupported,
    StaleCatalogRevision,
    InvalidCatalog,
    TooManySources,
    SourceIdentityChanged,
    RunMismatch,
    MemberCommandTimeout,
    MemberCommandRejected,
    MemberConfigPersistFailed,
    CommandBoundaryViolation,
    CommandScopeMismatch,
    PersistenceFailed,
    RegistryStateCorrupt,
    RoomServiceUnreachable,
    SourceUnavailable,
    RoomSendMuted,
    LocalEmergencyStop,
    PeerMediaUnreachable,
    MediaPathMtuUnsupported,
    MediaProtocolVersion,
    MediaPacketMalformed,
    MediaIdentityMismatch,
    MediaStaleRun,
    MediaSubscriptionDenied,
    MediaEpochUnannounced,
    MediaEpochConflict,
    MediaTimelineConflict,
    MediaReceiveCapacity,
    MediaSourceStalled,
    MediaReprimeActive,
    MediaPlaybackResync,
    MediaClockDriftOutOfRange,
    MediaRenderXrun,
    MediaNonFiniteAudio,
    MediaCallbackContractViolation,
}

/// Typed object associated with a structured error.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ErrorEntity {
    /// A protocol frame with no accepted authority scope.
    Protocol,
    /// The local engine process.
    Engine {
        /// Stable local member identity.
        member_id: MemberId,
        /// Current process run identity.
        run_id: RunId,
    },
    /// Authority-owned room state.
    Room {
        /// Stable room identity.
        room_id: RoomId,
    },
    /// A member in one room.
    Member {
        /// Stable room identity.
        room_id: RoomId,
        /// Stable member identity.
        member_id: MemberId,
    },
    /// A stable source slot in one room.
    Source {
        /// Stable room identity.
        room_id: RoomId,
        /// Stable owner member identity.
        member_id: MemberId,
        /// Stable source identity.
        source_id: SourceId,
    },
    /// A local audio device or binding.
    Device {
        /// Source affected by the device when source-scoped.
        source_id: Option<SourceId>,
    },
    /// One delegated room capability.
    Permission {
        /// Stable room identity.
        room_id: RoomId,
        /// Target member identity.
        member_id: MemberId,
        /// Capability whose gate rejected the operation.
        capability: Capability,
    },
    /// Member-local logging.
    Logging {
        /// Stable local member identity.
        member_id: MemberId,
    },
}

/// Current user-visible effect of an error.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorImpact {
    /// No canonical state changed.
    NoStateChange,
    /// Room or local control is unavailable.
    ControlUnavailable,
    /// Sending stopped for the scoped object.
    SendingStopped,
    /// Playback stopped for the scoped object.
    PlaybackStopped,
    /// One source is degraded while unrelated sources continue.
    SourceDegraded,
    /// One malformed or unauthorized datagram was discarded.
    DatagramDropped,
    /// Audio continues and the condition is diagnostic.
    DiagnosticOnly,
}

/// Stable error data retained in snapshots until recovery or acknowledgement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredError {
    /// Machine-readable error code.
    pub code: ErrorCode,
    /// Typed affected object.
    pub entity: ErrorEntity,
    /// Current effect on operation.
    pub impact: ErrorImpact,
    /// Whether retrying after refreshing state can be useful.
    pub retryable: bool,
    /// Local correlation identity for diagnostics, when available.
    pub diagnostic_id: Option<DiagnosticId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_have_stable_closed_json_names() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::MemberOffline).unwrap(),
            "\"member_offline\""
        );
        assert_eq!(
            serde_json::from_str::<ErrorCode>("\"quality_policy_out_of_sync\"").unwrap(),
            ErrorCode::QualityPolicyOutOfSync
        );
        assert!(serde_json::from_str::<ErrorCode>("\"connection_failed\"").is_err());
    }
}
