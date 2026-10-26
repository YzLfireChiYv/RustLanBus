//! F64 accumulation retained through the final endpoint-format conversion.
//!
//! Gains are explicit. Source count does not scale the mix.

use super::contract::RenderError;
#[cfg(test)]
use super::contract::{ABSOLUTE_MAX_CHANNELS, TimelineId};
#[cfg(test)]
use super::contract::{ChannelMatrix, ChannelRoute};

/// A short output-time interpolation. It stores no audio and adds no queue.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GainRamp {
    current: f64,
    target: f64,
    remaining: u32,
}

impl GainRamp {
    pub(crate) const fn fixed(value: f64) -> Self {
        Self {
            current: value,
            target: value,
            remaining: 0,
        }
    }

    pub(crate) fn target(&self) -> f64 {
        self.target
    }

    pub(crate) fn set_target(&mut self, value: f64, frames: u32) {
        if self.target != value {
            self.target = value;
            self.remaining = frames.max(1);
        }
    }

    pub(crate) fn next(&mut self) -> f64 {
        if self.remaining != 0 {
            let weight = 1.0 / f64::from(self.remaining);
            self.current = self.current * (1.0 - weight) + self.target * weight;
            self.remaining -= 1;
        } else {
            self.current = self.target;
        }
        self.current
    }
}

pub(crate) fn smoothing_frames(rate: u32) -> u32 {
    // Two milliseconds change only the transition envelope. No source frame
    // is delayed or replayed to fill this interval.
    rate.div_ceil(500).max(1)
}

