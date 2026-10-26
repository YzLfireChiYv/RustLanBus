//! Closed RLBREG2 JSON message schemas.

use std::fmt;
use std::net::Ipv4Addr;

use rlb_control::strict_json::{JsonLimits, StrictJsonError, parse_typed};
use rlb_control::{
    CanonicalRevisions, Field, RoomCommandArgs, RoomCommandExpected, RoomCommandName,
    RoomCommandScope, Version2, WarningCode,
};
use rlb_model::{
    AdmissionState, BindNonce, Capability, CapabilityGrants, CaptureMethod, CatalogRevision,
    Endpoint, EndpointMode, ErrorCode, GrantRevision, MemberConfigRevision, MemberId,
    PresenceRevision, QosPolicy, QualityProfile, RegistrySessionId, RequestId, RoomId,
    RoomRevision, RunId, ServerGeneration, SourceDescriptor, SourceId, SourceKind,
};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::framing::registry_json_error_code;
use crate::token::{ClaimSecret, MemberToken, TokenDigest};

/// Canonical decimal-string u64 used outside the six revision domains.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DecimalU64(u64);

impl DecimalU64 {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for DecimalU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DecimalVisitor;

        impl Visitor<'_> for DecimalVisitor {
            type Value = DecimalU64;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical u64 decimal string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.is_empty()
                    || (value.len() > 1 && value.starts_with('0'))
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(E::custom("u64 string is not canonical"));
                }
                value
                    .parse()
                    .map(DecimalU64)
                    .map_err(|_| E::custom("u64 string exceeds its range"))
            }
        }

        deserializer.deserialize_str(DecimalVisitor)
    }
}

/// Stable member source-slot identity established during enrollment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSlot {
    pub source_id: SourceId,
    pub kind: SourceKind,
}

/// Revisions a client has fully applied.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedRoomRevisions {
    pub room_revision: RoomRevision,
    pub presence_revision: PresenceRevision,
}

/// Full member projection safe for a room authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberProjection {
    pub local_emergency_stop: bool,
    pub auto_reconnect: bool,
    pub effective_room_send_muted: bool,
    pub effective_qos_policy: QosPolicy,
    pub safe_local_fields: SafeLocalFields,
}

/// Field-level read-only local projection allowed by CONTROL.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SafeLocalFields {
    pub capture_method: CaptureMethod,
    pub capture_endpoint_mode: EndpointMode,
    pub capture_friendly_label: String,
    pub capture_available: bool,
    pub playback_endpoint_mode: EndpointMode,
    pub playback_friendly_label: String,
    pub playback_available: bool,
    pub driver_version: Option<String>,
    pub driver_signing_state: DriverSigningState,
    pub driver_available: bool,
    pub master_capture_enabled: bool,
    pub local_emergency_stop: bool,
    pub startup_enabled: bool,
    pub file_logging_enabled: bool,
}

/// Closed driver signing projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverSigningState {
    NotInstalled,
    TestSigned,
    ProductionSigned,
    Invalid,
}

/// Snapshot resynchronization reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    Initial,
    RevisionGap,
    GenerationChanged,
    LocalProjectionLost,
    EventRingMiss,
}

/// Explicit member leave reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaveReason {
    UserLeave,
    EngineExit,
}

/// Member command terminal status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberCommandStatus {
    Applied,
    Unchanged,
    Rejected,
    OutcomeUnknown,
}

/// Stable local publication blocking reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationBlock {
    MasterCaptureDisabled,
    SourceUnavailable,
    RoomSendMuted,
    LocalEmergencyStop,
}

/// Closed effective-result object used by member command replies.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberCommandEffective {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub publish_requested: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub effective_publishing: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub blocking_reasons: Field<Vec<PublicationBlock>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub selected_profile: Field<QualityProfile>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub effective_profile: Field<QualityProfile>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub gain_db: Field<rlb_model::GainDb>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub pan: Field<rlb_model::Pan>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub mute: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub solo: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub enabled: Field<bool>,
}

