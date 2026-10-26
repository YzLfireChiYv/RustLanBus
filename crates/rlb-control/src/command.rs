//! Closed A and B command schemas.

use std::fmt;

use rlb_model::{
    Capability, CatalogRevision, Endpoint, EndpointMode, ErrorCode, GainDb, GrantRevision,
    MemberConfigRevision, MemberId, OperationId, Pan, QualityProfile, RequestId, RoomId,
    RoomRevision, RunId, SourceId,
};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::strict_json::{JsonLimits, StrictJsonError, decode_value, parse_value};

/// Protocol-major marker that only accepts and emits JSON number `2`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Version2;

impl Serialize for Version2 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(2)
    }
}

impl<'de> Deserialize<'de> for Version2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionVisitor;

        impl Visitor<'_> for VersionVisitor {
            type Value = Version2;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("protocol version number 2")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == 2 {
                    Ok(Version2)
                } else {
                    Err(E::custom("unsupported protocol version"))
                }
            }
        }

        deserializer.deserialize_u64(VersionVisitor)
    }
}

/// Presence-aware field used by command unions.
///
/// A present JSON `null` is rejected because `T` is decoded directly.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Field<T> {
    /// The field was absent.
    #[default]
    Missing,
    /// The field was present with a typed value.
    Present(T),
}

impl<T> Field<T> {
    /// Reports whether the field was present.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    /// Returns the present value by reference.
    #[must_use]
    pub const fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }

    /// Reports whether the field was absent.
    #[must_use]
    pub const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    fn required(&self, field: &'static str) -> Result<&T, CommandValidationError> {
        self.as_ref()
            .ok_or(CommandValidationError::MissingField(field))
    }
}

impl<'de, T> Deserialize<'de> for Field<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Present)
    }
}

impl<T> Serialize for Field<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Missing => serializer.serialize_unit(),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}

/// Stable validation failure for an otherwise well-formed command object.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CommandValidationError {
    /// A command omitted a required field.
    #[error("required field `{0}` is missing")]
    MissingField(&'static str),
    /// A command supplied a known field outside that command's whitelist.
    #[error("field `{0}` is outside the command whitelist")]
    FieldNotAllowed(&'static str),
    /// A typed value violated a command-specific rule.
    #[error("field `{0}` has an invalid value")]
    InvalidValue(&'static str),
}

impl CommandValidationError {
    /// Returns the stable protocol error code.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::FieldNotAllowed(_) => ErrorCode::FieldNotAllowed,
            Self::MissingField(_) | Self::InvalidValue(_) => ErrorCode::InvalidValue,
        }
    }
}

/// The only accepted command-envelope type.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMessageType {
    /// A mutation request.
    Command,
}

/// Canonical A command names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum LocalCommandName {
    #[serde(rename = "local.room.join")]
    RoomJoin,
    #[serde(rename = "local.room.leave")]
    RoomLeave,
    #[serde(rename = "local.room.reconnect")]
    RoomReconnect,
    #[serde(rename = "local.room.reconnect.cancel")]
    RoomReconnectCancel,
    #[serde(rename = "local.room.request_readmission")]
    RoomRequestReadmission,
    #[serde(rename = "local.room.auto_reconnect.set")]
    RoomAutoReconnectSet,
    #[serde(rename = "local.source.capture.set")]
    SourceCaptureSet,
    #[serde(rename = "local.source.publication.set")]
    SourcePublicationSet,
    #[serde(rename = "local.source.binding.set")]
    SourceBindingSet,
    #[serde(rename = "local.source.monitor.set")]
    SourceMonitorSet,
    #[serde(rename = "local.source.quality.set")]
    SourceQualitySet,
    #[serde(rename = "local.playback.binding.set")]
    PlaybackBindingSet,
    #[serde(rename = "local.receive_buffer.set")]
    ReceiveBufferSet,
    #[serde(rename = "local.mix.member.set")]
    MixMemberSet,
    #[serde(rename = "local.mix.source.set")]
    MixSourceSet,
    #[serde(rename = "local.emergency_stop.set")]
    EmergencyStopSet,
    #[serde(rename = "local.capability_grant.set")]
    CapabilityGrantSet,
    #[serde(rename = "local.startup.set")]
    StartupSet,
    #[serde(rename = "local.logging.set")]
    LoggingSet,
    #[serde(rename = "local.logging.delete")]
    LoggingDelete,
    #[serde(rename = "local.diagnostics.export")]
    DiagnosticsExport,
    #[serde(rename = "local.diagnostics.ab.start")]
    DiagnosticsAbStart,
    #[serde(rename = "local.diagnostics.ab.cancel")]
    DiagnosticsAbCancel,
    #[serde(rename = "local.driver.install")]
    DriverInstall,
    #[serde(rename = "local.driver.uninstall")]
    DriverUninstall,
    #[serde(rename = "local.driver.enable")]
    DriverEnable,
    #[serde(rename = "local.driver.disable")]
    DriverDisable,
    #[serde(rename = "local.engine.restart")]
    EngineRestart,
    #[serde(rename = "local.engine.shutdown")]
    EngineShutdown,
    #[serde(rename = "local.confirmation.prepare")]
    ConfirmationPrepare,
}

/// Closed identity scope used by A commands.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalScope {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub room_id: Field<RoomId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_id: Field<MemberId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub run_id: Field<RunId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub source_id: Field<SourceId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub remote_member_id: Field<MemberId>,
}

/// Closed expected-observation set used by A commands.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalExpected {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_config_revision: Field<MemberConfigRevision>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub room_revision: Field<RoomRevision>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub catalog_revision: Field<CatalogRevision>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub grant_revision: Field<GrantRevision>,
}

/// Closed receive-buffer mode accepted by A.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveBufferMode {
    Automatic,
    Manual,
}

/// Closed log-deletion selection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSelection {
    All,
    Current,
}

/// Closed local-device flow query value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceFlow {
    Capture,
    Render,
    All,
}

/// All fields known to the A command family. Per-command masks are checked by
/// [`LocalCommandEnvelope::validate`].
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalArgs {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub authority_url: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_registry_endpoint: Field<Endpoint>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub enabled: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub master_capture_enabled: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub publish_requested: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub capture_method: Field<rlb_model::CaptureMethod>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub endpoint_mode: Field<EndpointMode>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub endpoint_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub gain_db: Field<GainDb>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub pan: Field<Pan>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub mute: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub solo: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub selected_profile: Field<QualityProfile>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub mode: Field<ReceiveBufferMode>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub target_ms: Field<u16>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub capability: Field<Capability>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub start_with_os: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub start_engine_on_ui_open: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub directory: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub max_bytes: Field<u64>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub retention_count: Field<u16>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub selection: Field<LogSelection>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub confirmation_token: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub destination: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub include_optional_logs: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub candidate_a: Field<rlb_model::CaptureMethod>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub candidate_b: Field<rlb_model::CaptureMethod>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub duration_ms: Field<u32>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub operation_id: Field<OperationId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub package_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub command: Field<LocalCommandName>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub canonical_args_hash: Field<String>,
}

