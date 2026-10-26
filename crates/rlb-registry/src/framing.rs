//! Incremental RLBREG2 TCP preface and frame codec.

use std::collections::VecDeque;

use rlb_control::strict_json::{JsonLimits, StrictJsonError, parse_typed};
use rlb_model::ErrorCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// Exact client and server preface.
pub const RLBREG2_PREFACE: &[u8; 8] = b"RLBREG2\n";
/// Smallest legal JSON frame payload.
pub const MIN_FRAME_BYTES: usize = 2;
/// Largest legal JSON frame payload.
pub const MAX_FRAME_BYTES: usize = 65_536;
/// Maximum buffered unread bytes for two complete maximum frames.
pub const MAX_RECEIVE_BUFFER_BYTES: usize = 2 * (4 + MAX_FRAME_BYTES);
/// Maximum queued encoded bytes for one session.
pub const MAX_SEND_QUEUE_BYTES: usize = 262_144;

/// Incremental decoder state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecoderPhase {
    Preface,
    Frames,
}

/// Fatal or semantic frame codec failure.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("RLBREG2 preface is invalid")]
    PrefaceInvalid,
    #[error("frame payload is shorter than two bytes")]
    FrameTooSmall,
    #[error("frame payload exceeds 65536 bytes")]
    FrameTooLarge,
    #[error("receive buffer exceeds two maximum frames")]
    ReceiveBufferLimit,
    #[error("session send queue exceeds 262144 bytes")]
    SendQueueFull,
    #[error(transparent)]
    Json(#[from] StrictJsonError),
    #[error("JSON serialization failed")]
    Serialize(#[from] serde_json::Error),
}

impl FrameError {
    /// Returns the registry protocol's stable error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::PrefaceInvalid => ErrorCode::PrefaceInvalid,
            Self::FrameTooLarge | Self::ReceiveBufferLimit => ErrorCode::FrameTooLarge,
            Self::SendQueueFull => ErrorCode::SlowConsumer,
            Self::FrameTooSmall | Self::Serialize(_) => ErrorCode::MalformedJson,
            Self::Json(error) => registry_json_error_code(*error),
        }
    }

    /// Reports whether the TCP session must close immediately.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::PrefaceInvalid
                | Self::FrameTooLarge
                | Self::ReceiveBufferLimit
                | Self::SendQueueFull
                | Self::FrameTooSmall
        )
    }
}

/// Bounded FIFO of already encoded RLBREG2 frames.
#[derive(Debug, Default)]
pub struct FrameSendQueue {
    frames: VecDeque<Vec<u8>>,
    queued_bytes: usize,
}

impl FrameSendQueue {
    /// Creates an empty send queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    /// Returns the total encoded bytes waiting for the socket.
    #[must_use]
    pub const fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// Encodes and appends one message without crossing the hard byte limit.
    pub fn push_json<T: Serialize>(&mut self, message: &T) -> Result<(), FrameError> {
        self.push_encoded(encode_json_frame(message)?)
    }

    /// Appends one encoded frame without crossing the hard byte limit.
    pub fn push_encoded(&mut self, frame: Vec<u8>) -> Result<(), FrameError> {
        if frame.len() < 4 {
            return Err(FrameError::FrameTooSmall);
        }
        let payload_length =
            u32::from_be_bytes(frame[..4].try_into().expect("four bytes were checked")) as usize;
        if payload_length < MIN_FRAME_BYTES || frame.len() != payload_length + 4 {
            return Err(FrameError::FrameTooSmall);
        }
        if payload_length > MAX_FRAME_BYTES {
            return Err(FrameError::FrameTooLarge);
        }
        if self.queued_bytes.saturating_add(frame.len()) > MAX_SEND_QUEUE_BYTES {
            return Err(FrameError::SendQueueFull);
        }
        self.queued_bytes += frame.len();
        self.frames.push_back(frame);
        Ok(())
    }

    /// Removes the next complete encoded frame.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let frame = self.frames.pop_front()?;
        self.queued_bytes -= frame.len();
        Some(frame)
    }
}

pub(crate) const fn registry_json_error_code(error: StrictJsonError) -> ErrorCode {
    match error {
        StrictJsonError::MessageTooLarge => ErrorCode::FrameTooLarge,
        StrictJsonError::InvalidUtf8 => ErrorCode::InvalidUtf8,
        StrictJsonError::DuplicateKey => ErrorCode::DuplicateJsonKey,
        StrictJsonError::UnknownField => ErrorCode::UnknownField,
        StrictJsonError::UnknownVariant => ErrorCode::UnknownType,
        StrictJsonError::ProtocolVersion => ErrorCode::ProtocolVersion,
        StrictJsonError::NonFinite => ErrorCode::InvalidValue,
        StrictJsonError::Utf8Bom
        | StrictJsonError::Malformed
        | StrictJsonError::RootNotObject
        | StrictJsonError::DepthExceeded
        | StrictJsonError::TooManyFields
        | StrictJsonError::TooManyElements
        | StrictJsonError::StringTooLong
        | StrictJsonError::IntegerOutOfRange
        | StrictJsonError::Schema => ErrorCode::MalformedJson,
    }
}