/// Client-to-registry messages. Every variant has an exact field whitelist.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ClientMessage {
    #[serde(rename = "enrollment_begin")]
    EnrollmentBegin {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        member_id: MemberId,
        display_name: String,
        source_slots: Vec<SourceSlot>,
        claim_sha256: TokenDigest,
        request_readmission: bool,
    },
    #[serde(rename = "enrollment_status")]
    EnrollmentStatus {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        member_id: MemberId,
        claim_secret: ClaimSecret,
    },
    #[serde(rename = "enrollment_claim")]
    EnrollmentClaim {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        member_id: MemberId,
        claim_secret: ClaimSecret,
    },
    #[serde(rename = "engine_hello")]
    EngineHello {
        v: Version2,
        request_id: RequestId,
        member_token: MemberToken,
        token_generation: DecimalU64,
        token_sha256: TokenDigest,
        room_id: RoomId,
        member_id: MemberId,
        run_id: RunId,
        display_name: String,
        client_build: String,
        catalog_revision: CatalogRevision,
        sources: Vec<SourceDescriptor>,
        member_config_revision: MemberConfigRevision,
        member_projection: MemberProjection,
        grant_revision: GrantRevision,
        capability_grants: CapabilityGrants,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        applied_revisions: AppliedRoomRevisions,
    },
    #[serde(rename = "catalog_replace")]
    CatalogReplace {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        run_id: RunId,
        catalog_revision: CatalogRevision,
        sources: Vec<SourceDescriptor>,
    },
    #[serde(rename = "member_projection_replace")]
    MemberProjectionReplace {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        member_config_revision: MemberConfigRevision,
        member_projection: MemberProjection,
    },
    #[serde(rename = "capability_grants_replace")]
    CapabilityGrantsReplace {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        grant_revision: GrantRevision,
        capability_grants: CapabilityGrants,
    },
    #[serde(rename = "snapshot_request")]
    SnapshotRequest {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        applied_revisions: AppliedRoomRevisions,
        reason: SnapshotReason,
    },
    #[serde(rename = "media_rebind_request")]
    MediaRebindRequest {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        current_endpoint: Option<Endpoint>,
    },
    #[serde(rename = "member_command_result")]
    MemberCommandResult {
        v: Version2,
        request_id: RequestId,
        registry_session_id: RegistrySessionId,
        capability: Capability,
        status: MemberCommandStatus,
        revisions: CanonicalRevisions,
        code: Option<ErrorCode>,
        effective: MemberCommandEffective,
        warnings: Vec<WarningCode>,
    },
    #[serde(rename = "leave")]
    Leave {
        v: Version2,
        request_id: RequestId,
        room_id: RoomId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        reason: LeaveReason,
    },
}

impl ClientMessage {
    /// Validates cross-field and collection invariants.
    pub fn validate(&self) -> Result<(), WireValidationError> {
        match self {
            Self::EnrollmentBegin {
                display_name,
                source_slots,
                ..
            } => {
                validate_display_name(display_name)?;
                validate_slots(source_slots)?;
            }
            Self::EngineHello {
                member_token,
                token_sha256,
                display_name,
                client_build,
                sources,
                member_projection,
                ..
            } => {
                validate_display_name(display_name)?;
                if *token_sha256 != TokenDigest::of(member_token.expose_bytes()) {
                    return Err(WireValidationError::TokenDigest);
                }
                if client_build.is_empty() || client_build.len() > 64 || !client_build.is_ascii() {
                    return Err(WireValidationError::ClientBuild);
                }
                validate_sources(sources)?;
                validate_projection(member_projection)?;
            }
            Self::CatalogReplace { sources, .. } => validate_sources(sources)?,
            Self::MemberProjectionReplace {
                member_projection, ..
            } => validate_projection(member_projection)?,
            Self::MemberCommandResult {
                status,
                code,
                warnings,
                ..
            } => {
                if warnings.len() > 16 {
                    return Err(WireValidationError::TooManyWarnings);
                }
                if matches!(status, MemberCommandStatus::Rejected) != code.is_some() {
                    return Err(WireValidationError::ResultCode);
                }
            }
            Self::EnrollmentStatus { .. }
            | Self::EnrollmentClaim { .. }
            | Self::Heartbeat { .. }
            | Self::CapabilityGrantsReplace { .. }
            | Self::SnapshotRequest { .. }
            | Self::MediaRebindRequest { .. }
            | Self::Leave { .. } => {}
        }
        Ok(())
    }

