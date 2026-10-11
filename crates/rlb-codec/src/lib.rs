#![forbid(unsafe_code)]

//! Allocation-free PCM primitives for the RLB2 media payload.
//!
//! Inputs and outputs use frame-interleaved sample order. Callers own all
//! storage, allowing the same `*_into` API to operate on preallocated realtime
//! buffers.

use core::fmt;

use rlb_model::QualityProfile;

/// MEDIA wire codec number. Its numeric order is independent of product rank.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum WireCodec {
    /// IEEE 754 binary32 little-endian, wire value 1.
    F32Le = 1,
    /// Signed two's-complement 16-bit little-endian, wire value 2.
    S16Le = 2,
}

impl WireCodec {
    /// Returns the exact MEDIA header value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Returns bytes used by one interleaved sample.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::F32Le => 4,
            Self::S16Le => 2,
        }
    }

    /// Returns the corresponding product-level quality profile.
    #[must_use]
    pub const fn quality_profile(self) -> QualityProfile {
        match self {
            Self::F32Le => QualityProfile::PcmF32V1,
            Self::S16Le => QualityProfile::PcmS16V1,
        }
    }
}

impl From<QualityProfile> for WireCodec {
    fn from(profile: QualityProfile) -> Self {
        match profile {
            QualityProfile::PcmS16V1 => Self::S16Le,
            QualityProfile::PcmF32V1 => Self::F32Le,
        }
    }
}

impl TryFrom<u8> for WireCodec {
    type Error = CodecError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::F32Le),
            2 => Ok(Self::S16Le),
            value => Err(CodecError::UnsupportedWireCodec { value }),
        }
    }
}

/// Validation failure from an allocation-free codec operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    /// MEDIA V2 only permits mono or stereo payloads.
    UnsupportedChannels {
        /// Rejected channel count.
        channels: u8,
    },
    /// Every operation must contain at least one interleaved frame.
    ZeroFrames,
    /// Frame counts are represented by fixed-width protocol integers.
    TooManyFrames {
        /// Rejected frame count.
        frames: usize,
    },
    /// A sample or byte count overflowed `usize`.
    LengthOverflow,
    /// The input sample slice was not exactly `frames * channels`.
    SampleCountMismatch {
        /// Required number of interleaved samples.
        expected: usize,
        /// Actual input sample count.
        actual: usize,
    },
    /// Encoded input contained fewer or more bytes than the exact payload.
    PayloadLengthMismatch {
        /// Required payload bytes.
        expected: usize,
        /// Actual payload bytes.
        actual: usize,
    },
    /// A caller-provided byte output buffer was too short.
    ByteOutputTooSmall {
        /// Required writable bytes.
        required: usize,
        /// Available writable bytes.
        available: usize,
    },
    /// A caller-provided sample output buffer was too short.
    SampleOutputTooSmall {
        /// Required writable samples.
        required: usize,
        /// Available writable samples.
        available: usize,
    },
    /// The MEDIA header contained an unknown codec value.
    UnsupportedWireCodec {
        /// Rejected wire value.
        value: u8,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnsupportedChannels { channels } => {
                write!(formatter, "channels must be 1 or 2, got {channels}")
            }
            Self::ZeroFrames => formatter.write_str("frame count must be positive"),
            Self::TooManyFrames { frames } => {
                write!(formatter, "frame count exceeds u32, got {frames}")
            }
            Self::LengthOverflow => formatter.write_str("codec length arithmetic overflowed"),
            Self::SampleCountMismatch { expected, actual } => {
                write!(
                    formatter,
                    "sample count mismatch: expected {expected}, got {actual}"
                )
            }
            Self::PayloadLengthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "payload length mismatch: expected {expected}, got {actual}"
                )
            }
            Self::ByteOutputTooSmall {
                required,
                available,
            } => write!(
                formatter,
                "byte output too small: need {required}, have {available}"
            ),
            Self::SampleOutputTooSmall {
                required,
                available,
            } => write!(
                formatter,
                "sample output too small: need {required}, have {available}"
            ),
            Self::UnsupportedWireCodec { value } => {
                write!(formatter, "unsupported wire codec {value}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// Result metadata from encoding into caller-owned storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodeReport {
    /// Bytes written at the beginning of the output slice.
    pub bytes_written: usize,
    /// NaN or infinite inputs replaced with positive zero.
    pub non_finite_samples: usize,
}

/// Result metadata from decoding into caller-owned storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeReport {
    /// Interleaved samples written at the beginning of the output slice.
    pub samples_written: usize,
    /// Non-finite f32 wire values replaced with positive zero.
    pub non_finite_samples: usize,
}

