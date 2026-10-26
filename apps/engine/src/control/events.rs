//! WebSocket event fan-out and overflow resync.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use base64::Engine as _;
use rlb_control::CanonicalRevisions;
use sha1::{Digest, Sha1};

use super::auth::origin_is_loopback;
use super::parse::{HttpRequest, write_json_status};
use super::*;

pub(crate) fn upgrade_websocket(
    stream: &mut TcpStream,
    request: HttpRequest,
    state: Arc<ControlState>,
) {
    if let Some(origin) = request.headers.get("origin")
        && !origin_is_loopback(origin)
    {
        let body = error_envelope(
            unparseable_request_id(),
            ErrorCode::OriginDenied,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        );
        let _ = write_json_status(stream, 403, &body);
        return;
    }
    let Some(key) = request.headers.get("sec-websocket-key") else {
        let body = error_envelope(
            unparseable_request_id(),
            ErrorCode::InvalidValue,
            state.member_id,
            state.run_id,
            CanonicalRevisions::default(),
        );
        let _ = write_json_status(stream, 400, &body);
        return;
    };
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID);
    let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
    let _ = write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    let _ = stream.flush();
    let (tx, rx) = sync_channel::<String>(16);
    if let Ok(mut clients) = state.events.lock() {
        clients.push(tx);
    }
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut incoming = Vec::new();
    while !state.stop.load(Ordering::Relaxed) {
        while let Ok(text) = rx.try_recv() {
            if write_ws_text(stream, &text).is_err() {
                return;
            }
        }
        let mut peek = [0_u8; 256];
        match stream.read(&mut peek) {
            Ok(0) => return,
            Ok(n) => {
                incoming.extend_from_slice(&peek[..n]);
                match decode_ws_frame(&incoming) {
                    WsDecode::NeedMore => {}
                    WsDecode::Ping(payload, consumed) => {
                        incoming.drain(..consumed);
                        let _ = write_ws_pong(stream, &payload);
                    }
                    WsDecode::Pong(consumed) => {
                        incoming.drain(..consumed);
                    }
                    WsDecode::Close => {
                        let _ = write_ws_close(stream);
                        return;
                    }
                    WsDecode::Text | WsDecode::Binary | WsDecode::Invalid => {
                        let _ = write_ws_close(stream);
                        return;
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }
    }
}

pub(crate) enum WsDecode {
    NeedMore,
    Ping(Vec<u8>, usize),
    Pong(usize),
    Close,
    Text,
    Binary,
    Invalid,
}

pub(crate) fn decode_ws_frame(bytes: &[u8]) -> WsDecode {
    if bytes.len() < 2 {
        return WsDecode::NeedMore;
    }
    let opcode = bytes[0] & 0x0f;
    let masked = bytes[1] & 0x80 != 0;
    let mut len = u64::from(bytes[1] & 0x7f);
    let mut offset = 2_usize;
    if len == 126 {
        if bytes.len() < 4 {
            return WsDecode::NeedMore;
        }
        len = u64::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        offset = 4;
    } else if len == 127 {
        if bytes.len() < 10 {
            return WsDecode::NeedMore;
        }
        len = u64::from_be_bytes(bytes[2..10].try_into().unwrap_or([0; 8]));
        offset = 10;
    }
    let mask = if masked {
        if bytes.len() < offset + 4 {
            return WsDecode::NeedMore;
        }
        let mask = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        offset += 4;
        Some(mask)
    } else {
        None
    };
    let total = offset.saturating_add(len as usize);
    if bytes.len() < total {
        return WsDecode::NeedMore;
    }
    let mut payload = bytes[offset..total].to_vec();
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    match opcode {
        0x8 => WsDecode::Close,
        0x9 => WsDecode::Ping(payload, total),
        0xA => WsDecode::Pong(total),
        0x1 => WsDecode::Text,
        0x2 => WsDecode::Binary,
        _ => WsDecode::Invalid,
    }
}

pub(crate) fn write_ws_text(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    write_ws_frame(stream, 0x81, text.as_bytes())
}

pub(crate) fn write_ws_pong(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    write_ws_frame(stream, 0x8A, payload)
}

pub(crate) fn write_ws_close(stream: &mut TcpStream) -> std::io::Result<()> {
    write_ws_frame(stream, 0x88, &[])
}

pub(crate) fn write_ws_frame(
    stream: &mut TcpStream,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut header = Vec::new();
    header.push(opcode);
    if payload.len() < 126 {
        header.push(payload.len() as u8);
    } else if payload.len() <= 65_535 {
        header.push(126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

pub(crate) fn resync_required_json(event_seq: u64, revisions: CanonicalRevisions) -> String {
    serde_json::to_string(&serde_json::json!({
        "v": 2,
        "type": "event",
        "event": "resync_required",
        "event_seq": event_seq,
        "revisions": revisions,
        "entity": {},
        "value": { "reason": "event_overflow" }
    }))
    .unwrap_or_else(|_| {
        r#"{"v":2,"type":"event","event":"resync_required","event_seq":0,"revisions":{},"entity":{},"value":{}}"#
            .to_owned()
    })
}