    fn normalize_names(&mut self) -> Result<(), WireValidationError> {
        match self {
            Self::EnrollmentBegin { display_name, .. } => {
                *display_name = display_name.nfc().collect();
            }
            Self::EngineHello {
                display_name,
                sources,
                ..
            } => {
                *display_name = display_name.nfc().collect();
                normalize_source_names(sources)?;
            }
            Self::CatalogReplace { sources, .. } => normalize_source_names(sources)?,
            Self::EnrollmentStatus { .. }
            | Self::EnrollmentClaim { .. }
            | Self::Heartbeat { .. }
            | Self::MemberProjectionReplace { .. }
            | Self::CapabilityGrantsReplace { .. }
            | Self::SnapshotRequest { .. }
            | Self::MediaRebindRequest { .. }
            | Self::MemberCommandResult { .. }
            | Self::Leave { .. } => {}
        }
        Ok(())
    }
}

/// Decodes one strict client message from a complete frame payload.
pub fn parse_client_message(input: &[u8]) -> Result<ClientMessage, WireDecodeError> {
    let mut message: ClientMessage = parse_typed(input, JsonLimits::REGISTRY)?;
    message.normalize_names()?;
    message.validate()?;
    Ok(message)
}

/// Room policy embedded in member snapshots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomView {
    pub name: String,
    pub quality_cap: QualityProfile,
    pub qos_policy: QosPolicy,
    pub room_send_muted: bool,
    pub max_approved_members: u8,
}

/// Snapshot self projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelfView {
    pub member_id: MemberId,
    pub admission: AdmissionState,
}

/// Media presence in a room snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPresence {
    Bound,
    Unbound,
}

/// Control presence in a room snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlPresence {
    Online,
    Offline,
}

/// Approved member projection filtered for one snapshot viewer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMember {
    pub member_id: MemberId,
    pub display_name: String,
    pub admission: AdmissionState,
    pub control_presence: ControlPresence,
    pub media_presence: MediaPresence,
    pub run_id: Option<RunId>,
    pub media_endpoint: Option<Endpoint>,
    pub catalog_revision: Option<CatalogRevision>,
    pub sources: Vec<SourceDescriptor>,
}

/// Complete permission-filtered member room snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomSnapshot {
    pub server_generation: ServerGeneration,
    pub room_id: RoomId,
    pub room_revision: RoomRevision,
    pub presence_revision: PresenceRevision,
    pub event_cursor: DecimalU64,
    pub room: RoomView,
    #[serde(rename = "self")]
    pub self_view: SelfView,
    pub members: Vec<SnapshotMember>,
}

/// Endpoint summary carried by bind/rebind messages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedMediaEndpoint {
    pub observed_media_ipv4: Ipv4Addr,
    pub observed_media_port: u16,
}

/// Closed room-event value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoomEventValue {
    RoomNameChanged {
        name: String,
    },
    AdmissionChanged {
        member_id: MemberId,
        admission: AdmissionState,
    },
    MemberControlUp {
        member_id: MemberId,
        run_id: RunId,
    },
    MemberControlDown {
        member_id: MemberId,
    },
    MemberMediaBound {
        member_id: MemberId,
        endpoint: Endpoint,
    },
    MemberMediaUnbound {
        member_id: MemberId,
    },
    MemberRunReplaced {
        member_id: MemberId,
        old_run_id: RunId,
        new_run_id: RunId,
    },
    MediaEndpointChanged {
        member_id: MemberId,
        endpoint: Endpoint,
    },
    CatalogReplaced {
        member_id: MemberId,
        catalog_revision: CatalogRevision,
        sources: Vec<SourceDescriptor>,
    },
    SourceVisibilityChanged {
        publisher_member_id: MemberId,
        source_id: SourceId,
        viewer_member_ids: Vec<MemberId>,
    },
    QualityCapChanged {
        quality_cap: QualityProfile,
    },
    QosPolicyChanged {
        qos_policy: QosPolicy,
    },
    RoomSendMutedChanged {
        room_send_muted: bool,
    },
    CapabilityGrantsChanged {
        member_id: MemberId,
        grant_revision: GrantRevision,
        grants: CapabilityGrants,
    },
    MemberProjectionChanged {
        member_id: MemberId,
        member_config_revision: MemberConfigRevision,
        projection: MemberProjection,
    },
    ResyncRequired,
}

