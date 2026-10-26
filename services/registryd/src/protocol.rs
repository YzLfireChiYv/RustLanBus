//! Error mapping and clocks for the registry daemon.

use std::time::{SystemTime, UNIX_EPOCH};

use rlb_control::Version2;
use rlb_model::ErrorCode;
use rlb_model::RequestId;
use rlb_registry::{RegistryErrorDetails, RegistryErrorScope, ServerMessage};

/// Milliseconds since Unix epoch for lease and bind clocks.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Builds a closed `error` server frame.
#[must_use]
pub fn error_message(
    reply_to: Option<RequestId>,
    code: ErrorCode,
    scope: RegistryErrorScope,
    message: impl Into<String>,
) -> ServerMessage {
    ServerMessage::Error {
        v: Version2,
        reply_to,
        code,
        scope,
        retryable: retryable(code),
        message: message.into(),
        details: RegistryErrorDetails::default(),
    }
}

/// Maps a stable code onto a coarse protocol scope.
#[must_use]
pub const fn scope_for(code: ErrorCode) -> RegistryErrorScope {
    match code {
        ErrorCode::PrefaceInvalid
        | ErrorCode::MalformedJson
        | ErrorCode::MalformedMessage
        | ErrorCode::InvalidUtf8
        | ErrorCode::FrameTooLarge
        | ErrorCode::UnknownType
        | ErrorCode::UnknownField
        | ErrorCode::DuplicateJsonKey
        | ErrorCode::ProtocolVersion
        | ErrorCode::ProtocolVersionUnsupported => RegistryErrorScope::Protocol,
        ErrorCode::RoomScopeMismatch
        | ErrorCode::RoomFull
        | ErrorCode::PendingCapacity
        | ErrorCode::RemovedCapacity
        | ErrorCode::StaleRoomRevision
        | ErrorCode::RegistryStateCorrupt => RegistryErrorScope::Room,
        ErrorCode::UnknownSession
        | ErrorCode::LeaseExpired
        | ErrorCode::StaleGeneration
        | ErrorCode::Superseded
        | ErrorCode::DuplicateIdentity => RegistryErrorScope::Session,
        ErrorCode::SourceNotFound
        | ErrorCode::SourceIdentityChanged
        | ErrorCode::InvalidCatalog => RegistryErrorScope::Source,
        ErrorCode::UnknownCommand
        | ErrorCode::CommandOutcomeUnknown
        | ErrorCode::CommandScopeMismatch
        | ErrorCode::CommandBoundaryViolation => RegistryErrorScope::Command,
        _ => RegistryErrorScope::Member,
    }
}

const fn retryable(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::RateLimited
            | ErrorCode::ServerBusy
            | ErrorCode::StaleGeneration
            | ErrorCode::StaleRoomRevision
            | ErrorCode::LeaseExpired
            | ErrorCode::MemberOffline
    )
}
