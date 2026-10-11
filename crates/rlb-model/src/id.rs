use core::fmt;
use core::str::FromStr;

use rand::{TryRng, rngs::SysRng};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

const ID_BYTES: usize = 16;
const ID_HEX_BYTES: usize = ID_BYTES * 2;

/// Failure returned when the operating-system random source is unavailable.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the operating-system random source is unavailable")]
pub struct IdGenerationError;

/// Why a textual or binary 128-bit identity was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IdParseError {
    /// The textual representation did not contain exactly 32 bytes.
    #[error("an identity must contain exactly 32 lowercase hexadecimal characters, got {actual}")]
    InvalidLength {
        /// Observed UTF-8 byte length.
        actual: usize,
    },
    /// A byte was outside lowercase hexadecimal syntax.
    #[error("identity byte {index} is not lowercase hexadecimal")]
    InvalidHex {
        /// Zero-based byte position.
        index: usize,
    },
    /// The decoded identity was the reserved all-zero value.
    #[error("the all-zero identity is reserved")]
    Zero,
}

fn parse_hex(value: &str) -> Result<[u8; ID_BYTES], IdParseError> {
    if value.len() != ID_HEX_BYTES {
        return Err(IdParseError::InvalidLength {
            actual: value.len(),
        });
    }

    let mut output = [0_u8; ID_BYTES];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or(IdParseError::InvalidHex { index: index * 2 })?;
        let low = hex_nibble(pair[1]).ok_or(IdParseError::InvalidHex {
            index: index * 2 + 1,
        })?;
        output[index] = (high << 4) | low;
    }

    if output == [0; ID_BYTES] {
        return Err(IdParseError::Zero);
    }
    Ok(output)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn write_hex(bytes: &[u8; ID_BYTES], formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name([u8; ID_BYTES]);

        impl $name {
            /// Creates an identity from raw network-order bytes.
            pub fn from_bytes(bytes: [u8; ID_BYTES]) -> Result<Self, IdParseError> {
                if bytes == [0; ID_BYTES] {
                    Err(IdParseError::Zero)
                } else {
                    Ok(Self(bytes))
                }
            }

            /// Generates an identity directly from the operating-system CSPRNG.
            pub fn generate() -> Result<Self, IdGenerationError> {
                loop {
                    let mut bytes = [0_u8; ID_BYTES];
                    SysRng
                        .try_fill_bytes(&mut bytes)
                        .map_err(|_| IdGenerationError)?;
                    if bytes != [0; ID_BYTES] {
                        return Ok(Self(bytes));
                    }
                }
            }

            /// Returns the raw 16-byte identity.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; ID_BYTES] {
                &self.0
            }

            /// Consumes the value and returns its raw bytes.
            #[must_use]
            pub const fn into_bytes(self) -> [u8; ID_BYTES] {
                self.0
            }
        }

        impl AsRef<[u8; ID_BYTES]> for $name {
            fn as_ref(&self) -> &[u8; ID_BYTES] {
                self.as_bytes()
            }
        }

        impl TryFrom<[u8; ID_BYTES]> for $name {
            type Error = IdParseError;

            fn try_from(value: [u8; ID_BYTES]) -> Result<Self, Self::Error> {
                Self::from_bytes(value)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_hex(value).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(&self.0, formatter)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "("))?;
                write_hex(&self.0, formatter)?;
                formatter.write_str(")")
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct IdentityVisitor;

                impl Visitor<'_> for IdentityVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("exactly 32 lowercase hexadecimal characters")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        value.parse().map_err(E::custom)
                    }
                }

                deserializer.deserialize_str(IdentityVisitor)
            }
        }
    };
}

id_type!(
    /// Stable identity of the single room managed by an authority.
    RoomId
);
id_type!(
    /// Stable identity of one member-engine configuration.
    MemberId
);
id_type!(
    /// Identity of one member-engine process run.
    RunId
);
id_type!(
    /// Stable identity of a member's source slot.
    SourceId
);
id_type!(
    /// Identity of one continuous source format and sample timeline.
    StreamEpoch
);
id_type!(
    /// Identity of one registry daemon run.
    ServerGeneration
);
id_type!(
    /// Identity of one accepted registry TCP session.
    RegistrySessionId
);
id_type!(
    /// Idempotency identity shared by a request and all of its retries.
    RequestId
);
id_type!(
    /// Identity of one accepted asynchronous control operation.
    OperationId
);
id_type!(
    /// Identity of one peer-to-peer direct-media session.
    DirectSessionId
);
id_type!(
    /// Registry nonce binding a member session to its real media socket.
    BindNonce
);
id_type!(
    /// Nonce used by the direct-media hello exchange.
    HelloNonce
);
id_type!(
    /// Local correlation identity attached to unexpected diagnostic failures.
    DiagnosticId
);

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "000102030405060708090a0b0c0d0e0f";

    #[test]
    fn identity_text_and_json_are_canonical() {
        let id: RoomId = EXAMPLE.parse().expect("valid fixture");
        assert_eq!(id.to_string(), EXAMPLE);
        assert_eq!(
            serde_json::to_string(&id).expect("serialize"),
            format!("\"{EXAMPLE}\"")
        );
        assert_eq!(
            serde_json::from_str::<RoomId>(&format!("\"{EXAMPLE}\"")).expect("deserialize"),
            id
        );
        assert_eq!(format!("{id:?}"), format!("RoomId({EXAMPLE})"));
    }

    #[test]
    fn identity_rejects_noncanonical_and_zero_values() {
        assert_eq!(
            "00000000000000000000000000000000".parse::<MemberId>(),
            Err(IdParseError::Zero)
        );
        assert!(matches!(
            "000102030405060708090A0B0C0D0E0F".parse::<MemberId>(),
            Err(IdParseError::InvalidHex { index: 21 })
        ));
        assert!(matches!(
            "abcd".parse::<MemberId>(),
            Err(IdParseError::InvalidLength { actual: 4 })
        ));
        assert_eq!(MemberId::from_bytes([0; ID_BYTES]), Err(IdParseError::Zero));
        assert!(serde_json::from_str::<MemberId>("17").is_err());
    }

    #[test]
    fn every_identity_kind_uses_the_os_random_source() {
        macro_rules! assert_generated {
            ($($kind:ty),+ $(,)?) => {
                $(
                    assert_ne!(
                        <$kind>::generate().expect("OS random source").into_bytes(),
                        [0; ID_BYTES]
                    );
                )+
            };
        }

        assert_generated!(
            RoomId,
            MemberId,
            RunId,
            SourceId,
            StreamEpoch,
            ServerGeneration,
            RegistrySessionId,
            RequestId,
            OperationId,
            DirectSessionId,
            BindNonce,
            HelloNonce,
            DiagnosticId,
        );
    }
}
