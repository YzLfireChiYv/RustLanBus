use crate::format::{NativePcmFormat, SampleEncoding};
use rlb_audio::{AudioError, CaptureProcessBuffer};

pub(crate) fn copy_interleaved_f32_to_planar(
    interleaved: &[f32],
    frames: usize,
    channels: u8,
    destination: &mut CaptureProcessBuffer<'_>,
) -> Result<u64, AudioError> {
    ensure_destination_capacity(destination, frames, channels)?;
    let required =
        frames
            .checked_mul(usize::from(channels))
            .ok_or(AudioError::CapacityOverflow {
                field: "capture_interleaved_samples",
            })?;
    if interleaved.len() < required {
        return Err(AudioError::BackendContractViolation);
    }
    let mut non_finite = 0_u64;
    for channel in 0..channels {
        let dest = destination
            .channel_mut(channel)
            .ok_or(AudioError::BackendContractViolation)?;
        for frame in 0..frames {
            let sample = interleaved[frame * usize::from(channels) + usize::from(channel)];
            dest[frame] = sanitize(sample, &mut non_finite);
        }
    }
    Ok(non_finite)
}

pub(crate) fn write_silence(
    frames: usize,
    channels: u8,
    destination: &mut CaptureProcessBuffer<'_>,
) -> Result<(), AudioError> {
    ensure_destination_capacity(destination, frames, channels)?;
    for channel in 0..channels {
        let dest = destination
            .channel_mut(channel)
            .ok_or(AudioError::BackendContractViolation)?;
        dest[..frames].fill(0.0);
    }
    Ok(())
}

fn ensure_destination_capacity(
    destination: &CaptureProcessBuffer<'_>,
    frames: usize,
    channels: u8,
) -> Result<(), AudioError> {
    if destination.channel_capacity() < channels || destination.frame_capacity() < frames {
        Err(AudioError::CaptureCapacityExceeded)
    } else {
        Ok(())
    }
}

fn sanitize(sample: f32, non_finite: &mut u64) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        *non_finite = non_finite.saturating_add(1);
        0.0
    }
}

/// Failure while copying one native 32-bit float packet into caller storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeCopyFailure {
    /// The caller buffer is shorter than the packet.
    TooSmall {
        /// Samples required by the packet.
        required: usize,
        /// Samples available in the caller buffer.
        actual: usize,
    },
    /// The byte length is not a whole number of 32-bit samples.
    MalformedBytes {
        /// Byte length that was supplied.
        bytes: usize,
    },
    /// At least one sample was NaN or infinite. The destination is unchanged.
    NonFinite {
        /// Non-finite samples in the source.
        samples: u64,
    },
}

/// Decodes a native integer or floating-point packet directly into canonical
/// F64 storage. Integer padding occupies the least significant container bits.
/// Refused floating-point packets leave caller storage unchanged.
pub(crate) fn copy_native_to_f64(
    format: NativePcmFormat,
    source: &[u8],
    destination: &mut [f64],
) -> Result<(), NativeCopyFailure> {
    let width = usize::from(format.bits_per_sample / 8);
    if width == 0 || !source.len().is_multiple_of(width) {
        return Err(NativeCopyFailure::MalformedBytes {
            bytes: source.len(),
        });
    }
    let samples = source.len() / width;
    if destination.len() < samples {
        return Err(NativeCopyFailure::TooSmall {
            required: samples,
            actual: destination.len(),
        });
    }
    if format.encoding == SampleEncoding::Float {
        let non_finite = source
            .chunks_exact(width)
            .filter(|bytes| !decode_native_sample(format, bytes).is_finite())
            .count();
        if non_finite != 0 {
            return Err(NativeCopyFailure::NonFinite {
                samples: non_finite as u64,
            });
        }
    }
    for (bytes, sample) in source.chunks_exact(width).zip(destination.iter_mut()) {
        *sample = decode_native_sample(format, bytes);
    }
    Ok(())
}

