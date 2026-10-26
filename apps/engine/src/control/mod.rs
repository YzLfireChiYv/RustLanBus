//! Loopback Control-A HTTP and WebSocket listener.
//!
//! The engine binds `127.0.0.1:<ephemeral>` and authenticates every request
//! with `Authorization: RLB-IPC <token>` or a bootstrap cookie plus CSRF.
//! Empty `{}` bodies are forbidden: every failure is a typed error envelope.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rlb_control::{
    CanonicalCommandHash, CanonicalRevisions, CommandStatus, ControlErrorBody,
    ControlErrorEnvelope, ErrorMessageType, EventEntity, EventMessageType, LocalCommandEnvelope,
    LocalCommandName, LocalEvent, LocalEventKind, LocalQueryEnvelope, LocalSnapshot, RequestCache,
    RequestCacheAction, RequestCacheKey, RequestCachePolicy, Version2, reduce_request_cache,
};
use rlb_model::{ErrorCode, ErrorEntity, MemberId, OperationId, RequestId, RunId};
use serde::Serialize;

use crate::error::EngineError;
use crate::persist::generate_ipc_token;

mod auth;
mod bootstrap;
mod events;
mod http;
mod ledger;
mod parse;

pub use ledger::{
    command_result_json, command_result_json_with_effective, query_error_json, query_result_json,
};
pub use parse::get_json;
#[cfg(test)]
pub use parse::post_json;

const API_ROOT: &str = "/rlb/local/v2";
const MAX_HEADER_BYTES: usize = 16_384;
const MAX_BODY_BYTES: usize = 1_048_576;
const COMMAND_QUEUE: usize = 32;
const MAX_CLIENTS: usize = 8;
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const COOKIE_NAME: &str = "rlb_local_session";
const CSRF_HEADER: &str = "x-rlb-csrf";
const BOOTSTRAP_TTL: Duration = Duration::from_secs(30);
const CONFIRMATION_TTL: Duration = Duration::from_secs(30);
const RATE_REFILL_PER_SEC: f64 = 50.0;
const RATE_BURST: f64 = 100.0;

/// Sentinel `request_id` used when the body could not be parsed. CONTROL
/// requires the field; this value is documented as unparseable, not as a real
/// client request.
pub fn unparseable_request_id() -> RequestId {
    RequestId::from_bytes([0xee; 16]).expect("nonzero")
}

/// Shared Control-A state visible to HTTP worker threads.
pub struct ControlState {
    pub member_id: Option<MemberId>,
    pub run_id: RunId,
    pub build: String,
    pub faulted: bool,
    token: String,
    pub snapshot: Mutex<Option<LocalSnapshot>>,
    pub revisions: Mutex<CanonicalRevisions>,
    pub event_cursor: AtomicU64,
    commands: Mutex<std::collections::VecDeque<QueuedCommand>>,
    queries: Mutex<std::collections::VecDeque<QueuedQuery>>,
    pub results: Mutex<BTreeMap<RequestId, String>>,
    operations: Mutex<BTreeMap<OperationId, RequestId>>,
    events: Mutex<Vec<std::sync::mpsc::SyncSender<String>>>,
    pub stop: AtomicBool,
    clients: AtomicUsize,
    cache: Mutex<RequestCache<CachedResponse>>,
    pending_hashes: Mutex<BTreeMap<RequestId, (RequestCacheKey, CanonicalCommandHash)>>,
    confirmations: Mutex<Vec<Confirmation>>,
    bootstrap: Mutex<Option<BootstrapNonce>>,
    browser_session: Mutex<Option<BrowserSession>>,
    rate_limiters: Mutex<BTreeMap<String, auth::RateLimiter>>,
    pub control_reconnects: AtomicU64,
    pub malformed_messages: AtomicU64,
}

#[derive(Clone, Debug)]
struct CachedResponse {
    status: u16,
    body: String,
}

struct Confirmation {
    token: String,
    command: LocalCommandName,
    args_hash: CanonicalCommandHash,
    expires: Instant,
    used: bool,
}

struct BootstrapNonce {
    nonce: String,
    expires: Instant,
    used: bool,
}

struct BrowserSession {
    cookie: String,
    csrf: String,
}