/// One ordered permission-filtered room event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomEvent {
    pub event_seq: DecimalU64,
    pub server_generation: ServerGeneration,
    pub room_id: RoomId,
    pub room_revision: RoomRevision,
    pub presence_revision: PresenceRevision,
    pub event: RoomEventValue,
}

/// Structured registry error details.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryErrorDetails {
    pub server_generation: Option<ServerGeneration>,
    pub room_revision: Option<RoomRevision>,
    pub presence_revision: Option<PresenceRevision>,
    pub member_id: Option<MemberId>,
    pub source_id: Option<SourceId>,
}

/// Closed server-to-client message family.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ServerMessage {
    #[serde(rename = "enrollment_pending")]
    EnrollmentPending {
        v: Version2,
        reply_to: RequestId,
        room_id: RoomId,
        room_name: String,
        admission: AdmissionState,
        room_revision: RoomRevision,
    },
    #[serde(rename = "enrollment_status_result")]
    EnrollmentStatusResult {
        v: Version2,
        reply_to: RequestId,
        room_id: RoomId,
        room_name: String,
        admission: AdmissionState,
        room_revision: RoomRevision,
    },
    #[serde(rename = "enrollment_claim_result")]
    EnrollmentClaimResult {
        v: Version2,
        reply_to: RequestId,
        room_id: RoomId,
        room_name: String,
        admission: AdmissionState,
        room_revision: RoomRevision,
        member_token: MemberToken,
        token_generation: DecimalU64,
    },
    #[serde(rename = "session_ready")]
    SessionReady {
        v: Version2,
        reply_to: RequestId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        room_id: RoomId,
        room_name: String,
        admission: AdmissionState,
        lease_ms: u32,
        heartbeat_ms: u32,
        bind_timeout_ms: u32,
        bind_nonce: BindNonce,
        room_revision: RoomRevision,
        presence_revision: PresenceRevision,
        quality_cap: QualityProfile,
        qos_policy: QosPolicy,
        room_send_muted: bool,
    },
    #[serde(rename = "admission_update")]
    AdmissionUpdate {
        v: Version2,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        member_id: MemberId,
        old_admission: AdmissionState,
        new_admission: AdmissionState,
        room_revision: RoomRevision,
        code: ErrorCode,
    },
    #[serde(rename = "heartbeat_ack")]
    HeartbeatAck {
        v: Version2,
        reply_to: RequestId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        room_revision: RoomRevision,
        presence_revision: PresenceRevision,
        lease_remaining_ms: u32,
    },
    #[serde(rename = "media_bound")]
    MediaBound {
        v: Version2,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        member_id: MemberId,
        run_id: RunId,
        observed_media_ipv4: Ipv4Addr,
        observed_media_port: u16,
        room_revision: RoomRevision,
        presence_revision: PresenceRevision,
    },
    #[serde(rename = "media_rebind_ready")]
    MediaRebindReady {
        v: Version2,
        reply_to: RequestId,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        bind_nonce: BindNonce,
        bind_timeout_ms: u32,
        old_endpoint: Option<Endpoint>,
    },
    #[serde(rename = "room_snapshot")]
    RoomSnapshot {
        v: Version2,
        server_generation: ServerGeneration,
        room_id: RoomId,
        room_revision: RoomRevision,
        presence_revision: PresenceRevision,
        event_cursor: DecimalU64,
        room: RoomView,
        #[serde(rename = "self")]
        self_view: SelfView,
        members: Vec<SnapshotMember>,
    },
    #[serde(rename = "room_event")]
    RoomEvent {
        v: Version2,
        event_seq: DecimalU64,
        server_generation: ServerGeneration,
        room_id: RoomId,
        room_revision: RoomRevision,
        presence_revision: PresenceRevision,
        event: RoomEventValue,
    },
    #[serde(rename = "member_command")]
    MemberCommand {
        v: Version2,
        request_id: RequestId,
        registry_session_id: RegistrySessionId,
        capability: Capability,
        command: RoomCommandName,
        scope: RoomCommandScope,
        expected: RoomCommandExpected,
        args: RoomCommandArgs,
        delivery_ttl_ms: u32,
    },
    #[serde(rename = "superseded")]
    Superseded {
        v: Version2,
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        new_run_id: RunId,
        replacement_diagnostic_id: String,
        reason: SupersedeReason,
    },
    #[serde(rename = "error")]
    Error {
        v: Version2,
        reply_to: Option<RequestId>,
        code: ErrorCode,
        scope: RegistryErrorScope,
        retryable: bool,
        message: String,
        details: RegistryErrorDetails,
    },
}