/// Returns the exact sample count for a validated interleaved buffer.
pub fn interleaved_sample_count(channels: u8, frames: usize) -> Result<usize, CodecError> {
    validate_shape(channels, frames)
}

/// Returns the exact encoded payload length.
pub fn encoded_len(codec: WireCodec, channels: u8, frames: usize) -> Result<usize, CodecError> {
    validate_shape(channels, frames)?
        .checked_mul(codec.bytes_per_sample())
        .ok_or(CodecError::LengthOverflow)
}

/// Encodes interleaved f32 samples with the selected wire codec.
///
/// Finite f32 payload values are bit-exact for [`WireCodec::F32Le`].
/// Non-finite input is replaced by positive zero and counted.
pub fn encode_interleaved_into(
    codec: WireCodec,
    channels: u8,
    frames: usize,
    samples: &[f32],
    output: &mut [u8],
) -> Result<EncodeReport, CodecError> {
    match codec {
        WireCodec::F32Le => encode_f32le_into(channels, frames, samples, output),
        WireCodec::S16Le => encode_s16le_into(channels, frames, samples, output),
    }
}

/// Decodes an exact interleaved payload with the selected wire codec.
///
/// Non-finite f32 wire values are replaced by positive zero and counted.
pub fn decode_interleaved_into(
    codec: WireCodec,
    channels: u8,
    frames: usize,
    input: &[u8],
    output: &mut [f32],
) -> Result<DecodeReport, CodecError> {
    match codec {
        WireCodec::F32Le => decode_f32le_into(channels, frames, input, output),
        WireCodec::S16Le => decode_s16le_into(channels, frames, input, output),
    }
}

/// Encodes finite f32 bit patterns as little-endian binary32.
pub fn encode_f32le_into(
    channels: u8,
    frames: usize,
    samples: &[f32],
    output: &mut [u8],
) -> Result<EncodeReport, CodecError> {
    let sample_count = validate_encode(WireCodec::F32Le, channels, frames, samples, output.len())?;
    let mut non_finite_samples = 0;

    for (sample, destination) in samples
        .iter()
        .take(sample_count)
        .zip(output.chunks_exact_mut(4))
    {
        let sanitized = sanitize(*sample, &mut non_finite_samples);
        destination.copy_from_slice(&sanitized.to_bits().to_le_bytes());
    }

    Ok(EncodeReport {
        bytes_written: sample_count * 4,
        non_finite_samples,
    })
}

/// Encodes f32 samples as saturating, ties-to-even signed PCM16.
pub fn encode_s16le_into(
    channels: u8,
    frames: usize,
    samples: &[f32],
    output: &mut [u8],
) -> Result<EncodeReport, CodecError> {
    let sample_count = validate_encode(WireCodec::S16Le, channels, frames, samples, output.len())?;
    let mut non_finite_samples = 0;

    for (sample, destination) in samples
        .iter()
        .take(sample_count)
        .zip(output.chunks_exact_mut(2))
    {
        let sanitized = sanitize(*sample, &mut non_finite_samples);
        let scaled = (sanitized.clamp(-1.0, 1.0) * 32_768.0).round_ties_even();
        let quantized = scaled.clamp(-32_768.0, 32_767.0) as i16;
        destination.copy_from_slice(&quantized.to_le_bytes());
    }

    Ok(EncodeReport {
        bytes_written: sample_count * 2,
        non_finite_samples,
    })
}

/// Decodes an exact little-endian binary32 payload.
pub fn decode_f32le_into(
    channels: u8,
    frames: usize,
    input: &[u8],
    output: &mut [f32],
) -> Result<DecodeReport, CodecError> {
    let sample_count = validate_decode(
        WireCodec::F32Le,
        channels,
        frames,
        input.len(),
        output.len(),
    )?;
    let mut non_finite_samples = 0;

    for (source, destination) in input
        .chunks_exact(4)
        .take(sample_count)
        .zip(output.iter_mut())
    {
        let sample = f32::from_bits(u32::from_le_bytes([
            source[0], source[1], source[2], source[3],
        ]));
        *destination = sanitize(sample, &mut non_finite_samples);
    }

    Ok(DecodeReport {
        samples_written: sample_count,
        non_finite_samples,
    })
}

/// Decodes an exact little-endian signed PCM16 payload.
pub fn decode_s16le_into(
    channels: u8,
    frames: usize,
    input: &[u8],
    output: &mut [f32],
) -> Result<DecodeReport, CodecError> {
    let sample_count = validate_decode(
        WireCodec::S16Le,
        channels,
        frames,
        input.len(),
        output.len(),
    )?;

    for (source, destination) in input
        .chunks_exact(2)
        .take(sample_count)
        .zip(output.iter_mut())
    {
        let quantized = i16::from_le_bytes([source[0], source[1]]);
        *destination = f32::from(quantized) / 32_768.0;
    }

    Ok(DecodeReport {
        samples_written: sample_count,
        non_finite_samples: 0,
    })
}