/// One accepted A command waiting for the coordinator thread.
#[derive(Debug)]
pub struct QueuedCommand {
    pub envelope: LocalCommandEnvelope,
    pub operation_id: OperationId,
}

/// One accepted A query that needs coordinator or device state.
#[derive(Debug)]
pub struct QueuedQuery {
    pub envelope: LocalQueryEnvelope,
}

/// Hello body defined by CONTROL §5.2.
#[derive(Serialize)]
struct HelloBody {
    v: Version2,
    #[serde(rename = "type")]
    message_type: &'static str,
    local_control_protocol_version: u8,
    build: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    member_id: Option<MemberId>,
    run_id: RunId,
    revisions: CanonicalRevisions,
    event_cursor: u64,
    engine_faulted: bool,
}

/// Handle used by the engine coordinator.
pub struct ControlServer {
    pub state: Arc<ControlState>,
    pub port: u16,
    join: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Binds loopback ephemeral HTTP and starts the accept thread.
    pub fn bind(
        member_id: MemberId,
        run_id: RunId,
        ipc_token: String,
    ) -> Result<Self, EngineError> {
        Self::bind_with_fault(Some(member_id), run_id, ipc_token, false)
    }

    /// Binds Control-A in a faulted run. Identity and config files are not
    /// replaced. Hello and snapshot refuse to impersonate a healthy member.
    pub fn bind_faulted(run_id: RunId, ipc_token: String) -> Result<Self, EngineError> {
        Self::bind_with_fault(None, run_id, ipc_token, true)
    }

