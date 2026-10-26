#![forbid(unsafe_code)]

//! RustLanBus V2 single-room authority state and RLBREG2 wire contracts.
//!
//! Socket ownership remains with `registryd`. This crate provides incremental
//! framing, strict message schemas, binary endpoint binding, persistent room
//! reduction, token verification, and member-session state.

mod bind;
mod framing;
mod operator;
mod room;
mod session;
mod token;
mod wire;

pub use bind::{BIND_MEDIA_BYTES, BIND_OK_BYTES, BindMedia, BindOk, BindPacketError};
pub use framing::{
    DecoderPhase, FrameDecoder, FrameError, FrameSendQueue, MAX_FRAME_BYTES,
    MAX_RECEIVE_BUFFER_BYTES, MAX_SEND_QUEUE_BYTES, MIN_FRAME_BYTES, RLBREG2_PREFACE,
    decode_json_frame, encode_json_frame, encode_payload,
};
pub use operator::{
    OPERATOR_AUDIT_PAGE_LIMIT, OPERATOR_AUDIT_RETENTION_MS, OPERATOR_LEDGER_CAPACITY,
    OperatorAuditContext, OperatorAuditInput, OperatorAuditLedger, OperatorAuditRecord,
    OperatorCommandResult, OperatorCommandStatus, OperatorLedgerError, OperatorOutcomeInput,
    OperatorPendingRequest, OperatorPrincipalKind, OperatorRequestDecision, OperatorRequestLedger,
    OperatorState,
};
pub use room::{
    EnrollmentView, MAX_APPROVED_MEMBERS, MAX_PENDING_MEMBERS, MAX_REMOVED_MEMBERS, MemberRecord,
    PersistentRoom, PersistentRoomEvent, RoomAction, RoomReduceError, RoomState, RoomStateError,
    RoomStateLoadError, RoomTransition, VisibilityOverride, reduce_authority_command, reduce_room,
};
pub use session::{
    AuthorityCommandError, AuthorityCommandOutcome, AuthorityEvent, BIND_REPLAY_MS,
    BIND_TIMEOUT_MS, BindOutcome, CompletedMemberCommand, EVENT_RING_CAPACITY, EngineHello,
    HEARTBEAT_MS, InFlightCommand, InterruptedCommand, LEASE_MS, MAX_IN_FLIGHT_COMMANDS,
    MemberCommandRoute, MemberSession, MemberSessionState, OPERATOR_COMMAND_EVENT_CAPACITY,
    OperatorCommandEvent, RegistryAuthority, RouteError, RoutedMemberCommand, SessionError,
    SessionReady, SessionStateError, SessionTransition, SupersededSession,
};
pub use token::{
    ClaimSecret, CredentialError, MemberCredential, MemberToken, SecretGenerationError,
    SecretParseError, Sha256TokenVerifier, TokenDigest, TokenDigestVerifier,
};
pub use wire::{
    AppliedRoomRevisions, ClientMessage, ControlPresence, DecimalU64, DriverSigningState,
    LeaveReason, MediaPresence, MemberCommandEffective, MemberCommandStatus, MemberProjection,
    ObservedMediaEndpoint, PublicationBlock, RegistryErrorDetails, RegistryErrorScope, RoomEvent,
    RoomEventValue, RoomSnapshot, RoomView, SafeLocalFields, SelfView, ServerMessage,
    SnapshotMember, SnapshotReason, SourceSlot, SupersedeReason, WireDecodeError,
    WireValidationError, parse_client_message, parse_server_message,
};
