#![forbid(unsafe_code)]

//! Closed RustLanBus V2 local and room-control contracts.
//!
//! Wire objects use strict serde schemas. Command parsing produces typed,
//! command-specific mutations before any executor sees a request. The pure
//! reducers in this crate perform no socket, process, or filesystem operations.

mod command;
mod envelope;
mod idempotency;
mod member_reducer;
pub mod strict_json;

pub use command::{
    AdmissionReason, AuthorityMutation, CommandDecodeError, CommandMessageType,
    CommandValidationError, DelegatedExpected, DelegatedMutation, DeviceFlow, Field, LocalArgs,
    LocalCommandEnvelope, LocalCommandName, LocalExpected, LocalScope, LogSelection,
    ReceiveBufferMode, RoomCommandArgs, RoomCommandEnvelope, RoomCommandExpected, RoomCommandName,
    RoomCommandScope, ValidatedDelegatedCommand, ValidatedRoomCommand, Version2,
    confirmation_bind_hash, parse_local_command, parse_room_command,
};
pub use envelope::{
    AckMessageType, AckStatus, ActiveCommandSummary, AdmissionProjection, CanonicalRevisions,
    CommandAck, CommandResult, CommandStatus, ControlErrorBody, ControlErrorEnvelope,
    DiagnosticsProjection, DriverProjection, EngineProjection, EngineState, ErrorMessageType,
    EventEntity, EventMessageType, LifecycleProjection, LocalEvent, LocalEventKind, LocalQueryArgs,
    LocalQueryEnvelope, LocalQueryName, LocalQueryResult, LocalQueryScope, LocalRoomProjection,
    LocalSnapshot, LoggingProjection, MemberMixProjection, MixerProjection, OnlineMemberSummary,
    OperationProjection, OperatorEvent, OperatorEventKind, OperatorSnapshot, PermissionProjection,
    PlaybackProjection, QueryMessageType, QueryResultMessageType, ResultMessageType,
    SourceMixProjection, SourceVisibilityProjection, WarningCode, parse_local_query,
};
pub use idempotency::{
    CanonicalCommandHash, CanonicalHashError, RequestBoundary, RequestCache, RequestCacheAction,
    RequestCacheEffect, RequestCacheKey, RequestCachePolicy, RequestCacheRecord, RequestKeyError,
    RequestScope, canonical_command_hash, reduce_request_cache,
};
pub use member_reducer::{
    DelegatedApplyError, DelegatedEffective, DelegatedMemberState, DelegatedStateError,
    DelegatedTransition, MemberSourceConfig, RemoteMemberMix, RemoteSourceMix,
    reduce_delegated_command,
};
