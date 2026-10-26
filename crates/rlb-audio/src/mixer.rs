use core::f32::consts::{FRAC_PI_2, FRAC_PI_4};
use std::sync::Arc;

use rlb_model::{MemberId, MemberMix, SAMPLE_RATE_HZ, SourceId, SourceMix};

use crate::{AudioError, AudioMetrics};

/// Two milliseconds at the fixed 48 kHz engine rate.
pub const RAMP_FRAMES: usize = (SAMPLE_RATE_HZ as usize * 2) / 1_000;
/// Default onset of the bounded final soft limiter.
pub const DEFAULT_LIMITER_KNEE: f32 = 0.95;
/// Stable diagnostics identity of the final protection stage.
pub const LIMITER_ALGORITHM_ID: &str = "bounded_soft_limiter_v1";

/// Result returned by one per-source reader call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceReadOutcome {
    /// Number of valid frames written from the start of every channel plane.
    pub frames_written: usize,
}

/// A render-owned view of one independently advancing source timeline.
///
/// `planar_output` has `frames * channels` samples. Channel planes are compact:
/// channel `c` begins at `c * frames`. Implementations leave no owning
/// reference whose final drop can occur inside the call.
pub trait SourceReader: Send {
    /// Reads immediately and returns a short result for unavailable tail frames.
    fn read(&mut self, planar_output: &mut [f32], frames: usize, channels: u8)
    -> SourceReadOutcome;
}

/// Prepared identity, controls, and routing for one render source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceRenderConfig {
    /// Stable owner member identity.
    pub member_id: MemberId,
    /// Stable source identity.
    pub source_id: SourceId,
    /// Mono or stereo source format.
    pub channels: u8,
    /// Whether the source is locally routed to system playback.
    pub routed: bool,
    /// Whether this online, approved, locally visible member activates solo.
    pub member_solo_eligible: bool,
    /// Persistent source-level controls from `rlb-model`.
    pub source_mix: SourceMix,
    /// Persistent member-level controls from `rlb-model`.
    pub member_mix: MemberMix,
}

impl SourceRenderConfig {
    fn validate(self) -> Result<(), AudioError> {
        if matches!(self.channels, 1 | 2) {
            Ok(())
        } else {
            Err(AudioError::UnsupportedChannels {
                channels: self.channels,
            })
        }
    }
}

/// Cumulative counters for one independently read source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceRenderMetricsSnapshot {
    /// Stable owner identity.
    pub member_id: MemberId,
    /// Stable source identity.
    pub source_id: SourceId,
    /// Render frames for which this reader advanced.
    pub render_frames_total: u64,
    /// Exact frames returned by the source reader.
    pub exact_frames_total: u64,
    /// Missing frames replaced by source-local silence.
    pub underrun_frames_total: u64,
    /// Reader calls containing at least one missing frame.
    pub underrun_events_total: u64,
    /// Source samples sanitized before gain and summing.
    pub non_finite_samples_total: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct SourceRenderMetrics {
    render_frames: u64,
    exact_frames: u64,
    underrun_frames: u64,
    underrun_events: u64,
    non_finite_samples: u64,
}

#[derive(Clone, Copy, Debug)]
struct CoefficientRamp {
    current: [f32; 2],
    target: [f32; 2],
    step: [f32; 2],
    remaining: usize,
}

impl CoefficientRamp {
    const fn silent() -> Self {
        Self {
            current: [0.0; 2],
            target: [0.0; 2],
            step: [0.0; 2],
            remaining: 0,
        }
    }

    fn retarget(&mut self, target: [f32; 2]) {
        self.target = target;
        if self.current == target {
            self.step = [0.0; 2];
            self.remaining = 0;
            return;
        }
        self.step = [
            (target[0] - self.current[0]) / RAMP_FRAMES as f32,
            (target[1] - self.current[1]) / RAMP_FRAMES as f32,
        ];
        self.remaining = RAMP_FRAMES;
    }