fn validate_shape(channels: u8, frames: usize) -> Result<usize, CodecError> {
    if !matches!(channels, 1 | 2) {
        return Err(CodecError::UnsupportedChannels { channels });
    }
    if frames == 0 {
        return Err(CodecError::ZeroFrames);
    }
    if u32::try_from(frames).is_err() {
        return Err(CodecError::TooManyFrames { frames });
    }
    frames
        .checked_mul(usize::from(channels))
        .ok_or(CodecError::LengthOverflow)
}

fn validate_encode(
    codec: WireCodec,
    channels: u8,
    frames: usize,
    samples: &[f32],
    output_len: usize,
) -> Result<usize, CodecError> {
    let sample_count = validate_shape(channels, frames)?;
    if samples.len() != sample_count {
        return Err(CodecError::SampleCountMismatch {
            expected: sample_count,
            actual: samples.len(),
        });
    }
    let byte_count = sample_count
        .checked_mul(codec.bytes_per_sample())
        .ok_or(CodecError::LengthOverflow)?;
    if output_len < byte_count {
        return Err(CodecError::ByteOutputTooSmall {
            required: byte_count,
            available: output_len,
        });
    }
    Ok(sample_count)
}

fn validate_decode(
    codec: WireCodec,
    channels: u8,
    frames: usize,
    input_len: usize,
    output_len: usize,
) -> Result<usize, CodecError> {
    let sample_count = validate_shape(channels, frames)?;
    let byte_count = sample_count
        .checked_mul(codec.bytes_per_sample())
        .ok_or(CodecError::LengthOverflow)?;
    if input_len != byte_count {
        return Err(CodecError::PayloadLengthMismatch {
            expected: byte_count,
            actual: input_len,
        });
    }
    if output_len < sample_count {
        return Err(CodecError::SampleOutputTooSmall {
            required: sample_count,
            available: output_len,
        });
    }
    Ok(sample_count)
}