/// Incremental decoder supporting fragmented reads and multiple frames per read.
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
    /// Creates a decoder waiting for the exact eight-byte preface.
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

    /// Appends arbitrary TCP read fragments and returns every complete payload.
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
            let comparable = self.buffer.len().min(RLBREG2_PREFACE.len());
            if self.buffer[..comparable] != RLBREG2_PREFACE[..comparable] {
                return Err(FrameError::PrefaceInvalid);
            }
            if self.buffer.len() < RLBREG2_PREFACE.len() {
                return Ok(());
            }
            self.buffer.drain(..RLBREG2_PREFACE.len());
            self.phase = DecoderPhase::Frames;
        }

        loop {
            if self.buffer.len() < 4 {
                return Ok(());
            }
            let length = u32::from_be_bytes(
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

/// Encodes one bounded JSON object with a big-endian length prefix.
pub fn encode_json_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(message)?;
    encode_payload(&payload)
}

/// Encodes one already serialized payload after checking the frame bounds.
pub fn encode_payload(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() < MIN_FRAME_BYTES {
        return Err(FrameError::FrameTooSmall);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge);
    }
    let length = u32::try_from(payload.len()).expect("frame maximum fits u32");
    let mut encoded = Vec::with_capacity(4 + payload.len());
    encoded.extend_from_slice(&length.to_be_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

/// Decodes one complete frame payload through the registry's bounded strict JSON
/// parser and a caller-selected closed schema.
pub fn decode_json_frame<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    Ok(parse_typed(payload, JsonLimits::REGISTRY)?)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Message {
        value: u8,
    }

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Empty {}

    #[test]
    fn fragmented_preface_length_and_payload_are_reassembled() {
        let encoded = encode_json_frame(&Message { value: 7 }).unwrap();
        let mut decoder = FrameDecoder::new();
        assert!(decoder.feed(b"RLB").unwrap().is_empty());
        assert!(decoder.feed(b"REG2\n\0\0").unwrap().is_empty());
        assert!(decoder.feed(&encoded[2..5]).unwrap().is_empty());
        let frames = decoder.feed(&encoded[5..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            decode_json_frame::<Message>(&frames[0]).unwrap(),
            Message { value: 7 }
        );
    }

    #[test]
    fn multiple_frames_in_one_read_are_emitted_in_order() {
        let first = encode_json_frame(&Message { value: 1 }).unwrap();
        let second = encode_json_frame(&Message { value: 2 }).unwrap();
        let mut input = RLBREG2_PREFACE.to_vec();
        input.extend_from_slice(&first);
        input.extend_from_slice(&second);
        let frames = FrameDecoder::new().feed(&input).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(decode_json_frame::<Message>(&frames[0]).unwrap().value, 1);
        assert_eq!(decode_json_frame::<Message>(&frames[1]).unwrap().value, 2);
    }

    #[test]
    fn frame_limits_are_exact() {
        let mut maximum_json = b"{}".to_vec();
        maximum_json.resize(MAX_FRAME_BYTES, b' ');
        let encoded = encode_payload(&maximum_json).unwrap();
        assert_eq!(encoded.len(), MAX_FRAME_BYTES + 4);
        assert_eq!(decode_json_frame::<Empty>(&maximum_json).unwrap(), Empty {});
        assert!(matches!(
            encode_payload(&vec![b' '; MAX_FRAME_BYTES + 1]),
            Err(FrameError::FrameTooLarge)
        ));

        let mut oversized_header = RLBREG2_PREFACE.to_vec();
        oversized_header.extend_from_slice(&65_537_u32.to_be_bytes());
        assert!(matches!(
            FrameDecoder::new().feed(&oversized_header),
            Err(FrameError::FrameTooLarge)
        ));
    }

    #[test]
    fn complete_payloads_retain_strict_json_failures() {
        assert!(matches!(
            decode_json_frame::<Message>(br#"{"value":1,"value":2}"#),
            Err(FrameError::Json(StrictJsonError::DuplicateKey))
        ));
        assert!(matches!(
            decode_json_frame::<Message>(&[b'{', 0xff, b'}']),
            Err(FrameError::Json(StrictJsonError::InvalidUtf8))
        ));
    }

    #[test]
    fn send_queue_refuses_the_first_byte_over_its_limit() {
        let mut queue = FrameSendQueue::new();
        let maximum = encode_payload(&vec![b' '; MAX_FRAME_BYTES]).unwrap();
        for _ in 0..3 {
            queue.push_encoded(maximum.clone()).unwrap();
        }
        let remainder_payload = MAX_SEND_QUEUE_BYTES - queue.queued_bytes() - 4;
        queue
            .push_encoded(encode_payload(&vec![b' '; remainder_payload]).unwrap())
            .unwrap();
        assert_eq!(queue.queued_bytes(), MAX_SEND_QUEUE_BYTES);
        assert!(matches!(
            queue.push_encoded(encode_payload(b"{}").unwrap()),
            Err(FrameError::SendQueueFull)
        ));
        while queue.pop().is_some() {}
        assert_eq!(queue.queued_bytes(), 0);
    }
}
