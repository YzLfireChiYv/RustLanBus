//! Allocation-free dispatch hints for hosts sharing a UDP socket between peers.

use rlb_model::{DirectSessionId, MemberId, RunId};

use crate::{PeerError, media, wire};

/// Header identity used to select a candidate peer before full admission.
///
/// A hint does not admit a datagram. The socket owner must check its current
/// endpoint mapping and pass the bytes to the selected peer's ingest method,
/// which validates the complete message, session and permission generations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatagramRoute {
    Control { from: MemberId, to: MemberId },
    Media { session: DirectSessionId },
}

/// Reads only the fixed routing header, without decoding PCM or catalogues.
///
/// Control bodies remain unchecked until the selected peer ingests them.
/// Unknown protocol versions and malformed fixed headers are rejected.
pub fn datagram_route(bytes: &[u8]) -> Result<DatagramRoute, PeerError> {
    if bytes.len() > media::MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    if bytes.starts_with(media::MEDIA_MAGIC) {
        return media::header(bytes).map(|header| DatagramRoute::Media {
            session: header.session,
        });
    }
    if bytes.len() < 56
        || !bytes.starts_with(wire::CONTROL_MAGIC)
        || !(1..=6).contains(&bytes[4])
        || bytes[5..8] != [0, 0, 0]
    {
        return Err(PeerError::MalformedControl);
    }
    let from = MemberId::from_bytes(bytes[8..24].try_into().unwrap())
        .map_err(|_| PeerError::MalformedControl)?;
    let to = MemberId::from_bytes(bytes[24..40].try_into().unwrap())
        .map_err(|_| PeerError::MalformedControl)?;
    RunId::from_bytes(bytes[40..56].try_into().unwrap())
        .map_err(|_| PeerError::MalformedControl)?;
    Ok(DatagramRoute::Control { from, to })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalogue, Intent, Peer};

    #[test]
    fn control_hints_keep_member_identity_and_reject_invalid_headers() {
        let from = MemberId::from_bytes([1; 16]).unwrap();
        let to = MemberId::from_bytes([2; 16]).unwrap();
        let mut peer = Peer::new(
            from,
            to,
            RunId::from_bytes([3; 16]).unwrap(),
            Intent::default(),
            Catalogue::default(),
            0,
        )
        .unwrap();
        let hello = peer.take_control().unwrap();
        assert_eq!(
            datagram_route(&hello),
            Ok(DatagramRoute::Control { from, to })
        );
        for length in 0..56 {
            assert!(datagram_route(&hello[..length]).is_err());
        }
        for index in [4, 5, 8, 24, 40] {
            let mut invalid = hello.clone();
            if index == 4 {
                invalid[index] = 0;
            } else if index == 5 {
                invalid[index] = 1;
            } else {
                invalid[index..index + 16].fill(0);
            }
            assert!(datagram_route(&invalid).is_err());
        }
        assert!(datagram_route(b"RLC2").is_err());
        let mut old_version = hello.clone();
        old_version[3] = b'3';
        assert!(datagram_route(&old_version).is_err());
        let mut subscriptions = hello.clone();
        subscriptions[4] = 6;
        // Dispatch validates the fixed header; Peer validates the body.
        assert_eq!(
            datagram_route(&subscriptions),
            Ok(DatagramRoute::Control { from, to })
        );
        assert!(datagram_route(&[0; media::MAX_DATAGRAM_BYTES + 1]).is_err());
    }

    #[test]
    fn media_hints_use_session_without_decoding_pcm() {
        let session = DirectSessionId::from_bytes([1; 16]).unwrap();
        let packet = media::encode(
            media::Header {
                session,
                send_generation: 1,
                receive_generation: 1,
                source: rlb_model::SourceId::from_bytes([2; 16]).unwrap(),
                epoch: 1,
                sequence: 1,
                start_frame: 0,
                frames: 1,
                payload_bytes: 4,
            },
            &f32::NAN.to_le_bytes(),
        )
        .unwrap();
        // Payload validity belongs to Peer admission; dispatch reads no PCM.
        assert_eq!(
            datagram_route(&packet),
            Ok(DatagramRoute::Media { session })
        );
        assert!(datagram_route(&packet[..packet.len() - 1]).is_err());
        for token in [0, 2, u64::from(u32::MAX) + 2] {
            let mut invalid = packet.clone();
            invalid[20..28].copy_from_slice(&token.to_le_bytes());
            assert!(datagram_route(&invalid).is_err());
        }
        let mut old_version = packet.clone();
        old_version[3] = b'3';
        assert!(datagram_route(&old_version).is_err());
        let mut invalid = packet;
        invalid[4..20].fill(0);
        assert!(datagram_route(&invalid).is_err());
    }
}
