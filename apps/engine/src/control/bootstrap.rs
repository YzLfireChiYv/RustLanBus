//! Browser bootstrap nonce and cookie/CSRF session.

use std::io::Write;
use std::net::TcpStream;
use std::time::Instant;

use super::auth::{constant_time_eq, origin_is_loopback};
use super::parse::{HttpRequest, write_json_status};
use super::*;
use crate::persist::generate_ipc_token;

pub(crate) fn handle_bootstrap(
    stream: &mut TcpStream,
    state: &ControlState,
    request: &HttpRequest,
    body: &[u8],
) -> Result<(), (u16, Vec<u8>)> {
    if request.headers.contains_key("authorization") {
        let nonce = generate_ipc_token().map_err(|_| io_fail(state))?;
        if let Ok(mut slot) = state.bootstrap.lock() {
            *slot = Some(BootstrapNonce {
                nonce: nonce.clone(),
                expires: Instant::now() + BOOTSTRAP_TTL,
                used: false,
            });
        }
        let payload = serde_json::json!({
            "v": 2,
            "type": "browser_bootstrap",
            "expires_in_s": 30,
            "nonce": nonce,
        });
        let bytes = serde_json::to_vec(&payload).map_err(|_| io_fail(state))?;
        return write_json_status(stream, 200, &bytes).map_err(|_| io_fail(state));
    }
    let origin = request.headers.get("origin").ok_or_else(|| {
        (
            403,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::OriginDenied,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        )
    })?;
    if !origin_is_loopback(origin) {
        return Err((
            403,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::OriginDenied,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ));
    }
    let offered = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("nonce")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let Some(offered) = offered else {
        return Err((
            400,
            error_envelope(
                unparseable_request_id(),
                ErrorCode::InvalidValue,
                state.member_id,
                state.run_id,
                CanonicalRevisions::default(),
            ),
        ));
    };
    let accepted = {
        let Ok(mut slot) = state.bootstrap.lock() else {
            return Err(io_fail(state));
        };
        match slot.as_mut() {
            Some(nonce)
                if !nonce.used
                    && nonce.expires > Instant::now()
                    && constant_time_eq(nonce.nonce.as_bytes(), offered.as_bytes()) =>
            {
                nonce.used = true;
                true
            }
            _ => false,
        }
    };
    if !accepted {
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
    let cookie = generate_ipc_token().map_err(|_| io_fail(state))?;
    let csrf = generate_ipc_token().map_err(|_| io_fail(state))?;
    if let Ok(mut session) = state.browser_session.lock() {
        *session = Some(BrowserSession {
            cookie: cookie.clone(),
            csrf: csrf.clone(),
        });
    }
    let payload = serde_json::json!({
        "v": 2,
        "type": "browser_bootstrap_session",
        "csrf_token": csrf,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|_| io_fail(state))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nSet-Cookie: {COOKIE_NAME}={cookie}; HttpOnly; SameSite=Strict; Path={API_ROOT}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )
    .map_err(|_| io_fail(state))?;
    stream.write_all(&bytes).map_err(|_| io_fail(state))?;
    stream.flush().map_err(|_| io_fail(state))?;
    Ok(())
}
