//! One reserve controller per original.
//!
//! Reserve is source extent minus the next kernel input, in seconds.
//! Positive excess lowers the output/input ratio. The correction is clamped
//! to the configured parts-per-million bound. Large backlog is discarded by
//! the core; this controller does not accelerate catch-up past that bound.

use super::contract::TimingPolicy;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RatioSample {
    pub nominal: f64,
    pub reserve_frames: i64,
    pub target_frames: u64,
    pub sample_rate: u32,
    pub dt_seconds: f64,
    pub allow: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ReserveController {
    filtered_seconds: f64,
    integral: f64,
    held_offset: f64,
}

impl ReserveController {
    pub(crate) fn new() -> Self {
        Self {
            filtered_seconds: 0.0,
            integral: 0.0,
            held_offset: 0.0,
        }
    }

    /// Returns the output/input ratio. Adaptation stays frozen when `sample.allow` is false.
    pub(crate) fn ratio(&mut self, policy: &TimingPolicy, sample: RatioSample) -> f64 {
        let RatioSample {
            nominal,
            reserve_frames,
            target_frames,
            sample_rate,
            dt_seconds,
            allow,
        } = sample;
        if allow && sample_rate > 0 && dt_seconds.is_finite() && dt_seconds > 0.0 && nominal > 0.0 {
            let reserve_seconds = reserve_frames as f64 / f64::from(sample_rate);
            let target_seconds = target_frames as f64 / f64::from(sample_rate);
            let error = reserve_seconds - target_seconds;
            let alpha = dt_seconds / (policy.filter_seconds + dt_seconds);
            self.filtered_seconds += alpha * (error - self.filtered_seconds);
            self.integral += self.filtered_seconds * dt_seconds;
            let raw = -(policy.kp_per_second * self.filtered_seconds
                + policy.ki_per_second_squared * self.integral);
            let limit = policy.correction_ppm / 1_000_000.0;
            let clamped = raw.clamp(-limit, limit);
            if clamped != raw {
                self.integral -= self.filtered_seconds * dt_seconds;
            }
            self.held_offset = clamped;
        }
        let min = nominal / policy.max_ratio_relative;
        let max = nominal * policy.max_ratio_relative;
        (nominal * (1.0 + self.held_offset)).clamp(min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excess_reserve_lowers_the_ratio_and_shortage_raises_it_within_the_bound() {
        let policy = TimingPolicy::provisional();
        let mut excess = ReserveController::new();
        let mut shortage = ReserveController::new();
        let mut excess_ratio = 1.0;
        let mut shortage_ratio = 1.0;
        for _ in 0..40 {
            excess_ratio = excess.ratio(
                &policy,
                RatioSample {
                    nominal: 1.0,
                    reserve_frames: 4_000,
                    target_frames: 1_000,
                    sample_rate: 48_000,
                    dt_seconds: 0.02,
                    allow: true,
                },
            );
            shortage_ratio = shortage.ratio(
                &policy,
                RatioSample {
                    nominal: 1.0,
                    reserve_frames: 0,
                    target_frames: 1_000,
                    sample_rate: 48_000,
                    dt_seconds: 0.02,
                    allow: true,
                },
            );
        }
        assert!(excess_ratio < 1.0, "{excess_ratio}");
        assert!(shortage_ratio > 1.0, "{shortage_ratio}");
        assert!(excess_ratio >= 1.0 / policy.max_ratio_relative - 1.0e-9);
        assert!(shortage_ratio <= policy.max_ratio_relative + 1.0e-9);
        let held = excess.ratio(
            &policy,
            RatioSample {
                nominal: 1.0,
                reserve_frames: 0,
                target_frames: 0,
                sample_rate: 48_000,
                dt_seconds: 0.02,
                allow: false,
            },
        );
        assert_eq!(held, excess_ratio);
    }
}
