//! Opaque member-token generation, digest storage, and verification.

use std::fmt;

use rand::{TryRng, rngs::SysRng};
use rlb_model::{ErrorCode, MemberId, RoomId};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MIN_TOKEN_BYTES: usize = 32;
const CLAIM_BYTES: usize = 32;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Opaque member bearer token. Debug output is always redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct MemberToken(Vec<u8>);

impl MemberToken {
    /// Generates a 32-byte token from the operating-system CSPRNG.
    pub fn generate() -> Result<Self, SecretGenerationError> {
        let mut bytes = vec![0_u8; MIN_TOKEN_BYTES];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| SecretGenerationError)?;
        Ok(Self(bytes))
    }

    /// Creates a token from already generated material.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, SecretParseError> {
        if bytes.len() < MIN_TOKEN_BYTES {
            return Err(SecretParseError::TooShort);
        }
        Ok(Self(bytes))
    }

    /// Parses canonical unpadded base64url.
    pub fn parse(encoded: &str) -> Result<Self, SecretParseError> {
        Self::from_bytes(decode_base64url(encoded)?)
    }

    /// Exposes token bytes only to credential-storage and verification code.
    #[must_use]
    pub fn expose_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns canonical unpadded base64url.
    #[must_use]
    pub fn expose_encoded(&self) -> String {
        encode_base64url(&self.0)
    }
}

impl fmt::Debug for MemberToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MemberToken(<redacted>)")
    }
}

impl Serialize for MemberToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.expose_encoded())
    }
}

impl<'de> Deserialize<'de> for MemberToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_secret(
            deserializer,
            Self::parse,
            "an unpadded base64url member token",
        )
    }
}

/// One-time provisioning claim secret. Debug output is always redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct ClaimSecret([u8; CLAIM_BYTES]);

impl ClaimSecret {
    /// Generates claim material from the operating-system CSPRNG.
    pub fn generate() -> Result<Self, SecretGenerationError> {
        let mut bytes = [0_u8; CLAIM_BYTES];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| SecretGenerationError)?;
        Ok(Self(bytes))
    }

    /// Parses exactly 32 bytes of canonical unpadded base64url.
    pub fn parse(encoded: &str) -> Result<Self, SecretParseError> {
        let bytes = decode_base64url(encoded)?;
        let bytes: [u8; CLAIM_BYTES] = bytes
            .try_into()
            .map_err(|_| SecretParseError::InvalidClaimLength)?;
        Ok(Self(bytes))
    }

    /// Returns the SHA-256 digest stored in a pending record.
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        TokenDigest::of(&self.0)
    }

    /// Returns canonical unpadded base64url for provisioning.
    #[must_use]
    pub fn expose_encoded(&self) -> String {
        encode_base64url(&self.0)
    }
}

impl fmt::Debug for ClaimSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaimSecret(<redacted>)")
    }
}

impl Serialize for ClaimSecret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.expose_encoded())
    }
}

impl<'de> Deserialize<'de> for ClaimSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_secret(
            deserializer,
            Self::parse,
            "exactly 32 bytes of unpadded base64url claim material",
        )
    }
}

fn deserialize_secret<'de, D, T>(
    deserializer: D,
    parser: impl FnOnce(&str) -> Result<T, SecretParseError>,
    expectation: &'static str,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    struct SecretVisitor<P, T> {
        parser: P,
        expectation: &'static str,
        marker: std::marker::PhantomData<T>,
    }

    impl<'de, P, T> Visitor<'de> for SecretVisitor<P, T>
    where
        P: FnOnce(&str) -> Result<T, SecretParseError>,
    {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.expectation)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            (self.parser)(value).map_err(E::custom)
        }
    }

    deserializer.deserialize_str(SecretVisitor {
        parser,
        expectation,
        marker: std::marker::PhantomData,
    })
}

/// Persistable SHA-256 token or claim digest.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Digests opaque secret material.
    #[must_use]
    pub fn of(secret: &[u8]) -> Self {
        Self(Sha256::digest(secret).into())
    }

    /// Returns digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenDigest(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