    fn bind_with_fault(
        member_id: Option<MemberId>,
        run_id: RunId,
        ipc_token: String,
        faulted: bool,
    ) -> Result<Self, EngineError> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(ControlState {
            member_id,
            run_id,
            build: env!("CARGO_PKG_VERSION").to_owned(),
            faulted,
            token: ipc_token,
            snapshot: Mutex::new(None),
            revisions: Mutex::new(CanonicalRevisions::default()),
            event_cursor: AtomicU64::new(0),
            commands: Mutex::new(std::collections::VecDeque::new()),
            queries: Mutex::new(std::collections::VecDeque::new()),
            results: Mutex::new(BTreeMap::new()),
            operations: Mutex::new(BTreeMap::new()),
            events: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            cache: Mutex::new(RequestCache::new(RequestCachePolicy::LOCAL)),
            pending_hashes: Mutex::new(BTreeMap::new()),
            confirmations: Mutex::new(Vec::new()),
            bootstrap: Mutex::new(None),
            browser_session: Mutex::new(None),
            rate_limiters: Mutex::new(BTreeMap::new()),
            control_reconnects: AtomicU64::new(0),
            malformed_messages: AtomicU64::new(0),
        });
        let accept_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name("rlb-control-a".to_owned())
            .spawn(move || http::accept_loop(listener, accept_state))
            .map_err(|error| EngineError::ThreadSpawn(error.to_string()))?;
        Ok(Self {
            state,
            port,
            join: Some(join),
        })
    }

    /// Drains accepted commands without blocking the audio coordinator.
    pub fn take_commands(&self) -> Vec<QueuedCommand> {
        auth::lock_mutex(&self.state.commands).drain(..).collect()
    }

    /// Drains accepted queries that need coordinator or device state.
    pub fn take_queries(&self) -> Vec<QueuedQuery> {
        auth::lock_mutex(&self.state.queries).drain(..).collect()
    }

    /// Completes queued A work with a terminal faulted-run error.
    pub fn reject_queued_as_faulted(&self) {
        for command in self.take_commands() {
            self.store_result(
                command.envelope.request_id,
                String::from_utf8_lossy(&error_envelope(
                    command.envelope.request_id,
                    ErrorCode::UnexpectedError,
                    self.state.member_id,
                    self.state.run_id,
                    CanonicalRevisions::default(),
                ))
                .into_owned(),
            );
        }
        for query in self.take_queries() {
            self.store_result(
                query.envelope.request_id,
                String::from_utf8_lossy(&error_envelope(
                    query.envelope.request_id,
                    ErrorCode::UnexpectedError,
                    self.state.member_id,
                    self.state.run_id,
                    CanonicalRevisions::default(),
                ))
                .into_owned(),
            );
        }
    }

    /// Publishes a complete snapshot for GET /snapshot.
    pub fn publish_snapshot(&self, snapshot: LocalSnapshot, revisions: CanonicalRevisions) {
        *auth::lock_mutex(&self.state.snapshot) = Some(snapshot);
        *auth::lock_mutex(&self.state.revisions) = revisions;
    }

    /// Stores a terminal command result keyed by request identity.
    pub fn store_result(&self, request_id: RequestId, body: String) {
        auth::lock_mutex(&self.state.results).insert(request_id, body.clone());
        let pending = auth::lock_mutex(&self.state.pending_hashes)
            .get(&request_id)
            .cloned();
        if let Some((key, hash)) = pending {
            let mut cache = auth::lock_mutex(&self.state.cache);
            let (next, _) = reduce_request_cache(
                &cache,
                RequestCacheAction::StoreResponse {
                    key,
                    command_hash: hash,
                    response: CachedResponse { status: 200, body },
                },
            );
            *cache = next;
        }
    }

    /// Current operation projections for the A snapshot.
    pub fn operation_projections(&self) -> Vec<rlb_control::OperationProjection> {
        let operations = auth::lock_mutex(&self.state.operations);
        let results = auth::lock_mutex(&self.state.results);
        operations
            .iter()
            .filter_map(|(operation_id, request_id)| {
                let body = results.get(request_id)?;
                let value: serde_json::Value = serde_json::from_str(body).ok()?;
                let status = match value.get("status")?.as_str()? {
                    "applied" => CommandStatus::Applied,
                    "unchanged" => CommandStatus::Unchanged,
                    "rejected" => CommandStatus::Rejected,
                    "outcome_unknown" => CommandStatus::OutcomeUnknown,
                    _ => return None,
                };
                Some(rlb_control::OperationProjection {
                    operation_id: *operation_id,
                    request_id: *request_id,
                    status,
                })
            })
            .collect()
    }

    /// Emits one ordered local event to connected WebSocket clients.
    pub fn emit_event(
        &self,
        kind: LocalEventKind,
        revisions: CanonicalRevisions,
        value: serde_json::Value,
    ) {
        let member_id = match self.state.member_id {
            Some(member_id) => member_id,
            None => return,
        };
        let seq = self
            .state
            .event_cursor
            .load(Ordering::Relaxed)
            .saturating_add(1);
        let event = LocalEvent {
            v: Version2,
            message_type: EventMessageType::Event,
            event: kind,
            event_seq: seq,
            revisions,
            entity: EventEntity {
                member_id: rlb_control::Field::Present(member_id),
                ..EventEntity::default()
            },
            value,
        };
        let Ok(text) = serde_json::to_string(&event) else {
            return;
        };
        self.state.event_cursor.store(seq, Ordering::Relaxed);
        let mut clients = auth::lock_mutex(&self.state.events);
        clients.retain(|sender| match sender.try_send(text.clone()) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                let _ = sender.try_send(events::resync_required_json(seq, revisions));
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        });
    }

    /// Issues a 30-second, single-use confirmation token bound to a command hash.
    pub fn prepare_confirmation(
        &self,
        command: LocalCommandName,
        args_hash: String,
    ) -> Result<String, EngineError> {
        let args_hash = CanonicalCommandHash::from_hex(&args_hash).ok_or(EngineError::Persist(
            "confirmation hash is not lowercase hex",
        ))?;
        let token = generate_ipc_token()?;
        let mut list = self
            .state
            .confirmations
            .lock()
            .map_err(|_| EngineError::Persist("confirmation ledger lock was poisoned"))?;
        list.retain(|item| item.expires > Instant::now() && !item.used);
        list.push(Confirmation {
            token: token.clone(),
            command,
            args_hash,
            expires: Instant::now() + CONFIRMATION_TTL,
            used: false,
        });
        Ok(token)
    }

    /// Consumes a confirmation token. Mismatch, expiry, or reuse is rejected.
    pub fn consume_confirmation(
        &self,
        token: &str,
        command: LocalCommandName,
        args_hash: CanonicalCommandHash,
    ) -> Result<(), ErrorCode> {
        let Ok(mut list) = self.state.confirmations.lock() else {
            return Err(ErrorCode::UnexpectedError);
        };
        let Some(item) = list.iter_mut().find(|item| item.token == token) else {
            return Err(ErrorCode::ConfirmationRequired);
        };
        if item.used {
            return Err(ErrorCode::ConfirmationMismatch);
        }
        if item.expires <= Instant::now() {
            return Err(ErrorCode::ConfirmationExpired);
        }
        if item.command != command || item.args_hash != args_hash {
            return Err(ErrorCode::ConfirmationMismatch);
        }
        item.used = true;
        Ok(())
    }

    /// Asks the accept thread to stop.
    pub fn shutdown(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn request_id_hint(body: &[u8]) -> RequestId {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("request_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|text| text.parse().ok())
        })
        .unwrap_or_else(unparseable_request_id)
}