fn decode_native_sample(format: NativePcmFormat, bytes: &[u8]) -> f64 {
    match (format.encoding, format.bits_per_sample) {
        (SampleEncoding::Float, 32) => f64::from(f32::from_le_bytes(bytes.try_into().unwrap())),
        (SampleEncoding::Float, 64) => f64::from_le_bytes(bytes.try_into().unwrap()),
        (SampleEncoding::IntegerPcm, bits) => {
            let container = match bits {
                16 => i32::from(i16::from_le_bytes(bytes.try_into().unwrap())),
                24 => i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8,
                32 => i32::from_le_bytes(bytes.try_into().unwrap()),
                _ => unreachable!("validated native integer container"),
            };
            let valid = format.valid_bits_per_sample;
            let integer = container >> (bits - valid);
            f64::from(integer) / (1_u64 << (valid - 1)) as f64
        }
        _ => unreachable!("validated native sample encoding"),
    }
}

/// Validates values before a render buffer is acquired. Integer output uses
/// explicit full-scale saturation; floating-point output retains values above
/// unity, while values outside the finite device container are refused.
pub(crate) fn validate_native_f64(
    format: NativePcmFormat,
    source: &[f64],
) -> Result<(), AudioError> {
    if source.iter().any(|sample| !sample.is_finite())
        || (format.is_f32()
            && source
                .iter()
                .any(|sample| sample.abs() > f64::from(f32::MAX)))
    {
        return Err(AudioError::BackendContractViolation);
    }
    Ok(())
}

/// Writes a previously validated render span into its native PCM container.
/// Quantization occurs here, after F64 conversion, gains and accumulation.
pub(crate) fn write_native_f64(format: NativePcmFormat, source: &[f64], destination: &mut [u8]) {
    let width = usize::from(format.bits_per_sample / 8);
    for (&sample, bytes) in source.iter().zip(destination.chunks_exact_mut(width)) {
        match (format.encoding, format.bits_per_sample) {
            (SampleEncoding::Float, 32) => bytes.copy_from_slice(&(sample as f32).to_le_bytes()),
            (SampleEncoding::Float, 64) => bytes.copy_from_slice(&sample.to_le_bytes()),
            (SampleEncoding::IntegerPcm, bits) => {
                let valid = format.valid_bits_per_sample;
                let scale = (1_u64 << (valid - 1)) as f64;
                let integer = (sample * scale).round().clamp(-scale, scale - 1.0) as i64;
                let container = integer << (bits - valid);
                match bits {
                    16 => bytes.copy_from_slice(&(container as i16).to_le_bytes()),
                    24 => bytes.copy_from_slice(&(container as i32).to_le_bytes()[..3]),
                    32 => bytes.copy_from_slice(&(container as i32).to_le_bytes()),
                    _ => unreachable!("validated native integer container"),
                }
            }
            _ => unreachable!("validated native sample encoding"),
        }
    }
}

/// Copies little-endian 32-bit float bytes when every sample is finite.
///
/// Finite values keep their bits, including negative zero, subnormals, and
/// magnitudes above 1. A non-finite source leaves `destination` unchanged.
pub(crate) fn copy_finite_f32_le(
    source: &[u8],
    destination: &mut [f32],
) -> Result<(), NativeCopyFailure> {
    if !source.len().is_multiple_of(core::mem::size_of::<f32>()) {
        return Err(NativeCopyFailure::MalformedBytes {
            bytes: source.len(),
        });
    }
    let samples = source.len() / core::mem::size_of::<f32>();
    if destination.len() < samples {
        return Err(NativeCopyFailure::TooSmall {
            required: samples,
            actual: destination.len(),
        });
    }
    let mut non_finite = 0_u64;
    for chunk in source.chunks_exact(core::mem::size_of::<f32>()) {
        let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
        if !f32::from_le_bytes(bytes).is_finite() {
            non_finite = non_finite.saturating_add(1);
        }
    }
    if non_finite > 0 {
        return Err(NativeCopyFailure::NonFinite {
            samples: non_finite,
        });
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            source.as_ptr(),
            destination.as_mut_ptr().cast::<u8>(),
            source.len(),
        );
    }
    Ok(())
}