fn sanitize(sample: f32, counter: &mut usize) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        *counter += 1;
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMITIVE_SAMPLES: [f32; 5] = [-1.0, -0.5, 0.0, 0.5, 1.0];

    #[test]
    fn product_rank_and_wire_codec_spaces_remain_independent() {
        assert_eq!(QualityProfile::PcmS16V1.rank(), 1);
        assert_eq!(QualityProfile::PcmF32V1.rank(), 2);
        assert_eq!(WireCodec::from(QualityProfile::PcmF32V1), WireCodec::F32Le);
        assert_eq!(WireCodec::F32Le.as_u8(), 1);
        assert_eq!(WireCodec::from(QualityProfile::PcmS16V1), WireCodec::S16Le);
        assert_eq!(WireCodec::S16Le.as_u8(), 2);
        assert_eq!(
            WireCodec::try_from(3),
            Err(CodecError::UnsupportedWireCodec { value: 3 })
        );
    }

    #[test]
    fn normative_primitive_vectors_match_exactly() {
        let mut f32_bytes = [0_u8; 20];
        let report = encode_f32le_into(
            1,
            PRIMITIVE_SAMPLES.len(),
            &PRIMITIVE_SAMPLES,
            &mut f32_bytes,
        )
        .unwrap();
        assert_eq!(report.bytes_written, 20);
        assert_eq!(
            f32_bytes,
            [
                0x00, 0x00, 0x80, 0xbf, 0x00, 0x00, 0x00, 0xbf, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x3f, 0x00, 0x00, 0x80, 0x3f,
            ]
        );

        let mut s16_bytes = [0_u8; 10];
        encode_s16le_into(
            1,
            PRIMITIVE_SAMPLES.len(),
            &PRIMITIVE_SAMPLES,
            &mut s16_bytes,
        )
        .unwrap();
        assert_eq!(
            s16_bytes,
            [0x00, 0x80, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x40, 0xff, 0x7f]
        );
    }

    #[test]
    fn finite_f32_roundtrip_is_bit_exact_and_interleaved() {
        let samples = [-0.0, 0.0, 1.25, -2.5, f32::from_bits(1), f32::MAX];
        let mut encoded = [0_u8; 24];
        let mut decoded = [0.0_f32; 6];
        encode_interleaved_into(WireCodec::F32Le, 2, 3, &samples, &mut encoded).unwrap();
        decode_interleaved_into(WireCodec::F32Le, 2, 3, &encoded, &mut decoded).unwrap();
        assert_eq!(
            decoded.map(f32::to_bits),
            samples.map(f32::to_bits),
            "frame-interleaved L/R order and finite bit patterns must survive"
        );
    }

    #[test]
    fn pcm16_is_saturating_and_ties_to_even() {
        let quantum = 1.0 / 32_768.0;
        let samples = [
            -4.0,
            -1.0,
            -2.5 * quantum,
            -1.5 * quantum,
            -0.5 * quantum,
            0.5 * quantum,
            1.5 * quantum,
            2.5 * quantum,
            1.0,
            4.0,
        ];
        let mut encoded = [0_u8; 20];
        encode_s16le_into(1, samples.len(), &samples, &mut encoded).unwrap();
        let quantized: Vec<i16> = encoded
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            quantized,
            [-32_768, -32_768, -2, -2, 0, 0, 2, 2, 32_767, 32_767]
        );
    }

    #[test]
    fn pcm16_roundtrip_has_quantization_bound() {
        let samples = [-1.0, -0.75, -0.1, 0.0, 0.1, 0.75, 0.999];
        let mut encoded = [0_u8; 14];
        let mut decoded = [0.0_f32; 7];
        encode_s16le_into(1, samples.len(), &samples, &mut encoded).unwrap();
        decode_s16le_into(1, samples.len(), &encoded, &mut decoded).unwrap();
        for (expected, actual) in samples.into_iter().zip(decoded) {
            assert!((expected - actual).abs() <= 1.0 / 32_768.0);
        }
    }

    #[test]
    fn non_finite_samples_are_zeroed_and_counted_at_both_boundaries() {
        let samples = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25];
        let mut encoded = [0xff_u8; 16];
        let report = encode_f32le_into(1, 4, &samples, &mut encoded).unwrap();
        assert_eq!(report.non_finite_samples, 3);
        assert_eq!(&encoded[..12], &[0; 12]);

        encoded[..4].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
        let mut decoded = [1.0_f32; 4];
        let report = decode_f32le_into(1, 4, &encoded, &mut decoded).unwrap();
        assert_eq!(report.non_finite_samples, 1);
        assert_eq!(decoded[0].to_bits(), 0.0_f32.to_bits());

        let mut pcm16 = [0xff_u8; 8];
        let report = encode_s16le_into(1, 4, &samples, &mut pcm16).unwrap();
        assert_eq!(report.non_finite_samples, 3);
        assert_eq!(&pcm16[..6], &[0; 6]);
    }

    #[test]
    fn shape_and_buffer_errors_are_deterministic() {
        let samples = [0.0_f32; 2];
        let mut bytes = [0_u8; 8];
        let mut output = [0.0_f32; 2];

        assert_eq!(
            encode_f32le_into(0, 2, &samples, &mut bytes),
            Err(CodecError::UnsupportedChannels { channels: 0 })
        );
        assert_eq!(
            encode_f32le_into(3, 2, &samples, &mut bytes),
            Err(CodecError::UnsupportedChannels { channels: 3 })
        );
        assert_eq!(
            encode_f32le_into(1, 0, &[], &mut bytes),
            Err(CodecError::ZeroFrames)
        );
        assert_eq!(
            encode_f32le_into(2, 2, &samples, &mut bytes),
            Err(CodecError::SampleCountMismatch {
                expected: 4,
                actual: 2
            })
        );
        assert_eq!(
            encode_f32le_into(1, 2, &samples, &mut bytes[..7]),
            Err(CodecError::ByteOutputTooSmall {
                required: 8,
                available: 7
            })
        );
        assert_eq!(
            decode_f32le_into(1, 2, &bytes[..7], &mut output),
            Err(CodecError::PayloadLengthMismatch {
                expected: 8,
                actual: 7
            })
        );
        assert_eq!(
            decode_f32le_into(1, 2, &bytes, &mut output[..1]),
            Err(CodecError::SampleOutputTooSmall {
                required: 2,
                available: 1
            })
        );
        if usize::BITS > 32 {
            assert!(matches!(
                encoded_len(WireCodec::F32Le, 2, u32::MAX as usize + 1),
                Err(CodecError::TooManyFrames { .. })
            ));
        }
    }

    #[test]
    fn output_tail_remains_available_to_preallocated_callers() {
        let samples = [0.25_f32, -0.25];
        let mut bytes = [0xaa_u8; 16];
        let report = encode_f32le_into(1, 2, &samples, &mut bytes).unwrap();
        assert_eq!(report.bytes_written, 8);
        assert_eq!(&bytes[8..], &[0xaa; 8]);

        let mut decoded = [99.0_f32; 4];
        let report = decode_f32le_into(1, 2, &bytes[..8], &mut decoded).unwrap();
        assert_eq!(report.samples_written, 2);
        assert_eq!(&decoded[2..], &[99.0; 2]);
    }
}
