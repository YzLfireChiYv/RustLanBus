#![forbid(unsafe_code)]

//! Pure V2 member-engine coordinator and reducer.
//!
//! The crate owns member configuration candidates, authority projections,
//! source and peer lifecycles, publication eligibility, direct subscriptions,
//! and immutable audio/media plans. Socket, file, device, and thread ownership
//! remains in an engine composition root.

mod config;
mod coordinator;
mod types;

pub use config::{
    DeviceBindingConfig, DriverSelection, JoinTarget, LoggingConfig, MEMBER_CONFIG_SCHEMA_V2,
    MEMBER_CONFIG_SCHEMA_VERSION, MemberCommandResultRecord, MemberConfig, MemberConfigError,
    PersistentRoomConfig, PersistentSourceConfig, SourceMonitorConfig, StartupConfig,
};
pub use coordinator::{CoordinatorError, MemberSessionCoordinator};
pub use types::{
    AudioMixPlan, AuthoritySnapshot, CaptureState, ConfigTransactionId, CoordinatorEffect,
    CoordinatorEvent, DiagnosticsSnapshot, EffectiveSendBlock, EngineState, EpochChangeReason,
    LocalConfigMutation, LocalConfigOutcome, LocalConfigRequest, LocalSourceDiagnostics,
    LocalSourceVisibility, MediaReceivePlan, MediaSourceProjection, MemberCommandDelivery,
    MemberCommandDeliveryError, PeerConnectPlan, PeerConnectionState, PeerDiagnostics,
    PersistenceResult, RecipientEligibility, RegistryProjectionUpdate, RegistryState,
    RemoteMediaMetrics, RemoteMediaState, RemoteSourceDiagnostics, SendPlan, SendRecipient,
    ShutdownStage, SnapshotError, SourceEpochEndPlan, SourceEpochStartPlan, SourceRestartPlan,
    SourceSendPlan, SourceStopReason, SourceStreamStarted,
};

#[cfg(test)]
mod tests;