/// Strict A mutation envelope.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCommandEnvelope {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: CommandMessageType,
    pub request_id: RequestId,
    pub command: LocalCommandName,
    pub scope: LocalScope,
    pub expected: LocalExpected,
    pub args: LocalArgs,
}

impl LocalCommandEnvelope {
    /// Applies the command-specific scope, revision, and argument whitelist.
    pub fn validate(&self) -> Result<(), CommandValidationError> {
        use LocalCommandName as C;

        match self.command {
            C::RoomJoin => {
                self.scope.ensure(ScopeMask::ROOM)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::AUTHORITY_URL | ArgMask::REGISTRY_ENDPOINT)?;
                validate_authority_url(self.args.authority_url.required("authority_url")?)?;
                let endpoint = self
                    .args
                    .member_registry_endpoint
                    .required("member_registry_endpoint")?;
                if endpoint.port() != 17_099 || !is_lan_ipv4(endpoint.ip()) {
                    return Err(CommandValidationError::InvalidValue(
                        "member_registry_endpoint",
                    ));
                }
            }
            C::RoomLeave => self.validate_empty(ScopeMask::ROOM, ExpectedMask::MEMBER_CONFIG)?,
            C::RoomReconnect | C::RoomReconnectCancel => {
                self.validate_empty(ScopeMask::ROOM, ExpectedMask::NONE)?;
            }
            C::RoomRequestReadmission => {
                self.scope.ensure(ScopeMask::ROOM)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args.ensure(ArgMask::ENABLED)?;
                if !*self.args.enabled.required("enabled")? {
                    return Err(CommandValidationError::InvalidValue("enabled"));
                }
            }
            C::RoomAutoReconnectSet => {
                self.validate_one(
                    ScopeMask::ROOM,
                    ExpectedMask::MEMBER_CONFIG,
                    ArgMask::ENABLED,
                    "enabled",
                )?;
            }
            C::SourceCaptureSet => self.validate_one(
                ScopeMask::SOURCE,
                ExpectedMask::MEMBER_CONFIG,
                ArgMask::MASTER_CAPTURE,
                "master_capture_enabled",
            )?,
            C::SourcePublicationSet => self.validate_one(
                ScopeMask::ROOM | ScopeMask::SOURCE,
                ExpectedMask::MEMBER_CONFIG,
                ArgMask::PUBLISH_REQUESTED,
                "publish_requested",
            )?,
            C::SourceBindingSet => {
                self.scope.ensure(ScopeMask::SOURCE)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                let mode = *self.args.endpoint_mode.required("endpoint_mode")?;
                let allowed =
                    ArgMask::CAPTURE_METHOD | ArgMask::ENDPOINT_MODE | ArgMask::ENDPOINT_ID;
                self.args.ensure(allowed)?;
                self.args.capture_method.required("capture_method")?;
                validate_endpoint_choice(mode, &self.args.endpoint_id)?;
            }
            C::SourceMonitorSet => {
                self.scope.ensure(ScopeMask::SOURCE)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args.ensure(ArgMask::ENABLED | ArgMask::GAIN_DB)?;
                self.args.enabled.required("enabled")?;
                self.args.gain_db.required("gain_db")?;
            }
            C::SourceQualitySet => {
                self.validate_one(
                    ScopeMask::ROOM | ScopeMask::SOURCE,
                    ExpectedMask::MEMBER_CONFIG,
                    ArgMask::SELECTED_PROFILE,
                    "selected_profile",
                )?;
            }
            C::PlaybackBindingSet => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::ENDPOINT_MODE | ArgMask::ENDPOINT_ID)?;
                let mode = *self.args.endpoint_mode.required("endpoint_mode")?;
                validate_endpoint_choice(mode, &self.args.endpoint_id)?;
            }
            C::ReceiveBufferSet => {
                self.scope.ensure(ScopeMask::ROOM)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args.ensure(ArgMask::MODE | ArgMask::TARGET_MS)?;
                match self.args.mode.required("mode")? {
                    ReceiveBufferMode::Automatic if self.args.target_ms.is_present() => {
                        return Err(CommandValidationError::FieldNotAllowed("target_ms"));
                    }
                    ReceiveBufferMode::Manual => {
                        let target = *self.args.target_ms.required("target_ms")?;
                        if !(10..=250).contains(&target) {
                            return Err(CommandValidationError::InvalidValue("target_ms"));
                        }
                    }
                    ReceiveBufferMode::Automatic => {}
                }
            }
            C::MixMemberSet => {
                self.scope
                    .ensure(ScopeMask::ROOM | ScopeMask::REMOTE_MEMBER)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::GAIN_DB | ArgMask::MUTE | ArgMask::SOLO)?;
                self.args.gain_db.required("gain_db")?;
                self.args.mute.required("mute")?;
                self.args.solo.required("solo")?;
            }
            C::MixSourceSet => {
                self.scope
                    .ensure(ScopeMask::ROOM | ScopeMask::REMOTE_MEMBER | ScopeMask::SOURCE)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::GAIN_DB | ArgMask::PAN | ArgMask::MUTE)?;
                self.args.gain_db.required("gain_db")?;
                self.args.pan.required("pan")?;
                self.args.mute.required("mute")?;
            }
            C::EmergencyStopSet => self.validate_one(
                ScopeMask::MEMBER_RUN,
                ExpectedMask::MEMBER_CONFIG,
                ArgMask::ENABLED,
                "enabled",
            )?,
            C::CapabilityGrantSet => {
                self.scope.ensure(ScopeMask::ROOM)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args.ensure(ArgMask::CAPABILITY | ArgMask::ENABLED)?;
                self.args.capability.required("capability")?;
                self.args.enabled.required("enabled")?;
            }
            C::StartupSet => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::START_WITH_OS | ArgMask::START_ON_OPEN)?;
                self.args.start_with_os.required("start_with_os")?;
                self.args
                    .start_engine_on_ui_open
                    .required("start_engine_on_ui_open")?;
            }
            C::LoggingSet => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args.ensure(
                    ArgMask::ENABLED
                        | ArgMask::DIRECTORY
                        | ArgMask::MAX_BYTES
                        | ArgMask::RETENTION_COUNT,
                )?;
                self.args.enabled.required("enabled")?;
                validate_local_string(self.args.directory.required("directory")?, 4_096)?;
                if *self.args.max_bytes.required("max_bytes")? == 0 {
                    return Err(CommandValidationError::InvalidValue("max_bytes"));
                }
                if *self.args.retention_count.required("retention_count")? == 0 {
                    return Err(CommandValidationError::InvalidValue("retention_count"));
                }
            }
            C::LoggingDelete => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::SELECTION | ArgMask::CONFIRMATION_TOKEN)?;
                self.args.selection.required("selection")?;
                validate_nonempty(
                    self.args
                        .confirmation_token
                        .required("confirmation_token")?,
                )?;
            }
            C::DiagnosticsExport => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::NONE)?;
                self.args
                    .ensure(ArgMask::DESTINATION | ArgMask::INCLUDE_LOGS)?;
                validate_local_string(self.args.destination.required("destination")?, 4_096)?;
                self.args
                    .include_optional_logs
                    .required("include_optional_logs")?;
            }
            C::DiagnosticsAbStart => {
                self.scope.ensure(ScopeMask::SOURCE)?;
                self.expected.ensure(ExpectedMask::NONE)?;
                self.args
                    .ensure(ArgMask::CANDIDATE_A | ArgMask::CANDIDATE_B | ArgMask::DURATION_MS)?;
                self.args.candidate_a.required("candidate_a")?;
                self.args.candidate_b.required("candidate_b")?;
                if *self.args.duration_ms.required("duration_ms")? == 0 {
                    return Err(CommandValidationError::InvalidValue("duration_ms"));
                }
            }
            C::DiagnosticsAbCancel => self.validate_one(
                ScopeMask::MEMBER_RUN,
                ExpectedMask::NONE,
                ArgMask::OPERATION_ID,
                "operation_id",
            )?,
            C::DriverInstall | C::DriverUninstall | C::DriverEnable | C::DriverDisable => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::MEMBER_CONFIG)?;
                self.args
                    .ensure(ArgMask::PACKAGE_ID | ArgMask::CONFIRMATION_TOKEN)?;
                validate_nonempty(self.args.package_id.required("package_id")?)?;
                validate_nonempty(
                    self.args
                        .confirmation_token
                        .required("confirmation_token")?,
                )?;
            }
            C::EngineRestart | C::EngineShutdown => {
                self.validate_one(
                    ScopeMask::MEMBER_RUN,
                    ExpectedMask::NONE,
                    ArgMask::CONFIRMATION_TOKEN,
                    "confirmation_token",
                )?;
                validate_nonempty(
                    self.args
                        .confirmation_token
                        .required("confirmation_token")?,
                )?;
            }
            C::ConfirmationPrepare => {
                self.scope.ensure(ScopeMask::MEMBER_RUN)?;
                self.expected.ensure(ExpectedMask::NONE)?;
                self.args
                    .ensure(ArgMask::COMMAND | ArgMask::CANONICAL_ARGS_HASH)?;
                self.args.command.required("command")?;
                validate_sha256_hex(
                    self.args
                        .canonical_args_hash
                        .required("canonical_args_hash")?,
                )?;
            }
        }
        Ok(())
    }

    fn validate_empty(
        &self,
        scope: ScopeMask,
        expected: ExpectedMask,
    ) -> Result<(), CommandValidationError> {
        self.scope.ensure(scope)?;
        self.expected.ensure(expected)?;
        self.args.ensure(ArgMask::NONE)
    }

    fn validate_one(
        &self,
        scope: ScopeMask,
        expected: ExpectedMask,
        arg: ArgMask,
        field: &'static str,
    ) -> Result<(), CommandValidationError> {
        self.scope.ensure(scope)?;
        self.expected.ensure(expected)?;
        self.args.ensure(arg)?;
        if !self.args.mask().contains(arg) {
            return Err(CommandValidationError::MissingField(field));
        }
        Ok(())
    }
}

