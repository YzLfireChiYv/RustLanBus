use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Lock-free counters shared by capture, render, and a low-rate reporter.
#[derive(Debug, Default)]
pub struct AudioMetrics {
    capture_blocks: AtomicU64,
    capture_frames: AtomicU64,
    capture_non_finite_samples: AtomicU64,
    capture_pool_exhaustions: AtomicU64,
    capture_queue_overflows: AtomicU64,
    reblock_units: AtomicU64,
    reblock_dropped_frames: AtomicU64,
    render_callbacks: AtomicU64,
    render_frames: AtomicU64,
    source_underrun_events: AtomicU64,
    source_underrun_frames: AtomicU64,
    mix_non_finite_samples: AtomicU64,
    limited_samples: AtomicU64,
    max_pre_limiter_abs_bits: AtomicU32,
    active_render_sources: AtomicUsize,
    control_updates: AtomicU64,
    callback_contract_violations: AtomicU64,
}

impl AudioMetrics {
    /// Creates a zeroed metrics bank.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capture_blocks: AtomicU64::new(0),
            capture_frames: AtomicU64::new(0),
            capture_non_finite_samples: AtomicU64::new(0),
            capture_pool_exhaustions: AtomicU64::new(0),
            capture_queue_overflows: AtomicU64::new(0),
            reblock_units: AtomicU64::new(0),
            reblock_dropped_frames: AtomicU64::new(0),
            render_callbacks: AtomicU64::new(0),
            render_frames: AtomicU64::new(0),
            source_underrun_events: AtomicU64::new(0),
            source_underrun_frames: AtomicU64::new(0),
            mix_non_finite_samples: AtomicU64::new(0),
            limited_samples: AtomicU64::new(0),
            max_pre_limiter_abs_bits: AtomicU32::new(0),
            active_render_sources: AtomicUsize::new(0),
            control_updates: AtomicU64::new(0),
            callback_contract_violations: AtomicU64::new(0),
        }
    }

    /// Increments shared-render callback counters from a platform adapter.
    pub fn record_render_callback(&self, frames: usize) {
        self.add_render_callback(frames);
    }

    /// Reads a coherent-enough cumulative snapshot using relaxed atomics.
    ///
    /// Individual counters can advance while this method runs. A reporter adds
    /// its own snapshot sequence and interval timestamps.
    #[must_use]
    pub fn snapshot(&self) -> AudioMetricsSnapshot {
        AudioMetricsSnapshot {
            capture_blocks_total: self.capture_blocks.load(Ordering::Relaxed),
            capture_frames_total: self.capture_frames.load(Ordering::Relaxed),
            capture_non_finite_samples_total: self
                .capture_non_finite_samples
                .load(Ordering::Relaxed),
            capture_pool_exhaustions_total: self.capture_pool_exhaustions.load(Ordering::Relaxed),
            capture_queue_overflows_total: self.capture_queue_overflows.load(Ordering::Relaxed),
            reblock_units_total: self.reblock_units.load(Ordering::Relaxed),
            reblock_dropped_frames_total: self.reblock_dropped_frames.load(Ordering::Relaxed),
            render_callback_count_total: self.render_callbacks.load(Ordering::Relaxed),
            render_frames_total: self.render_frames.load(Ordering::Relaxed),
            source_underrun_events_total: self.source_underrun_events.load(Ordering::Relaxed),
            source_underrun_frames_total: self.source_underrun_frames.load(Ordering::Relaxed),
            mix_non_finite_samples_total: self.mix_non_finite_samples.load(Ordering::Relaxed),
            limited_samples_total: self.limited_samples.load(Ordering::Relaxed),
            max_pre_limiter_abs: f32::from_bits(
                self.max_pre_limiter_abs_bits.load(Ordering::Relaxed),
            ),
            active_render_sources: self.active_render_sources.load(Ordering::Relaxed),
            control_updates_total: self.control_updates.load(Ordering::Relaxed),
            callback_contract_violations_total: self
                .callback_contract_violations
                .load(Ordering::Relaxed),
        }
    }

    pub(crate) fn add_capture_block(&self, frames: usize) {
        self.capture_blocks.fetch_add(1, Ordering::Relaxed);
        self.capture_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_capture_non_finite(&self, samples: u64) {
        self.capture_non_finite_samples
            .fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn add_pool_exhaustion(&self) {
        self.capture_pool_exhaustions
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_queue_overflow(&self) {
        self.capture_queue_overflows.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_reblock_unit(&self) {
        self.reblock_units.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_reblock_dropped_frames(&self, frames: usize) {
        self.reblock_dropped_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_render_callback(&self, frames: usize) {
        self.render_callbacks.fetch_add(1, Ordering::Relaxed);
        self.render_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_underrun(&self, frames: usize) {
        self.source_underrun_events.fetch_add(1, Ordering::Relaxed);
        self.source_underrun_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_mix_non_finite(&self, samples: u64) {
        self.mix_non_finite_samples
            .fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn add_limited_samples(&self, samples: u64) {
        self.limited_samples.fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn observe_pre_limiter_abs(&self, value: f32) {
        debug_assert!(value.is_finite() && value >= 0.0);
        let mut current = self.max_pre_limiter_abs_bits.load(Ordering::Relaxed);
        loop {
            if f32::from_bits(current) >= value {
                return;
            }
            match self.max_pre_limiter_abs_bits.compare_exchange_weak(
                current,
                value.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn set_active_render_sources(&self, sources: usize) {
        self.active_render_sources.store(sources, Ordering::Relaxed);
    }

    pub(crate) fn add_control_update(&self) {
        self.control_updates.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_callback_contract_violation(&self) {
        self.callback_contract_violations
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Cumulative audio counters suitable for a low-rate diagnostics snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioMetricsSnapshot {
    /// Capture callback blocks accepted by a reblocker.
    pub capture_blocks_total: u64,
    /// Source frames observed before queueing.
    pub capture_frames_total: u64,
    /// Capture samples replaced with positive zero.
    pub capture_non_finite_samples_total: u64,
    /// Completed units dropped because no pool block was free.
    pub capture_pool_exhaustions_total: u64,
    /// Completed units dropped because the bounded output queue was full.
    pub capture_queue_overflows_total: u64,
    /// Complete 480-frame units published.
    pub reblock_units_total: u64,
    /// Captured frames discarded at a discontinuity or bounded-resource limit.
    pub reblock_dropped_frames_total: u64,
    /// Shared render callback count.
    pub render_callback_count_total: u64,
    /// Stereo output frames rendered.
    pub render_frames_total: u64,
    /// Per-source read calls that returned a short result.
    pub source_underrun_events_total: u64,
    /// Missing source frames replaced by silence.
    pub source_underrun_frames_total: u64,
    /// Non-finite source or mixed samples replaced by positive zero.
    pub mix_non_finite_samples_total: u64,
    /// Output samples processed by the limiter above its knee.
    pub limited_samples_total: u64,
    /// Largest finite absolute sum observed before limiting.
    pub max_pre_limiter_abs: f32,
    /// Number of currently registered source readers.
    pub active_render_sources: usize,
    /// Prepared graph/control target changes.
    pub control_updates_total: u64,
    /// Reader or backend results outside their declared callback contract.
    pub callback_contract_violations_total: u64,
}
