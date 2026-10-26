//! Host, Origin, token/cookie, and cross-connection rate limit.

use std::sync::Mutex;
use std::time::Instant;

use super::parse::HttpRequest;
use super::*;

pub(crate) fn host_is_loopback(request: &HttpRequest) -> bool {
    let Some(host) = request.headers.get("host") else {
        return false;
    };
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case("127.0.0.1")
}

pub(crate) fn origin_is_loopback(origin: &str) -> bool {
    origin.starts_with("http://127.0.0.1")
}

pub(crate) fn authorized(request: &HttpRequest, state: &ControlState, mutation: bool) -> bool {
    if let Some(header) = request.headers.get("authorization")
        && let Some(provided) = header.strip_prefix("RLB-IPC ")
    {
        return constant_time_eq(provided.as_bytes(), state.token.as_bytes());
    }
    let Some(cookie_header) = request.headers.get("cookie") else {
        return false;
    };
    let Some(cookie) = cookie_value(cookie_header, COOKIE_NAME) else {
        return false;
    };
    let Ok(session) = state.browser_session.lock() else {
        return false;
    };
    let Some(session) = session.as_ref() else {
        return false;
    };
    if !constant_time_eq(cookie.as_bytes(), session.cookie.as_bytes()) {
        return false;
    }
    if mutation {
        let Some(origin) = request.headers.get("origin") else {
            return false;
        };
        if !origin_is_loopback(origin) {
            return false;
        }
        let Some(csrf) = request.headers.get(CSRF_HEADER) else {
            return false;
        };
        return constant_time_eq(csrf.as_bytes(), session.csrf.as_bytes());
    }
    true
}

pub(crate) fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub(crate) fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn rate_allow(state: &ControlState, request: &HttpRequest) -> bool {
    let key = request
        .headers
        .get("authorization")
        .cloned()
        .or_else(|| {
            request
                .headers
                .get("cookie")
                .and_then(|header| cookie_value(header, COOKIE_NAME).map(str::to_owned))
        })
        .unwrap_or_else(|| "anonymous".to_owned());
    let mut limiters = lock_mutex(&state.rate_limiters);
    limiters.entry(key).or_default().allow()
}

pub(crate) struct RateLimiter {
    tokens: f64,
    last: Instant,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            tokens: RATE_BURST,
            last: Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        let elapsed = self.last.elapsed().as_secs_f64();
        self.last = Instant::now();
        self.tokens = (self.tokens + elapsed * RATE_REFILL_PER_SEC).min(RATE_BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