/// Decodes and validates one strict A command.
pub fn parse_local_command(input: &[u8]) -> Result<LocalCommandEnvelope, CommandDecodeError> {
    let value = parse_value(input, JsonLimits::LOCAL)?;
    validate_command_header::<LocalCommandName>(&value)?;
    let command: LocalCommandEnvelope = decode_value(value)?;
    command.validate()?;
    Ok(command)
}

/// Closed reason for an authority admission mutation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    OperatorRejected,
    OperatorRemoved,
    RoomReset,
}

/// Canonical B command names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum RoomCommandName {
    #[serde(rename = "room.name.set")]
    RoomNameSet,
    #[serde(rename = "admission.approve")]
    AdmissionApprove,
    #[serde(rename = "admission.reject")]
    AdmissionReject,
    #[serde(rename = "admission.remove")]
    AdmissionRemove,
    #[serde(rename = "source.visibility.replace")]
    SourceVisibilityReplace,
    #[serde(rename = "room.quality_cap.set")]
    RoomQualityCapSet,
    #[serde(rename = "room.qos.set")]
    RoomQosSet,
    #[serde(rename = "room.send_mute.set")]
    RoomSendMuteSet,
    #[serde(rename = "member.source_publication.set")]
    MemberSourcePublicationSet,
    #[serde(rename = "member.send_quality.set")]
    MemberSendQualitySet,
    #[serde(rename = "member.receive_mix.member.set")]
    MemberReceiveMixMemberSet,
    #[serde(rename = "member.receive_mix.source.set")]
    MemberReceiveMixSourceSet,
    #[serde(rename = "member.auto_reconnect.set")]
    MemberAutoReconnectSet,
}

/// Closed identity scope used by B commands.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomCommandScope {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub room_id: Field<RoomId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_id: Field<MemberId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub source_id: Field<SourceId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub remote_member_id: Field<MemberId>,
}

/// Closed canonical revisions used by B commands.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomCommandExpected {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub room_revision: Field<RoomRevision>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_config_revision: Field<MemberConfigRevision>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub catalog_revision: Field<CatalogRevision>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub grant_revision: Field<GrantRevision>,
}

/// All fields admitted anywhere in the B command family.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomCommandArgs {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub name: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub member_id: Field<MemberId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub reason_code: Field<AdmissionReason>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub publisher_member_id: Field<MemberId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub source_id: Field<SourceId>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub viewer_member_ids: Field<Vec<MemberId>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub profile: Field<QualityProfile>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub mode: Field<rlb_model::QosPolicy>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub enabled: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub publish_requested: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub selected_profile: Field<QualityProfile>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub gain_db: Field<GainDb>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub pan: Field<Pan>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub mute: Field<bool>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub solo: Field<bool>,
}

/// Strict B command envelope.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomCommandEnvelope {
    pub v: Version2,
    #[serde(rename = "type")]
    pub message_type: CommandMessageType,
    pub request_id: RequestId,
    pub command: RoomCommandName,
    pub scope: RoomCommandScope,
    pub expected: RoomCommandExpected,
    pub args: RoomCommandArgs,
}

/// A validated authority-owned mutation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityMutation {
    SetName {
        name: String,
    },
    Approve {
        member_id: MemberId,
    },
    Reject {
        member_id: MemberId,
    },
    Remove {
        member_id: MemberId,
        reason: AdmissionReason,
    },
    ReplaceVisibility {
        publisher_member_id: MemberId,
        source_id: SourceId,
        viewer_member_ids: Vec<MemberId>,
    },
    SetQualityCap {
        profile: QualityProfile,
    },
    SetQos {
        mode: rlb_model::QosPolicy,
    },
    SetSendMute {
        enabled: bool,
    },
}

/// A validated member-owned room mutation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedMutation {
    SetSourcePublication {
        source_id: SourceId,
        publish_requested: bool,
    },
    SetSendQuality {
        source_id: SourceId,
        selected_profile: QualityProfile,
    },
    SetMemberMix {
        remote_member_id: MemberId,
        gain_db: GainDb,
        mute: bool,
        solo: bool,
    },
    SetSourceMix {
        remote_member_id: MemberId,
        source_id: SourceId,
        gain_db: GainDb,
        pan: Pan,
        mute: bool,
    },
    SetAutoReconnect {
        enabled: bool,
    },
}