impl From<RoomSnapshot> for ServerMessage {
    fn from(snapshot: RoomSnapshot) -> Self {
        Self::RoomSnapshot {
            v: Version2,
            server_generation: snapshot.server_generation,
            room_id: snapshot.room_id,
            room_revision: snapshot.room_revision,
            presence_revision: snapshot.presence_revision,
            event_cursor: snapshot.event_cursor,
            room: snapshot.room,
            self_view: snapshot.self_view,
            members: snapshot.members,
        }
    }
}

impl From<RoomEvent> for ServerMessage {
    fn from(event: RoomEvent) -> Self {
        Self::RoomEvent {
            v: Version2,
            event_seq: event.event_seq,
            server_generation: event.server_generation,
            room_id: event.room_id,
            room_revision: event.room_revision,
            presence_revision: event.presence_revision,
            event: event.event,
        }
    }
}

impl ServerMessage {
    /// Validates fixed constants and delegated capability mapping.
    pub fn validate(&self) -> Result<(), WireValidationError> {
        match self {
            Self::SessionReady {
                admission,
                lease_ms,
                heartbeat_ms,
                bind_timeout_ms,
                ..
            } => {
                if *admission != AdmissionState::Approved
                    || *lease_ms != 30_000
                    || *heartbeat_ms != 10_000
                    || *bind_timeout_ms != 10_000
                {
                    return Err(WireValidationError::ProtocolConstant);
                }
            }
            Self::HeartbeatAck {
                lease_remaining_ms, ..
            } if *lease_remaining_ms > 30_000 => {
                return Err(WireValidationError::ProtocolConstant);
            }
            Self::MediaRebindReady {
                bind_timeout_ms, ..
            } if *bind_timeout_ms != 10_000 => {
                return Err(WireValidationError::ProtocolConstant);
            }
            Self::MemberCommand {
                capability,
                command,
                scope,
                expected,
                args,
                delivery_ttl_ms,
                request_id,
                ..
            } => {
                if *delivery_ttl_ms == 0 || *delivery_ttl_ms > 5_000 {
                    return Err(WireValidationError::DeliveryTtl);
                }
                let envelope = rlb_control::RoomCommandEnvelope {
                    v: Version2,
                    message_type: rlb_control::CommandMessageType::Command,
                    request_id: *request_id,
                    command: *command,
                    scope: scope.clone(),
                    expected: expected.clone(),
                    args: args.clone(),
                };
                let validated = envelope
                    .validate()
                    .map_err(|_| WireValidationError::MemberCommand)?;
                let rlb_control::ValidatedRoomCommand::Delegated(delegated) = validated else {
                    return Err(WireValidationError::MemberCommand);
                };
                if delegated.capability() != *capability {
                    return Err(WireValidationError::CapabilityMapping);
                }
            }
            Self::EnrollmentPending { .. }
            | Self::EnrollmentStatusResult { .. }
            | Self::EnrollmentClaimResult { .. }
            | Self::AdmissionUpdate { .. }
            | Self::HeartbeatAck { .. }
            | Self::MediaBound { .. }
            | Self::MediaRebindReady { .. }
            | Self::RoomSnapshot { .. }
            | Self::RoomEvent { .. }
            | Self::Superseded { .. }
            | Self::Error { .. } => {}
        }
        Ok(())
    }
}

