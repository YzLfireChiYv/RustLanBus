//! Canonical command hashing and pure bounded request-cache reduction.

use std::cmp::Ordering;

use rlb_model::{RequestId, RoomId, RunId};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// SHA-256 of an RFC 8785-style canonical JSON command body.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CanonicalCommandHash([u8; 32]);

impl CanonicalCommandHash {
    /// Creates a command hash from raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hexadecimal encoding used on the A wire.
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        encoded
    }

    /// Parses a 64-character lowercase hex digest.
    #[must_use]
    pub fn from_hex(value: &str) -> Option<Self> {
        if value.len() != 64 {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = digest_nibble(pair[0])?;
            let low = digest_nibble(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Some(Self(bytes))
    }
}

impl std::fmt::Debug for CanonicalCommandHash {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CanonicalCommandHash(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

impl Serialize for CanonicalCommandHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for CanonicalCommandHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HashVisitor;

        impl Visitor<'_> for HashVisitor {
            type Value = CanonicalCommandHash;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("64 lowercase hexadecimal characters")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() != 64 {
                    return Err(E::custom("canonical hash length is invalid"));
                }
                let mut bytes = [0_u8; 32];
                for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
                    let high = digest_nibble(pair[0])
                        .ok_or_else(|| E::custom("canonical hash is not lowercase hex"))?;
                    let low = digest_nibble(pair[1])
                        .ok_or_else(|| E::custom("canonical hash is not lowercase hex"))?;
                    bytes[index] = (high << 4) | low;
                }
                Ok(CanonicalCommandHash(bytes))
            }
        }

        deserializer.deserialize_str(HashVisitor)
    }
}

const fn digest_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Failure while creating a canonical command hash.
#[derive(Debug, Error)]
pub enum CanonicalHashError {
    /// Serde could not represent the command as a JSON data model value.
    #[error("command cannot be represented as canonical JSON")]
    Serialize(#[from] serde_json::Error),
}

/// Serializes a typed command canonically and hashes it with SHA-256.
pub fn canonical_command_hash<T: Serialize>(
    command: &T,
) -> Result<CanonicalCommandHash, CanonicalHashError> {
    let value = serde_json::to_value(command)?;
    let mut canonical = Vec::new();
    write_canonical(&value, &mut canonical)?;
    Ok(CanonicalCommandHash(Sha256::digest(canonical).into()))
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), serde_json::Error> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                output.extend_from_slice(value.to_string().as_bytes());
            } else if let Some(value) = number.as_u64() {
                output.extend_from_slice(value.to_string().as_bytes());
            } else {
                let value = number
                    .as_f64()
                    .expect("serde_json numbers are integers or finite f64");
                output.extend_from_slice(canonical_f64(value).as_bytes());
            }
        }
        Value::String(text) => serde_json::to_writer(output, text)?,
        Value::Array(items) => {
            output.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical(item, output)?;
            }
            output.push(b']');
        }
        Value::Object(fields) => {
            output.push(b'{');
            let mut entries: Vec<_> = fields.iter().collect();
            entries.sort_by(|(left, _), (right, _)| compare_utf16(left, right));
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                write_canonical(item, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn compare_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn canonical_f64(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let negative = value.is_sign_negative();
    let shortest = value.abs().to_string();
    let (mantissa, exponent) = shortest.split_once(['e', 'E']).map_or(
        (shortest.as_str(), 0_i32),
        |(mantissa, exponent)| {
            (
                mantissa,
                exponent
                    .parse::<i32>()
                    .expect("finite f64 exponent is an i32"),
            )
        },
    );
    let decimal_position = mantissa
        .find('.')
        .map_or_else(|| mantissa.len() as i32, |index| index as i32);
    let mut digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let leading_zeroes = digits.bytes().take_while(|byte| *byte == b'0').count();
    digits.drain(..leading_zeroes);
    let point = decimal_position + exponent - leading_zeroes as i32;
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let digit_count = digits.len() as i32;

    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if digit_count <= point && point <= 21 {
        output.push_str(&digits);
        output.extend(std::iter::repeat_n('0', (point - digit_count) as usize));
    } else if 0 < point && point <= 21 {
        let split = point as usize;
        output.push_str(&digits[..split]);
        output.push('.');
        output.push_str(&digits[split..]);
    } else if -6 < point && point <= 0 {
        output.push_str("0.");
        output.extend(std::iter::repeat_n('0', (-point) as usize));
        output.push_str(&digits);
    } else {
        output.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        let scientific_exponent = point - 1;
        output.push('e');
        if scientific_exponent >= 0 {
            output.push('+');
        }
        output.push_str(&scientific_exponent.to_string());
    }
    output
}

/// Control boundary participating in an idempotency key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestBoundary {
    Local,
    Operator,
    Member,
}

/// Stable authority scope participating in an idempotency key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestScope {
    Room { room_id: RoomId },
    Run { run_id: RunId },
}