impl DelegatedMutation {
    /// The single capability allowed to authorize this mutation.
    #[must_use]
    pub const fn capability(&self) -> Capability {
        match self {
            Self::SetSourcePublication { .. } => Capability::SourcePublication,
            Self::SetSendQuality { .. } => Capability::SendQuality,
            Self::SetMemberMix { .. } | Self::SetSourceMix { .. } => Capability::ReceiveMix,
            Self::SetAutoReconnect { .. } => Capability::AutoReconnect,
        }
    }

    /// Returns the canonical B command name.
    #[must_use]
    pub const fn command_name(&self) -> RoomCommandName {
        match self {
            Self::SetSourcePublication { .. } => RoomCommandName::MemberSourcePublicationSet,
            Self::SetSendQuality { .. } => RoomCommandName::MemberSendQualitySet,
            Self::SetMemberMix { .. } => RoomCommandName::MemberReceiveMixMemberSet,
            Self::SetSourceMix { .. } => RoomCommandName::MemberReceiveMixSourceSet,
            Self::SetAutoReconnect { .. } => RoomCommandName::MemberAutoReconnectSet,
        }
    }
}

/// Canonical observations required by every delegated command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DelegatedExpected {
    pub room_revision: RoomRevision,
    pub member_config_revision: MemberConfigRevision,
    pub catalog_revision: Option<CatalogRevision>,
    pub grant_revision: GrantRevision,
}

/// Result of parsing and validating one B command.
#[derive(Clone, Debug, PartialEq)]
pub enum ValidatedRoomCommand {
    Authority {
        request_id: RequestId,
        room_id: RoomId,
        expected_room_revision: RoomRevision,
        mutation: AuthorityMutation,
    },
    Delegated(ValidatedDelegatedCommand),
}

/// Fully validated delegated member command.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ValidatedDelegatedCommand {
    pub request_id: RequestId,
    pub room_id: RoomId,
    pub member_id: MemberId,
    pub expected: DelegatedExpected,
    pub mutation: DelegatedMutation,
}

impl ValidatedDelegatedCommand {
    /// Returns the sole capability allowed for this command.
    #[must_use]
    pub const fn capability(&self) -> Capability {
        self.mutation.capability()
    }
}

impl RoomCommandEnvelope {
    /// Converts the broad closed wire union into a command-specific typed value.
    pub fn validate(&self) -> Result<ValidatedRoomCommand, CommandValidationError> {
        use RoomCommandName as C;

        let room_id = *self.scope.room_id.required("scope.room_id")?;
        match self.command {
            C::RoomNameSet => {
                self.ensure_authority(RoomArgMask::NAME)?;
                let name = self.args.name.required("args.name")?;
                validate_room_name(name)?;
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::SetName { name: name.clone() },
                })
            }
            C::AdmissionApprove => {
                self.ensure_authority(RoomArgMask::MEMBER_ID)?;
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::Approve {
                        member_id: *self.args.member_id.required("args.member_id")?,
                    },
                })
            }
            C::AdmissionReject => {
                self.ensure_authority(RoomArgMask::MEMBER_ID | RoomArgMask::REASON)?;
                if *self.args.reason_code.required("args.reason_code")?
                    != AdmissionReason::OperatorRejected
                {
                    return Err(CommandValidationError::InvalidValue("args.reason_code"));
                }
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::Reject {
                        member_id: *self.args.member_id.required("args.member_id")?,
                    },
                })
            }
            C::AdmissionRemove => {
                self.ensure_authority(RoomArgMask::MEMBER_ID | RoomArgMask::REASON)?;
                let reason = *self.args.reason_code.required("args.reason_code")?;
                if reason == AdmissionReason::OperatorRejected {
                    return Err(CommandValidationError::InvalidValue("args.reason_code"));
                }
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::Remove {
                        member_id: *self.args.member_id.required("args.member_id")?,
                        reason,
                    },
                })
            }
            C::SourceVisibilityReplace => {
                self.ensure_authority(
                    RoomArgMask::PUBLISHER | RoomArgMask::SOURCE_ID | RoomArgMask::VIEWERS,
                )?;
                let viewers = self
                    .args
                    .viewer_member_ids
                    .required("args.viewer_member_ids")?;
                if viewers.len() > 31 || has_duplicates(viewers) {
                    return Err(CommandValidationError::InvalidValue(
                        "args.viewer_member_ids",
                    ));
                }
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::ReplaceVisibility {
                        publisher_member_id: *self
                            .args
                            .publisher_member_id
                            .required("args.publisher_member_id")?,
                        source_id: *self.args.source_id.required("args.source_id")?,
                        viewer_member_ids: viewers.clone(),
                    },
                })
            }
            C::RoomQualityCapSet => {
                self.ensure_authority(RoomArgMask::PROFILE)?;
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::SetQualityCap {
                        profile: *self.args.profile.required("args.profile")?,
                    },
                })
            }
            C::RoomQosSet => {
                self.ensure_authority(RoomArgMask::MODE)?;
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::SetQos {
                        mode: *self.args.mode.required("args.mode")?,
                    },
                })
            }
            C::RoomSendMuteSet => {
                self.ensure_authority(RoomArgMask::ENABLED)?;
                Ok(ValidatedRoomCommand::Authority {
                    request_id: self.request_id,
                    room_id,
                    expected_room_revision: self.expected_room()?,
                    mutation: AuthorityMutation::SetSendMute {
                        enabled: *self.args.enabled.required("args.enabled")?,
                    },
                })
            }
            C::MemberSourcePublicationSet => self.validate_delegated(
                RoomScopeMask::ROOM | RoomScopeMask::MEMBER | RoomScopeMask::SOURCE,
                RoomExpectedMask::DELEGATED_SOURCE,
                RoomArgMask::PUBLISH,
                DelegatedMutation::SetSourcePublication {
                    source_id: *self.scope.source_id.required("scope.source_id")?,
                    publish_requested: *self
                        .args
                        .publish_requested
                        .required("args.publish_requested")?,
                },
            ),
            C::MemberSendQualitySet => self.validate_delegated(
                RoomScopeMask::ROOM | RoomScopeMask::MEMBER | RoomScopeMask::SOURCE,
                RoomExpectedMask::DELEGATED_SOURCE,
                RoomArgMask::SELECTED_PROFILE,
                DelegatedMutation::SetSendQuality {
                    source_id: *self.scope.source_id.required("scope.source_id")?,
                    selected_profile: *self
                        .args
                        .selected_profile
                        .required("args.selected_profile")?,
                },
            ),
            C::MemberReceiveMixMemberSet => self.validate_delegated(
                RoomScopeMask::ROOM | RoomScopeMask::MEMBER | RoomScopeMask::REMOTE_MEMBER,
                RoomExpectedMask::DELEGATED,
                RoomArgMask::GAIN_DB | RoomArgMask::MUTE | RoomArgMask::SOLO,
                DelegatedMutation::SetMemberMix {
                    remote_member_id: *self
                        .scope
                        .remote_member_id
                        .required("scope.remote_member_id")?,
                    gain_db: *self.args.gain_db.required("args.gain_db")?,
                    mute: *self.args.mute.required("args.mute")?,
                    solo: *self.args.solo.required("args.solo")?,
                },
            ),
            C::MemberReceiveMixSourceSet => self.validate_delegated(
                RoomScopeMask::ROOM
                    | RoomScopeMask::MEMBER
                    | RoomScopeMask::REMOTE_MEMBER
                    | RoomScopeMask::SOURCE,
                RoomExpectedMask::DELEGATED,
                RoomArgMask::GAIN_DB | RoomArgMask::PAN | RoomArgMask::MUTE,
                DelegatedMutation::SetSourceMix {
                    remote_member_id: *self
                        .scope
                        .remote_member_id
                        .required("scope.remote_member_id")?,
                    source_id: *self.scope.source_id.required("scope.source_id")?,
                    gain_db: *self.args.gain_db.required("args.gain_db")?,
                    pan: *self.args.pan.required("args.pan")?,
                    mute: *self.args.mute.required("args.mute")?,
                },
            ),
            C::MemberAutoReconnectSet => self.validate_delegated(
                RoomScopeMask::ROOM | RoomScopeMask::MEMBER,
                RoomExpectedMask::DELEGATED,
                RoomArgMask::ENABLED,
                DelegatedMutation::SetAutoReconnect {
                    enabled: *self.args.enabled.required("args.enabled")?,
                },
            ),
        }
    }

    fn ensure_authority(&self, args: RoomArgMask) -> Result<(), CommandValidationError> {
        self.scope.ensure(RoomScopeMask::ROOM)?;
        self.expected.ensure(RoomExpectedMask::ROOM)?;
        self.args.ensure(args)
    }

    fn expected_room(&self) -> Result<RoomRevision, CommandValidationError> {
        self.expected
            .room_revision
            .as_ref()
            .copied()
            .ok_or(CommandValidationError::MissingField(
                "expected.room_revision",
            ))
    }

    fn validate_delegated(
        &self,
        scope: RoomScopeMask,
        expected: RoomExpectedMask,
        args: RoomArgMask,
        mutation: DelegatedMutation,
    ) -> Result<ValidatedRoomCommand, CommandValidationError> {
        self.scope.ensure(scope)?;
        self.expected.ensure(expected)?;
        self.args.ensure(args)?;
        Ok(ValidatedRoomCommand::Delegated(ValidatedDelegatedCommand {
            request_id: self.request_id,
            room_id: *self.scope.room_id.required("scope.room_id")?,
            member_id: *self.scope.member_id.required("scope.member_id")?,
            expected: DelegatedExpected {
                room_revision: *self
                    .expected
                    .room_revision
                    .required("expected.room_revision")?,
                member_config_revision: *self
                    .expected
                    .member_config_revision
                    .required("expected.member_config_revision")?,
                catalog_revision: self.expected.catalog_revision.as_ref().copied(),
                grant_revision: *self
                    .expected
                    .grant_revision
                    .required("expected.grant_revision")?,
            },
            mutation,
        }))
    }
}

