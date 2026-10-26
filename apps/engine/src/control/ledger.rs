//! Command and query ledgers: RequestCache, confirmation, query wait.

use std::thread;
use std::time::Duration;

use rlb_control::{
    AckMessageType, AckStatus, CanonicalCommandHash, CommandAck, CommandResult, CommandStatus,
    LocalCommandEnvelope, LocalQueryName, LocalQueryResult, QueryResultMessageType,
    RequestBoundary, RequestCacheAction, RequestCacheEffect, RequestCacheKey, RequestScope,
    ResultMessageType, Version2, canonical_command_hash, parse_local_command, parse_local_query,
    reduce_request_cache,
};
use rlb_model::{ErrorCode, MemberId, OperationId, RequestId, RunId};
use serde::Serialize;

use super::auth::lock_mutex;
use super::*;

pub(crate) fn command_result_body(state: &ControlState, id_hex: &str) -> Vec<u8> {
    let Ok(request_id) = id_hex.parse::<RequestId>() else {
        return error_envelope(
            unparseable_request_id(),
            ErrorCode::InvalidValue,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        );
    };
    match state.results.lock() {
        Ok(guard) => guard
            .get(&request_id)
            .cloned()
            .map(String::into_bytes)
            .unwrap_or_else(|| {
                error_envelope(
                    request_id,
                    ErrorCode::ObjectNotFound,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                )
            }),
        Err(_) => error_envelope(
            request_id,
            ErrorCode::UnexpectedError,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        ),
    }
}