    fn advance(&mut self) -> [f32; 2] {
        if self.remaining > 0 {
            self.current[0] += self.step[0];
            self.current[1] += self.step[1];
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }
}

struct PreparedSourceSlot {
    config: Option<SourceRenderConfig>,
    reader: Option<Box<dyn SourceReader>>,
    scratch: Box<[f32]>,
    ramp: CoefficientRamp,
    metrics: SourceRenderMetrics,
}

impl PreparedSourceSlot {
    fn new(max_render_frames: usize) -> Result<Self, AudioError> {
        let samples = max_render_frames
            .checked_mul(2)
            .ok_or(AudioError::CapacityOverflow {
                field: "source_render_scratch",
            })?;
        Ok(Self {
            config: None,
            reader: None,
            scratch: vec![0.0; samples].into_boxed_slice(),
            ramp: CoefficientRamp::silent(),
            metrics: SourceRenderMetrics::default(),
        })
    }
}

/// Stateless, zero-lookahead soft limiter with a unity region below its knee.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoftLimiter {
    knee: f32,
}

impl SoftLimiter {
    /// Validates a finite knee in the open interval `(0, 1)`.
    pub fn new(knee: f32) -> Result<Self, AudioError> {
        if !knee.is_finite() || !(0.0..1.0).contains(&knee) {
            return Err(AudioError::InvalidLimiterKnee);
        }
        Ok(Self { knee })
    }

    /// Returns the start of soft limiting.
    #[must_use]
    pub const fn knee(self) -> f32 {
        self.knee
    }

    /// Bounds one finite sample to `[-1, 1]`.
    #[must_use]
    pub fn process(self, sample: f32) -> f32 {
        if !sample.is_finite() {
            return 0.0;
        }
        let magnitude = sample.abs();
        if magnitude <= self.knee {
            return sample;
        }
        let shoulder = 1.0 - self.knee;
        let excess = magnitude - self.knee;
        let limited = self.knee + shoulder * (excess / (excess + shoulder));
        sample.signum() * limited.min(1.0)
    }

    /// Same optional limiter at the render core's working precision.
    #[must_use]
    pub fn process_f64(self, sample: f64) -> f64 {
        if !sample.is_finite() { return 0.0; }
        let knee = f64::from(self.knee);
        let magnitude = sample.abs();
        if magnitude <= knee { return sample; }
        let shoulder = 1.0 - knee;
        let excess = magnitude - knee;
        sample.signum() * (knee + shoulder * (excess / (excess + shoulder))).min(1.0)
    }
}

impl Default for SoftLimiter {
    fn default() -> Self {
        Self {
            knee: DEFAULT_LIMITER_KNEE,
        }
    }
}

/// Summary of one shared render callback.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderReport {
    /// Output frames written.
    pub frames: usize,
    /// Independently advanced source readers.
    pub active_sources: usize,
    /// Source readers returning a short result.
    pub underrun_sources: usize,
    /// Missing source frames replaced with source-local silence.
    pub underrun_frames: usize,
    /// Non-finite samples replaced with positive zero.
    pub non_finite_samples: u64,
    /// Stereo output samples above the limiter knee.
    pub limited_samples: u64,
    /// Maximum finite absolute sum before limiting.
    pub max_pre_limiter_abs: f32,
}

/// Fixed-slot mixer driven by one shared render callback and output clock.
pub struct SharedRenderMixer {
    slots: Vec<PreparedSourceSlot>,
    max_render_frames: usize,
    limiter: SoftLimiter,
    metrics: Arc<AudioMetrics>,
}