/// Decodes and validates one strict room-scoped B command.
pub fn parse_room_command(input: &[u8]) -> Result<ValidatedRoomCommand, CommandDecodeError> {
    let value = parse_value(input, JsonLimits::OPERATOR)?;
    validate_command_header::<RoomCommandName>(&value)?;
    let command: RoomCommandEnvelope = decode_value(value)?;
    Ok(command.validate()?)
}

/// Failure while decoding or validating a command.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CommandDecodeError {
    #[error(transparent)]
    Json(#[from] StrictJsonError),
    #[error(transparent)]
    Validation(#[from] CommandValidationError),
    #[error("command rejected with stable protocol code {0:?}")]
    Stable(ErrorCode),
}

impl CommandDecodeError {
    /// Returns the stable protocol error code.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Json(error) => error.code(),
            Self::Validation(error) => error.code(),
            Self::Stable(code) => code,
        }
    }
}

fn validate_command_header<N>(value: &serde_json::Value) -> Result<(), CommandDecodeError>
where
    N: for<'de> Deserialize<'de>,
{
    let object = value
        .as_object()
        .ok_or(CommandDecodeError::Stable(ErrorCode::MalformedMessage))?;
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("command") => {}
        Some(_) => return Err(CommandDecodeError::Stable(ErrorCode::UnknownType)),
        None => return Err(CommandDecodeError::Stable(ErrorCode::InvalidValue)),
    }
    let Some(command) = object.get("command") else {
        return Err(CommandDecodeError::Stable(ErrorCode::InvalidValue));
    };
    if serde_json::from_value::<N>(command.clone()).is_err() {
        return Err(CommandDecodeError::Stable(ErrorCode::UnknownCommand));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ScopeMask(u8);

impl ScopeMask {
    const NONE: Self = Self(0);
    const ROOM: Self = Self(1 << 0);
    const MEMBER: Self = Self(1 << 1);
    const RUN: Self = Self(1 << 2);
    const SOURCE: Self = Self(1 << 3);
    const REMOTE_MEMBER: Self = Self(1 << 4);
    const MEMBER_RUN: Self = Self(Self::MEMBER.0 | Self::RUN.0);
}

impl std::ops::BitOr for ScopeMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl LocalScope {
    fn mask(&self) -> ScopeMask {
        let mut mask = ScopeMask::NONE;
        if self.room_id.is_present() {
            mask = mask | ScopeMask::ROOM;
        }
        if self.member_id.is_present() {
            mask = mask | ScopeMask::MEMBER;
        }
        if self.run_id.is_present() {
            mask = mask | ScopeMask::RUN;
        }
        if self.source_id.is_present() {
            mask = mask | ScopeMask::SOURCE;
        }
        if self.remote_member_id.is_present() {
            mask = mask | ScopeMask::REMOTE_MEMBER;
        }
        mask
    }

    fn ensure(&self, expected: ScopeMask) -> Result<(), CommandValidationError> {
        ensure_mask(
            self.mask().0,
            expected.0,
            &[
                ("scope.room_id", ScopeMask::ROOM.0),
                ("scope.member_id", ScopeMask::MEMBER.0),
                ("scope.run_id", ScopeMask::RUN.0),
                ("scope.source_id", ScopeMask::SOURCE.0),
                ("scope.remote_member_id", ScopeMask::REMOTE_MEMBER.0),
            ],
        )
    }
}

#[derive(Clone, Copy)]
struct ExpectedMask(u8);

impl ExpectedMask {
    const NONE: Self = Self(0);
    const MEMBER_CONFIG: Self = Self(1 << 0);
    const ROOM: Self = Self(1 << 1);
    const CATALOG: Self = Self(1 << 2);
    const GRANT: Self = Self(1 << 3);
}

impl LocalExpected {
    fn mask(&self) -> ExpectedMask {
        ExpectedMask(
            present_bit(
                self.member_config_revision.is_present(),
                ExpectedMask::MEMBER_CONFIG.0,
            ) | present_bit(self.room_revision.is_present(), ExpectedMask::ROOM.0)
                | present_bit(self.catalog_revision.is_present(), ExpectedMask::CATALOG.0)
                | present_bit(self.grant_revision.is_present(), ExpectedMask::GRANT.0),
        )
    }

    fn ensure(&self, expected: ExpectedMask) -> Result<(), CommandValidationError> {
        ensure_mask(
            self.mask().0,
            expected.0,
            &[
                (
                    "expected.member_config_revision",
                    ExpectedMask::MEMBER_CONFIG.0,
                ),
                ("expected.room_revision", ExpectedMask::ROOM.0),
                ("expected.catalog_revision", ExpectedMask::CATALOG.0),
                ("expected.grant_revision", ExpectedMask::GRANT.0),
            ],
        )
    }
}

#[derive(Clone, Copy)]
struct ArgMask(u64);

impl ArgMask {
    const NONE: Self = Self(0);
    const AUTHORITY_URL: Self = Self(1 << 0);
    const REGISTRY_ENDPOINT: Self = Self(1 << 1);
    const ENABLED: Self = Self(1 << 2);
    const MASTER_CAPTURE: Self = Self(1 << 3);
    const PUBLISH_REQUESTED: Self = Self(1 << 4);
    const CAPTURE_METHOD: Self = Self(1 << 5);
    const ENDPOINT_MODE: Self = Self(1 << 6);
    const ENDPOINT_ID: Self = Self(1 << 7);
    const GAIN_DB: Self = Self(1 << 8);
    const PAN: Self = Self(1 << 9);
    const MUTE: Self = Self(1 << 10);
    const SOLO: Self = Self(1 << 11);
    const SELECTED_PROFILE: Self = Self(1 << 12);
    const MODE: Self = Self(1 << 13);
    const TARGET_MS: Self = Self(1 << 14);
    const CAPABILITY: Self = Self(1 << 15);
    const START_WITH_OS: Self = Self(1 << 16);
    const START_ON_OPEN: Self = Self(1 << 17);
    const DIRECTORY: Self = Self(1 << 18);
    const MAX_BYTES: Self = Self(1 << 19);
    const RETENTION_COUNT: Self = Self(1 << 20);
    const SELECTION: Self = Self(1 << 21);
    const CONFIRMATION_TOKEN: Self = Self(1 << 22);
    const DESTINATION: Self = Self(1 << 23);
    const INCLUDE_LOGS: Self = Self(1 << 24);
    const CANDIDATE_A: Self = Self(1 << 25);
    const CANDIDATE_B: Self = Self(1 << 26);
    const DURATION_MS: Self = Self(1 << 27);
    const OPERATION_ID: Self = Self(1 << 28);
    const PACKAGE_ID: Self = Self(1 << 29);
    const COMMAND: Self = Self(1 << 30);
    const CANONICAL_ARGS_HASH: Self = Self(1 << 31);

    const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for ArgMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl LocalArgs {
    fn mask(&self) -> ArgMask {
        let fields = [
            self.authority_url.is_present(),
            self.member_registry_endpoint.is_present(),
            self.enabled.is_present(),
            self.master_capture_enabled.is_present(),
            self.publish_requested.is_present(),
            self.capture_method.is_present(),
            self.endpoint_mode.is_present(),
            self.endpoint_id.is_present(),
            self.gain_db.is_present(),
            self.pan.is_present(),
            self.mute.is_present(),
            self.solo.is_present(),
            self.selected_profile.is_present(),
            self.mode.is_present(),
            self.target_ms.is_present(),
            self.capability.is_present(),
            self.start_with_os.is_present(),
            self.start_engine_on_ui_open.is_present(),
            self.directory.is_present(),
            self.max_bytes.is_present(),
            self.retention_count.is_present(),
            self.selection.is_present(),
            self.confirmation_token.is_present(),
            self.destination.is_present(),
            self.include_optional_logs.is_present(),
            self.candidate_a.is_present(),
            self.candidate_b.is_present(),
            self.duration_ms.is_present(),
            self.operation_id.is_present(),
            self.package_id.is_present(),
            self.command.is_present(),
            self.canonical_args_hash.is_present(),
        ];
        let mut mask = 0_u64;
        for (index, present) in fields.into_iter().enumerate() {
            if present {
                mask |= 1_u64 << index;
            }
        }
        ArgMask(mask)
    }

    fn ensure(&self, expected: ArgMask) -> Result<(), CommandValidationError> {
        ensure_mask(
            self.mask().0,
            expected.0,
            &[
                ("args.authority_url", ArgMask::AUTHORITY_URL.0),
                (
                    "args.member_registry_endpoint",
                    ArgMask::REGISTRY_ENDPOINT.0,
                ),
                ("args.enabled", ArgMask::ENABLED.0),
                ("args.master_capture_enabled", ArgMask::MASTER_CAPTURE.0),
                ("args.publish_requested", ArgMask::PUBLISH_REQUESTED.0),
                ("args.capture_method", ArgMask::CAPTURE_METHOD.0),
                ("args.endpoint_mode", ArgMask::ENDPOINT_MODE.0),
                ("args.endpoint_id", ArgMask::ENDPOINT_ID.0),
                ("args.gain_db", ArgMask::GAIN_DB.0),
                ("args.pan", ArgMask::PAN.0),
                ("args.mute", ArgMask::MUTE.0),
                ("args.solo", ArgMask::SOLO.0),
                ("args.selected_profile", ArgMask::SELECTED_PROFILE.0),
                ("args.mode", ArgMask::MODE.0),
                ("args.target_ms", ArgMask::TARGET_MS.0),
                ("args.capability", ArgMask::CAPABILITY.0),
                ("args.start_with_os", ArgMask::START_WITH_OS.0),
                ("args.start_engine_on_ui_open", ArgMask::START_ON_OPEN.0),
                ("args.directory", ArgMask::DIRECTORY.0),
                ("args.max_bytes", ArgMask::MAX_BYTES.0),
                ("args.retention_count", ArgMask::RETENTION_COUNT.0),
                ("args.selection", ArgMask::SELECTION.0),
                ("args.confirmation_token", ArgMask::CONFIRMATION_TOKEN.0),
                ("args.destination", ArgMask::DESTINATION.0),
                ("args.include_optional_logs", ArgMask::INCLUDE_LOGS.0),
                ("args.candidate_a", ArgMask::CANDIDATE_A.0),
                ("args.candidate_b", ArgMask::CANDIDATE_B.0),
                ("args.duration_ms", ArgMask::DURATION_MS.0),
                ("args.operation_id", ArgMask::OPERATION_ID.0),
                ("args.package_id", ArgMask::PACKAGE_ID.0),
                ("args.command", ArgMask::COMMAND.0),
                ("args.canonical_args_hash", ArgMask::CANONICAL_ARGS_HASH.0),
            ],
        )
    }
}

#[derive(Clone, Copy)]
struct RoomScopeMask(u8);

impl RoomScopeMask {
    const ROOM: Self = Self(1 << 0);
    const MEMBER: Self = Self(1 << 1);
    const SOURCE: Self = Self(1 << 2);
    const REMOTE_MEMBER: Self = Self(1 << 3);
}

impl std::ops::BitOr for RoomScopeMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl RoomCommandScope {
    fn ensure(&self, expected: RoomScopeMask) -> Result<(), CommandValidationError> {
        let actual = present_bit(self.room_id.is_present(), RoomScopeMask::ROOM.0)
            | present_bit(self.member_id.is_present(), RoomScopeMask::MEMBER.0)
            | present_bit(self.source_id.is_present(), RoomScopeMask::SOURCE.0)
            | present_bit(
                self.remote_member_id.is_present(),
                RoomScopeMask::REMOTE_MEMBER.0,
            );
        ensure_mask(
            actual,
            expected.0,
            &[
                ("scope.room_id", RoomScopeMask::ROOM.0),
                ("scope.member_id", RoomScopeMask::MEMBER.0),
                ("scope.source_id", RoomScopeMask::SOURCE.0),
                ("scope.remote_member_id", RoomScopeMask::REMOTE_MEMBER.0),
            ],
        )
    }
}

#[derive(Clone, Copy)]
struct RoomExpectedMask(u8);

impl RoomExpectedMask {
    const ROOM: Self = Self(1 << 0);
    const MEMBER_CONFIG: Self = Self(1 << 1);
    const CATALOG: Self = Self(1 << 2);
    const GRANT: Self = Self(1 << 3);
    const DELEGATED: Self = Self(Self::ROOM.0 | Self::MEMBER_CONFIG.0 | Self::GRANT.0);
    const DELEGATED_SOURCE: Self = Self(Self::DELEGATED.0 | Self::CATALOG.0);
}

impl RoomCommandExpected {
    fn ensure(&self, expected: RoomExpectedMask) -> Result<(), CommandValidationError> {
        let actual = present_bit(self.room_revision.is_present(), RoomExpectedMask::ROOM.0)
            | present_bit(
                self.member_config_revision.is_present(),
                RoomExpectedMask::MEMBER_CONFIG.0,
            )
            | present_bit(
                self.catalog_revision.is_present(),
                RoomExpectedMask::CATALOG.0,
            )
            | present_bit(self.grant_revision.is_present(), RoomExpectedMask::GRANT.0);
        ensure_mask(
            actual,
            expected.0,
            &[
                ("expected.room_revision", RoomExpectedMask::ROOM.0),
                (
                    "expected.member_config_revision",
                    RoomExpectedMask::MEMBER_CONFIG.0,
                ),
                ("expected.catalog_revision", RoomExpectedMask::CATALOG.0),
                ("expected.grant_revision", RoomExpectedMask::GRANT.0),
            ],
        )
    }
}

#[derive(Clone, Copy)]
struct RoomArgMask(u16);

impl RoomArgMask {
    const NAME: Self = Self(1 << 0);
    const MEMBER_ID: Self = Self(1 << 1);
    const REASON: Self = Self(1 << 2);
    const PUBLISHER: Self = Self(1 << 3);
    const SOURCE_ID: Self = Self(1 << 4);
    const VIEWERS: Self = Self(1 << 5);
    const PROFILE: Self = Self(1 << 6);
    const MODE: Self = Self(1 << 7);
    const ENABLED: Self = Self(1 << 8);
    const PUBLISH: Self = Self(1 << 9);
    const SELECTED_PROFILE: Self = Self(1 << 10);
    const GAIN_DB: Self = Self(1 << 11);
    const PAN: Self = Self(1 << 12);
    const MUTE: Self = Self(1 << 13);
    const SOLO: Self = Self(1 << 14);
}

impl std::ops::BitOr for RoomArgMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl RoomCommandArgs {
    fn ensure(&self, expected: RoomArgMask) -> Result<(), CommandValidationError> {
        let fields = [
            self.name.is_present(),
            self.member_id.is_present(),
            self.reason_code.is_present(),
            self.publisher_member_id.is_present(),
            self.source_id.is_present(),
            self.viewer_member_ids.is_present(),
            self.profile.is_present(),
            self.mode.is_present(),
            self.enabled.is_present(),
            self.publish_requested.is_present(),
            self.selected_profile.is_present(),
            self.gain_db.is_present(),
            self.pan.is_present(),
            self.mute.is_present(),
            self.solo.is_present(),
        ];
        let mut actual = 0_u16;
        for (index, present) in fields.into_iter().enumerate() {
            if present {
                actual |= 1_u16 << index;
            }
        }
        ensure_mask(
            u64::from(actual),
            u64::from(expected.0),
            &[
                ("args.name", u64::from(SelfMask::NAME)),
                ("args.member_id", u64::from(SelfMask::MEMBER_ID)),
                ("args.reason_code", u64::from(SelfMask::REASON)),
                ("args.publisher_member_id", u64::from(SelfMask::PUBLISHER)),
                ("args.source_id", u64::from(SelfMask::SOURCE_ID)),
                ("args.viewer_member_ids", u64::from(SelfMask::VIEWERS)),
                ("args.profile", u64::from(SelfMask::PROFILE)),
                ("args.mode", u64::from(SelfMask::MODE)),
                ("args.enabled", u64::from(SelfMask::ENABLED)),
                ("args.publish_requested", u64::from(SelfMask::PUBLISH)),
                (
                    "args.selected_profile",
                    u64::from(SelfMask::SELECTED_PROFILE),
                ),
                ("args.gain_db", u64::from(SelfMask::GAIN_DB)),
                ("args.pan", u64::from(SelfMask::PAN)),
                ("args.mute", u64::from(SelfMask::MUTE)),
                ("args.solo", u64::from(SelfMask::SOLO)),
            ],
        )
    }
}

struct SelfMask;

impl SelfMask {
    const NAME: u16 = 1 << 0;
    const MEMBER_ID: u16 = 1 << 1;
    const REASON: u16 = 1 << 2;
    const PUBLISHER: u16 = 1 << 3;
    const SOURCE_ID: u16 = 1 << 4;
    const VIEWERS: u16 = 1 << 5;
    const PROFILE: u16 = 1 << 6;
    const MODE: u16 = 1 << 7;
    const ENABLED: u16 = 1 << 8;
    const PUBLISH: u16 = 1 << 9;
    const SELECTED_PROFILE: u16 = 1 << 10;
    const GAIN_DB: u16 = 1 << 11;
    const PAN: u16 = 1 << 12;
    const MUTE: u16 = 1 << 13;
    const SOLO: u16 = 1 << 14;
}

fn ensure_mask<T>(
    actual: T,
    expected: T,
    fields: &[(&'static str, T)],
) -> Result<(), CommandValidationError>
where
    T: Copy + Eq + std::ops::BitAnd<Output = T> + std::ops::Not<Output = T>,
{
    if actual == expected {
        return Ok(());
    }
    for &(name, bit) in fields {
        if actual & bit == bit && expected & bit != bit {
            return Err(CommandValidationError::FieldNotAllowed(name));
        }
        if expected & bit == bit && actual & bit != bit {
            return Err(CommandValidationError::MissingField(name));
        }
    }
    Err(CommandValidationError::InvalidValue("command_fields"))
}

const fn present_bit(present: bool, bit: u8) -> u8 {
    if present { bit } else { 0 }
}

fn validate_endpoint_choice(
    mode: EndpointMode,
    endpoint_id: &Field<String>,
) -> Result<(), CommandValidationError> {
    match mode {
        EndpointMode::FollowDefault if endpoint_id.is_present() => {
            Err(CommandValidationError::FieldNotAllowed("endpoint_id"))
        }
        EndpointMode::Pinned => validate_local_string(endpoint_id.required("endpoint_id")?, 4_096),
        EndpointMode::FollowDefault => Ok(()),
    }
}

fn validate_authority_url(value: &str) -> Result<(), CommandValidationError> {
    validate_local_string(value, 1_024)?;
    let remainder = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .ok_or(CommandValidationError::InvalidValue("authority_url"))?;
    if value.contains(char::is_whitespace)
        || value.contains('@')
        || value.contains('#')
        || remainder.is_empty()
    {
        return Err(CommandValidationError::InvalidValue("authority_url"));
    }
    let authority = remainder.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.starts_with('[') {
        return Err(CommandValidationError::InvalidValue("authority_url"));
    }
    let host = authority.split(':').next().unwrap_or_default();
    let host_is_lan = host.parse::<std::net::Ipv4Addr>().map_or_else(
        |_| {
            host.is_ascii()
                && !host.is_empty()
                && (host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    || host.ends_with(".local")
                    || host.ends_with(".lan"))
        },
        is_lan_ipv4,
    );
    if !host_is_lan {
        return Err(CommandValidationError::InvalidValue("authority_url"));
    }
    Ok(())
}

fn is_lan_ipv4(ip: std::net::Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

fn validate_nonempty(value: &str) -> Result<(), CommandValidationError> {
    if value.is_empty() {
        Err(CommandValidationError::InvalidValue("string"))
    } else {
        Ok(())
    }
}

fn validate_local_string(value: &str, maximum: usize) -> Result<(), CommandValidationError> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        Err(CommandValidationError::InvalidValue("string"))
    } else {
        Ok(())
    }
}

fn validate_sha256_hex(value: &str) -> Result<(), CommandValidationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Err(CommandValidationError::InvalidValue("canonical_args_hash"))
    } else {
        Ok(())
    }
}

/// Canonical hash bound by `local.confirmation.prepare` and checked on consume.
///
/// The confirmation token itself is omitted so prepare can hash the intended
/// command before the token exists.
pub fn confirmation_bind_hash(
    command: LocalCommandName,
    args: &LocalArgs,
) -> Result<crate::CanonicalCommandHash, crate::CanonicalHashError> {
    let mut args = args.clone();
    args.confirmation_token = Field::Missing;
    #[derive(Serialize)]
    struct Bind<'a> {
        command: LocalCommandName,
        args: &'a LocalArgs,
    }
    crate::canonical_command_hash(&Bind {
        command,
        args: &args,
    })
}

