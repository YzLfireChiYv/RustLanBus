//! Closed snapshots, results, events, and query envelopes.

use rlb_model::{
    AdmissionState, CapabilityGrants, CatalogRevision, EndpointMode, ErrorCode, ErrorEntity,
    GrantRevision, MemberConfigRevision, MemberId, OperationId, PresenceRevision, QosPolicy,
    QualityProfile, RequestId, RoomId, RoomPolicy, RoomRevision, RunId, ServerGeneration,
    SourceDescriptor, SourceId, StructuredError,
};
use serde::{Deserialize, Serialize};

use crate::command::{CommandDecodeError, CommandValidationError, DeviceFlow, Field, Version2};
use crate::strict_json::{JsonLimits, decode_value, parse_value};

/// Canonical revision object. Irrelevant domains remain absent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalRevisions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_revision: Option<RoomRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_revision: Option<PresenceRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<CatalogRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_config_revision: Option<MemberConfigRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_revision: Option<GrantRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_revision: Option<rlb_model::SubscriptionRevision>,
}

/// Local engine lifecycle state exposed to A.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    Starting,
    Running,
    Stopping,
    Faulted,
}

/// Closed A engine projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineProjection {
    pub state: EngineState,
    pub build: String,
}

/// Closed A room projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRoomProjection {
    pub room_id: Option<RoomId>,
    pub admission: Option<AdmissionState>,
    pub connected: bool,
    pub policy: Option<RoomPolicy>,
}

/// Closed A playback projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackProjection {
    pub endpoint_mode: EndpointMode,
    pub endpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_endpoint_id: Option<String>,
    pub available: bool,
}

/// One room-scoped member mix entry.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberMixProjection {
    pub room_id: RoomId,
    pub remote_member_id: MemberId,
    pub gain_db: rlb_model::GainDb,
    pub mute: bool,
    pub solo: bool,
}

/// One room-scoped source mix entry.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMixProjection {
    pub room_id: RoomId,
    pub remote_member_id: MemberId,
    pub source_id: SourceId,
    pub gain_db: rlb_model::GainDb,
    pub pan: rlb_model::Pan,
    pub mute: bool,
}

/// Closed A mixer projection.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MixerProjection {
    pub members: Vec<MemberMixProjection>,
    pub sources: Vec<SourceMixProjection>,
}

/// One room's complete grant set in A.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionProjection {
    pub room_id: RoomId,
    pub grant_revision: GrantRevision,
    pub grants: CapabilityGrants,
}

/// Bounded A diagnostics projection.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsProjection {
    pub control_reconnects: u64,
    pub malformed_messages: u64,
    #[serde(default)]
    pub ingress_dropped: u64,
    pub active_alert_codes: Vec<ErrorCode>,
}

/// A-only file-logging projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingProjection {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    pub max_bytes: u64,
    pub retention_count: u16,
}

/// A-only driver projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DriverProjection {
    pub package_id: Option<String>,
    pub version: Option<String>,
    pub enabled: bool,
    pub available: bool,
}

/// A-only process and startup projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleProjection {
    pub start_with_os: bool,
    pub start_engine_on_ui_open: bool,
}

/// One bounded operation projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationProjection {
    pub operation_id: OperationId,
    pub request_id: RequestId,
    pub status: CommandStatus,
}

/// Complete A snapshot with an exact top-level field set.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSnapshot {
    pub schema: Version2,
    pub captured_at: String,
    pub member_id: MemberId,
    pub run_id: RunId,
    pub engine: EngineProjection,
    pub revisions: CanonicalRevisions,
    pub room: LocalRoomProjection,
    pub sources: Vec<SourceDescriptor>,
    pub playback: PlaybackProjection,
    pub receive_buffer: rlb_model::ReceiveBufferConfig,
    pub mixer: MixerProjection,
    pub permissions: Vec<PermissionProjection>,
    pub local_emergency_stop: bool,
    pub diagnostics: DiagnosticsProjection,
    pub logging: LoggingProjection,
    pub driver: DriverProjection,
    pub lifecycle: LifecycleProjection,
    pub operations: Vec<OperationProjection>,
    pub errors: Vec<StructuredError>,
    pub event_cursor: u64,
}

