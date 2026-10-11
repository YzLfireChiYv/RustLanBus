use rlb_codec::{decode_f32le_into, encode_f32le_into};
use rlb_model::DirectSessionId;

use crate::PeerError;

pub const MAX_DATAGRAM_BYTES: usize = 1200;
pub(crate) const MEDIA_MAGIC: &[u8; 4] = b"RLP1";
pub(crate) const HEADER_BYTES: usize = 60;

/// Original samples received from the configured peer. No local mixing occurs.
#[derive(Clone, Debug, PartialEq)]
pub struct OriginalAudio {
    pub sample_rate: u32,
    pub channels: u8,
    pub start_frame: u64,
    pub sequence: u64,
    pub samples: Vec<f32>,
}

#[derive(Clone, Copy)]
pub(crate) struct Header {
    pub session: DirectSessionId,
    pub send_generation: u64,
    pub receive_generation: u64,
    pub sequence: u64,
    pub start_frame: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub frames: u16,
}

pub(crate) fn encode(header: Header, samples: &[f32]) -> Result<Vec<u8>, PeerError> {
    let len = HEADER_BYTES + samples.len() * 4;
    if len > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    let mut bytes = vec![0; len];
    bytes[..4].copy_from_slice(MEDIA_MAGIC);
    bytes[4..20].copy_from_slice(header.session.as_bytes());
    bytes[20..28].copy_from_slice(&header.send_generation.to_le_bytes());
    bytes[28..36].copy_from_slice(&header.receive_generation.to_le_bytes());
    bytes[36..44].copy_from_slice(&header.sequence.to_le_bytes());
    bytes[44..52].copy_from_slice(&header.start_frame.to_le_bytes());
    bytes[52..56].copy_from_slice(&header.sample_rate.to_le_bytes());
    bytes[56] = header.channels;
    bytes[57..59].copy_from_slice(&header.frames.to_le_bytes());
    encode_f32le_into(
        header.channels,
        usize::from(header.frames),
        samples,
        &mut bytes[HEADER_BYTES..],
    )
    .map_err(|_| PeerError::InvalidAudio)?;
    Ok(bytes)
}

pub(crate) fn header(bytes: &[u8]) -> Result<Header, PeerError> {
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    if bytes.len() < HEADER_BYTES || !bytes.starts_with(MEDIA_MAGIC) || bytes[59] != 0 {
        return Err(PeerError::InvalidAudio);
    }
    let header = Header {
        session: DirectSessionId::from_bytes(bytes[4..20].try_into().unwrap())
            .map_err(|_| PeerError::InvalidAudio)?,
        send_generation: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        receive_generation: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
        sequence: u64::from_le_bytes(bytes[36..44].try_into().unwrap()),
        start_frame: u64::from_le_bytes(bytes[44..52].try_into().unwrap()),
        sample_rate: u32::from_le_bytes(bytes[52..56].try_into().unwrap()),
        channels: bytes[56],
        frames: u16::from_le_bytes(bytes[57..59].try_into().unwrap()),
    };
    if !matches!(header.channels, 1 | 2)
        || !(8_000..=384_000).contains(&header.sample_rate)
        || header.frames == 0
        || header.sequence == 0
        || bytes.len()
            != HEADER_BYTES + usize::from(header.frames) * usize::from(header.channels) * 4
        || header
            .start_frame
            .checked_add(u64::from(header.frames))
            .is_none()
    {
        return Err(PeerError::InvalidAudio);
    }
    Ok(header)
}

pub(crate) fn decode(bytes: &[u8], header: Header) -> Result<OriginalAudio, PeerError> {
    let payload = &bytes[HEADER_BYTES..];
    // Avoid the existing codec's sanitization policy: corrupt/non-finite media
    // is rejected, and every accepted captured finite F32 retains its bits.
    if payload
        .chunks_exact(4)
        .any(|sample| !f32::from_le_bytes(sample.try_into().unwrap()).is_finite())
    {
        return Err(PeerError::NonFiniteSample);
    }
    let mut samples = vec![0.0; usize::from(header.frames) * usize::from(header.channels)];
    decode_f32le_into(
        header.channels,
        usize::from(header.frames),
        payload,
        &mut samples,
    )
    .map_err(|_| PeerError::InvalidAudio)?;
    Ok(OriginalAudio {
        sample_rate: header.sample_rate,
        channels: header.channels,
        start_frame: header.start_frame,
        sequence: header.sequence,
        samples,
    })
}

/// A fixed 64-packet window admits modest reordering and rejects duplicates.
#[derive(Default)]
pub(crate) struct Seen {
    newest: u64,
    bits: u64,
}

impl Seen {
    pub fn insert(&mut self, sequence: u64) -> bool {
        if sequence > self.newest {
            let distance = sequence - self.newest;
            self.bits = if distance >= 64 {
                1
            } else {
                (self.bits << distance) | 1
            };
            self.newest = sequence;
            true
        } else {
            let distance = self.newest - sequence;
            if distance >= 64 || self.bits & (1 << distance) != 0 {
                return false;
            }
            self.bits |= 1 << distance;
            true
        }
    }
}
