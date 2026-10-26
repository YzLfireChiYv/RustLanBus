//! Accept loop and one-connection HTTP dispatch.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use rlb_control::CanonicalRevisions;
use rlb_model::ErrorCode;

use super::auth::{authorized, host_is_loopback, rate_allow};
use super::bootstrap::handle_bootstrap;
use super::events::upgrade_websocket;
use super::ledger::{command_result_body, handle_command, handle_query};
use super::parse::{parse_request, read_body, read_headers, write_json_status};
use super::*;

pub(crate) fn accept_loop(listener: TcpListener, state: Arc<ControlState>) {
    while !state.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if state.clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                    let body = error_envelope(
                        unparseable_request_id(),
                        ErrorCode::RequestLimit,
                        state.member_id,
                        state.run_id,
                        CanonicalRevisions::default(),
                    );
                    let _ = write_json_status(&mut stream, 429, &body);
                    continue;
                }
                state.clients.fetch_add(1, Ordering::Relaxed);
                let worker = Arc::clone(&state);
                if thread::Builder::new()
                    .name("rlb-control-conn".to_owned())
                    .spawn(move || {
                        handle_connection(stream, worker);
                    })
                    .is_err()
                {
                    state.clients.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub(crate) fn handle_connection(mut stream: TcpStream, state: Arc<ControlState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let result = serve_one(&mut stream, &state);
    if let Err((status, body)) = result {
        let _ = write_json_status(&mut stream, status, &body);
    }
    state.clients.fetch_sub(1, Ordering::Relaxed);
}

pub(crate) fn serve_one(
    stream: &mut TcpStream,
    state: &Arc<ControlState>,
) -> Result<(), (u16, Vec<u8>)> {
    let (header_bytes, rest) = read_headers(stream).map_err(|_| {
        (
            400,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::MalformedMessage,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        )
    })?;
    let request = parse_request(&header_bytes).ok_or_else(|| {
        (
            400,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::MalformedMessage,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        )
    })?;
    if !host_is_loopback(&request) {
        return Err((
            400,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::OriginDenied,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ));
    }
    if !rate_allow(state, &request) {
        return Err((
            429,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::RateLimited,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ));
    }
    let is_bootstrap_redeem = request.method == "POST"
        && request.path == format!("{API_ROOT}/browser-bootstrap")
        && !request.headers.contains_key("authorization");
    let mutation = request.method == "POST";
    if !is_bootstrap_redeem && !authorized(&request, state, mutation) {
        return Err((
            401,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::IpcAuthFailed,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ));
    }
    let path = request.path.clone();
    if request.method == "GET" && path == format!("{API_ROOT}/hello") {
        if state.faulted {
            return Err((
                503,
                error_envelope(
                    unparseable_request_id(),
                    ErrorCode::UnexpectedError,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            ));
        }
        let body = hello_body(state);
        return write_json_status(stream, 200, &body).map_err(|_| {
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
        });
    }
    if request.method == "GET" && path == format!("{API_ROOT}/snapshot") {
        return send_snapshot(stream, state);
    }
    if request.method == "GET" && path.starts_with(&format!("{API_ROOT}/commands/")) {
        let id_hex = path.trim_start_matches(&format!("{API_ROOT}/commands/"));
        let body = command_result_body(state, id_hex);
        return write_json_status(stream, 200, &body).map_err(|_| io_fail(state));
    }
    if request.method == "POST" && path == format!("{API_ROOT}/commands") {
        let body = read_body(stream, &rest, request.content_length).map_err(|_| {
            (
                400,
                error_envelope(
                    unparseable_request_id(),
                    ErrorCode::MalformedMessage,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            )
        })?;
        let (status, response) = handle_command(state, &body);
        return write_json_status(stream, status, &response).map_err(|_| io_fail(state));
    }
    if request.method == "POST" && path == format!("{API_ROOT}/queries") {
        let body = read_body(stream, &rest, request.content_length).map_err(|_| {
            (
                400,
                error_envelope(
                    unparseable_request_id(),
                    ErrorCode::MalformedMessage,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            )
        })?;
        let (status, response) = handle_query(state, &body);
        return write_json_status(stream, status, &response).map_err(|_| io_fail(state));
    }
    if request.method == "POST" && path == format!("{API_ROOT}/browser-bootstrap") {
        let body = read_body(stream, &rest, request.content_length).unwrap_or_default();
        return handle_bootstrap(stream, state, &request, &body);
    }
    if request.method == "GET" && path == format!("{API_ROOT}/events") {
        upgrade_websocket(stream, request, Arc::clone(state));
        return Ok(());
    }
    Err((
        404,
        error_envelope(
            unparseable_request_id(),
            ErrorCode::ObjectNotFound,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        ),
    ))
}

pub(crate) fn send_snapshot(
    stream: &mut TcpStream,
    state: &ControlState,
) -> Result<(), (u16, Vec<u8>)> {
    if state.faulted {
        return Err((
            503,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::UnexpectedError,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ));
    }
    match state.snapshot.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(snapshot) => {
                let body = serde_json::to_vec(snapshot).map_err(|_| io_fail(state))?;
                write_json_status(stream, 200, &body).map_err(|_| io_fail(state))
            }
            None => Err((
                503,
                error_envelope(
                    unparseable_request_id(),
                    ErrorCode::ServerBusy,
                    state.member_id,
                    state.run_id,
                    CanonicalRevisions::default(),
                ),
            )),
        },
        Err(_) => Err(io_fail(state)),
    }
}

pub(crate) fn hello_body(state: &ControlState) -> Vec<u8> {
    let revisions = state
        .revisions
        .lock()
        .map(|guard| *guard)
        .unwrap_or_default();
    serde_json::to_vec(&HelloBody {
        v: Version2,
        message_type: "hello",
        local_control_protocol_version: 2,
        build: state.build.clone(),
        member_id: state.member_id,
        run_id: state.run_id,
        revisions,
        event_cursor: state.event_cursor.load(Ordering::Relaxed),
        engine_faulted: state.faulted,
    })
    .unwrap_or_else(|_| {
        error_envelope(
            unparseable_request_id(),
            ErrorCode::UnexpectedError,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        )
    })
}