/// Operator-visible admission record. Credential material is absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionProjection {
    pub member_id: MemberId,
    pub admission: AdmissionState,
    pub last_display_name: String,
    pub token_generation: Option<u64>,
    pub request_readmission: bool,
}

/// Operator-visible online member summary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnlineMemberSummary {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub display_name: String,
    pub member_config_revision: MemberConfigRevision,
    pub catalog_revision: CatalogRevision,
    pub grant_revision: GrantRevision,
    pub grants: CapabilityGrants,
    pub media_bound: bool,
}

/// Complete visibility set for one source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceVisibilityProjection {
    pub publisher_member_id: MemberId,
    pub source_id: SourceId,
    pub viewer_member_ids: Vec<MemberId>,
}

/// Operator-visible in-flight command metadata.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveCommandSummary {
    pub request_id: RequestId,
    pub target_member_id: MemberId,
    pub operation_id: OperationId,
}

/// Complete B operator snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorSnapshot {
    pub server_generation: ServerGeneration,
    pub room_id: RoomId,
    pub room_name: String,
    pub room_revision: RoomRevision,
    pub presence_revision: PresenceRevision,
    pub quality_cap: QualityProfile,
    pub qos: QosPolicy,
    pub room_send_muted: bool,
    pub admission_records: Vec<AdmissionProjection>,
    pub online_member_summaries: Vec<OnlineMemberSummary>,
    pub source_visibility: Vec<SourceVisibilityProjection>,
    pub active_command_summaries: Vec<ActiveCommandSummary>,
    pub errors: Vec<StructuredError>,
    pub event_cursor: u64,
}

/// The only command-ack status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Accepted,
}

/// Accepted-command acknowledgement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandAck {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: AckMessageType,
    pub request_id: RequestId,
    pub status: AckStatus,
    pub operation_id: OperationId,
}

/// The only ack envelope type.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AckMessageType {
    #[serde(rename = "command.ack")]
    CommandAck,
}

/// Terminal command status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Applied,
    Unchanged,
    Rejected,
    OutcomeUnknown,
}

/// Closed warning names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCode {
    QualityCapped,
    SafetyStopVolatile,
    EffectiveStateBlocked,
    EffectiveStateRefreshRequired,
}

/// Closed terminal result envelope. `E` is a command-specific closed effective
/// projection selected by the caller.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult<E> {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: ResultMessageType,
    pub request_id: RequestId,
    pub operation_id: OperationId,
    pub status: CommandStatus,
    pub revisions: CanonicalRevisions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective: Option<E>,
    pub warnings: Vec<WarningCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlErrorBody>,
}

/// The only terminal-result envelope type.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ResultMessageType {
    #[serde(rename = "command.result")]
    CommandResult,
}

/// The only query-result envelope type.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum QueryResultMessageType {
    #[serde(rename = "query.result")]
    QueryResult,
}

/// Closed A query result. `R` is the query-specific observation object.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalQueryResult<R> {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: QueryResultMessageType,
    pub request_id: RequestId,
    pub query: LocalQueryName,
    pub revisions: CanonicalRevisions,
    pub result: R,
}

/// Stable structured control error body.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlErrorBody {
    pub code: ErrorCode,
    pub message_key: String,
    pub retryable: bool,
    pub scope: ErrorEntity,
    pub current_revisions: CanonicalRevisions,
}

/// Standalone control error envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlErrorEnvelope {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: ErrorMessageType,
    pub request_id: RequestId,
    pub error: ControlErrorBody,
}

/// The only standalone error envelope type.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorMessageType {
    Error,
}

/// Closed local event names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalEventKind {
    #[serde(rename = "engine.state_changed")]
    EngineStateChanged,
    #[serde(rename = "room.state_changed")]
    RoomStateChanged,
    #[serde(rename = "source.state_changed")]
    SourceStateChanged,
    #[serde(rename = "playback.state_changed")]
    PlaybackStateChanged,
    #[serde(rename = "buffer.state_changed")]
    BufferStateChanged,
    #[serde(rename = "mix.member_changed")]
    MixMemberChanged,
    #[serde(rename = "mix.source_changed")]
    MixSourceChanged,
    #[serde(rename = "permission.changed")]
    PermissionChanged,
    #[serde(rename = "diagnostic.sample")]
    DiagnosticSample,
    #[serde(rename = "operation.changed")]
    OperationChanged,
    #[serde(rename = "error.raised")]
    ErrorRaised,
    #[serde(rename = "error.cleared")]
    ErrorCleared,
    #[serde(rename = "engine.stopping")]
    EngineStopping,
    #[serde(rename = "resync_required")]
    ResyncRequired,
}

