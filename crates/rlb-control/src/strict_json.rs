//! Bounded JSON decoding shared by the control and registry protocols.

use std::collections::HashSet;
use std::fmt;

use rlb_model::ErrorCode;
use serde::Deserializer;
use serde::de::{DeserializeOwned, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use thiserror::Error;

const DUPLICATE_KEY_MARKER: &str = "rlb_duplicate_json_key";
const DEPTH_MARKER: &str = "rlb_json_depth_exceeded";
const FIELD_COUNT_MARKER: &str = "rlb_json_field_count_exceeded";
const ARRAY_COUNT_MARKER: &str = "rlb_json_array_count_exceeded";
const STRING_LENGTH_MARKER: &str = "rlb_json_string_too_long";
const INTEGER_RANGE_MARKER: &str = "rlb_json_integer_out_of_range";
const NON_FINITE_MARKER: &str = "rlb_json_non_finite";
const JS_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;

/// Resource limits applied before a JSON value reaches a protocol schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonLimits {
    /// Maximum encoded document size.
    pub max_document_bytes: usize,
    /// Maximum number of nested JSON containers, including the root object.
    pub max_depth: usize,
    /// Maximum fields in any one object.
    pub max_object_fields: usize,
    /// Maximum elements in any one array.
    pub max_array_elements: usize,
    /// Maximum encoded UTF-8 bytes in a key or string value.
    pub max_string_bytes: usize,
    /// Whether integral JSON tokens are restricted to IEEE-754 safe magnitude.
    pub safe_integer_magnitude: bool,
}

impl JsonLimits {
    /// V2 local-control limits.
    pub const LOCAL: Self = Self {
        max_document_bytes: 1_048_576,
        max_depth: 16,
        max_object_fields: 256,
        max_array_elements: 4_096,
        max_string_bytes: 16_384,
        safe_integer_magnitude: true,
    };

    /// V2 operator-control limits.
    pub const OPERATOR: Self = Self {
        max_document_bytes: 65_536,
        max_depth: 12,
        max_object_fields: 128,
        max_array_elements: 4_096,
        max_string_bytes: 4_096,
        safe_integer_magnitude: true,
    };

    /// V2 RLBREG2 frame JSON limits.
    pub const REGISTRY: Self = Self {
        max_document_bytes: 65_536,
        max_depth: 8,
        max_object_fields: 128,
        max_array_elements: 4_096,
        max_string_bytes: 1_024,
        safe_integer_magnitude: false,
    };
}

/// Stable failure returned by bounded JSON decoding.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StrictJsonError {
    /// The encoded document exceeded the boundary limit.
    #[error("JSON document exceeds its boundary size limit")]
    MessageTooLarge,
    /// Input bytes were not UTF-8.
    #[error("JSON document is not valid UTF-8")]
    InvalidUtf8,
    /// A UTF-8 byte-order mark was present.
    #[error("UTF-8 BOM is forbidden")]
    Utf8Bom,
    /// JSON syntax, trailing data, or a scalar representation was malformed.
    #[error("malformed JSON")]
    Malformed,
    /// The root value was not an object.
    #[error("JSON root must be an object")]
    RootNotObject,
    /// An object repeated a decoded key.
    #[error("JSON object contains a duplicate key")]
    DuplicateKey,
    /// Container nesting exceeded the boundary limit.
    #[error("JSON nesting exceeds its boundary limit")]
    DepthExceeded,
    /// One object contained too many fields.
    #[error("JSON object contains too many fields")]
    TooManyFields,
    /// One array contained too many elements.
    #[error("JSON array contains too many elements")]
    TooManyElements,
    /// One key or string exceeded the boundary limit.
    #[error("JSON string exceeds its boundary limit")]
    StringTooLong,
    /// An integral token exceeded the safe control range.
    #[error("JSON integer exceeds its boundary range")]
    IntegerOutOfRange,
    /// A deserializer produced a non-finite floating-point marker.
    #[error("JSON number is not finite")]
    NonFinite,
    /// A typed schema rejected the bounded JSON value.
    #[error("JSON value does not match the closed protocol schema")]
    Schema,
    /// A closed object contained a field outside its whitelist.
    #[error("JSON object contains an unknown field")]
    UnknownField,
    /// A closed enum contained an unknown value.
    #[error("JSON enum contains an unknown value")]
    UnknownVariant,
    /// A protocol-major marker was not version 2.
    #[error("protocol version is unsupported")]
    ProtocolVersion,
}

impl StrictJsonError {
    /// Returns the protocol-stable error code.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::MessageTooLarge => ErrorCode::MessageTooLarge,
            Self::InvalidUtf8 => ErrorCode::InvalidUtf8,
            Self::DuplicateKey => ErrorCode::DuplicateJsonKey,
            Self::DepthExceeded => ErrorCode::JsonDepthExceeded,
            Self::UnknownField => ErrorCode::UnknownField,
            Self::UnknownVariant => ErrorCode::UnknownType,
            Self::ProtocolVersion => ErrorCode::ProtocolVersionUnsupported,
            Self::NonFinite => ErrorCode::InvalidValue,
            Self::Utf8Bom
            | Self::Malformed
            | Self::RootNotObject
            | Self::TooManyFields
            | Self::TooManyElements
            | Self::StringTooLong
            | Self::IntegerOutOfRange
            | Self::Schema => ErrorCode::MalformedMessage,
        }
    }

    fn from_serde(error: &serde_json::Error) -> Self {
        let message = error.to_string();
        if message.contains(DUPLICATE_KEY_MARKER) {
            Self::DuplicateKey
        } else if message.contains(DEPTH_MARKER) {
            Self::DepthExceeded
        } else if message.contains(FIELD_COUNT_MARKER) {
            Self::TooManyFields
        } else if message.contains(ARRAY_COUNT_MARKER) {
            Self::TooManyElements
        } else if message.contains(STRING_LENGTH_MARKER) {
            Self::StringTooLong
        } else if message.contains(INTEGER_RANGE_MARKER) {
            Self::IntegerOutOfRange
        } else if message.contains(NON_FINITE_MARKER) {
            Self::NonFinite
        } else {
            Self::Malformed
        }
    }
}

