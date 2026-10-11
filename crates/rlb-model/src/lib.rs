#![forbid(unsafe_code)]

//! Pure V2 domain values shared by control, registry, media, and audio crates.
//!
//! Identity types reject the reserved all-zero value and use a canonical
//! 32-character lowercase hexadecimal representation. Domain constructors keep
//! invalid floating-point values and inconsistent snapshots out of runtime
//! state.

mod error;
mod id;
mod model;
mod revision;
mod values;

pub use error::{ErrorCode, ErrorEntity, ErrorImpact, StructuredError};
pub use id::{
    BindNonce, DiagnosticId, DirectSessionId, HelloNonce, IdGenerationError, IdParseError,
    MemberId, OperationId, RegistrySessionId, RequestId, RoomId, RunId, ServerGeneration, SourceId,
    StreamEpoch,
};
pub use model::{
    Endpoint, EndpointError, IdentityRecord, IdentityRecordError, MemberSnapshotMeta,
    RoomPolicySnapshot, RoomSnapshotMeta, SourceDescriptor, SourceDescriptorError, SourceFormat,
    SourceState,
};
pub use revision::{
    CanonicalRevision, CatalogRevision, GrantRevision, MemberConfigRevision, PresenceRevision,
    RevisionParseError, RoomRevision, SubscriptionRevision,
};
pub use values::{
    AUTOMATIC_MAX_FRAMES, AUTOMATIC_MIN_FRAMES, AUTOMATIC_TARGET_FRAMES, AdmissionState,
    Capability, CapabilityGrants, CaptureMethod, ChannelLayout, EndpointMode, GainDb,
    LocalSourceConfig, MEDIA_UNIT_FRAMES, ManualBufferTarget, MemberMix, MemberRoomConfig, Pan,
    QosPolicy, QualityProfile, RECEIVE_CAPACITY_FRAMES, ReceiveBufferConfig, RoomPolicy,
    SAMPLE_RATE_HZ, SourceKind, SourceMix, ValueError,
};