/// Decodes one strict server message from a complete frame payload.
pub fn parse_server_message(input: &[u8]) -> Result<ServerMessage, WireDecodeError> {
    let message: ServerMessage = parse_typed(input, JsonLimits::REGISTRY)?;
    message.validate()?;
    Ok(message)
}

/// Registry error scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryErrorScope {
    Protocol,
    Room,
    Member,
    Source,
    Session,
    Command,
}

/// Why an active session was replaced.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupersedeReason {
    TransportReplaced,
    RunReplaced,
}

/// Cross-field wire validation failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WireValidationError {
    #[error("display name violates its bound")]
    DisplayName,
    #[error("client build violates its bound")]
    ClientBuild,
    #[error("member token digest does not match the bearer token")]
    TokenDigest,
    #[error("source slots must contain system_audio and microphone exactly once")]
    SourceSlots,
    #[error("catalog must contain system_audio and microphone exactly once")]
    Sources,
    #[error("safe local projection string violates its bound")]
    SafeLocalProjection,
    #[error("member result code does not match terminal status")]
    ResultCode,
    #[error("member result contains too many warnings")]
    TooManyWarnings,
    #[error("protocol constant has an invalid value")]
    ProtocolConstant,
    #[error("member command delivery TTL is invalid")]
    DeliveryTtl,
    #[error("member command schema is invalid")]
    MemberCommand,
    #[error("member command capability does not match its command")]
    CapabilityMapping,
}

/// Failure while decoding a strict wire message.
#[derive(Debug, Error)]
pub enum WireDecodeError {
    #[error(transparent)]
    Json(#[from] StrictJsonError),
    #[error(transparent)]
    Validation(#[from] WireValidationError),
}

impl WireDecodeError {
    /// Returns a stable protocol error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Json(error) => registry_json_error_code(*error),
            Self::Validation(_) => ErrorCode::InvalidValue,
        }
    }
}

fn validate_display_name(value: &str) -> Result<(), WireValidationError> {
    if value.is_empty()
        || value.len() > 64
        || value.nfc().ne(value.chars())
        || value
            .chars()
            .any(|character| matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'))
    {
        Err(WireValidationError::DisplayName)
    } else {
        Ok(())
    }
}

fn validate_slots(slots: &[SourceSlot]) -> Result<(), WireValidationError> {
    if slots.len() != 2
        || slots[0].source_id == slots[1].source_id
        || !has_both_kinds(slots.iter().map(|slot| slot.kind))
    {
        Err(WireValidationError::SourceSlots)
    } else {
        Ok(())
    }
}

fn validate_sources(sources: &[SourceDescriptor]) -> Result<(), WireValidationError> {
    if sources.len() != 2
        || sources[0].source_id() == sources[1].source_id()
        || !has_both_kinds(sources.iter().map(SourceDescriptor::kind))
        || sources.iter().any(|source| {
            source
                .display_name()
                .nfc()
                .ne(source.display_name().chars())
        })
    {
        Err(WireValidationError::Sources)
    } else {
        Ok(())
    }
}

fn normalize_source_names(sources: &mut [SourceDescriptor]) -> Result<(), WireValidationError> {
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
            .map_err(|_| WireValidationError::Sources)?;
        }
    }
    Ok(())
}

fn validate_projection(projection: &MemberProjection) -> Result<(), WireValidationError> {
    let fields = &projection.safe_local_fields;
    for label in [
        fields.capture_friendly_label.as_str(),
        fields.playback_friendly_label.as_str(),
    ] {
        if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
            return Err(WireValidationError::SafeLocalProjection);
        }
    }
    if fields
        .driver_version
        .as_ref()
        .is_some_and(|version| version.len() > 64 || !version.is_ascii())
    {
        return Err(WireValidationError::SafeLocalProjection);
    }
    Ok(())
}