/// Closed event identity projection.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventEntity {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub room_id: Field<RoomId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_id: Field<MemberId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub remote_member_id: Field<MemberId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub source_id: Field<SourceId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub operation_id: Field<OperationId>,
}

/// Closed A event envelope with a caller-selected complete value projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalEvent<V> {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: EventMessageType,
    pub event: LocalEventKind,
    pub event_seq: u64,
    pub revisions: CanonicalRevisions,
    pub entity: EventEntity,
    pub value: V,
}

/// Closed operator event names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OperatorEventKind {
    #[serde(rename = "room.changed")]
    RoomChanged,
    #[serde(rename = "admission.changed")]
    AdmissionChanged,
    #[serde(rename = "presence.changed")]
    PresenceChanged,
    #[serde(rename = "visibility.changed")]
    VisibilityChanged,
    #[serde(rename = "policy.changed")]
    PolicyChanged,
    #[serde(rename = "member.projection_changed")]
    MemberProjectionChanged,
    #[serde(rename = "command.completed")]
    CommandCompleted,
    #[serde(rename = "error.raised")]
    ErrorRaised,
    #[serde(rename = "error.cleared")]
    ErrorCleared,
    #[serde(rename = "resync_required")]
    ResyncRequired,
}

/// Closed B event envelope with a caller-selected complete value projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorEvent<V> {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: EventMessageType,
    pub event: OperatorEventKind,
    pub event_seq: u64,
    pub server_generation: ServerGeneration,
    pub room_id: RoomId,
    pub revisions: CanonicalRevisions,
    pub entity: EventEntity,
    pub value: V,
}

/// The only event-envelope type.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventMessageType {
    Event,
}

/// Canonical A query names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalQueryName {
    #[serde(rename = "snapshot.get")]
    SnapshotGet,
    #[serde(rename = "local.devices.enumerate")]
    DevicesEnumerate,
    #[serde(rename = "local.driver.inspect")]
    DriverInspect,
    #[serde(rename = "local.diagnostics.current")]
    DiagnosticsCurrent,
    #[serde(rename = "local.operation.get")]
    OperationGet,
    #[serde(rename = "local.audit.get")]
    AuditGet,
}

/// Closed A query scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalQueryScope {
    pub member_id: MemberId,
    pub run_id: RunId,
}

/// Closed A query argument union. Query-specific presence is validated by the
/// query handler.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalQueryArgs {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub flow: Field<DeviceFlow>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub domain: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub operation_id: Field<OperationId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub after_audit_seq: Field<u64>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub limit: Field<u8>,
}

/// Strict A query envelope. Queries deliberately have no `expected` field.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalQueryEnvelope {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: QueryMessageType,
    pub request_id: RequestId,
    pub query: LocalQueryName,
    pub scope: LocalQueryScope,
    pub args: LocalQueryArgs,
}

/// Query envelope marker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMessageType {
    Query,
}