/// Full V2 request-cache key.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCacheKey {
    principal: String,
    boundary: RequestBoundary,
    scope: RequestScope,
    request_id: RequestId,
}

impl RequestCacheKey {
    /// Creates a key after bounding its non-secret authenticated principal.
    pub fn new(
        principal: impl Into<String>,
        boundary: RequestBoundary,
        scope: RequestScope,
        request_id: RequestId,
    ) -> Result<Self, RequestKeyError> {
        let principal = principal.into();
        if principal.is_empty() || principal.len() > 128 || principal.chars().any(char::is_control)
        {
            return Err(RequestKeyError);
        }
        Ok(Self {
            principal,
            boundary,
            scope,
            request_id,
        })
    }

    /// Returns the authenticated principal label.
    #[must_use]
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// Returns the authenticated control boundary.
    #[must_use]
    pub const fn boundary(&self) -> RequestBoundary {
        self.boundary
    }

    /// Returns the stable authority scope.
    #[must_use]
    pub const fn scope(&self) -> RequestScope {
        self.scope
    }

    /// Returns the request identity.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }
}

/// Invalid bounded principal label.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("authenticated principal label must contain 1..=128 non-control UTF-8 bytes")]
pub struct RequestKeyError;

/// Capacity and minimum-retention policy for a request cache.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCachePolicy {
    pub capacity: usize,
    pub minimum_retention_ms: u64,
}

impl RequestCachePolicy {
    /// A run-local A cache: 4,096 records retained for at least 15 minutes.
    pub const LOCAL: Self = Self {
        capacity: 4_096,
        minimum_retention_ms: 15 * 60 * 1_000,
    };

    /// A member's persisted B-command cache.
    pub const MEMBER: Self = Self {
        capacity: 256,
        minimum_retention_ms: 0,
    };

    /// Authority operator ledger cache.
    pub const OPERATOR: Self = Self {
        capacity: 1_024,
        minimum_retention_ms: 10 * 60 * 1_000,
    };
}

/// One request cache record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCacheRecord<R> {
    pub key: RequestCacheKey,
    pub command_hash: CanonicalCommandHash,
    pub first_seen_ms: u64,
    pub response: Option<R>,
}

/// Serializable pure state of a bounded request cache.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCache<R> {
    pub policy: RequestCachePolicy,
    pub records: Vec<RequestCacheRecord<R>>,
}

impl<R> RequestCache<R> {
    /// Creates an empty cache.
    #[must_use]
    pub const fn new(policy: RequestCachePolicy) -> Self {
        Self {
            policy,
            records: Vec::new(),
        }
    }
}

/// Input to the pure request-cache reducer.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestCacheAction<R> {
    Begin {
        key: RequestCacheKey,
        command_hash: CanonicalCommandHash,
        now_ms: u64,
    },
    StoreResponse {
        key: RequestCacheKey,
        command_hash: CanonicalCommandHash,
        response: R,
    },
    PurgeExpired {
        now_ms: u64,
    },
}

/// Observable outcome of one request-cache reduction.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestCacheEffect<R> {
    Execute,
    Replay(Option<R>),
    RequestIdConflict,
    Stored,
    Missing,
    Full,
    Purged(usize),
}

/// Applies one idempotency action without clocks, I/O, global state, or an
/// executor. The returned cache is the only candidate state.
#[must_use]
pub fn reduce_request_cache<R: Clone>(
    current: &RequestCache<R>,
    action: RequestCacheAction<R>,
) -> (RequestCache<R>, RequestCacheEffect<R>) {
    let mut next = current.clone();
    match action {
        RequestCacheAction::Begin {
            key,
            command_hash,
            now_ms,
        } => {
            if let Some(existing) = next.records.iter().find(|record| record.key == key) {
                let effect = if existing.command_hash == command_hash {
                    RequestCacheEffect::Replay(existing.response.clone())
                } else {
                    RequestCacheEffect::RequestIdConflict
                };
                return (next, effect);
            }

            purge_for_capacity(&mut next, now_ms);
            if next.policy.capacity == 0 || next.records.len() >= next.policy.capacity {
                return (next, RequestCacheEffect::Full);
            }
            next.records.push(RequestCacheRecord {
                key,
                command_hash,
                first_seen_ms: now_ms,
                response: None,
            });
            (next, RequestCacheEffect::Execute)
        }
        RequestCacheAction::StoreResponse {
            key,
            command_hash,
            response,
        } => {
            let Some(existing) = next.records.iter_mut().find(|record| record.key == key) else {
                return (next, RequestCacheEffect::Missing);
            };
            if existing.command_hash != command_hash {
                return (next, RequestCacheEffect::RequestIdConflict);
            }
            existing.response = Some(response);
            (next, RequestCacheEffect::Stored)
        }
        RequestCacheAction::PurgeExpired { now_ms } => {
            let before = next.records.len();
            let retention = next.policy.minimum_retention_ms;
            next.records
                .retain(|record| now_ms.saturating_sub(record.first_seen_ms) < retention);
            let purged = before - next.records.len();
            (next, RequestCacheEffect::Purged(purged))
        }
    }
}

