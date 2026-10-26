//! HTTP/1.1 request framing for Control-A.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use super::*;
use crate::error::EngineError;

pub(crate) struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub content_length: usize,
}

pub(crate) fn read_headers(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 512];
    loop {
        let n = stream.read(&mut tmp).map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_HEADER_BYTES {
            return Err(());
        }
        if let Some(pos) = find_header_end(&buf) {
            let rest = buf[pos..].to_vec();
            buf.truncate(pos);
            return Ok((buf, rest));
        }
    }
}

pub(crate) fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

pub(crate) fn parse_request(bytes: &[u8]) -> Option<HttpRequest> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(parsed.parse(bytes), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    let method = parsed.method?.to_owned();
    let mut path = parsed.path?.to_owned();
    if let Some((clean, _)) = path.split_once('?') {
        path = clean.to_owned();
    }
    let mut map = BTreeMap::new();
    for header in parsed.headers.iter().filter(|h| !h.name.is_empty()) {
        map.insert(
            header.name.to_ascii_lowercase(),
            std::str::from_utf8(header.value).ok()?.trim().to_owned(),
        );
    }
    let content_length = map
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Some(HttpRequest {
        method,
        path,
        headers: map,
        content_length,
    })
}

pub(crate) fn read_body(
    stream: &mut TcpStream,
    prefix: &[u8],
    content_length: usize,
) -> Result<Vec<u8>, ()> {
    if content_length > MAX_BODY_BYTES {
        return Err(());
    }
    let mut body = Vec::with_capacity(content_length);
    let take = prefix.len().min(content_length);
    body.extend_from_slice(&prefix[..take]);
    while body.len() < content_length {
        let mut tmp = [0_u8; 1024];
        let n = stream.read(&mut tmp).map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        let remain = content_length - body.len();
        body.extend_from_slice(&tmp[..n.min(remain)]);
    }
    Ok(body)
}

pub(crate) fn write_json_status(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn get_json(port: u16, token: &str, path: &str) -> Result<String, EngineError> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: RLB-IPC {token}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(String::from_utf8_lossy(&response).into_owned())
}

/// POST helper used by control tests. The token is not logged.
#[cfg(test)]
pub fn post_json(port: u16, token: &str, path: &str, body: &str) -> Result<String, EngineError> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: RLB-IPC {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(String::from_utf8_lossy(&response).into_owned())
}
