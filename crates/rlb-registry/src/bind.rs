//! Fixed binary UDP endpoint-binding packets.

use std::net::Ipv4Addr;

use rlb_model::{
    BindNonce, Endpoint, EndpointError, IdParseError, RegistrySessionId, ServerGeneration,
};
use thiserror::Error;

const MAGIC: &[u8; 4] = b"RLBU";
const VERSION: u8 = 2;
const BIND_MEDIA_KIND: u8 = 1;
const BIND_OK_KIND: u8 = 2;
/// Exact `BIND_MEDIA` packet length.
pub const BIND_MEDIA_BYTES: usize = 56;
/// Exact `BIND_OK` packet length.
pub const BIND_OK_BYTES: usize = 64;

/// Parsed 56-byte `BIND_MEDIA` challenge response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindMedia {
    pub server_generation: ServerGeneration,
    pub registry_session_id: RegistrySessionId,
    pub bind_nonce: BindNonce,
}

impl BindMedia {
    /// Encodes the fixed network-order packet.
    #[must_use]
    pub fn encode(self) -> [u8; BIND_MEDIA_BYTES] {
        let mut packet = [0_u8; BIND_MEDIA_BYTES];
        write_header(&mut packet, BIND_MEDIA_KIND);
        packet[8..24].copy_from_slice(self.server_generation.as_bytes());
        packet[24..40].copy_from_slice(self.registry_session_id.as_bytes());
        packet[40..56].copy_from_slice(self.bind_nonce.as_bytes());
        packet
    }

    /// Parses and validates every fixed field and reserved byte.
    pub fn decode(packet: &[u8]) -> Result<Self, BindPacketError> {
        validate_header(packet, BIND_MEDIA_BYTES, BIND_MEDIA_KIND)?;
        Ok(Self {
            server_generation: parse_id(&packet[8..24])?,
            registry_session_id: parse_id(&packet[24..40])?,
            bind_nonce: parse_id(&packet[40..56])?,
        })
    }
}

/// Parsed 64-byte `BIND_OK` reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindOk {
    pub server_generation: ServerGeneration,
    pub registry_session_id: RegistrySessionId,
    pub bind_nonce: BindNonce,
    pub observed_endpoint: Endpoint,
}

impl BindOk {
    /// Creates a reply from the actual UDP datagram source.
    pub fn new(
        server_generation: ServerGeneration,
        registry_session_id: RegistrySessionId,
        bind_nonce: BindNonce,
        observed_ipv4: Ipv4Addr,
        observed_port: u16,
    ) -> Result<Self, BindPacketError> {
        Ok(Self {
            server_generation,
            registry_session_id,
            bind_nonce,
            observed_endpoint: Endpoint::new(observed_ipv4, observed_port)?,
        })
    }

    /// Encodes the fixed network-order packet.
    #[must_use]
    pub fn encode(self) -> [u8; BIND_OK_BYTES] {
        let mut packet = [0_u8; BIND_OK_BYTES];
        write_header(&mut packet, BIND_OK_KIND);
        packet[8..24].copy_from_slice(self.server_generation.as_bytes());
        packet[24..40].copy_from_slice(self.registry_session_id.as_bytes());
        packet[40..56].copy_from_slice(self.bind_nonce.as_bytes());
        packet[56..60].copy_from_slice(&self.observed_endpoint.ip().octets());
        packet[60..62].copy_from_slice(&self.observed_endpoint.port().to_be_bytes());
        packet
    }

    /// Parses and validates every fixed field and reserved byte.
    pub fn decode(packet: &[u8]) -> Result<Self, BindPacketError> {
        validate_header(packet, BIND_OK_BYTES, BIND_OK_KIND)?;
        if packet[62..64] != [0, 0] {
            return Err(BindPacketError::Reserved);
        }
        let endpoint = Endpoint::new(
            Ipv4Addr::new(packet[56], packet[57], packet[58], packet[59]),
            u16::from_be_bytes([packet[60], packet[61]]),
        )?;
        Ok(Self {
            server_generation: parse_id(&packet[8..24])?,
            registry_session_id: parse_id(&packet[24..40])?,
            bind_nonce: parse_id(&packet[40..56])?,
            observed_endpoint: endpoint,
        })
    }
}

fn write_header<const N: usize>(packet: &mut [u8; N], kind: u8) {
    packet[..4].copy_from_slice(MAGIC);
    packet[4] = VERSION;
    packet[5] = kind;
}

fn validate_header(
    packet: &[u8],
    expected_length: usize,
    expected_kind: u8,
) -> Result<(), BindPacketError> {
    if packet.len() != expected_length {
        return Err(BindPacketError::Length);
    }
    if &packet[..4] != MAGIC {
        return Err(BindPacketError::Magic);
    }
    if packet[4] != VERSION {
        return Err(BindPacketError::Version);
    }
    if packet[5] != expected_kind {
        return Err(BindPacketError::Kind);
    }
    if packet[6..8] != [0, 0] {
        return Err(BindPacketError::Reserved);
    }
    Ok(())
}

fn parse_id<T>(bytes: &[u8]) -> Result<T, BindPacketError>
where
    T: TryFrom<[u8; 16], Error = IdParseError>,
{
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| BindPacketError::Length)?;
    T::try_from(bytes).map_err(BindPacketError::Identity)
}

/// Stable binary binding-packet decode failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BindPacketError {
    #[error("binding packet length is invalid")]
    Length,
    #[error("binding packet magic is invalid")]
    Magic,
    #[error("binding packet version is unsupported")]
    Version,
    #[error("binding packet kind is invalid")]
    Kind,
    #[error("binding packet reserved bytes are nonzero")]
    Reserved,
    #[error("binding packet identity is invalid")]
    Identity(#[from] IdParseError),
    #[error("observed endpoint is invalid")]
    Endpoint(#[from] EndpointError),
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn id<T: FromStr>(value: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        value.parse().unwrap()
    }

    #[test]
    fn bind_media_roundtrips_exact_offsets() {
        let bind = BindMedia {
            server_generation: id("01010101010101010101010101010101"),
            registry_session_id: id("02020202020202020202020202020202"),
            bind_nonce: id("03030303030303030303030303030303"),
        };
        let packet = bind.encode();
        assert_eq!(&packet[..8], b"RLBU\x02\x01\0\0");
        assert_eq!(BindMedia::decode(&packet).unwrap(), bind);
    }

    #[test]
    fn bind_ok_roundtrips_observed_source() {
        let bind = BindOk::new(
            id("01010101010101010101010101010101"),
            id("02020202020202020202020202020202"),
            id("03030303030303030303030303030303"),
            Ipv4Addr::new(192, 0, 2, 26),
            17_001,
        )
        .unwrap();
        let packet = bind.encode();
        assert_eq!(&packet[56..60], &[192, 0, 2, 26]);
        assert_eq!(&packet[60..62], &17_001_u16.to_be_bytes());
        assert_eq!(BindOk::decode(&packet).unwrap(), bind);
    }

    #[test]
    fn reserved_and_zero_identity_are_rejected() {
        let mut packet = BindMedia {
            server_generation: id("01010101010101010101010101010101"),
            registry_session_id: id("02020202020202020202020202020202"),
            bind_nonce: id("03030303030303030303030303030303"),
        }
        .encode();
        packet[7] = 1;
        assert_eq!(BindMedia::decode(&packet), Err(BindPacketError::Reserved));
        packet[7] = 0;
        packet[8..24].fill(0);
        assert!(matches!(
            BindMedia::decode(&packet),
            Err(BindPacketError::Identity(IdParseError::Zero))
        ));
    }
}