pub(crate) fn handle_command(state: &ControlState, body: &[u8]) -> (u16, Vec<u8>) {
    if state.faulted {
        return (
            503,
            error_envelope(
                request_id_hint(body),
                ErrorCode::UnexpectedError,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        );
    }
    match parse_local_command(body) {
        Ok(envelope) => begin_command(state, envelope),
        Err(error) => (
            400,
            error_envelope(
                request_id_hint(body),
                error.code(),
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ),
    }
}

pub(crate) fn begin_command(
    state: &ControlState,
    envelope: LocalCommandEnvelope,
) -> (u16, Vec<u8>) {
    let request_id = envelope.request_id;
    let Ok(hash) = canonical_command_hash(&envelope) else {
        return (
            500,
            error_envelope(
                request_id,
                ErrorCode::UnexpectedError,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        );
    };
    let Ok(key) = RequestCacheKey::new(
        match state.member_id {
            Some(member_id) => format!("member:{member_id}"),
            None => "faulted".to_owned(),
        },
        RequestBoundary::Local,
        RequestScope::Run {
            run_id: state.run_id,
        },
        request_id,
    ) else {
        return (
            400,
            error_envelope(
                request_id,
                ErrorCode::InvalidValue,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        );
    };
    let now_ms = unix_ms();
    let effect = {
        let Ok(mut cache) = state.cache.lock() else {
            return (
                500,
                error_envelope(
                    request_id,
                    ErrorCode::UnexpectedError,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            );
        };
        let (next, effect) = reduce_request_cache(
            &cache,
            RequestCacheAction::Begin {
                key: key.clone(),
                command_hash: hash,
                now_ms,
            },
        );
        *cache = next;
        effect
    };
    match effect {
        RequestCacheEffect::Replay(Some(cached)) => (cached.status, cached.body.into_bytes()),
        RequestCacheEffect::Replay(None) => match operation_id_for_request(state, request_id) {
            Some(operation_id) => (
                202,
                serde_json::to_vec(&CommandAck {
                    v: Version2,
                    message_type: AckMessageType::CommandAck,
                    request_id,
                    status: AckStatus::Accepted,
                    operation_id,
                })
                .unwrap_or_else(|_| {
                    error_envelope(
                        request_id,
                        ErrorCode::UnexpectedError,
                        state.member_id,
                        state.run_id,
                        CanonicalRevisions::default(),
                    )
                }),
            ),
            None => (
                500,
                error_envelope(
                    request_id,
                    ErrorCode::UnexpectedError,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            ),
        },
        RequestCacheEffect::RequestIdConflict => (
            409,
            error_envelope(
                request_id,
                ErrorCode::RequestIdConflict,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ),
        RequestCacheEffect::Full => (
            429,
            error_envelope(
                request_id,
                ErrorCode::RequestLimit,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ),
        RequestCacheEffect::Execute => queue_new_command(state, envelope, key, hash),
        RequestCacheEffect::Stored
        | RequestCacheEffect::Missing
        | RequestCacheEffect::Purged(_) => (
            500,
            error_envelope(
                request_id,
                ErrorCode::UnexpectedError,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ),
    }
}

pub(crate) fn queue_new_command(
    state: &ControlState,
    envelope: LocalCommandEnvelope,
    key: RequestCacheKey,
    hash: CanonicalCommandHash,
) -> (u16, Vec<u8>) {
    let request_id = envelope.request_id;
    let operation_id = match OperationId::generate() {
        Ok(id) => id,
        Err(_) => {
            return (
                500,
                error_envelope(
                    request_id,
                    ErrorCode::UnexpectedError,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            );
        }
    };
    let queued = QueuedCommand {
        envelope,
        operation_id,
    };
    let queued_ok = {
        let mut queue = lock_mutex(&state.commands);
        if queue.len() >= COMMAND_QUEUE {
            false
        } else {
            queue.push_back(queued);
            true
        }
    };
    if !queued_ok {
        return (
            429,
            error_envelope(
                request_id,
                ErrorCode::RequestLimit,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        );
    }
    if let Ok(mut pending) = state.pending_hashes.lock() {
        pending.insert(request_id, (key.clone(), hash));
    }
    if let Ok(mut operations) = state.operations.lock() {
        operations.insert(operation_id, request_id);
    }
    let ack = CommandAck {
        v: Version2,
        message_type: AckMessageType::CommandAck,
        request_id,
        status: AckStatus::Accepted,
        operation_id,
    };
    let body = serde_json::to_vec(&ack).unwrap_or_else(|_| {
        error_envelope(
            request_id,
            ErrorCode::UnexpectedError,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        )
    });
    if let Ok(mut cache) = state.cache.lock() {
        let (next, _) = reduce_request_cache(
            &cache,
            RequestCacheAction::StoreResponse {
                key,
                command_hash: hash,
                response: CachedResponse {
                    status: 202,
                    body: String::from_utf8_lossy(&body).into_owned(),
                },
            },
        );
        *cache = next;
    }
    (202, body)
}

pub(crate) fn handle_query(state: &ControlState, body: &[u8]) -> (u16, Vec<u8>) {
    if state.faulted {
        return (
            503,
            error_envelope(
                request_id_hint(body),
                ErrorCode::UnexpectedError,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        );
    }
    match parse_local_query(body) {
        Ok(envelope) => match envelope.query {
            rlb_control::LocalQueryName::SnapshotGet => match state.snapshot.lock() {
                Ok(guard) => match guard.as_ref() {
                    Some(snapshot) => {
                        let revisions = state
                            .revisions
                            .lock()
                            .map(|guard| *guard)
                            .unwrap_or_default();
                        (
                            200,
                            query_result_json(
                                envelope.request_id,
                                LocalQueryName::SnapshotGet,
                                revisions,
                                snapshot,
                            )
                            .into_bytes(),
                        )
                    }
                    None => (
                        503,
                        error_envelope(
                            envelope.request_id,
                            ErrorCode::ServerBusy,
                            state.member_id,
                            state.run_id,
                            CanonicalRevisions::default(),
                        ),
                    ),
                },
                Err(_) => io_fail(state),
            },
            rlb_control::LocalQueryName::OperationGet => {
                let Some(operation_id) = envelope.args.operation_id.as_ref().copied() else {
                    return (
                        400,
                        error_envelope(
                            envelope.request_id,
                            ErrorCode::InvalidValue,
                            state.member_id,
                            state.run_id,
                            CanonicalRevisions::default(),
                        ),
                    );
                };
                let mapped = state
                    .operations
                    .lock()
                    .ok()
                    .and_then(|guard| guard.get(&operation_id).copied());
                match mapped {
                    Some(command_request_id) => {
                        let revisions = state
                            .revisions
                            .lock()
                            .map(|guard| *guard)
                            .unwrap_or_default();
                        let terminal = state
                            .results
                            .lock()
                            .ok()
                            .and_then(|guard| guard.get(&command_request_id).cloned());
                        let result = match terminal {
                            Some(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                                Ok(value) => value,
                                Err(_) => {
                                    return (
                                        500,
                                        error_envelope(
                                            envelope.request_id,
                                            ErrorCode::UnexpectedError,
                                            state.member_id,
                                            state.run_id,
                                            revisions,
                                        ),
                                    );
                                }
                            },
                            None => serde_json::json!({
                                "status": "accepted",
                                "request_id": command_request_id,
                                "operation_id": operation_id,
                            }),
                        };
                        (
                            200,
                            query_result_json(
                                envelope.request_id,
                                LocalQueryName::OperationGet,
                                revisions,
                                result,
                            )
                            .into_bytes(),
                        )
                    }
                    None => (
                        404,
                        error_envelope(
                            envelope.request_id,
                            ErrorCode::ObjectNotFound,
                            state.member_id,
                            state.run_id,
                            CanonicalRevisions::default(),
                        ),
                    ),
                }
            }
            _ => {
                let queued_ok = {
                    let mut queue = lock_mutex(&state.queries);
                    if queue.len() >= COMMAND_QUEUE {
                        false
                    } else {
                        queue.push_back(QueuedQuery {
                            envelope: envelope.clone(),
                        });
                        true
                    }
                };
                if !queued_ok {
                    return (
                        429,
                        error_envelope(
                            envelope.request_id,
                            ErrorCode::RequestLimit,
                            state.member_id,
                            state.run_id,
                            CanonicalRevisions::default(),
                        ),
                    );
                }
                for _ in 0..40 {
                    if let Some(body) = lock_mutex(&state.results)
                        .get(&envelope.request_id)
                        .cloned()
                    {
                        return (200, body.into_bytes());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                (
                    503,
                    error_envelope(
                        envelope.request_id,
                        ErrorCode::ServerBusy,
                        state.member_id,
                        state.run_id,
                        CanonicalRevisions::default(),
                    ),
                )
            }
        },
        Err(error) => (
            400,
            error_envelope(
                request_id_hint(body),
                error.code(),
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ),
    }
}

pub(crate) fn operation_id_for_request(
    state: &ControlState,
    request_id: RequestId,
) -> Option<OperationId> {
    state
        .operations
        .lock()
        .ok()?
        .iter()
        .find_map(|(operation_id, mapped)| (*mapped == request_id).then_some(*operation_id))
}

/// Builds a closed `query.result` envelope.
pub fn query_result_json<R: Serialize>(
    request_id: RequestId,
    query: LocalQueryName,
    revisions: CanonicalRevisions,
    result: R,
) -> String {
    let envelope = LocalQueryResult {
        v: Version2,
        message_type: QueryResultMessageType::QueryResult,
        request_id,
        query,
        revisions,
        result,
    };
    serde_json::to_string(&envelope).unwrap_or_else(|_| {
        format!(
            r#"{{"v":2,"type":"error","request_id":"{request_id}","error":{{"code":"unexpected_error","message_key":"control.unexpected_error","retryable":false}}}}"#
        )
    })
}

/// Typed query failure. Does not impersonate a command result.
pub fn query_error_json(
    request_id: RequestId,
    code: ErrorCode,
    member_id: MemberId,
    run_id: RunId,
    revisions: CanonicalRevisions,
) -> String {
    String::from_utf8_lossy(&error_envelope(
        request_id,
        code,
        Some(member_id),
        run_id,
        revisions,
    ))
    .into_owned()
}

/// Builds a terminal command result for the A result query path.
pub fn command_result_json(
    request_id: RequestId,
    operation_id: OperationId,
    status: CommandStatus,
    revisions: CanonicalRevisions,
    code: Option<ErrorCode>,
    member_id: MemberId,
    run_id: RunId,
) -> String {
    let result = CommandResult::<serde_json::Value> {
        v: Version2,
        message_type: ResultMessageType::CommandResult,
        request_id,
        operation_id,
        status,
        revisions,
        effective: None,
        warnings: Vec::new(),
        error: code.map(|code| ControlErrorBody {
            code,
            message_key: error_message_key(code),
            retryable: false,
            scope: ErrorEntity::Engine { member_id, run_id },
            current_revisions: revisions,
        }),
    };
    serde_json::to_string(&result).unwrap_or_else(|_| {
        String::from_utf8_lossy(&error_envelope(
            request_id,
            ErrorCode::UnexpectedError,
            Some(member_id),
            run_id,
            revisions,
        ))
        .into_owned()
    })
}

/// Terminal result that includes a closed `effective` object.
pub fn command_result_json_with_effective(
    request_id: RequestId,
    operation_id: OperationId,
    status: CommandStatus,
    revisions: CanonicalRevisions,
    effective: serde_json::Value,
    member_id: MemberId,
    run_id: RunId,
) -> String {
    let result = CommandResult {
        v: Version2,
        message_type: ResultMessageType::CommandResult,
        request_id,
        operation_id,
        status,
        revisions,
        effective: Some(effective),
        warnings: Vec::new(),
        error: None,
    };
    serde_json::to_string(&result).unwrap_or_else(|_| {
        String::from_utf8_lossy(&error_envelope(
            request_id,
            ErrorCode::UnexpectedError,
            Some(member_id),
            run_id,
            revisions,
        ))
        .into_owned()
    })
}