impl SharedRenderMixer {
    /// Preallocates every source scratch plane before rendering begins.
    pub fn new(
        source_capacity: usize,
        max_render_frames: usize,
        limiter: SoftLimiter,
        metrics: Arc<AudioMetrics>,
    ) -> Result<Self, AudioError> {
        if source_capacity == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "render_source_capacity",
            });
        }
        if max_render_frames == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "render_frame_capacity",
            });
        }
        let mut slots = Vec::with_capacity(source_capacity);
        for _ in 0..source_capacity {
            slots.push(PreparedSourceSlot::new(max_render_frames)?);
        }
        Ok(Self {
            slots,
            max_render_frames,
            limiter,
            metrics,
        })
    }

    /// Installs a prepared per-source reader on a non-realtime control thread.
    pub fn add_source(
        &mut self,
        config: SourceRenderConfig,
        reader: Box<dyn SourceReader>,
    ) -> Result<(), AudioError> {
        self.try_add_source(config, reader)
            .map_err(|(error, _)| error)
    }

    /// Installs a reader and returns ownership when the graph cannot accept it.
    ///
    /// Mix-graph apply on the render thread uses this so a rejected reader can
    /// be retired on the coordinator thread instead of dropped in the callback.
    #[allow(clippy::result_large_err)]
    pub fn try_add_source(
        &mut self,
        config: SourceRenderConfig,
        reader: Box<dyn SourceReader>,
    ) -> Result<(), (AudioError, Box<dyn SourceReader>)> {
        if let Err(error) = config.validate() {
            return Err((error, reader));
        }
        if self.slots.iter().any(|slot| {
            slot.config
                .is_some_and(|item| item.source_id == config.source_id)
        }) {
            return Err((AudioError::DuplicateSource, reader));
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.config.is_none()) else {
            return Err((AudioError::SourceCapacityExceeded, reader));
        };
        slot.config = Some(config);
        slot.reader = Some(reader);
        slot.metrics = SourceRenderMetrics::default();
        slot.ramp = CoefficientRamp::silent();
        self.refresh_targets();
        self.publish_control_update();
        Ok(())
    }

    /// Removes one reader on a non-realtime control thread.
    pub fn remove_source(&mut self, source_id: SourceId) -> Result<(), AudioError> {
        drop(self.take_source(source_id)?);
        Ok(())
    }

    /// Uninstalls one reader and returns it for deferred drop.
    pub fn take_source(
        &mut self,
        source_id: SourceId,
    ) -> Result<Box<dyn SourceReader>, AudioError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.config.is_some_and(|item| item.source_id == source_id))
            .ok_or(AudioError::SourceNotFound)?;
        let reader = slot.reader.take().ok_or(AudioError::SourceNotFound)?;
        slot.config = None;
        slot.ramp = CoefficientRamp::silent();
        slot.metrics = SourceRenderMetrics::default();
        self.refresh_targets();
        self.publish_control_update();
        Ok(reader)
    }

    /// Counts a mix-graph apply that could not complete inside a callback.
    pub fn record_callback_contract_violation(&self) {
        self.metrics.add_callback_contract_violation();
    }

    /// Updates source gain, pan, and mute using validated `rlb-model` values.
    pub fn set_source_mix(
        &mut self,
        source_id: SourceId,
        source_mix: SourceMix,
    ) -> Result<(), AudioError> {
        let config = self
            .config_mut(source_id)
            .ok_or(AudioError::SourceNotFound)?;
        config.source_mix = source_mix;
        self.refresh_targets();
        self.publish_control_update();
        Ok(())
    }

    /// Updates member gain, mute, and solo for all of that member's sources.
    pub fn set_member_mix(
        &mut self,
        member_id: MemberId,
        member_mix: MemberMix,
    ) -> Result<(), AudioError> {
        let mut found = false;
        for slot in &mut self.slots {
            if let Some(config) = &mut slot.config
                && config.member_id == member_id
            {
                config.member_mix = member_mix;
                found = true;
            }
        }
        if !found {
            return Err(AudioError::MemberNotFound);
        }
        self.refresh_targets();
        self.publish_control_update();
        Ok(())
    }

    /// Changes whether a source is routed while its reader continues advancing.
    pub fn set_source_routed(
        &mut self,
        source_id: SourceId,
        routed: bool,
    ) -> Result<(), AudioError> {
        let config = self
            .config_mut(source_id)
            .ok_or(AudioError::SourceNotFound)?;
        config.routed = routed;
        self.refresh_targets();
        self.publish_control_update();
        Ok(())
    }

    /// Changes whether an online member participates in the active solo set.
    pub fn set_member_solo_eligible(
        &mut self,
        member_id: MemberId,
        eligible: bool,
    ) -> Result<(), AudioError> {
        let mut found = false;
        for slot in &mut self.slots {
            if let Some(config) = &mut slot.config
                && config.member_id == member_id
            {
                config.member_solo_eligible = eligible;
                found = true;
            }
        }
        if !found {
            return Err(AudioError::MemberNotFound);
        }
        self.refresh_targets();
        self.publish_control_update();
        Ok(())
    }

    /// Renders all readers into one interleaved stereo endpoint buffer.
    ///
    /// This realtime entry point only touches constructor-prepared storage,
    /// source readers, and atomic counters.
    pub fn render(
        &mut self,
        interleaved_stereo: &mut [f32],
        frames: usize,
    ) -> Result<RenderReport, AudioError> {
        if frames == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "render_frames",
            });
        }
        if frames > self.max_render_frames {
            return Err(AudioError::RenderCapacityExceeded);
        }
        let required = frames.checked_mul(2).ok_or(AudioError::CapacityOverflow {
            field: "render_samples",
        })?;
        if interleaved_stereo.len() != required {
            return Err(AudioError::RenderStorageLength {
                required,
                actual: interleaved_stereo.len(),
            });
        }
        interleaved_stereo.fill(0.0);

        let mut report = RenderReport {
            frames,
            ..RenderReport::default()
        };
        for slot in &mut self.slots {
            let Some(config) = slot.config else {
                continue;
            };
            let Some(reader) = slot.reader.as_mut() else {
                self.metrics.add_callback_contract_violation();
                continue;
            };
            report.active_sources += 1;

            let channels = usize::from(config.channels);
            let scratch_samples = frames * channels;
            let scratch = &mut slot.scratch[..scratch_samples];
            scratch.fill(0.0);
            let outcome = reader.read(scratch, frames, config.channels);
            let exact_frames = if outcome.frames_written > frames {
                self.metrics.add_callback_contract_violation();
                frames
            } else {
                outcome.frames_written
            };
            let missing_frames = frames - exact_frames;
            slot.metrics.render_frames = slot.metrics.render_frames.saturating_add(frames as u64);
            slot.metrics.exact_frames = slot
                .metrics
                .exact_frames
                .saturating_add(exact_frames as u64);
            if missing_frames > 0 {
                report.underrun_sources += 1;
                report.underrun_frames += missing_frames;
                slot.metrics.underrun_events = slot.metrics.underrun_events.saturating_add(1);
                slot.metrics.underrun_frames = slot
                    .metrics
                    .underrun_frames
                    .saturating_add(missing_frames as u64);
                self.metrics.add_underrun(missing_frames);
            }

            for frame in 0..frames {
                let coefficients = slot.ramp.advance();
                if config.channels == 1 {
                    let sample =
                        sanitize_source_sample(scratch[frame], &mut slot.metrics, &mut report);
                    interleaved_stereo[frame * 2] += sample * coefficients[0];
                    interleaved_stereo[frame * 2 + 1] += sample * coefficients[1];
                } else {
                    let left =
                        sanitize_source_sample(scratch[frame], &mut slot.metrics, &mut report);
                    let right = sanitize_source_sample(
                        scratch[frames + frame],
                        &mut slot.metrics,
                        &mut report,
                    );
                    interleaved_stereo[frame * 2] += left * coefficients[0];
                    interleaved_stereo[frame * 2 + 1] += right * coefficients[1];
                }
            }
        }

        for sample in interleaved_stereo {
            let finite = if sample.is_finite() {
                *sample
            } else {
                report.non_finite_samples += 1;
                0.0
            };
            let magnitude = finite.abs();
            report.max_pre_limiter_abs = report.max_pre_limiter_abs.max(magnitude);
            if magnitude > self.limiter.knee() {
                report.limited_samples += 1;
            }
            *sample = self.limiter.process(finite);
        }

        self.metrics.add_render_callback(frames);
        self.metrics.add_mix_non_finite(report.non_finite_samples);
        self.metrics.add_limited_samples(report.limited_samples);
        self.metrics
            .observe_pre_limiter_abs(report.max_pre_limiter_abs);
        Ok(report)
    }

    /// Returns one source's cumulative counters without allocating.
    #[must_use]
    pub fn source_metrics(&self, source_id: SourceId) -> Option<SourceRenderMetricsSnapshot> {
        self.slots.iter().find_map(|slot| {
            let config = slot.config?;
            if config.source_id != source_id {
                return None;
            }
            Some(SourceRenderMetricsSnapshot {
                member_id: config.member_id,
                source_id: config.source_id,
                render_frames_total: slot.metrics.render_frames,
                exact_frames_total: slot.metrics.exact_frames,
                underrun_frames_total: slot.metrics.underrun_frames,
                underrun_events_total: slot.metrics.underrun_events,
                non_finite_samples_total: slot.metrics.non_finite_samples,
            })
        })
    }

    /// Returns the number of installed readers.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.config.is_some())
            .count()
    }

    /// Returns the constructor-fixed source slot count.
    #[must_use]
    pub fn source_capacity(&self) -> usize {
        self.slots.len()
    }

    /// Returns the constructor-fixed maximum callback frame count.
    #[must_use]
    pub const fn max_render_frames(&self) -> usize {
        self.max_render_frames
    }

    /// Returns the shared lock-free metrics bank.
    #[must_use]
    pub fn metrics(&self) -> Arc<AudioMetrics> {
        Arc::clone(&self.metrics)
    }

    fn config_mut(&mut self, source_id: SourceId) -> Option<&mut SourceRenderConfig> {
        self.slots.iter_mut().find_map(|slot| {
            let config = slot.config.as_mut()?;
            (config.source_id == source_id).then_some(config)
        })
    }

    fn refresh_targets(&mut self) {
        let active_solo = self.slots.iter().any(|slot| {
            slot.config
                .is_some_and(|config| config.member_solo_eligible && config.member_mix.solo)
        });
        for slot in &mut self.slots {
            let Some(config) = slot.config else {
                continue;
            };
            slot.ramp.retarget(target_coefficients(config, active_solo));
        }
    }

    fn publish_control_update(&self) {
        self.metrics.add_control_update();
        self.metrics.set_active_render_sources(self.source_count());
    }

    #[cfg(test)]
    fn slot_scratch_address(&self, index: usize) -> usize {
        self.slots[index].scratch.as_ptr() as usize
    }

    #[cfg(test)]
    fn slot_storage_capacity(&self) -> usize {
        self.slots.capacity()
    }
}