impl Serialize for TokenDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for TokenDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DigestVisitor;

        impl Visitor<'_> for DigestVisitor {
            type Value = TokenDigest;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("64 lowercase hexadecimal characters")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() != 64 {
                    return Err(E::custom("digest length is invalid"));
                }
                let mut bytes = [0_u8; 32];
                for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
                    let high = hex_nibble(pair[0])
                        .ok_or_else(|| E::custom("digest is not lowercase hex"))?;
                    let low = hex_nibble(pair[1])
                        .ok_or_else(|| E::custom("digest is not lowercase hex"))?;
                    bytes[index] = (high << 4) | low;
                }
                Ok(TokenDigest(bytes))
            }
        }

        deserializer.deserialize_str(DigestVisitor)
    }
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Pluggable token-digest verifier.
pub trait TokenDigestVerifier {
    /// Compares a presented bearer token with a persisted digest.
    fn verify(&self, expected: &TokenDigest, presented: &MemberToken) -> bool;
}

/// SHA-256 verifier using a constant-work digest comparison.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sha256TokenVerifier;

impl TokenDigestVerifier for Sha256TokenVerifier {
    fn verify(&self, expected: &TokenDigest, presented: &MemberToken) -> bool {
        let actual = TokenDigest::of(presented.expose_bytes());
        constant_time_equal(expected.as_bytes(), actual.as_bytes())
    }
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Persisted member credential scope and digest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberCredential {
    pub room_id: RoomId,
    pub member_id: MemberId,
    pub token_generation: u64,
    pub token_sha256: TokenDigest,
}

impl MemberCredential {
    /// Verifies room/member scope before checking token material.
    pub fn verify<V: TokenDigestVerifier>(
        &self,
        room_id: RoomId,
        member_id: MemberId,
        presented: &MemberToken,
        verifier: &V,
    ) -> Result<(), CredentialError> {
        self.verify_bootstrap(
            room_id,
            member_id,
            self.token_generation,
            TokenDigest::of(presented.expose_bytes()),
            presented,
            verifier,
        )
    }

    /// Verifies the explicit generation and digest carried by a session
    /// bootstrap before accepting the bearer token.
    pub fn verify_bootstrap<V: TokenDigestVerifier>(
        &self,
        room_id: RoomId,
        member_id: MemberId,
        token_generation: u64,
        token_sha256: TokenDigest,
        presented: &MemberToken,
        verifier: &V,
    ) -> Result<(), CredentialError> {
        if room_id != self.room_id {
            return Err(CredentialError::RoomScope);
        }
        if member_id != self.member_id {
            return Err(CredentialError::MemberScope);
        }
        if token_generation != self.token_generation {
            return Err(CredentialError::StaleGeneration);
        }
        let presented_digest = TokenDigest::of(presented.expose_bytes());
        if !constant_time_equal(self.token_sha256.as_bytes(), token_sha256.as_bytes())
            || !constant_time_equal(token_sha256.as_bytes(), presented_digest.as_bytes())
        {
            return Err(CredentialError::InvalidToken);
        }
        if !verifier.verify(&self.token_sha256, presented) {
            return Err(CredentialError::InvalidToken);
        }
        Ok(())
    }
}

/// Stable credential verification failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CredentialError {
    #[error("token room scope does not match")]
    RoomScope,
    #[error("token member scope does not match")]
    MemberScope,
    #[error("member token digest does not match")]
    InvalidToken,
    #[error("member token generation is stale")]
    StaleGeneration,
    #[error("member token was revoked")]
    Revoked,
}

impl CredentialError {
    /// Returns the stable protocol error code.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::RoomScope => ErrorCode::RoomScopeMismatch,
            Self::MemberScope => ErrorCode::MemberScopeMismatch,
            Self::InvalidToken => ErrorCode::MemberTokenInvalid,
            Self::StaleGeneration | Self::Revoked => ErrorCode::MemberTokenRevoked,
        }
    }
}

/// Operating-system random source failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("operating-system random source is unavailable")]
pub struct SecretGenerationError;