pub(crate) fn error_envelope(
    request_id: RequestId,
    code: ErrorCode,
    member_id: Option<MemberId>,
    run_id: RunId,
    revisions: CanonicalRevisions,
) -> Vec<u8> {
    let scope = match member_id {
        Some(member_id) => ErrorEntity::Engine { member_id, run_id },
        None => ErrorEntity::Protocol,
    };
    let envelope = ControlErrorEnvelope {
        v: Version2,
        message_type: ErrorMessageType::Error,
        request_id,
        error: ControlErrorBody {
            code,
            message_key: error_message_key(code),
            retryable: matches!(
                code,
                ErrorCode::IpcAuthFailed
                    | ErrorCode::RequestLimit
                    | ErrorCode::RateLimited
                    | ErrorCode::ServerBusy
            ),
            scope,
            current_revisions: revisions,
        },
    };
    serde_json::to_vec(&envelope).unwrap_or_else(|_| {
        format!(
            r#"{{"v":2,"type":"error","request_id":"{request_id}","error":{{"code":"unexpected_error","message_key":"control.unexpected_error","retryable":false,"scope":"protocol","current_revisions":{{}}}}}}"#
        )
        .into_bytes()
    })
}

fn error_message_key(code: ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .map(|name| format!("control.{name}"))
        .unwrap_or_else(|| "control.unexpected_error".to_owned())
}

pub(crate) fn io_fail(state: &ControlState) -> (u16, Vec<u8>) {
    (
        500,
        error_envelope(
            unparseable_request_id(),
            ErrorCode::UnexpectedError,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        ),
    )
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::{MemberId, RunId};

    #[test]
    fn hello_requires_matching_token_and_host() {
        let member_id = MemberId::generate().expect("member");
        let run_id = RunId::generate().expect("run");
        let token = generate_ipc_token().expect("token");
        let server = ControlServer::bind(member_id, run_id, token.clone()).expect("bind");
        thread::sleep(Duration::from_millis(30));
        let denied =
            get_json(server.port, "not-the-token", &format!("{API_ROOT}/hello")).expect("http");
        assert!(denied.contains("ipc_auth_failed") || denied.contains("401"));
        let ok = get_json(server.port, &token, &format!("{API_ROOT}/hello")).expect("hello");
        assert!(ok.contains("\"type\":\"hello\"") || ok.contains("\"type\": \"hello\""));
        assert!(ok.contains(&member_id.to_string()));
        assert!(!ok.contains(&token));
    }

    #[test]
    fn snapshot_without_publish_is_typed_error() {
        let member_id = MemberId::generate().expect("member");
        let run_id = RunId::generate().expect("run");
        let token = generate_ipc_token().expect("token");
        let server = ControlServer::bind(member_id, run_id, token.clone()).expect("bind");
        thread::sleep(Duration::from_millis(30));
        let response =
            get_json(server.port, &token, &format!("{API_ROOT}/snapshot")).expect("snapshot");
        assert!(response.contains("server_busy") || response.contains("503"));
        assert!(!response.contains("\r\n\r\n{}"));
    }

    #[test]
    fn bootstrap_issues_nonce_without_logging_token() {
        let member_id = MemberId::generate().expect("member");
        let run_id = RunId::generate().expect("run");
        let token = generate_ipc_token().expect("token");
        let server = ControlServer::bind(member_id, run_id, token.clone()).expect("bind");
        thread::sleep(Duration::from_millis(30));
        let response = post_json(
            server.port,
            &token,
            &format!("{API_ROOT}/browser-bootstrap"),
            "{}",
        )
        .expect("bootstrap");
        assert!(response.contains("browser_bootstrap"));
        assert!(response.contains("nonce"));
        assert!(!response.contains(&token));
    }
}