fn target_coefficients(config: SourceRenderConfig, active_solo: bool) -> [f32; 2] {
    let audible = config.routed
        && !config.source_mix.mute
        && !config.member_mix.mute
        && (!active_solo || config.member_mix.solo);
    if !audible {
        return [0.0; 2];
    }

    let gain = config.source_mix.gain_db.linear() * config.member_mix.gain_db.linear();
    let pan = config.source_mix.pan.get();
    if config.channels == 1 {
        let theta = (pan + 1.0) * FRAC_PI_4;
        [gain * theta.cos(), gain * theta.sin()]
    } else {
        let left_balance = (pan.max(0.0) * FRAC_PI_2).cos();
        let right_balance = ((-pan).max(0.0) * FRAC_PI_2).cos();
        [gain * left_balance, gain * right_balance]
    }
}

fn sanitize_source_sample(
    sample: f32,
    metrics: &mut SourceRenderMetrics,
    report: &mut RenderReport,
) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        metrics.non_finite_samples = metrics.non_finite_samples.saturating_add(1);
        report.non_finite_samples += 1;
        0.0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rlb_model::{GainDb, MemberId, Pan, SourceId};

    use super::*;

    struct ConstantReader {
        channels: u8,
        values: [f32; 2],
        remaining: Option<usize>,
    }

    impl ConstantReader {
        fn infinite(channels: u8, values: [f32; 2]) -> Self {
            Self {
                channels,
                values,
                remaining: None,
            }
        }

        fn finite(channels: u8, values: [f32; 2], frames: usize) -> Self {
            Self {
                channels,
                values,
                remaining: Some(frames),
            }
        }
    }

    impl SourceReader for ConstantReader {
        fn read(&mut self, output: &mut [f32], frames: usize, channels: u8) -> SourceReadOutcome {
            assert_eq!(channels, self.channels);
            let written = self
                .remaining
                .map_or(frames, |remaining| remaining.min(frames));
            for channel in 0..usize::from(channels) {
                output[channel * frames..channel * frames + written].fill(self.values[channel]);
            }
            if let Some(remaining) = &mut self.remaining {
                *remaining -= written;
            }
            SourceReadOutcome {
                frames_written: written,
            }
        }
    }

    fn member(byte: u8) -> MemberId {
        MemberId::from_bytes([byte; 16]).expect("nonzero member")
    }

    fn source(byte: u8) -> SourceId {
        SourceId::from_bytes([byte; 16]).expect("nonzero source")
    }

    fn config(member_id: MemberId, source_id: SourceId, channels: u8) -> SourceRenderConfig {
        SourceRenderConfig {
            member_id,
            source_id,
            channels,
            routed: true,
            member_solo_eligible: true,
            source_mix: SourceMix::default(),
            member_mix: MemberMix::default(),
        }
    }

    #[test]
    fn mono_pan_and_gain_use_model_values() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer =
            SharedRenderMixer::new(1, RAMP_FRAMES, SoftLimiter::default(), metrics).expect("mixer");
        let mut source_config = config(member(1), source(2), 1);
        source_config.source_mix.gain_db = GainDb::new(-6.0).expect("gain");
        source_config.source_mix.pan = Pan::CENTER;
        mixer
            .add_source(
                source_config,
                Box::new(ConstantReader::infinite(1, [0.5, 0.0])),
            )
            .expect("source");

        let mut output = vec![0.0; RAMP_FRAMES * 2];
        mixer.render(&mut output, RAMP_FRAMES).expect("warm ramp");
        let gain = GainDb::new(-6.0).expect("gain").linear();
        let expected = 0.5 * gain * core::f32::consts::FRAC_1_SQRT_2;
        assert!((output[(RAMP_FRAMES - 1) * 2] - expected).abs() < 1.0e-5);
        assert!((output[(RAMP_FRAMES - 1) * 2 + 1] - expected).abs() < 1.0e-5);
    }

    #[test]
    fn mute_reaches_zero_on_the_96th_frame_without_jump() {
        assert_eq!(RAMP_FRAMES, 96);
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer =
            SharedRenderMixer::new(1, RAMP_FRAMES, SoftLimiter::default(), metrics).expect("mixer");
        let source_id = source(2);
        mixer
            .add_source(
                config(member(1), source_id, 2),
                Box::new(ConstantReader::infinite(2, [0.5, 0.5])),
            )
            .expect("source");
        let mut output = vec![0.0; RAMP_FRAMES * 2];
        mixer.render(&mut output, RAMP_FRAMES).expect("warm ramp");

        mixer
            .set_source_mix(
                source_id,
                SourceMix {
                    mute: true,
                    ..SourceMix::default()
                },
            )
            .expect("mute");
        mixer.render(&mut output, RAMP_FRAMES).expect("mute ramp");
        assert!(output[0] < 0.5);
        assert!(output[0] > 0.0);
        assert!(output[(RAMP_FRAMES - 2) * 2] > 0.0);
        assert_eq!(output[(RAMP_FRAMES - 1) * 2], 0.0);
        assert_eq!(output[(RAMP_FRAMES - 1) * 2 + 1], 0.0);
    }

    #[test]
    fn one_source_underrun_leaves_other_source_running() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer = SharedRenderMixer::new(
            2,
            RAMP_FRAMES,
            SoftLimiter::new(0.99).expect("limiter"),
            Arc::clone(&metrics),
        )
        .expect("mixer");
        mixer
            .add_source(
                config(member(1), source(2), 2),
                Box::new(ConstantReader::finite(2, [0.1, 0.1], RAMP_FRAMES + 1)),
            )
            .expect("short source");
        mixer
            .add_source(
                config(member(3), source(4), 2),
                Box::new(ConstantReader::infinite(2, [0.2, 0.2])),
            )
            .expect("healthy source");
        let mut output = vec![0.0; RAMP_FRAMES * 2];
        mixer.render(&mut output, RAMP_FRAMES).expect("warm ramp");

        let mut second = vec![0.0; 8];
        let report = mixer.render(&mut second, 4).expect("render");
        assert_eq!(report.underrun_sources, 1);
        assert_eq!(report.underrun_frames, 3);
        assert!(second[6] > 0.19);
        assert!(second[7] > 0.19);
        assert_eq!(metrics.snapshot().source_underrun_frames_total, 3);
        let healthy = mixer.source_metrics(source(4)).expect("metrics");
        assert_eq!(healthy.underrun_frames_total, 0);
    }

    #[test]
    fn offline_solo_does_not_gate_an_online_member() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer =
            SharedRenderMixer::new(2, RAMP_FRAMES, SoftLimiter::default(), metrics).expect("mixer");
        let mut offline_solo = config(member(1), source(2), 2);
        offline_solo.member_solo_eligible = false;
        offline_solo.member_mix.solo = true;
        mixer
            .add_source(
                offline_solo,
                Box::new(ConstantReader::infinite(2, [0.1, 0.1])),
            )
            .expect("offline source");
        mixer
            .add_source(
                config(member(3), source(4), 2),
                Box::new(ConstantReader::infinite(2, [0.2, 0.2])),
            )
            .expect("online source");
        let mut output = vec![0.0; RAMP_FRAMES * 2];
        mixer.render(&mut output, RAMP_FRAMES).expect("render");
        assert!(output[(RAMP_FRAMES - 1) * 2] > 0.29);

        mixer
            .set_member_solo_eligible(member(1), true)
            .expect("online");
        mixer.render(&mut output, RAMP_FRAMES).expect("solo ramp");
        assert!((output[(RAMP_FRAMES - 1) * 2] - 0.1).abs() < 1.0e-5);
    }

    #[test]
    fn non_finite_samples_are_zeroed_and_soft_limit_is_bounded() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer =
            SharedRenderMixer::new(2, RAMP_FRAMES, SoftLimiter::default(), Arc::clone(&metrics))
                .expect("mixer");
        mixer
            .add_source(
                config(member(1), source(2), 2),
                Box::new(ConstantReader::infinite(2, [f32::NAN, f32::INFINITY])),
            )
            .expect("non-finite source");
        mixer
            .add_source(
                config(member(3), source(4), 2),
                Box::new(ConstantReader::infinite(2, [10.0, -10.0])),
            )
            .expect("hot source");
        let mut output = vec![0.0; RAMP_FRAMES * 2];
        let report = mixer.render(&mut output, RAMP_FRAMES).expect("render");
        assert_eq!(report.non_finite_samples, (RAMP_FRAMES * 2) as u64);
        assert!(report.limited_samples > 0);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| (-1.0..=1.0).contains(sample)));
        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.mix_non_finite_samples_total,
            report.non_finite_samples
        );
        assert!(snapshot.max_pre_limiter_abs > 1.0);
    }

    #[test]
    fn render_keeps_all_preallocated_capacities_and_addresses() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer =
            SharedRenderMixer::new(2, 256, SoftLimiter::default(), metrics).expect("mixer");
        mixer
            .add_source(
                config(member(1), source(2), 1),
                Box::new(ConstantReader::infinite(1, [0.1, 0.0])),
            )
            .expect("source");
        let slot_capacity = mixer.slot_storage_capacity();
        let first_address = mixer.slot_scratch_address(0);
        let second_address = mixer.slot_scratch_address(1);
        let mut output = Vec::with_capacity(512);
        output.resize(512, 0.0);
        let output_capacity = output.capacity();

        for _ in 0..32 {
            mixer.render(&mut output, 256).expect("render");
        }

        assert_eq!(output.capacity(), output_capacity);
        assert_eq!(mixer.slot_storage_capacity(), slot_capacity);
        assert_eq!(mixer.slot_scratch_address(0), first_address);
        assert_eq!(mixer.slot_scratch_address(1), second_address);
    }
}