fn has_both_kinds(kinds: impl Iterator<Item = SourceKind>) -> bool {
    let mut system_audio = 0_u8;
    let mut microphone = 0_u8;
    for kind in kinds {
        match kind {
            SourceKind::SystemAudio => system_audio += 1,
            SourceKind::Microphone => microphone += 1,
        }
    }
    system_audio == 1 && microphone == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_u64_is_a_canonical_json_string() {
        assert_eq!(
            serde_json::to_string(&DecimalU64::new(u64::MAX)).unwrap(),
            format!("\"{}\"", u64::MAX)
        );
        assert!(serde_json::from_str::<DecimalU64>("\"01\"").is_err());
        assert!(serde_json::from_str::<DecimalU64>("1").is_err());
    }

    #[test]
    fn unknown_client_fields_are_rejected() {
        let json = br#"{
            "type":"heartbeat",
            "v":2,
            "request_id":"01010101010101010101010101010101",
            "room_id":"02020202020202020202020202020202",
            "server_generation":"03030303030303030303030303030303",
            "registry_session_id":"04040404040404040404040404040404",
            "applied_revisions":{"room_revision":"1","presence_revision":"1"},
            "shell":"forbidden"
        }"#;
        assert!(matches!(
            parse_client_message(json),
            Err(WireDecodeError::Json(StrictJsonError::UnknownField))
        ));
    }

    #[test]
    fn unknown_type_and_protocol_version_have_stable_codes() {
        let unknown_type = br#"{"type":"future_message","v":2}"#;
        let error = parse_client_message(unknown_type).unwrap_err();
        assert_eq!(error.code(), ErrorCode::UnknownType);

        let wrong_version = br#"{
            "type":"heartbeat",
            "v":3,
            "request_id":"01010101010101010101010101010101",
            "room_id":"02020202020202020202020202020202",
            "server_generation":"03030303030303030303030303030303",
            "registry_session_id":"04040404040404040404040404040404",
            "applied_revisions":{"room_revision":"1","presence_revision":"1"}
        }"#;
        let error = parse_client_message(wrong_version).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ProtocolVersion);
    }

    #[test]
    fn client_names_are_normalized_to_nfc_before_validation() {
        let token = MemberToken::from_bytes(vec![7; 32]).unwrap();
        let token_json = token.expose_encoded();
        let digest_json = serde_json::to_string(&TokenDigest::of(token.expose_bytes())).unwrap();
        let json = format!(
            r#"{{
                "type":"engine_hello",
                "v":2,
                "request_id":"01010101010101010101010101010101",
                "member_token":"{token_json}",
                "token_generation":"1",
                "token_sha256":{digest_json},
                "room_id":"02020202020202020202020202020202",
                "member_id":"03030303030303030303030303030303",
                "run_id":"04040404040404040404040404040404",
                "display_name":"Cafe\u0301",
                "client_build":"test",
                "catalog_revision":"1",
                "sources":[],
                "member_config_revision":"1",
                "member_projection":{{
                    "local_emergency_stop":false,
                    "auto_reconnect":true,
                    "effective_room_send_muted":false,
                    "effective_qos_policy":"audio-priority",
                    "safe_local_fields":{{
                        "capture_method":"wasapi_loopback",
                        "capture_endpoint_mode":"follow_default",
                        "capture_friendly_label":"Default",
                        "capture_available":true,
                        "playback_endpoint_mode":"follow_default",
                        "playback_friendly_label":"Default",
                        "playback_available":true,
                        "driver_version":null,
                        "driver_signing_state":"not_installed",
                        "driver_available":false,
                        "master_capture_enabled":false,
                        "local_emergency_stop":false,
                        "startup_enabled":false,
                        "file_logging_enabled":false
                    }}
                }},
                "grant_revision":"1",
                "capability_grants":{{
                    "source_publication":false,
                    "send_quality":false,
                    "receive_mix":false,
                    "auto_reconnect":false
                }}
            }}"#
        );
        let error = parse_client_message(json.as_bytes()).unwrap_err();
        assert!(matches!(
            error,
            WireDecodeError::Validation(WireValidationError::Sources)
        ));

        let mut message: ClientMessage = serde_json::from_str(&json).expect("closed engine hello");
        message.normalize_names().unwrap();
        let ClientMessage::EngineHello { display_name, .. } = message else {
            panic!("engine hello");
        };
        assert_eq!(display_name, "Café");
    }
}