#[cfg(test)]
pub(crate) fn accumulate_matrix(
    mix: &mut [f64],
    output_channels: usize,
    source: &[f64],
    source_channels: usize,
    frames: usize,
    gain: f64,
    matrix: &ChannelMatrix,
) {
    if !gain.is_finite() || gain == 0.0 {
        return;
    }
    for frame in 0..frames {
        for output_channel in 0..output_channels.min(matrix.gains.len()) {
            for source_channel in 0..source_channels.min(matrix.gains[output_channel].len()) {
                let coefficient = matrix.gains[output_channel][source_channel];
                if coefficient != 0.0 {
                    mix[frame * output_channels + output_channel] +=
                        source[frame * source_channels + source_channel] * gain * coefficient;
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn accumulate(
    mix: &mut [f64],
    output_channels: usize,
    source: &[f64],
    source_channels: usize,
    frames: usize,
    gain: f64,
    route: &ChannelRoute,
) {
    if !gain.is_finite() || gain == 0.0 {
        return;
    }
    for frame in 0..frames {
        for source_channel in 0..source_channels.min(route.output_for_source.len()) {
            let Some(output_channel) = route.output_for_source[source_channel] else {
                continue;
            };
            let output_channel = usize::from(output_channel);
            if output_channel >= output_channels {
                continue;
            }
            let sample = source[frame * source_channels + source_channel] * gain;
            mix[frame * output_channels + output_channel] += sample;
        }
    }
}

#[cfg(test)]
pub(crate) fn identity_route(
    timeline: TimelineId,
    source_channels: usize,
    output_channels: usize,
) -> Option<ChannelRoute> {
    if source_channels == 0 || source_channels != output_channels {
        return None;
    }
    let mut output_for_source = [None; ABSOLUTE_MAX_CHANNELS];
    for channel in 0..source_channels.min(output_for_source.len()) {
        output_for_source[channel] = Some(u8::try_from(channel).unwrap_or(u8::MAX));
    }
    Some(ChannelRoute {
        timeline,
        output_for_source,
    })
}

#[cfg(test)]
pub(crate) fn finish_f64(
    mix: &mut [f64],
    destination: &mut [f64],
    master_gain: f64,
) -> Result<(f64, bool), RenderError> {
    if !master_gain.is_finite() {
        return Err(RenderError::NumericalOverflow);
    }
    if destination.len() < mix.len() {
        return Err(RenderError::Capacity(
            "the endpoint buffer is shorter than the mix",
        ));
    }
    let mut peak = 0.0_f64;
    for (index, sample) in mix.iter_mut().enumerate() {
        let scaled = *sample * master_gain;
        if !scaled.is_finite() {
            return Err(RenderError::NumericalOverflow);
        }
        peak = peak.max(scaled.abs());
        destination[index] = scaled;
        *sample = 0.0;
    }
    Ok((peak, peak > 1.0))
}

pub(crate) fn finish_smoothed_f64(
    mix: &mut [f64],
    destination: &mut [f64],
    channels: usize,
    master_gain: &mut GainRamp,
) -> Result<(f64, bool), RenderError> {
    if channels == 0 || destination.len() < mix.len() {
        return Err(RenderError::Capacity(
            "the endpoint buffer is shorter than the mix",
        ));
    }
    let mut peak = 0.0_f64;
    for (frame, source) in mix.chunks_mut(channels).enumerate() {
        let gain = master_gain.next();
        for (channel, sample) in source.iter_mut().enumerate() {
            let scaled = *sample * gain;
            if !scaled.is_finite() {
                return Err(RenderError::NumericalOverflow);
            }
            peak = peak.max(scaled.abs());
            destination[frame * channels + channel] = scaled;
            *sample = 0.0;
        }
    }
    Ok((peak, peak > 1.0))
}

#[cfg(test)]
pub(crate) fn finish_f32(
    mix: &mut [f64],
    destination: &mut [f32],
    master_gain: f64,
) -> Result<(f64, bool), RenderError> {
    if !master_gain.is_finite() {
        return Err(RenderError::NumericalOverflow);
    }
    if destination.len() < mix.len() {
        return Err(RenderError::Capacity(
            "the endpoint buffer is shorter than the mix",
        ));
    }
    let mut peak = 0.0_f64;
    for (index, sample) in mix.iter_mut().enumerate() {
        let scaled = *sample * master_gain;
        if !scaled.is_finite() || scaled > f64::from(f32::MAX) || scaled < f64::from(f32::MIN) {
            return Err(RenderError::NumericalOverflow);
        }
        peak = peak.max(scaled.abs());
        destination[index] = scaled as f32;
        *sample = 0.0;
    }
    Ok((peak, peak > 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::{RunId, SourceId, StreamEpoch};
    use rlb_peer::ChannelLayout;

    fn timeline() -> TimelineId {
        TimelineId::Local {
            run: RunId::from_bytes([1; 16]).unwrap(),
            source: SourceId::from_bytes([2; 16]).unwrap(),
            acquisition: StreamEpoch::from_bytes([3; 16]).unwrap(),
        }
    }

    #[test]
    fn relative_gains_are_preserved_and_overflow_is_reported() {
        let route = identity_route(timeline(), 1, 1).unwrap();
        let mut mix = [0.0; 2];
        accumulate(&mut mix, 1, &[0.5, -0.25], 1, 2, 0.5, &route);
        accumulate(&mut mix, 1, &[0.5, 0.25], 1, 2, 1.0, &route);
        let mut pcm = [0.0; 2];
        let (peak, over) = finish_f32(&mut mix, &mut pcm, 1.0).unwrap();
        assert_eq!(pcm[0], 0.75);
        assert_eq!(pcm[1], 0.125);
        assert!((peak - 0.75).abs() < 1.0e-9);
        assert!(!over);
        let mut hot = [2.0];
        let (_, over) = finish_f32(&mut hot, &mut [0.0], 1.0).unwrap();
        assert!(over);
        assert!(finish_f32(&mut [f64::INFINITY], &mut [0.0], 1.0).is_err());
    }

    #[test]
    fn matrix_duplicates_mono_and_balances_stereo_before_master_gain() {
        let mono = [0.5, -0.25];
        let stereo = [0.25, -0.5, -0.5, 0.25];
        let duplicate = super::super::default_channel_matrix(
            timeline(),
            ChannelLayout::Mono,
            ChannelLayout::StereoLeftRight,
            0.0,
        )
        .unwrap();
        let balance = super::super::default_channel_matrix(
            timeline(),
            ChannelLayout::StereoLeftRight,
            ChannelLayout::StereoLeftRight,
            0.5,
        )
        .unwrap();
        let mut mixed = [0.0; 4];
        accumulate_matrix(&mut mixed, 2, &mono, 1, 2, 0.5, &duplicate);
        accumulate_matrix(&mut mixed, 2, &stereo, 2, 2, 1.0, &balance);
        let mut pcm = [0.0; 4];
        finish_f32(&mut mixed, &mut pcm, 0.5).unwrap();
        assert_eq!(pcm, [0.1875, -0.125, -0.1875, 0.0625]);
        assert_eq!(mono, [0.5, -0.25]);
        assert_eq!(stereo, [0.25, -0.5, -0.5, 0.25]);
    }
}