/// Invalid base64url secret material.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SecretParseError {
    #[error("secret contains invalid base64url")]
    InvalidEncoding,
    #[error("member token contains fewer than 32 bytes")]
    TooShort,
    #[error("claim secret must contain exactly 32 bytes")]
    InvalidClaimLength,
}

fn encode_base64url(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(BASE64URL[(first >> 2) as usize] as char);
        if chunk.len() == 1 {
            encoded.push(BASE64URL[((first & 0x03) << 4) as usize] as char);
            continue;
        }
        let second = chunk[1];
        encoded.push(BASE64URL[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() == 2 {
            encoded.push(BASE64URL[((second & 0x0f) << 2) as usize] as char);
            continue;
        }
        let third = chunk[2];
        encoded.push(BASE64URL[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        encoded.push(BASE64URL[(third & 0x3f) as usize] as char);
    }
    encoded
}

fn decode_base64url(encoded: &str) -> Result<Vec<u8>, SecretParseError> {
    if encoded.is_empty() || encoded.len() % 4 == 1 {
        return Err(SecretParseError::InvalidEncoding);
    }
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    let mut decoded = Vec::with_capacity(encoded.len() * 3 / 4);
    for byte in encoded.bytes() {
        let value = base64url_value(byte).ok_or(SecretParseError::InvalidEncoding)?;
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits) - 1;
        }
    }
    if accumulator != 0 || encode_base64url(&decoded) != encoded {
        return Err(SecretParseError::InvalidEncoding);
    }
    Ok(decoded)
}

const fn base64url_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn token_encoding_is_unpadded_and_digest_verifies() {
        let token = MemberToken::from_bytes((0_u8..32).collect()).unwrap();
        let encoded = token.expose_encoded();
        assert_eq!(encoded.len(), 43);
        assert!(!encoded.contains('='));
        assert_eq!(MemberToken::parse(&encoded).unwrap(), token);
        let digest = TokenDigest::of(token.expose_bytes());
        assert!(Sha256TokenVerifier.verify(&digest, &token));
        assert_eq!(format!("{token:?}"), "MemberToken(<redacted>)");
    }

    #[test]
    fn credential_scope_precedes_digest_result() {
        let token = MemberToken::from_bytes(vec![7; 32]).unwrap();
        let credential = MemberCredential {
            room_id: RoomId::from_str("01010101010101010101010101010101").unwrap(),
            member_id: MemberId::from_str("02020202020202020202020202020202").unwrap(),
            token_generation: 3,
            token_sha256: TokenDigest::of(token.expose_bytes()),
        };
        assert!(
            credential
                .verify(
                    credential.room_id,
                    credential.member_id,
                    &token,
                    &Sha256TokenVerifier
                )
                .is_ok()
        );
        assert_eq!(
            credential.verify(
                RoomId::from_str("03030303030303030303030303030303").unwrap(),
                credential.member_id,
                &token,
                &Sha256TokenVerifier
            ),
            Err(CredentialError::RoomScope)
        );
    }

    #[test]
    fn bootstrap_rejects_old_generation_and_mismatched_digest() {
        let token = MemberToken::from_bytes(vec![7; 32]).unwrap();
        let credential = MemberCredential {
            room_id: RoomId::from_str("01010101010101010101010101010101").unwrap(),
            member_id: MemberId::from_str("02020202020202020202020202020202").unwrap(),
            token_generation: 3,
            token_sha256: TokenDigest::of(token.expose_bytes()),
        };
        assert_eq!(
            credential.verify_bootstrap(
                credential.room_id,
                credential.member_id,
                2,
                credential.token_sha256,
                &token,
                &Sha256TokenVerifier,
            ),
            Err(CredentialError::StaleGeneration)
        );
        assert_eq!(
            credential.verify_bootstrap(
                credential.room_id,
                credential.member_id,
                3,
                TokenDigest::of(&[9; 32]),
                &token,
                &Sha256TokenVerifier,
            ),
            Err(CredentialError::InvalidToken)
        );
    }
}
