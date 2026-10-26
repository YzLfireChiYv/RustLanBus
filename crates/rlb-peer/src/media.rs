//! RLP5 original-PCM datagrams. Finite F32 words pass through unchanged.

use rlb_model::{DirectSessionId, SourceId};

use crate::PeerError;
use crate::source::{MEDIA_HEADER_BYTES, PcmFormat};

pub const MAX_DATAGRAM_BYTES: usize = 1200;
pub(crate) const MEDIA_MAGIC: &[u8; 4] = b"RLP5";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Header {
    pub session: DirectSessionId,
    /// RLP5 source-send token: a positive odd u32 carried in this u64 slot.
    pub send_generation: u64,
    /// Receiver-issued token for this exact publication and send interval.
    pub receive_generation: u64,
    pub source: SourceId,
    pub epoch: u64,
    pub sequence: u64,
    pub start_frame: u64,
    pub frames: u16,
    pub payload_bytes: u16,
}

pub(crate) fn encode(header: Header, payload: &[u8]) -> Result<Vec<u8>, PeerError> {
    let len = MEDIA_HEADER_BYTES + payload.len();
    if len > MAX_DATAGRAM_BYTES || payload.len() != usize::from(header.payload_bytes) {
        return Err(PeerError::DatagramTooLarge);
    }
    let mut bytes = vec![0; len];
    encode_into(header, payload, &mut bytes)?;
    Ok(bytes)
}

pub(crate) fn encode_into(
    header: Header,
    payload: &[u8],
    destination: &mut [u8],
) -> Result<usize, PeerError> {
    let len = MEDIA_HEADER_BYTES + payload.len();
    if len > MAX_DATAGRAM_BYTES || payload.len() != usize::from(header.payload_bytes) {
        return Err(PeerError::DatagramTooLarge);
    }
    if destination.len() < len {
        return Err(PeerError::StorageTooSmall);
    }
    destination[..4].copy_from_slice(MEDIA_MAGIC);
    destination[4..20].copy_from_slice(header.session.as_bytes());
    destination[20..28].copy_from_slice(&header.send_generation.to_le_bytes());
    destination[28..36].copy_from_slice(&header.receive_generation.to_le_bytes());
    destination[36..52].copy_from_slice(header.source.as_bytes());
    destination[52..60].copy_from_slice(&header.epoch.to_le_bytes());
    destination[60..68].copy_from_slice(&header.sequence.to_le_bytes());
    destination[68..76].copy_from_slice(&header.start_frame.to_le_bytes());
    destination[76..78].copy_from_slice(&header.frames.to_le_bytes());
    destination[78..80].copy_from_slice(&header.payload_bytes.to_le_bytes());
    destination[MEDIA_HEADER_BYTES..len].copy_from_slice(payload);
    Ok(len)
}

pub(crate) fn header(bytes: &[u8]) -> Result<Header, PeerError> {
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    if bytes.len() < MEDIA_HEADER_BYTES || !bytes.starts_with(MEDIA_MAGIC) {
        return Err(PeerError::InvalidAudio);
    }
    let header = Header {
        session: DirectSessionId::from_bytes(bytes[4..20].try_into().unwrap())
            .map_err(|_| PeerError::InvalidAudio)?,
        send_generation: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        receive_generation: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
        source: SourceId::from_bytes(bytes[36..52].try_into().unwrap())
            .map_err(|_| PeerError::InvalidAudio)?,
        epoch: u64::from_le_bytes(bytes[52..60].try_into().unwrap()),
        sequence: u64::from_le_bytes(bytes[60..68].try_into().unwrap()),
        start_frame: u64::from_le_bytes(bytes[68..76].try_into().unwrap()),
        frames: u16::from_le_bytes(bytes[76..78].try_into().unwrap()),
        payload_bytes: u16::from_le_bytes(bytes[78..80].try_into().unwrap()),
    };
    if header.send_generation == 0
        || header.send_generation > u64::from(u32::MAX)
        || header.send_generation & 1 == 0
        || header.receive_generation == 0
        || header.epoch == 0
        || header.sequence == 0
        || header.frames == 0
        || usize::from(header.payload_bytes) != bytes.len() - MEDIA_HEADER_BYTES
        || header
            .start_frame
            .checked_add(u64::from(header.frames))
            .is_none()
    {
        return Err(PeerError::InvalidAudio);
    }
    Ok(header)
}

pub(crate) fn expected_payload(format: PcmFormat, frames: u16) -> Result<usize, PeerError> {
    usize::from(frames)
        .checked_mul(usize::from(format.channels()))
        .and_then(|samples| samples.checked_mul(usize::from(format.sample_bytes())))
        .ok_or(PeerError::InvalidAudio)
}

/// Decode the declared original encoding into exact canonical numeric storage.
pub(crate) fn decode_pcm(payload: &[u8], format: PcmFormat, frames: u16) -> Result<Vec<f64>, PeerError> {
    let mut samples = vec![0.0; usize::from(frames) * usize::from(format.channels())];
    decode_pcm_into(payload, format, frames, &mut samples)?;
    Ok(samples)
}
pub(crate) fn decode_pcm_into(payload: &[u8], format: PcmFormat, frames: u16, destination: &mut [f64]) -> Result<usize, PeerError> {
    if expected_payload(format, frames)? != payload.len() { return Err(PeerError::InvalidAudio); }
    rlb_model::pcm::decode_into(format, payload, destination).map_err(|error| match error {
        rlb_model::pcm::PcmError::StorageTooSmall => PeerError::StorageTooSmall,
        rlb_model::pcm::PcmError::NonFiniteSample => PeerError::NonFiniteSample,
        _ => PeerError::InvalidAudio,
    })
}

/// A fixed 64-packet window admits modest reordering and rejects duplicates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Seen {
    newest: u64,
    bits: u64,
}

impl Seen {
    pub(crate) fn insert(&mut self, sequence: u64) -> bool {
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

    #[cfg(test)]
    pub(crate) fn contains(&self, sequence: u64) -> bool {
        if sequence == 0 || self.newest == 0 || sequence > self.newest {
            return false;
        }
        let distance = self.newest - sequence;
        distance < 64 && self.bits & (1 << distance) != 0
    }
}
