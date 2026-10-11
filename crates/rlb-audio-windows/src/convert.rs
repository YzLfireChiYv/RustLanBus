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
}