fn purge_for_capacity<R>(cache: &mut RequestCache<R>, now_ms: u64) {
    while cache.records.len() >= cache.policy.capacity {
        let retention = cache.policy.minimum_retention_ms;
        let Some(index) = cache
            .records
            .iter()
            .position(|record| now_ms.saturating_sub(record.first_seen_ms) >= retention)
        else {
            return;
        };
        cache.records.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn request_id() -> RequestId {
        RequestId::from_str("0102030405060708090a0b0c0d0e0f10").unwrap()
    }

    fn room_id() -> RoomId {
        RoomId::from_str("1112131415161718191a1b1c1d1e1f20").unwrap()
    }

    fn key() -> RequestCacheKey {
        RequestCacheKey::new(
            "operator-1",
            RequestBoundary::Operator,
            RequestScope::Room { room_id: room_id() },
            request_id(),
        )
        .unwrap()
    }

    #[test]
    fn canonical_hash_ignores_object_input_order() {
        let left: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let right: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(
            canonical_command_hash(&left).unwrap(),
            canonical_command_hash(&right).unwrap()
        );
    }

    #[test]
    fn canonical_numbers_follow_ecmascript_thresholds() {
        assert_eq!(canonical_f64(-0.0), "0");
        assert_eq!(canonical_f64(0.000_001), "0.000001");
        assert_eq!(canonical_f64(1e-7), "1e-7");
        assert_eq!(canonical_f64(1e20), "100000000000000000000");
        assert_eq!(canonical_f64(1e21), "1e+21");
        assert_eq!(canonical_f64(333_333_333.333_333_3), "333333333.3333333");
    }

    #[test]
    fn reducer_replays_equal_body_and_rejects_conflicts() {
        let hash = canonical_command_hash(&serde_json::json!({"enabled": true})).unwrap();
        let other = canonical_command_hash(&serde_json::json!({"enabled": false})).unwrap();
        let cache = RequestCache::<u8>::new(RequestCachePolicy::MEMBER);
        let (cache, effect) = reduce_request_cache(
            &cache,
            RequestCacheAction::Begin {
                key: key(),
                command_hash: hash,
                now_ms: 10,
            },
        );
        assert_eq!(effect, RequestCacheEffect::Execute);
        let (cache, effect) = reduce_request_cache(
            &cache,
            RequestCacheAction::StoreResponse {
                key: key(),
                command_hash: hash,
                response: 7,
            },
        );
        assert_eq!(effect, RequestCacheEffect::Stored);
        let (_, effect) = reduce_request_cache(
            &cache,
            RequestCacheAction::Begin {
                key: key(),
                command_hash: hash,
                now_ms: 20,
            },
        );
        assert_eq!(effect, RequestCacheEffect::Replay(Some(7)));
        let (_, effect) = reduce_request_cache(
            &cache,
            RequestCacheAction::Begin {
                key: key(),
                command_hash: other,
                now_ms: 20,
            },
        );
        assert_eq!(effect, RequestCacheEffect::RequestIdConflict);
    }

    #[test]
    fn zero_retention_capacity_evicts_only_the_oldest_record() {
        let mut cache = RequestCache::<u8>::new(RequestCachePolicy {
            capacity: 2,
            minimum_retention_ms: 0,
        });
        for offset in 0..3_u8 {
            let request_id =
                RequestId::from_bytes([offset.saturating_add(1); 16]).expect("nonzero ID");
            let request_key = RequestCacheKey::new(
                "member",
                RequestBoundary::Member,
                RequestScope::Room { room_id: room_id() },
                request_id,
            )
            .unwrap();
            let (next, effect) = reduce_request_cache(
                &cache,
                RequestCacheAction::Begin {
                    key: request_key,
                    command_hash: CanonicalCommandHash::from_bytes([offset; 32]),
                    now_ms: u64::from(offset),
                },
            );
            assert_eq!(effect, RequestCacheEffect::Execute);
            cache = next;
        }
        assert_eq!(cache.records.len(), 2);
        assert_eq!(
            cache.records[0].key.request_id,
            RequestId::from_bytes([2; 16]).unwrap()
        );
        assert_eq!(
            cache.records[1].key.request_id,
            RequestId::from_bytes([3; 16]).unwrap()
        );
    }
}
