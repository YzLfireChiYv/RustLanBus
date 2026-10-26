//! Incremental `RLBCT1` preface and little-endian length framing.
//!
//! The shape follows the existing registry frame codec. This protocol uses its
//! own preface and a little-endian length.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Write};

use rlb_control::strict_json::{JsonLimits, StrictJsonError, parse_typed};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// Exact eight-byte preface: `RLBCT1`, carriage return, line feed.
pub const CONTACT_PREFACE: &[u8; 8] = b"RLBCT1\r\n";
/// Smallest legal JSON payload.
pub const MIN_FRAME_BYTES: usize = 2;
/// Largest legal metadata payload; enough for 64 bounded multi-address members.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
/// Maximum buffered unread bytes.
pub const MAX_RECEIVE_BUFFER_BYTES: usize = 2 * (4 + MAX_FRAME_BYTES);
/// Maximum queued encoded bytes for one connection.
pub const MAX_SEND_QUEUE_BYTES: usize = 2 * (4 + MAX_FRAME_BYTES) + CONTACT_PREFACE.len();

/// JSON limits for one contact frame.
pub const CONTACT_JSON_LIMITS: JsonLimits = JsonLimits {
    max_document_bytes: MAX_FRAME_BYTES,
    max_depth: 8,
    max_object_fields: 32,
    max_array_elements: 128,
    max_string_bytes: 1_024,
    safe_integer_magnitude: true,
};

/// Incremental decoder state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecoderPhase {
    /// Waiting for [`CONTACT_PREFACE`].
    Preface,
    /// Reading length-prefixed JSON frames.
    Frames,
}