impl LocalQueryEnvelope {
    /// Applies query-specific argument presence rules from CONTROL §6.
    pub fn validate(&self) -> Result<(), CommandValidationError> {
        match self.query {
            LocalQueryName::SnapshotGet | LocalQueryName::DriverInspect => {
                require_missing(&self.args.flow, "flow")?;
                require_missing(&self.args.domain, "domain")?;
                require_missing(&self.args.operation_id, "operation_id")?;
                require_missing(&self.args.after_audit_seq, "after_audit_seq")?;
                require_missing(&self.args.limit, "limit")?;
            }
            LocalQueryName::DevicesEnumerate => {
                if self.args.flow.as_ref().is_none() {
                    return Err(CommandValidationError::MissingField("flow"));
                }
                require_missing(&self.args.domain, "domain")?;
                require_missing(&self.args.operation_id, "operation_id")?;
                require_missing(&self.args.after_audit_seq, "after_audit_seq")?;
                require_missing(&self.args.limit, "limit")?;
            }
            LocalQueryName::DiagnosticsCurrent => {
                if self.args.domain.as_ref().is_none() {
                    return Err(CommandValidationError::MissingField("domain"));
                }
                require_missing(&self.args.flow, "flow")?;
                require_missing(&self.args.operation_id, "operation_id")?;
                require_missing(&self.args.after_audit_seq, "after_audit_seq")?;
                require_missing(&self.args.limit, "limit")?;
            }
            LocalQueryName::OperationGet => {
                if self.args.operation_id.as_ref().is_none() {
                    return Err(CommandValidationError::MissingField("operation_id"));
                }
                require_missing(&self.args.flow, "flow")?;
                require_missing(&self.args.domain, "domain")?;
                require_missing(&self.args.after_audit_seq, "after_audit_seq")?;
                require_missing(&self.args.limit, "limit")?;
            }
            LocalQueryName::AuditGet => {
                if self.args.after_audit_seq.as_ref().is_none() {
                    return Err(CommandValidationError::MissingField("after_audit_seq"));
                }
                let Some(limit) = self.args.limit.as_ref().copied() else {
                    return Err(CommandValidationError::MissingField("limit"));
                };
                if limit == 0 || limit > 100 {
                    return Err(CommandValidationError::InvalidValue("limit"));
                }
                require_missing(&self.args.flow, "flow")?;
                require_missing(&self.args.domain, "domain")?;
                require_missing(&self.args.operation_id, "operation_id")?;
            }
        }
        Ok(())
    }
}

fn require_missing<T>(field: &Field<T>, name: &'static str) -> Result<(), CommandValidationError> {
    if field.is_present() {
        Err(CommandValidationError::FieldNotAllowed(name))
    } else {
        Ok(())
    }
}

/// Decodes and validates one strict A query envelope.
pub fn parse_local_query(input: &[u8]) -> Result<LocalQueryEnvelope, CommandDecodeError> {
    let value = parse_value(input, JsonLimits::LOCAL)?;
    let object = value.as_object().ok_or(CommandDecodeError::Stable(
        rlb_model::ErrorCode::MalformedMessage,
    ))?;
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("query") => {}
        Some(_) => {
            return Err(CommandDecodeError::Stable(
                rlb_model::ErrorCode::UnknownType,
            ));
        }
        None => {
            return Err(CommandDecodeError::Stable(
                rlb_model::ErrorCode::InvalidValue,
            ));
        }
    }
    if object.get("query").is_none() {
        return Err(CommandDecodeError::Stable(
            rlb_model::ErrorCode::InvalidValue,
        ));
    }
    let query: LocalQueryEnvelope = decode_value(value)?;
    query.validate()?;
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::{MemberId, RunId};

    #[test]
    fn snapshot_query_roundtrip() {
        let body = format!(
            r#"{{"v":2,"type":"query","request_id":"0de68d04e4f548acafc9c67db33cd522","query":"snapshot.get","scope":{{"member_id":"{}","run_id":"{}"}},"args":{{}}}}"#,
            MemberId::from_bytes([3; 16]).unwrap(),
            RunId::from_bytes([4; 16]).unwrap(),
        );
        let parsed = parse_local_query(body.as_bytes()).expect("query");
        assert_eq!(parsed.query, LocalQueryName::SnapshotGet);
    }

    #[test]
    fn devices_query_rejects_missing_flow() {
        let body = format!(
            r#"{{"v":2,"type":"query","request_id":"0de68d04e4f548acafc9c67db33cd522","query":"local.devices.enumerate","scope":{{"member_id":"{}","run_id":"{}"}},"args":{{}}}}"#,
            MemberId::from_bytes([3; 16]).unwrap(),
            RunId::from_bytes([4; 16]).unwrap(),
        );
        let error = parse_local_query(body.as_bytes()).expect_err("flow required");
        assert_eq!(error.code(), rlb_model::ErrorCode::InvalidValue);
    }
}