fn validate_room_name(value: &str) -> Result<(), CommandValidationError> {
    if value.is_empty()
        || value.len() > 64
        || value
            .chars()
            .any(|character| matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'))
    {
        Err(CommandValidationError::InvalidValue("args.name"))
    } else {
        Ok(())
    }
}

fn has_duplicates(values: &[MemberId]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).any(|pair| pair[0] == pair[1])
}

#[cfg(test)]
mod tests {
    use rlb_model::{Capability, ErrorCode};

    use super::*;

    const ROOM: &str = "01010101010101010101010101010101";
    const MEMBER: &str = "02020202020202020202020202020202";
    const SOURCE: &str = "03030303030303030303030303030303";
    const REQUEST: &str = "04040404040404040404040404040404";

    fn publication_json(extra: &str) -> Vec<u8> {
        format!(
            r#"{{
                "v":2,
                "type":"command",
                "request_id":"{REQUEST}",
                "command":"member.source_publication.set",
                "scope":{{
                    "room_id":"{ROOM}",
                    "member_id":"{MEMBER}",
                    "source_id":"{SOURCE}"
                }},
                "expected":{{
                    "room_revision":"5",
                    "member_config_revision":"7",
                    "catalog_revision":"3",
                    "grant_revision":"2"
                }},
                "args":{{"publish_requested":true{extra}}}
            }}"#
        )
        .into_bytes()
    }

    #[test]
    fn delegated_command_parses_to_one_capability_and_field() {
        let command = parse_room_command(&publication_json("")).unwrap();
        let ValidatedRoomCommand::Delegated(command) = command else {
            panic!("expected delegated command");
        };
        assert_eq!(command.capability(), Capability::SourcePublication);
        assert!(matches!(
            command.mutation,
            DelegatedMutation::SetSourcePublication {
                publish_requested: true,
                ..
            }
        ));
    }

    #[test]
    fn generic_extension_fields_are_rejected_before_execution() {
        for field in [
            r#","patch":[] "#,
            r#","shell":"anything" "#,
            r#","path":"/tmp/anything" "#,
            r#","master_capture_enabled":true "#,
        ] {
            let error = parse_room_command(&publication_json(field)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::UnknownField);
        }
    }

    #[test]
    fn known_adjacent_fields_are_still_outside_each_command_whitelist() {
        let json = format!(
            r#"{{
                "v":2,
                "type":"command",
                "request_id":"{REQUEST}",
                "command":"member.source_publication.set",
                "scope":{{
                    "room_id":"{ROOM}",
                    "member_id":"{MEMBER}",
                    "source_id":"{SOURCE}"
                }},
                "expected":{{
                    "room_revision":"5",
                    "member_config_revision":"7",
                    "catalog_revision":"3",
                    "grant_revision":"2"
                }},
                "args":{{"publish_requested":true,"enabled":true}}
            }}"#
        );
        assert_eq!(
            parse_room_command(json.as_bytes()).unwrap_err().code(),
            ErrorCode::FieldNotAllowed
        );
    }

    #[test]
    fn unknown_command_and_type_have_distinct_stable_codes() {
        let unknown_command = publication_json("").replace(
            b"member.source_publication.set",
            b"member.source_publication.bad",
        );
        assert_eq!(
            parse_room_command(&unknown_command).unwrap_err().code(),
            ErrorCode::UnknownCommand
        );
        let unknown_type = publication_json("").replace(b"\"command\"", b"\"queryxx\"");
        assert_eq!(
            parse_room_command(&unknown_type).unwrap_err().code(),
            ErrorCode::UnknownType
        );
    }

    #[test]
    fn confirmation_bind_hash_ignores_confirmation_token() {
        let empty = confirmation_bind_hash(LocalCommandName::EngineShutdown, &LocalArgs::default())
            .expect("hash");
        let with_token = LocalArgs {
            confirmation_token: Field::Present("token-value".to_owned()),
            ..LocalArgs::default()
        };
        let stripped =
            confirmation_bind_hash(LocalCommandName::EngineShutdown, &with_token).expect("hash");
        assert_eq!(empty, stripped);
        let restart =
            confirmation_bind_hash(LocalCommandName::EngineRestart, &LocalArgs::default())
                .expect("hash");
        assert_ne!(empty, restart);
    }

    trait ReplaceBytes {
        fn replace(self, from: &[u8], to: &[u8]) -> Vec<u8>;
    }

    impl ReplaceBytes for Vec<u8> {
        fn replace(self, from: &[u8], to: &[u8]) -> Vec<u8> {
            let position = self
                .windows(from.len())
                .position(|window| window == from)
                .expect("fixture contains target");
            let mut output = Vec::with_capacity(self.len() - from.len() + to.len());
            output.extend_from_slice(&self[..position]);
            output.extend_from_slice(to);
            output.extend_from_slice(&self[position + from.len()..]);
            output
        }
    }
}