/// Counts NaN and infinite samples in caller-owned render storage.
#[must_use]
pub(crate) fn non_finite_sample_count(samples: &[f32]) -> u64 {
    let count = samples.iter().filter(|sample| !sample.is_finite()).count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Writes exact `+0.0` across `sample_count` interleaved samples.
#[cfg(test)]
pub(crate) fn write_interleaved_silence(
    sample_count: usize,
    destination: &mut [f32],
) -> Result<(), NativeCopyFailure> {
    if destination.len() < sample_count {
        return Err(NativeCopyFailure::TooSmall {
            required: sample_count,
            actual: destination.len(),
        });
    }
    destination[..sample_count].fill(0.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_stereo_becomes_planar_and_non_finite_becomes_zero() {
        let mut storage = [0.0_f32; 8];
        let mut destination = CaptureProcessBuffer::new(&mut storage, 2, 2).expect("dest");
        let non_finite = copy_interleaved_f32_to_planar(
            &[0.5, -0.25, f32::NAN, f32::INFINITY],
            2,
            2,
            &mut destination,
        )
        .expect("copy");
        assert_eq!(non_finite, 2);
        assert_eq!(destination.channel_mut(0).unwrap()[..2], [0.5, 0.0]);
        assert_eq!(destination.channel_mut(1).unwrap()[..2], [-0.25, 0.0]);
    }

    #[test]
    fn silence_fills_only_the_declared_frame_count() {
        let mut storage = [1.0_f32; 6];
        let mut destination = CaptureProcessBuffer::new(&mut storage, 3, 1).expect("dest");
        write_silence(2, 1, &mut destination).expect("silence");
        assert_eq!(destination.channel_mut(0).unwrap()[..3], [0.0, 0.0, 1.0]);
    }

    #[test]
    fn oversized_packet_is_a_capacity_violation() {
        let mut storage = [0.0_f32; 2];
        let mut destination = CaptureProcessBuffer::new(&mut storage, 1, 1).expect("dest");
        assert_eq!(
            copy_interleaved_f32_to_planar(&[0.1, 0.2], 2, 1, &mut destination),
            Err(AudioError::CaptureCapacityExceeded)
        );
    }

    #[test]
    fn native_copy_preserves_finite_bits_and_refuses_non_finite() {
        let mut source = [0_u8; 16];
        source[0..4].copy_from_slice(&(-0.0_f32).to_le_bytes());
        source[4..8].copy_from_slice(&f32::from_bits(1).to_le_bytes());
        source[8..12].copy_from_slice(&2.5_f32.to_le_bytes());
        source[12..16].copy_from_slice(&f32::MIN.to_le_bytes());
        let mut destination = [0.25_f32; 4];
        copy_finite_f32_le(&source, &mut destination).expect("finite copy");
        assert_eq!(destination[0].to_bits(), (-0.0_f32).to_bits());
        assert_eq!(destination[1].to_bits(), 1);
        assert_eq!(destination[2].to_bits(), 2.5_f32.to_bits());
        assert_eq!(destination[3].to_bits(), f32::MIN.to_bits());

        let mut rejected = [0_u8; 8];
        rejected[0..4].copy_from_slice(&0.5_f32.to_le_bytes());
        rejected[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
        let mut unchanged = [0.25_f32; 2];
        assert_eq!(
            copy_finite_f32_le(&rejected, &mut unchanged),
            Err(NativeCopyFailure::NonFinite { samples: 1 })
        );
        assert_eq!(unchanged, [0.25, 0.25]);
        assert_eq!(
            non_finite_sample_count(&[0.0, f32::INFINITY, f32::NEG_INFINITY]),
            2
        );
        assert_eq!(
            copy_finite_f32_le(&[1, 2, 3], &mut unchanged),
            Err(NativeCopyFailure::MalformedBytes { bytes: 3 })
        );
        write_interleaved_silence(1, &mut unchanged).expect("silence");
        assert_eq!(unchanged[0].to_bits(), 0);
        assert_eq!(unchanged[1].to_bits(), 0.25_f32.to_bits());
    }
}