/// Frame codec failure.
#[derive(Debug, Error)]
pub enum FrameError {
    /// The preface bytes differ from [`CONTACT_PREFACE`].
    #[error("RLBCT1 preface is invalid")]
    PrefaceInvalid,
    /// The declared payload is shorter than two bytes.
    #[error("frame payload is shorter than two bytes")]
    FrameTooSmall,
    /// The declared payload exceeds the metadata frame bound.
    #[error("frame payload exceeds 262144 bytes")]
    FrameTooLarge,
    /// The unread buffer exceeded its bound.
    #[error("receive buffer exceeds two maximum frames")]
    ReceiveBufferLimit,
    /// The outbound queue exceeded its bound.
    #[error("connection send queue exceeds two maximum frames and its preface")]
    SendQueueFull,
    /// Strict JSON rejected the payload.
    #[error(transparent)]
    Json(#[from] StrictJsonError),
    /// JSON encoding failed.
    #[error("JSON serialization failed")]
    Serialize(#[from] serde_json::Error),
    /// Media candidates failed their shared count/port/address contract.
    #[error(transparent)]
    InvalidMediaCandidates(#[from] crate::media::MediaError),
    /// A typed metadata record exceeds the room's semantic bound.
    #[error("invalid contact metadata: {0}")]
    InvalidMetadata(&'static str),
}

/// Bytes waiting to be written, with partial writes retained.
#[derive(Debug)]
pub(crate) struct ByteQueue {
    data: VecDeque<u8>,
    limit: usize,
}

impl ByteQueue {
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            data: VecDeque::new(),
            limit,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.data.len().saturating_add(bytes.len()) > self.limit {
            return Err(FrameError::SendQueueFull);
        }
        self.data.extend(bytes);
        Ok(())
    }

    /// Writes queued bytes. A short or pending write leaves the remainder queued.
    pub(crate) fn write_to<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        while !self.data.is_empty() {
            let (first, second) = self.data.as_slices();
            let chunk = if first.is_empty() { second } else { first };
            if chunk.is_empty() {
                return Ok(());
            }
            match writer.write(chunk) {
                Ok(0) => {
                    return Err(io::Error::new(ErrorKind::WriteZero, "contact write closed"));
                }
                Ok(written) => {
                    self.data.drain(..written);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        || error.kind() == ErrorKind::TimedOut =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Incremental decoder for fragmented reads and several frames in one read.
#[derive(Debug)]
pub struct FrameDecoder {
    phase: DecoderPhase,
    buffer: Vec<u8>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    /// Creates a decoder waiting for the preface.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: DecoderPhase::Preface,
            buffer: Vec::new(),
        }
    }

    /// Returns the current decode phase.
    #[must_use]
    pub const fn phase(&self) -> DecoderPhase {
        self.phase
    }

    /// Appends a read fragment and returns every complete JSON payload.
    pub fn feed(&mut self, mut input: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        let mut frames = Vec::new();
        while !input.is_empty() {
            self.decode_available(&mut frames)?;
            let remaining = MAX_RECEIVE_BUFFER_BYTES
                .checked_sub(self.buffer.len())
                .ok_or(FrameError::ReceiveBufferLimit)?;
            if remaining == 0 {
                return Err(FrameError::ReceiveBufferLimit);
            }
            let take = remaining.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
        }
        self.decode_available(&mut frames)?;
        Ok(frames)
    }

    fn decode_available(&mut self, frames: &mut Vec<Vec<u8>>) -> Result<(), FrameError> {
        if self.phase == DecoderPhase::Preface {
            let comparable = self.buffer.len().min(CONTACT_PREFACE.len());
            if self.buffer[..comparable] != CONTACT_PREFACE[..comparable] {
                return Err(FrameError::PrefaceInvalid);
            }
            if self.buffer.len() < CONTACT_PREFACE.len() {
                return Ok(());
            }
            self.buffer.drain(..CONTACT_PREFACE.len());
            self.phase = DecoderPhase::Frames;
        }

        loop {
            if self.buffer.len() < 4 {
                return Ok(());
            }
            let length = u32::from_le_bytes(
                self.buffer[..4]
                    .try_into()
                    .expect("four bytes were checked"),
            ) as usize;
            if length < MIN_FRAME_BYTES {
                return Err(FrameError::FrameTooSmall);
            }
            if length > MAX_FRAME_BYTES {
                return Err(FrameError::FrameTooLarge);
            }
            let encoded_length = 4 + length;
            if self.buffer.len() < encoded_length {
                return Ok(());
            }
            let payload = self.buffer[4..encoded_length].to_vec();
            self.buffer.drain(..encoded_length);
            frames.push(payload);
        }
    }
}

/// Encodes one bounded JSON object with a little-endian length prefix.
pub fn encode_json_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() < MIN_FRAME_BYTES {
        return Err(FrameError::FrameTooSmall);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge);
    }
    // Both peers use the same strict JSON boundary. Never emit a frame which
    // this codec would reject for depth, collection, string or integer limits.
    let _: serde_json::Value = parse_typed(&payload, CONTACT_JSON_LIMITS)?;
    encode_payload(&payload)
}

/// Encodes one payload after checking the frame bounds.
pub fn encode_payload(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() < MIN_FRAME_BYTES {
        return Err(FrameError::FrameTooSmall);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge);
    }
    let length = u32::try_from(payload.len()).expect("frame maximum fits u32");
    let mut encoded = Vec::with_capacity(4 + payload.len());
    encoded.extend_from_slice(&length.to_le_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

/// Decodes one payload with the contact JSON limits and a closed schema.
pub fn decode_json_frame<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    if payload.len() < MIN_FRAME_BYTES {
        return Err(FrameError::FrameTooSmall);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge);
    }
    Ok(parse_typed(payload, CONTACT_JSON_LIMITS)?)
}

#[cfg(test)]
mod tests {
    use std::io::{self, ErrorKind, Write};

    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Message {
        value: u8,
    }

    #[test]
    fn preface_is_eight_bytes_with_carriage_return_and_line_feed() {
        assert_eq!(CONTACT_PREFACE, b"RLBCT1\r\n");
        assert_eq!(CONTACT_PREFACE.len(), 8);
    }

    #[test]
    fn length_prefix_is_little_endian() {
        let encoded = encode_payload(b"{}").unwrap();
        assert_eq!(&encoded[..4], &2u32.to_le_bytes());
        assert_ne!(&encoded[..4], &2u32.to_be_bytes());
    }

    #[test]
    fn fragmented_preface_length_and_payload_are_reassembled() {
        let encoded = encode_json_frame(&Message { value: 7 }).unwrap();
        let mut decoder = FrameDecoder::new();
        assert!(decoder.feed(b"RL").unwrap().is_empty());
        assert!(decoder.feed(b"BCT1\r").unwrap().is_empty());
        assert!(decoder.feed(b"\n").unwrap().is_empty());
        assert_eq!(decoder.phase(), DecoderPhase::Frames);
        assert!(decoder.feed(&encoded[..3]).unwrap().is_empty());
        let frames = decoder.feed(&encoded[3..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            decode_json_frame::<Message>(&frames[0]).unwrap(),
            Message { value: 7 }
        );
    }

    #[test]
    fn registry_preface_and_oversized_length_are_rejected() {
        let mut decoder = FrameDecoder::new();
        assert!(matches!(
            decoder.feed(b"RLBREG2\n"),
            Err(FrameError::PrefaceInvalid)
        ));
        let mut oversized = CONTACT_PREFACE.to_vec();
        oversized.extend_from_slice(&u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_le_bytes());
        assert!(matches!(
            FrameDecoder::new().feed(&oversized),
            Err(FrameError::FrameTooLarge)
        ));
    }

    #[test]
    fn byte_queue_keeps_the_unwritten_tail() {
        let mut queue = ByteQueue::with_limit(64);
        queue.push(b"abcdef").unwrap();
        let mut writer = StepWriter {
            writes: 0,
            written: Vec::new(),
        };
        queue.write_to(&mut writer).unwrap();
        assert_eq!(writer.written, b"ab");
        assert_eq!(queue.len(), 4);
        queue.write_to(&mut writer).unwrap();
        assert!(queue.is_empty());
        assert_eq!(writer.written, b"abcdef");
    }

    struct StepWriter {
        writes: u8,
        written: Vec<u8>,
    }

    impl Write for StepWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.writes = self.writes.saturating_add(1);
            if self.writes == 2 {
                return Err(io::Error::new(ErrorKind::WouldBlock, "wait"));
            }
            let size = 2.min(data.len());
            self.written.extend_from_slice(&data[..size]);
            Ok(size)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