/// Parses a bounded JSON object while retaining duplicate-key detection.
pub fn parse_value(input: &[u8], limits: JsonLimits) -> Result<Value, StrictJsonError> {
    if input.len() > limits.max_document_bytes {
        return Err(StrictJsonError::MessageTooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| StrictJsonError::InvalidUtf8)?;
    if text.starts_with('\u{feff}') {
        return Err(StrictJsonError::Utf8Bom);
    }

    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictValueSeed { limits, depth: 1 }
        .deserialize(&mut deserializer)
        .map_err(|error| StrictJsonError::from_serde(&error))?;
    deserializer
        .end()
        .map_err(|error| StrictJsonError::from_serde(&error))?;
    if !value.is_object() {
        return Err(StrictJsonError::RootNotObject);
    }
    Ok(value)
}

/// Parses a bounded JSON object into a caller-selected closed serde schema.
pub fn parse_typed<T: DeserializeOwned>(
    input: &[u8],
    limits: JsonLimits,
) -> Result<T, StrictJsonError> {
    let value = parse_value(input, limits)?;
    decode_value(value)
}

/// Decodes an already bounded value into a closed serde schema.
pub fn decode_value<T: DeserializeOwned>(value: Value) -> Result<T, StrictJsonError> {
    serde_json::from_value(value).map_err(|error| {
        let message = error.to_string();
        if message.contains("unknown field") {
            StrictJsonError::UnknownField
        } else if message.contains("unknown variant") {
            StrictJsonError::UnknownVariant
        } else if message.contains("unsupported protocol version") {
            StrictJsonError::ProtocolVersion
        } else {
            StrictJsonError::Schema
        }
    })
}

#[derive(Clone, Copy)]
struct StrictValueSeed {
    limits: JsonLimits,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.depth > self.limits.max_depth {
            return Err(D::Error::custom(DEPTH_MARKER));
        }
        deserializer.deserialize_any(StrictValueVisitor {
            limits: self.limits,
            depth: self.depth,
        })
    }
}

struct StrictValueVisitor {
    limits: JsonLimits,
    depth: usize,
}

impl StrictValueVisitor {
    fn check_string<E: serde::de::Error>(&self, value: &str) -> Result<(), E> {
        if value.len() > self.limits.max_string_bytes {
            Err(E::custom(STRING_LENGTH_MARKER))
        } else {
            Ok(())
        }
    }

    fn child(&self) -> StrictValueSeed {
        StrictValueSeed {
            limits: self.limits,
            depth: self.depth + 1,
        }
    }
}

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if self.limits.safe_integer_magnitude && value.unsigned_abs() > JS_SAFE_INTEGER {
            return Err(E::custom(INTEGER_RANGE_MARKER));
        }
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if self.limits.safe_integer_magnitude && value > JS_SAFE_INTEGER {
            return Err(E::custom(INTEGER_RANGE_MARKER));
        }
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom(NON_FINITE_MARKER))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.check_string(value)?;
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.check_string(&value)?;
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.child().deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(self.child())? {
            if values.len() == self.limits.max_array_elements {
                return Err(A::Error::custom(ARRAY_COUNT_MARKER));
            }
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Map::new();
        let mut keys = HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            self.check_string(&key)?;
            if fields.len() == self.limits.max_object_fields {
                return Err(A::Error::custom(FIELD_COUNT_MARKER));
            }
            if !keys.insert(key.clone()) {
                return Err(A::Error::custom(DUPLICATE_KEY_MARKER));
            }
            let value = object.next_value_seed(self.child())?;
            fields.insert(key, value);
        }
        Ok(Value::Object(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, Eq, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Closed {
        value: u64,
    }

    #[test]
    fn duplicate_decoded_keys_and_unknown_fields_are_rejected() {
        assert_eq!(
            parse_typed::<Closed>(br#"{"value":1,"\u0076alue":2}"#, JsonLimits::OPERATOR),
            Err(StrictJsonError::DuplicateKey)
        );
        assert_eq!(
            parse_typed::<Closed>(br#"{"value":1,"other":2}"#, JsonLimits::OPERATOR),
            Err(StrictJsonError::UnknownField)
        );
    }

    #[test]
    fn bounds_are_checked_before_schema_decode() {
        let limits = JsonLimits {
            max_document_bytes: 64,
            max_depth: 2,
            max_object_fields: 1,
            max_array_elements: 1,
            max_string_bytes: 4,
            safe_integer_magnitude: true,
        };
        assert_eq!(
            parse_value(br#"{"a":{"b":{"c":1}}}"#, limits),
            Err(StrictJsonError::DepthExceeded)
        );
        assert_eq!(
            parse_value(br#"{"long":"value"}"#, limits),
            Err(StrictJsonError::StringTooLong)
        );
        assert_eq!(
            parse_value(br#"{"a":1,"b":2}"#, limits),
            Err(StrictJsonError::TooManyFields)
        );
    }

    #[test]
    fn non_finite_marker_has_stable_invalid_value_code() {
        let error = <serde_json::Error as serde::de::Error>::custom(NON_FINITE_MARKER);
        let strict = StrictJsonError::from_serde(&error);
        assert_eq!(strict, StrictJsonError::NonFinite);
        assert_eq!(strict.code(), ErrorCode::InvalidValue);
    }
}
