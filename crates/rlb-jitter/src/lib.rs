#![forbid(unsafe_code)]

//! Bounded, sample-indexed receive timeline for one remote source.
//!
//! Construction allocates every timeline, sample, and record slot. Ingress and
//! render operations only mutate this fixed storage. A separate instance is
//! required for every remote source.

use core::fmt;
use core::num::NonZeroU64;

use rlb_clock::MonotonicInstant;
use rlb_model::{
    AUTOMATIC_MAX_FRAMES, AUTOMATIC_MIN_FRAMES, AUTOMATIC_TARGET_FRAMES, MEDIA_UNIT_FRAMES,
    RECEIVE_CAPACITY_FRAMES, SAMPLE_RATE_HZ,
};

/// Physical f32 storage retained for one source.
pub const SAMPLE_RING_FRAMES: usize = 16_384;
/// Logical packet and reorder window, independent of the playout target.
pub const LOGICAL_CAPACITY_FRAMES: u64 = RECEIVE_CAPACITY_FRAMES as u64;
/// Fade and merge duration required by the first-release concealer.
pub const CONCEAL_RAMP_FRAMES: u32 = 96;
/// Consecutive concealment which starts re-prime.
pub const REPRIME_AFTER_FRAMES: u32 = 1_440;
const MAX_RECORD_SLOTS: usize = RECEIVE_CAPACITY_FRAMES as usize;
const AUTOMATIC_STEP_FRAMES: u16 = MEDIA_UNIT_FRAMES;

/// Local identity of a receiver playout generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct PlayoutGeneration(u64);

impl PlayoutGeneration {
    /// Returns the diagnostic generation counter.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Fixed receive target policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetMode {
    /// Bounded 20–50 ms automatic model.
    Automatic,
    /// One fixed target from the V2 manual set.
    Manual(u16),
}

impl TargetMode {
    /// Validates a manual target.
    pub const fn manual(frames: u16) -> Result<Self, JitterError> {
        match frames {
            480 | 720 | 960 | 1_440 | 2_400 => Ok(Self::Manual(frames)),
            _ => Err(JitterError::InvalidTarget { frames }),
        }
    }

    const fn initial_frames(self) -> u16 {
        match self {
            Self::Automatic => AUTOMATIC_TARGET_FRAMES,
            Self::Manual(frames) => frames,
        }
    }
}

/// Sender-declared GAP classification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum GapKind {
    /// Known capture adapter overrun.
    CaptureOverrun = 1,
    /// A numbered unit was removed from the bounded send queue.
    SendQueueOverflow = 2,
    /// The sender missed its ten-millisecond submission deadline.
    SendDeadlineDrop = 3,
    /// Another adapter discontinuity with a known frame count.
    KnownAdapterGap = 4,
}

impl GapKind {
    const fn index(self) -> usize {
        self as usize - 1
    }
}

/// Receiver-side playout state for one source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayoutState {
    /// The source has no effective local subscription.
    Disabled,
    /// A generation exists and is waiting for its first complete MEDIA unit.
    AwaitingMedia,
    /// The first complete MEDIA unit established the startup anchor.
    Preroll,
    /// The source cursor advances with the shared render callback.
    Playing,
    /// The cursor advances silently while looking for a future complete unit.
    ReprimeSeek,
    /// The cursor is frozen at a future complete unit while target fill builds.
    ReprimeFill,
    /// An announced final sample was reached.
    Ended,
    /// A sequence, descriptor, or source-range invariant failed.
    Faulted,
}

/// Successful disposition of one complete record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertOutcome {
    /// The record was committed to the bounded timeline.
    Inserted,
    /// The record was committed after a newer sample range had been observed.
    Reordered,
    /// An identical record descriptor was already present.
    Duplicate,
    /// The record belonged to an older local playout generation.
    StaleGeneration,
    /// The record was below this generation's preroll anchor.
    Stale,
    /// The record was wholly or partly behind the read cursor.
    Late,
    /// The record started beyond the 250 ms logical window.
    CapacityDropped,
}

/// Closed failure set for the sample timeline and render API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JitterError {
    /// Only mono and stereo are supported.
    UnsupportedChannels {
        /// Rejected channel count.
        channels: u8,
    },
    /// A manual target was outside the fixed V2 set.
    InvalidTarget {
        /// Rejected target in frames.
        frames: u16,
    },
    /// MEDIA records must contain exactly 480 frames.
    InvalidMediaFrames {
        /// Supplied interleaved sample count.
        samples: usize,
        /// Configured channel count.
        channels: u8,
    },
    /// A caller-provided output did not match `frames * channels`.
    InvalidOutputLength {
        /// Exact required sample count.
        expected: usize,
        /// Supplied sample count.
        actual: usize,
    },
    /// A source sample range overflowed `u64`.
    SampleRangeOverflow,
    /// A GAP frame count was zero or exceeded 48,000.
    InvalidGapFrames {
        /// Rejected frame count.
        frames: u32,
    },
    /// One sequence mapped to a different record or sample range.
    RecordSequenceConflict,
    /// Two records claimed overlapping source time.
    TimelineConflict,
    /// The fixed record metadata table was exhausted.
    RecordCapacityExceeded,
    /// An EPOCH_END contradicted known source time.
    InvalidEpochEnd,
    /// A faulted generation only accepts disable or replacement.
    Faulted,
}

impl fmt::Display for JitterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnsupportedChannels { channels } => {
                write!(formatter, "channels must be 1 or 2, got {channels}")
            }
            Self::InvalidTarget { frames } => {
                write!(formatter, "unsupported playout target {frames} frames")
            }
            Self::InvalidMediaFrames { samples, channels } => write!(
                formatter,
                "MEDIA requires 480 frames at {channels} channels, got {samples} samples"
            ),
            Self::InvalidOutputLength { expected, actual } => {
                write!(
                    formatter,
                    "render output requires {expected} samples, got {actual}"
                )
            }
            Self::SampleRangeOverflow => formatter.write_str("source sample range overflowed"),
            Self::InvalidGapFrames { frames } => {
                write!(
                    formatter,
                    "GAP frame count must be in 1..=48000, got {frames}"
                )
            }
            Self::RecordSequenceConflict => {
                formatter.write_str("record sequence mapped to conflicting source time")
            }
            Self::TimelineConflict => {
                formatter.write_str("source records claimed overlapping sample ranges")
            }
            Self::RecordCapacityExceeded => {
                formatter.write_str("bounded record metadata capacity was exhausted")
            }
            Self::InvalidEpochEnd => {
                formatter.write_str("EPOCH_END contradicted known source time")
            }
            Self::Faulted => formatter.write_str("playout generation is faulted"),
        }
    }
}

impl std::error::Error for JitterError {}

/// Why the automatic target last changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdaptationReason {
    /// Two network late-loss runs occurred in ten seconds.
    RepeatedLateLoss,
    /// One network concealment run reached twenty milliseconds.
    LongConcealment,
    /// One hundred twenty clean one-second snapshots elapsed.
    CleanWindow,
}

/// One-second evidence used by bounded automatic shrink.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AutomaticSnapshot {
    /// Frames which completed after deadline during this second.
    pub late_frames_delta: u64,
    /// Network frames concealed without recovery.
    pub unrecovered_frames_delta: u64,
    /// Re-prime transitions during this second.
    pub reprime_count_delta: u64,
    /// Shared render xruns during this second.
    pub render_xruns_delta: u64,
    /// Minimum exact contiguous fill observed during this second.
    pub minimum_ready_frames: u16,
    /// Current absolute hardware drift command.
    pub drift_command_ppm: i32,
    /// Current fill command.
    pub fill_command_ppm: i32,
}

impl AutomaticSnapshot {
    fn is_clean(self, target_frames: u16) -> bool {
        self.late_frames_delta == 0
            && self.unrecovered_frames_delta == 0
            && self.reprime_count_delta == 0
            && self.render_xruns_delta == 0
            && self.minimum_ready_frames >= target_frames.saturating_sub(AUTOMATIC_STEP_FRAMES)
            && self.drift_command_ppm.unsigned_abs() < 400
            && self.fill_command_ppm == 0
    }
}

/// Cumulative per-source metrics plus the current controller snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JitterMetrics {
    /// Current local generation.
    pub generation: u64,
    /// MEDIA units committed to the timeline.
    pub complete_units_total: u64,
    /// First complete MEDIA start selected for this local generation.
    pub preroll_anchor_sample: Option<u64>,
    /// NACK-repaired MEDIA units accepted before their playout deadline.
    pub recovered_before_deadline_units_total: u64,
    /// Frames in accepted NACK-repaired MEDIA units.
    pub recovered_before_deadline_frames_total: u64,
    /// MEDIA records which arrived after a newer sample start.
    pub reordered_records_total: u64,
    /// Identical complete record descriptors ignored.
    pub duplicate_records_total: u64,
    /// Records from older local generations.
    pub stale_generation_records_total: u64,
    /// Records below this generation's preroll anchor.
    pub stale_records_total: u64,
    /// Frames rejected after their local playout deadline.
    pub late_frames_total: u64,
    /// Records rejected by the independent 250 ms logical window.
    pub capacity_dropped_records_total: u64,
    /// Faulting sequence or overlap conflicts.
    pub timeline_conflicts_total: u64,
    /// Exact source frames rendered.
    pub exact_frames_total: u64,
    /// Source frames covered by Playing and ReprimeSeek.
    pub source_frames_advanced_total: u64,
    /// Output frames covered by Playing and ReprimeSeek.
    pub source_render_frames_total: u64,
    /// Sender-declared GAP frames rendered, indexed by [`GapKind`] minus one.
    pub sender_gap_frames_total: [u64; 4],
    /// Missing network frames rendered.
    pub unrecovered_frames_total: u64,
    /// All concealed source frames rendered.
    pub concealed_frames_total: u64,
    /// Number of concealment runs.
    pub concealment_events_total: u64,
    /// Longest consecutive concealment run.
    pub longest_concealment_frames: u64,
    /// Re-prime transitions.
    pub reprime_count_total: u64,
    /// Source frames crossed while seeking an anchor.
    pub reprime_seek_frames_total: u64,
    /// Output frames produced while the cursor was frozen.
    pub reprime_frozen_frames_total: u64,
    /// Re-prime duration in local clock ticks.
    pub reprime_duration_ticks_total: u64,
    /// Current target, always 960–2400 in Automatic mode.
    pub target_frames: u16,
    /// Exact contiguous fill at the most recent callback entry.
    pub ready_contiguous_frames: u16,
    /// All exact frames in the logical window at the most recent callback.
    pub ready_total_frames: u16,
    /// Requested startup instant in local clock ticks.
    pub requested_start_ticks: Option<u64>,
    /// Callback-rounded startup instant in local clock ticks.
    pub actual_start_ticks: Option<u64>,
    /// Startup callback rounding in local clock ticks.
    pub callback_rounding_ticks: Option<u64>,
    /// Last automatic target change.
    pub adaptation_reason: Option<AdaptationReason>,
    /// Last target-change instant in local clock ticks.
    pub adaptation_ticks: Option<u64>,
    /// Latest valid drift observation.
    pub drift_observed_ppm: i32,
    /// EWMA drift estimate.
    pub drift_estimate_ppm: i32,
    /// Hardware drift command, clamped to ±500 ppm.
    pub drift_command_ppm: i32,
    /// Fill command, clamped to ±2000 ppm.
    pub fill_command_ppm: i32,
    /// Slewed combined read command.
    pub read_command_ppm: i32,
    /// Completed one-frame consume advances.
    pub sample_slip_advance_frames_total: u64,
    /// Completed one-frame consume retards.
    pub sample_slip_retard_frames_total: u64,
    /// A ±500 ppm source remained out of range for five valid snapshots.
    pub clock_drift_out_of_range: bool,
    /// Accepted EPOCH_END records.
    pub epoch_end_received_total: u64,
}

/// Delta report from one allocation-free render call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderReport {
    /// State after the callback.
    pub state: PlayoutState,
    /// Exact source frames rendered by this call.
    pub exact_frames: u32,
    /// Concealed source frames rendered by this call.
    pub concealed_frames: u32,
    /// Source frames crossed by this call.
    pub source_frames_advanced: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellState {
    Empty,
    Exact,
    Gap(GapKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordKind {
    Empty,
    Media,
    Gap(GapKind),
}

#[derive(Clone, Copy, Debug)]
struct RecordSlot {
    kind: RecordKind,
    sequence: u32,
    start: u64,
    end: u64,
}

impl RecordSlot {
    const EMPTY: Self = Self {
        kind: RecordKind::Empty,
        sequence: 0,
        start: 0,
        end: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlipDirection {
    Advance,
    Retard,
}

#[derive(Clone, Copy, Debug)]
struct ActiveSlip {
    direction: SlipDirection,
    phase: u8,
}

/// Preallocated receive timeline for one remote source.
pub struct JitterBuffer {
    channels: u8,
    ticks_per_second: NonZeroU64,
    mode: TargetMode,
    target_frames: u16,
    state: PlayoutState,
    generation: PlayoutGeneration,
    cells: Box<[CellState]>,
    tags: Box<[u64]>,
    samples: Box<[f32]>,
    playout_deadline_tags: Box<[u64]>,
    playout_deadline_ticks: Box<[u64]>,
    records: Box<[RecordSlot]>,
    active_gap_cache: Option<RecordSlot>,
    next_gap_cache: Option<RecordSlot>,
    read_cursor: Option<u64>,
    max_observed_start: Option<u64>,
    maximum_record_end: u64,
    final_sample: Option<u64>,
    final_end: Option<(u32, u64)>,
    requested_start: Option<MonotonicInstant>,
    actual_start: Option<MonotonicInstant>,
    reprime_fill_until: Option<MonotonicInstant>,
    reprime_started: Option<MonotonicInstant>,
    in_concealment: bool,
    concealment_has_network_loss: bool,
    concealment_target_grown: bool,
    conceal_position: u32,
    conceal_run_frames: u32,
    conceal_base: [f32; 2],
    last_output: [f32; 2],
    merge_base: [f32; 2],
    merge_phase: Option<u32>,
    reprime_fade_base: [f32; 2],
    reprime_fade_position: u32,
    recent_late_loss: [Option<u64>; 2],
    automatic_cooldown_until: u64,
    clean_snapshots: u16,
    drift_initialized: bool,
    drift_estimate_ppm: i32,
    out_of_range_snapshots: u8,
    slip_accumulator: i64,
    active_slip: Option<ActiveSlip>,
    metrics: JitterMetrics,
}

impl JitterBuffer {
    /// Allocates all fixed storage for one source.
    pub fn new(
        channels: u8,
        mode: TargetMode,
        ticks_per_second: NonZeroU64,
    ) -> Result<Self, JitterError> {
        if !matches!(channels, 1 | 2) {
            return Err(JitterError::UnsupportedChannels { channels });
        }
        let target_frames = mode.initial_frames();
        if !matches!(mode, TargetMode::Automatic)
            && !matches!(target_frames, 480 | 720 | 960 | 1_440 | 2_400)
        {
            return Err(JitterError::InvalidTarget {
                frames: target_frames,
            });
        }
        let sample_count = SAMPLE_RING_FRAMES * usize::from(channels);
        Ok(Self {
            channels,
            ticks_per_second,
            mode,
            target_frames,
            state: PlayoutState::Disabled,
            generation: PlayoutGeneration(0),
            cells: vec![CellState::Empty; SAMPLE_RING_FRAMES].into_boxed_slice(),
            tags: vec![0; SAMPLE_RING_FRAMES].into_boxed_slice(),
            samples: vec![0.0; sample_count].into_boxed_slice(),
            playout_deadline_tags: vec![u64::MAX; SAMPLE_RING_FRAMES].into_boxed_slice(),
            playout_deadline_ticks: vec![0; SAMPLE_RING_FRAMES].into_boxed_slice(),
            records: vec![RecordSlot::EMPTY; MAX_RECORD_SLOTS].into_boxed_slice(),
            active_gap_cache: None,
            next_gap_cache: None,
            read_cursor: None,
            max_observed_start: None,
            maximum_record_end: 0,
            final_sample: None,
            final_end: None,
            requested_start: None,
            actual_start: None,
            reprime_fill_until: None,
            reprime_started: None,
            in_concealment: false,
            concealment_has_network_loss: false,
            concealment_target_grown: false,
            conceal_position: 0,
            conceal_run_frames: 0,
            conceal_base: [0.0; 2],
            last_output: [0.0; 2],
            merge_base: [0.0; 2],
            merge_phase: None,
            reprime_fade_base: [0.0; 2],
            reprime_fade_position: CONCEAL_RAMP_FRAMES,
            recent_late_loss: [None; 2],
            automatic_cooldown_until: 0,
            clean_snapshots: 0,
            drift_initialized: false,
            drift_estimate_ppm: 0,
            out_of_range_snapshots: 0,
            slip_accumulator: 0,
            active_slip: None,
            metrics: JitterMetrics {
                target_frames,
                ..JitterMetrics::default()
            },
        })
    }

    pub fn set_target_mode(&mut self, mode: TargetMode) {
        self.mode = mode;
        self.target_frames = mode.initial_frames();
        self.metrics.target_frames = self.target_frames;
    }

    /// Starts a distinct local generation after a matching subscription ACK.
    pub fn enable(&mut self) -> PlayoutGeneration {
        self.clear_generation();
        self.generation = PlayoutGeneration(self.generation.0.wrapping_add(1));
        self.state = PlayoutState::AwaitingMedia;
        self.metrics.generation = self.generation.0;
        self.generation
    }

    /// Disables this source without changing the sender's stream epoch.
    pub fn disable(&mut self) {
        self.clear_generation();
        self.state = PlayoutState::Disabled;
    }

    /// Returns the current state.
    #[must_use]
    pub const fn state(&self) -> PlayoutState {
        self.state
    }

    /// Returns the current local generation.
    #[must_use]
    pub const fn generation(&self) -> PlayoutGeneration {
        self.generation
    }

    /// Returns the next source sample to be covered.
    #[must_use]
    pub const fn read_cursor(&self) -> Option<u64> {
        self.read_cursor
    }

    /// Returns the current bounded playout target.
    #[must_use]
    pub const fn target_frames(&self) -> u16 {
        self.target_frames
    }

    /// Returns a cumulative metrics snapshot.
    #[must_use]
    pub fn metrics(&self) -> JitterMetrics {
        let mut metrics = self.metrics;
        metrics.target_frames = self.target_frames;
        metrics
    }

    /// Inserts one fully assembled, decoded 480-frame MEDIA unit.
    pub fn insert_media(
        &mut self,
        generation: PlayoutGeneration,
        sequence: u32,
        start_sample: u64,
        interleaved_samples: &[f32],
        completed_at: MonotonicInstant,
    ) -> Result<InsertOutcome, JitterError> {
        self.insert_media_with_recovery(
            generation,
            sequence,
            start_sample,
            interleaved_samples,
            completed_at,
            false,
        )
    }

    /// Inserts MEDIA and records whether its completion followed a local NACK.
    ///
    /// Recovery metrics increase only when this call commits the complete unit;
    /// late, stale, duplicate, and capacity-dropped units are excluded.
    pub fn insert_media_with_recovery(
        &mut self,
        generation: PlayoutGeneration,
        sequence: u32,
        start_sample: u64,
        interleaved_samples: &[f32],
        completed_at: MonotonicInstant,
        recovered_after_nack: bool,
    ) -> Result<InsertOutcome, JitterError> {
        if generation != self.generation || self.state == PlayoutState::Disabled {
            self.metrics.stale_generation_records_total += 1;
            return Ok(InsertOutcome::StaleGeneration);
        }
        if self.state == PlayoutState::Faulted {
            return Err(JitterError::Faulted);
        }
        let required = usize::from(MEDIA_UNIT_FRAMES) * usize::from(self.channels);
        if interleaved_samples.len() != required {
            return Err(JitterError::InvalidMediaFrames {
                samples: interleaved_samples.len(),
                channels: self.channels,
            });
        }
        let end = start_sample
            .checked_add(u64::from(MEDIA_UNIT_FRAMES))
            .ok_or(JitterError::SampleRangeOverflow)?;
        let admission = self.admit_range(start_sample, end, true)?;
        if admission != InsertOutcome::Inserted {
            return Ok(admission);
        }
        if self.completed_after_playout_deadline(start_sample, end, completed_at) {
            self.metrics.late_frames_total = self
                .metrics
                .late_frames_total
                .saturating_add(end.saturating_sub(start_sample));
            return Ok(InsertOutcome::Late);
        }
        let descriptor = self.insert_record(sequence, start_sample, end, RecordKind::Media)?;
        if descriptor == InsertOutcome::Duplicate {
            return Ok(descriptor);
        }

        for frame in 0..usize::from(MEDIA_UNIT_FRAMES) {
            let sample_index = start_sample + frame as u64;
            let ring_index = ring_index(sample_index);
            self.tags[ring_index] = sample_index;
            self.cells[ring_index] = CellState::Exact;
            let destination = ring_index * usize::from(self.channels);
            let source = frame * usize::from(self.channels);
            for channel in 0..usize::from(self.channels) {
                let sample = interleaved_samples[source + channel];
                self.samples[destination + channel] = if sample.is_finite() { sample } else { 0.0 };
            }
        }

        self.metrics.complete_units_total += 1;
        if recovered_after_nack {
            self.metrics.recovered_before_deadline_units_total += 1;
            self.metrics.recovered_before_deadline_frames_total = self
                .metrics
                .recovered_before_deadline_frames_total
                .saturating_add(u64::from(MEDIA_UNIT_FRAMES));
        }
        self.maximum_record_end = self.maximum_record_end.max(end);
        let reordered = self.note_arrival_order(start_sample);
        if self.state == PlayoutState::AwaitingMedia {
            self.read_cursor = Some(start_sample);
            self.metrics.preroll_anchor_sample = Some(start_sample);
            let requested = completed_at.checked_add(self.target_duration());
            self.requested_start = requested;
            self.metrics.requested_start_ticks = requested.map(MonotonicInstant::ticks);
            self.state = PlayoutState::Preroll;
        }
        if reordered {
            Ok(InsertOutcome::Reordered)
        } else {
            Ok(InsertOutcome::Inserted)
        }
    }

    /// Inserts one sender-declared GAP interval.
    pub fn insert_gap(
        &mut self,
        generation: PlayoutGeneration,
        sequence: u32,
        start_sample: u64,
        frame_count: u32,
        reason: GapKind,
    ) -> Result<InsertOutcome, JitterError> {
        if generation != self.generation || self.state == PlayoutState::Disabled {
            self.metrics.stale_generation_records_total += 1;
            return Ok(InsertOutcome::StaleGeneration);
        }
        if self.state == PlayoutState::Faulted {
            return Err(JitterError::Faulted);
        }
        if !(1..=48_000).contains(&frame_count) {
            return Err(JitterError::InvalidGapFrames {
                frames: frame_count,
            });
        }
        let end = start_sample
            .checked_add(u64::from(frame_count))
            .ok_or(JitterError::SampleRangeOverflow)?;
        let admission = self.admit_range(start_sample, end, false)?;
        if admission != InsertOutcome::Inserted {
            return Ok(admission);
        }
        let descriptor =
            self.insert_record(sequence, start_sample, end, RecordKind::Gap(reason))?;
        if descriptor == InsertOutcome::Duplicate {
            return Ok(descriptor);
        }
        self.next_gap_cache = None;
        self.maximum_record_end = self.maximum_record_end.max(end);
        if self.note_arrival_order(start_sample) {
            Ok(InsertOutcome::Reordered)
        } else {
            Ok(InsertOutcome::Inserted)
        }
    }

    /// Records an exact end for the active source epoch.
    pub fn insert_epoch_end(
        &mut self,
        generation: PlayoutGeneration,
        next_sequence: u32,
        final_sample: u64,
    ) -> Result<InsertOutcome, JitterError> {
        if generation != self.generation || self.state == PlayoutState::Disabled {
            self.metrics.stale_generation_records_total += 1;
            return Ok(InsertOutcome::StaleGeneration);
        }
        if final_sample < self.maximum_record_end {
            return self.fail(JitterError::InvalidEpochEnd);
        }
        if let Some(existing) = self.final_end {
            if existing != (next_sequence, final_sample) {
                return self.fail(JitterError::InvalidEpochEnd);
            }
            self.metrics.duplicate_records_total += 1;
            return Ok(InsertOutcome::Duplicate);
        }
        self.final_end = Some((next_sequence, final_sample));
        self.final_sample = Some(final_sample);
        self.metrics.epoch_end_received_total += 1;
        if self.read_cursor == Some(final_sample) {
            self.state = PlayoutState::Ended;
        }
        Ok(InsertOutcome::Inserted)
    }

    /// Fixes playout deadlines for source ranges covered by one callback.
    ///
    /// Callers with a split callback-entry/render integration invoke this at
    /// callback entry. [`Self::render`] also invokes it, and repeated calls for
    /// the same range retain the earliest deadline. This operation allocates
    /// no memory.
    pub fn note_render_callback_entry(&mut self, now: MonotonicInstant, frames: u32) {
        let effective_state = match self.state {
            PlayoutState::Preroll
                if self
                    .requested_start
                    .is_some_and(|requested| now >= requested) =>
            {
                PlayoutState::Playing
            }
            PlayoutState::ReprimeFill
                if self
                    .reprime_fill_until
                    .is_some_and(|fill_until| now >= fill_until) =>
            {
                PlayoutState::Playing
            }
            state => state,
        };
        if !matches!(
            effective_state,
            PlayoutState::Playing | PlayoutState::ReprimeSeek
        ) {
            return;
        }
        let Some(start) = self.read_cursor else {
            return;
        };
        let mut end = start.saturating_add(u64::from(frames));
        if let Some(final_sample) = self.final_sample {
            end = end.min(final_sample);
        }
        if effective_state == PlayoutState::ReprimeSeek
            && let Some(anchor) = self.first_complete_media_at_or_after(start)
        {
            end = end.min(anchor);
        }
        for sample_index in start..end {
            let index = ring_index(sample_index);
            if self.playout_deadline_tags[index] == sample_index {
                self.playout_deadline_ticks[index] =
                    self.playout_deadline_ticks[index].min(now.ticks());
            } else {
                self.playout_deadline_tags[index] = sample_index;
                self.playout_deadline_ticks[index] = now.ticks();
            }
        }
    }

    /// Renders one source contribution without allocation, blocking, or decoding.
    pub fn render(
        &mut self,
        now: MonotonicInstant,
        frames: u32,
        output: &mut [f32],
    ) -> Result<RenderReport, JitterError> {
        let expected = usize::try_from(frames)
            .ok()
            .and_then(|count| count.checked_mul(usize::from(self.channels)))
            .ok_or(JitterError::InvalidOutputLength {
                expected: usize::MAX,
                actual: output.len(),
            })?;
        if output.len() != expected {
            return Err(JitterError::InvalidOutputLength {
                expected,
                actual: output.len(),
            });
        }
        self.note_render_callback_entry(now, frames);
        output.fill(0.0);
        self.update_ready_metrics();

        if self.state == PlayoutState::Preroll
            && self
                .requested_start
                .is_some_and(|requested| now >= requested)
        {
            self.state = PlayoutState::Playing;
            self.actual_start = Some(now);
            self.metrics.actual_start_ticks = Some(now.ticks());
            self.metrics.callback_rounding_ticks = self
                .requested_start
                .and_then(|requested| now.checked_duration_since(requested))
                .map(|duration| duration.ticks());
        }
        if self.state == PlayoutState::ReprimeFill
            && self
                .reprime_fill_until
                .is_some_and(|fill_until| now >= fill_until)
        {
            self.state = PlayoutState::Playing;
            self.reprime_fill_until = None;
            self.merge_base = [0.0; 2];
            self.merge_phase = Some(0);
            self.reprime_fade_base = [0.0; 2];
            self.reprime_fade_position = CONCEAL_RAMP_FRAMES;
            if let Some(started) = self.reprime_started.take()
                && let Some(duration) = now.checked_duration_since(started)
            {
                self.metrics.reprime_duration_ticks_total = self
                    .metrics
                    .reprime_duration_ticks_total
                    .saturating_add(duration.ticks());
            }
        }

        let before_exact = self.metrics.exact_frames_total;
        let before_concealed = self.metrics.concealed_frames_total;
        let before_advanced = self.metrics.source_frames_advanced_total;

        match self.state {
            PlayoutState::Playing | PlayoutState::ReprimeSeek => {
                self.render_active(now, frames, output);
            }
            PlayoutState::ReprimeFill => {
                self.metrics.reprime_frozen_frames_total = self
                    .metrics
                    .reprime_frozen_frames_total
                    .saturating_add(u64::from(frames));
            }
            PlayoutState::Disabled
            | PlayoutState::AwaitingMedia
            | PlayoutState::Preroll
            | PlayoutState::Ended
            | PlayoutState::Faulted => {}
        }

        Ok(RenderReport {
            state: self.state,
            exact_frames: saturating_u32(self.metrics.exact_frames_total - before_exact),
            concealed_frames: saturating_u32(
                self.metrics.concealed_frames_total - before_concealed,
            ),
            source_frames_advanced: saturating_u32(
                self.metrics.source_frames_advanced_total - before_advanced,
            ),
        })
    }

    /// Applies one valid per-second drift observation.
    pub fn observe_drift(&mut self, delta_source_frames: u64, delta_render_frames: u64) {
        if delta_render_frames == 0 {
            return;
        }
        let difference = i128::from(delta_source_frames) - i128::from(delta_render_frames);
        let observed = difference
            .saturating_mul(1_000_000)
            .checked_div(i128::from(delta_render_frames))
            .unwrap_or(0)
            .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32;
        self.metrics.drift_observed_ppm = observed;
        if self.drift_initialized {
            self.drift_estimate_ppm += (observed - self.drift_estimate_ppm) / 8;
        } else {
            self.drift_estimate_ppm = observed;
            self.drift_initialized = true;
        }
        self.metrics.drift_estimate_ppm = self.drift_estimate_ppm;
        self.metrics.drift_command_ppm = self.drift_estimate_ppm.clamp(-500, 500);
        if observed.unsigned_abs() > 500 {
            self.out_of_range_snapshots = self.out_of_range_snapshots.saturating_add(1);
            if self.out_of_range_snapshots >= 5 {
                self.metrics.clock_drift_out_of_range = true;
            }
        } else {
            self.out_of_range_snapshots = 0;
        }
        self.update_read_command();
    }

    /// Applies one one-second snapshot to the automatic shrink model.
    pub fn observe_automatic_snapshot(
        &mut self,
        now: MonotonicInstant,
        snapshot: AutomaticSnapshot,
    ) {
        if self.mode != TargetMode::Automatic {
            return;
        }
        if snapshot.is_clean(self.target_frames) {
            self.clean_snapshots = self.clean_snapshots.saturating_add(1);
        } else {
            self.clean_snapshots = 0;
        }
        if self.clean_snapshots >= 120 && self.target_frames > AUTOMATIC_MIN_FRAMES {
            self.target_frames = self
                .target_frames
                .saturating_sub(AUTOMATIC_STEP_FRAMES)
                .max(AUTOMATIC_MIN_FRAMES);
            self.clean_snapshots = 0;
            self.metrics.adaptation_reason = Some(AdaptationReason::CleanWindow);
            self.metrics.adaptation_ticks = Some(now.ticks());
        }
        self.update_read_command();
    }

    fn clear_generation(&mut self) {
        self.cells.fill(CellState::Empty);
        self.records.fill(RecordSlot::EMPTY);
        self.playout_deadline_tags.fill(u64::MAX);
        self.playout_deadline_ticks.fill(0);
        self.active_gap_cache = None;
        self.next_gap_cache = None;
        self.samples.fill(0.0);
        self.read_cursor = None;
        self.max_observed_start = None;
        self.maximum_record_end = 0;
        self.final_sample = None;
        self.final_end = None;
        self.requested_start = None;
        self.actual_start = None;
        self.reprime_fill_until = None;
        self.reprime_started = None;
        self.in_concealment = false;
        self.concealment_has_network_loss = false;
        self.concealment_target_grown = false;
        self.conceal_position = 0;
        self.conceal_run_frames = 0;
        self.conceal_base = [0.0; 2];
        self.last_output = [0.0; 2];
        self.merge_base = [0.0; 2];
        self.merge_phase = None;
        self.reprime_fade_base = [0.0; 2];
        self.reprime_fade_position = CONCEAL_RAMP_FRAMES;
        self.recent_late_loss = [None; 2];
        self.automatic_cooldown_until = 0;
        self.clean_snapshots = 0;
        self.active_slip = None;
        self.slip_accumulator = 0;
        self.metrics.requested_start_ticks = None;
        self.metrics.actual_start_ticks = None;
        self.metrics.callback_rounding_ticks = None;
        self.metrics.preroll_anchor_sample = None;
    }

    fn admit_range(
        &mut self,
        start: u64,
        end: u64,
        media: bool,
    ) -> Result<InsertOutcome, JitterError> {
        if let Some(final_sample) = self.final_sample
            && end > final_sample
        {
            return self.fail(JitterError::TimelineConflict);
        }
        if let Some(cursor) = self.read_cursor {
            if start < cursor {
                if self.state == PlayoutState::Preroll {
                    self.metrics.stale_records_total += 1;
                    return Ok(InsertOutcome::Stale);
                }
                self.metrics.late_frames_total = self
                    .metrics
                    .late_frames_total
                    .saturating_add(end.saturating_sub(start));
                return Ok(InsertOutcome::Late);
            }
            let window_end = cursor
                .checked_add(LOGICAL_CAPACITY_FRAMES)
                .ok_or(JitterError::SampleRangeOverflow)?;
            let allowed_end = if media {
                window_end
                    .checked_add(u64::from(MEDIA_UNIT_FRAMES))
                    .ok_or(JitterError::SampleRangeOverflow)?
            } else {
                u64::MAX
            };
            if start >= window_end || end > allowed_end {
                self.metrics.capacity_dropped_records_total += 1;
                return Ok(InsertOutcome::CapacityDropped);
            }
        }
        Ok(InsertOutcome::Inserted)
    }

    fn insert_record(
        &mut self,
        sequence: u32,
        start: u64,
        end: u64,
        kind: RecordKind,
    ) -> Result<InsertOutcome, JitterError> {
        let mut empty = None;
        for (index, existing) in self.records.iter().enumerate() {
            if existing.kind == RecordKind::Empty {
                empty.get_or_insert(index);
                continue;
            }
            if existing.sequence == sequence {
                if existing.start == start && existing.end == end && existing.kind == kind {
                    self.metrics.duplicate_records_total += 1;
                    return Ok(InsertOutcome::Duplicate);
                }
                return self.fail(JitterError::RecordSequenceConflict);
            }
            if ranges_overlap(start, end, existing.start, existing.end) {
                return self.fail(JitterError::TimelineConflict);
            }
        }
        let index = empty.ok_or(JitterError::RecordCapacityExceeded)?;
        self.records[index] = RecordSlot {
            kind,
            sequence,
            start,
            end,
        };
        Ok(InsertOutcome::Inserted)
    }

    fn fail<T>(&mut self, error: JitterError) -> Result<T, JitterError> {
        self.state = PlayoutState::Faulted;
        self.metrics.timeline_conflicts_total += 1;
        Err(error)
    }

    fn note_arrival_order(&mut self, start: u64) -> bool {
        if self
            .max_observed_start
            .is_some_and(|maximum| start < maximum)
        {
            self.metrics.reordered_records_total += 1;
            true
        } else {
            self.max_observed_start = Some(
                self.max_observed_start
                    .map_or(start, |maximum| maximum.max(start)),
            );
            false
        }
    }

    fn completed_after_playout_deadline(
        &self,
        start: u64,
        end: u64,
        completed_at: MonotonicInstant,
    ) -> bool {
        (start..end).any(|sample_index| {
            let index = ring_index(sample_index);
            self.playout_deadline_tags[index] == sample_index
                && completed_at.ticks() > self.playout_deadline_ticks[index]
        })
    }

    fn render_active(&mut self, now: MonotonicInstant, frames: u32, output: &mut [f32]) {
        let channels = usize::from(self.channels);
        for output_frame in 0..usize::try_from(frames).unwrap_or(usize::MAX) {
            if self.state == PlayoutState::ReprimeFill || self.state == PlayoutState::Ended {
                break;
            }
            let Some(cursor) = self.read_cursor else {
                break;
            };
            if self.final_sample == Some(cursor) {
                self.finish_concealment(now);
                self.state = PlayoutState::Ended;
                break;
            }
            let destination = output_frame * channels;

            if self.state == PlayoutState::ReprimeSeek {
                if self
                    .first_complete_media_at_or_after(cursor)
                    .is_some_and(|anchor| anchor == cursor)
                {
                    self.state = PlayoutState::ReprimeFill;
                    self.reprime_fill_until = now.checked_add(self.target_duration());
                    self.metrics.reprime_frozen_frames_total = self
                        .metrics
                        .reprime_frozen_frames_total
                        .saturating_add(u64::from(frames) - output_frame as u64);
                    break;
                }
                self.render_seek_frame(cursor, &mut output[destination..destination + channels]);
                self.read_cursor = cursor.checked_add(1);
                self.metrics.reprime_seek_frames_total += 1;
                self.metrics.source_frames_advanced_total += 1;
                self.metrics.source_render_frames_total += 1;
                continue;
            }

            match self.playout_cell(cursor) {
                CellState::Exact => {
                    self.finish_concealment(now);
                    self.maybe_start_slip(cursor);
                    self.render_exact_frame(
                        cursor,
                        &mut output[destination..destination + channels],
                    );
                    self.metrics.exact_frames_total += 1;
                }
                CellState::Gap(reason) => {
                    self.active_slip = None;
                    self.render_concealed_frame(
                        now,
                        Some(reason),
                        &mut output[destination..destination + channels],
                    );
                }
                CellState::Empty => {
                    self.active_slip = None;
                    self.render_concealed_frame(
                        now,
                        None,
                        &mut output[destination..destination + channels],
                    );
                }
            }
            self.read_cursor = self.read_cursor.and_then(|current| current.checked_add(1));
            self.metrics.source_frames_advanced_total += 1;
            self.metrics.source_render_frames_total += 1;
            self.complete_slip();
            if self.conceal_run_frames >= REPRIME_AFTER_FRAMES {
                self.state = PlayoutState::ReprimeSeek;
                self.metrics.reprime_count_total += 1;
                self.reprime_started = Some(now);
                self.merge_phase = None;
                self.active_slip = None;
                self.reprime_fade_base = self.last_output;
                self.reprime_fade_position = 0;
            }
        }
        if let Some(cursor) = self.read_cursor {
            self.retire_records(cursor);
        }
    }

    fn render_exact_frame(&mut self, cursor: u64, output: &mut [f32]) {
        let slip = self.active_slip;
        for (channel, destination) in output.iter_mut().enumerate() {
            let exact = match slip {
                Some(ActiveSlip {
                    direction: SlipDirection::Advance,
                    phase,
                }) => {
                    let fraction = f32::from(phase + 1) / CONCEAL_RAMP_FRAMES as f32;
                    let current = self.sample(cursor, channel);
                    let next = self.sample(cursor + 1, channel);
                    current + (next - current) * fraction
                }
                Some(ActiveSlip {
                    direction: SlipDirection::Retard,
                    phase,
                }) => {
                    let fraction = f32::from(phase + 1) / CONCEAL_RAMP_FRAMES as f32;
                    let previous = self.sample(cursor - 1, channel);
                    let current = self.sample(cursor, channel);
                    previous * fraction + current * (1.0 - fraction)
                }
                None => self.sample(cursor, channel),
            };
            let rendered = if let Some(phase) = self.merge_phase {
                let theta =
                    (phase + 1) as f32 * core::f32::consts::FRAC_PI_2 / CONCEAL_RAMP_FRAMES as f32;
                self.merge_base[channel] * theta.cos() + exact * theta.sin()
            } else {
                exact
            };
            *destination = rendered;
            self.last_output[channel] = rendered;
        }
        if let Some(phase) = self.merge_phase {
            self.merge_phase = (phase + 1 < CONCEAL_RAMP_FRAMES).then_some(phase + 1);
        }
        self.slip_accumulator = self
            .slip_accumulator
            .saturating_add(i64::from(self.metrics.read_command_ppm));
    }

    fn render_concealed_frame(
        &mut self,
        now: MonotonicInstant,
        declared: Option<GapKind>,
        output: &mut [f32],
    ) {
        if !self.in_concealment {
            self.in_concealment = true;
            self.concealment_has_network_loss = declared.is_none();
            self.concealment_target_grown = false;
            self.conceal_position = 0;
            self.conceal_run_frames = 0;
            self.conceal_base = self.last_output;
            self.merge_phase = None;
            self.metrics.concealment_events_total += 1;
        }
        if declared.is_none() {
            self.concealment_has_network_loss = true;
            self.metrics.unrecovered_frames_total += 1;
        } else if let Some(reason) = declared {
            self.metrics.sender_gap_frames_total[reason.index()] += 1;
        }
        for (channel, destination) in output.iter_mut().enumerate() {
            let concealed = if self.conceal_position < CONCEAL_RAMP_FRAMES {
                let theta = (self.conceal_position + 1) as f32 * core::f32::consts::FRAC_PI_2
                    / CONCEAL_RAMP_FRAMES as f32;
                self.conceal_base[channel] * theta.cos()
            } else {
                0.0
            };
            *destination = concealed;
            self.last_output[channel] = concealed;
        }
        self.conceal_position = self.conceal_position.saturating_add(1);
        self.conceal_run_frames = self.conceal_run_frames.saturating_add(1);
        self.metrics.concealed_frames_total += 1;
        self.metrics.longest_concealment_frames = self
            .metrics
            .longest_concealment_frames
            .max(u64::from(self.conceal_run_frames));
        if self.concealment_has_network_loss
            && self.conceal_run_frames >= 960
            && !self.concealment_target_grown
        {
            self.increase_target(now, AdaptationReason::LongConcealment);
            self.concealment_target_grown = true;
        }
    }

    fn render_seek_frame(&mut self, cursor: u64, output: &mut [f32]) {
        match self.playout_cell(cursor) {
            CellState::Gap(reason) => self.metrics.sender_gap_frames_total[reason.index()] += 1,
            CellState::Exact => {}
            CellState::Empty => self.metrics.unrecovered_frames_total += 1,
        }
        self.metrics.concealed_frames_total += 1;
        self.metrics.longest_concealment_frames = self
            .metrics
            .longest_concealment_frames
            .max(u64::from(self.conceal_run_frames));
        for (channel, destination) in output.iter_mut().enumerate() {
            let faded = if self.reprime_fade_position < CONCEAL_RAMP_FRAMES {
                let theta = (self.reprime_fade_position + 1) as f32 * core::f32::consts::FRAC_PI_2
                    / CONCEAL_RAMP_FRAMES as f32;
                self.reprime_fade_base[channel] * theta.cos()
            } else {
                0.0
            };
            *destination = faded;
            self.last_output[channel] = faded;
        }
        self.reprime_fade_position = self.reprime_fade_position.saturating_add(1);
    }

    fn finish_concealment(&mut self, now: MonotonicInstant) {
        if !self.in_concealment {
            return;
        }
        if self.concealment_has_network_loss {
            self.record_late_loss_event(now);
        }
        self.merge_base = self.last_output;
        self.merge_phase = Some(0);
        self.in_concealment = false;
        self.concealment_has_network_loss = false;
        self.concealment_target_grown = false;
        self.conceal_position = 0;
        self.conceal_run_frames = 0;
    }

    fn record_late_loss_event(&mut self, now: MonotonicInstant) {
        if self.mode != TargetMode::Automatic {
            return;
        }
        let ten_seconds = self.seconds_to_ticks(10);
        let cutoff = now.ticks().saturating_sub(ten_seconds);
        let mut kept = [None; 2];
        let mut count = 0;
        for tick in self.recent_late_loss.into_iter().flatten() {
            if tick >= cutoff && count < kept.len() {
                kept[count] = Some(tick);
                count += 1;
            }
        }
        if count < kept.len() {
            kept[count] = Some(now.ticks());
            count += 1;
        } else {
            kept[0] = kept[1];
            kept[1] = Some(now.ticks());
        }
        self.recent_late_loss = kept;
        if count >= 2 {
            self.increase_target(now, AdaptationReason::RepeatedLateLoss);
        }
    }

    fn increase_target(&mut self, now: MonotonicInstant, reason: AdaptationReason) {
        if self.mode != TargetMode::Automatic
            || now.ticks() < self.automatic_cooldown_until
            || self.target_frames >= AUTOMATIC_MAX_FRAMES
        {
            return;
        }
        self.target_frames = self
            .target_frames
            .saturating_add(AUTOMATIC_STEP_FRAMES)
            .min(AUTOMATIC_MAX_FRAMES);
        self.automatic_cooldown_until = now.ticks().saturating_add(self.seconds_to_ticks(10));
        self.recent_late_loss = [None; 2];
        self.clean_snapshots = 0;
        self.metrics.adaptation_reason = Some(reason);
        self.metrics.adaptation_ticks = Some(now.ticks());
    }

    fn update_ready_metrics(&mut self) {
        let Some(cursor) = self.read_cursor else {
            self.metrics.ready_contiguous_frames = 0;
            self.metrics.ready_total_frames = 0;
            return;
        };
        let mut contiguous = 0_u16;
        for offset in 0..LOGICAL_CAPACITY_FRAMES {
            if self.cell(cursor + offset) == CellState::Exact {
                contiguous = contiguous.saturating_add(1);
            } else {
                break;
            }
        }
        let mut total = 0_u16;
        for offset in 0..LOGICAL_CAPACITY_FRAMES {
            if self.cell(cursor + offset) == CellState::Exact {
                total = total.saturating_add(1);
            }
        }
        self.metrics.ready_contiguous_frames = contiguous;
        self.metrics.ready_total_frames = total;
        let error = i32::from(contiguous) - i32::from(self.target_frames);
        self.metrics.fill_command_ppm = if error.unsigned_abs() <= 24 {
            0
        } else {
            error.saturating_mul(4).clamp(-2_000, 2_000)
        };
    }

    fn update_read_command(&mut self) {
        let desired = self
            .metrics
            .drift_command_ppm
            .saturating_add(self.metrics.fill_command_ppm)
            .clamp(-2_500, 2_500);
        let current = self.metrics.read_command_ppm;
        self.metrics.read_command_ppm = desired.clamp(current - 500, current + 500);
    }

    fn maybe_start_slip(&mut self, cursor: u64) {
        if self.active_slip.is_some() || self.merge_phase.is_some() {
            return;
        }
        if self.slip_accumulator >= 1_000_000
            && self.exact_window(cursor, u64::from(CONCEAL_RAMP_FRAMES) + 1)
        {
            self.active_slip = Some(ActiveSlip {
                direction: SlipDirection::Advance,
                phase: 0,
            });
            self.slip_accumulator -= 1_000_000;
        } else if self.slip_accumulator <= -1_000_000
            && cursor > 0
            && self.cell(cursor - 1) == CellState::Exact
            && self.exact_window(cursor, u64::from(CONCEAL_RAMP_FRAMES))
        {
            self.active_slip = Some(ActiveSlip {
                direction: SlipDirection::Retard,
                phase: 0,
            });
            self.slip_accumulator += 1_000_000;
        }
    }

    fn complete_slip(&mut self) {
        let Some(mut slip) = self.active_slip else {
            return;
        };
        slip.phase = slip.phase.saturating_add(1);
        if u32::from(slip.phase) < CONCEAL_RAMP_FRAMES {
            self.active_slip = Some(slip);
            return;
        }
        match slip.direction {
            SlipDirection::Advance => {
                self.read_cursor = self.read_cursor.and_then(|current| current.checked_add(1));
                self.metrics.source_frames_advanced_total += 1;
                self.metrics.sample_slip_advance_frames_total += 1;
            }
            SlipDirection::Retard => {
                self.read_cursor = self.read_cursor.and_then(|current| current.checked_sub(1));
                self.metrics.source_frames_advanced_total =
                    self.metrics.source_frames_advanced_total.saturating_sub(1);
                self.metrics.sample_slip_retard_frames_total += 1;
            }
        }
        self.active_slip = None;
    }

    fn first_complete_media_at_or_after(&self, cursor: u64) -> Option<u64> {
        self.records
            .iter()
            .filter(|record| record.kind == RecordKind::Media && record.start >= cursor)
            .map(|record| record.start)
            .min()
    }

    fn retire_records(&mut self, cursor: u64) {
        for record in &mut self.records {
            if record.kind != RecordKind::Empty && record.end <= cursor {
                *record = RecordSlot::EMPTY;
            }
        }
    }

    fn exact_window(&self, start: u64, frames: u64) -> bool {
        (0..frames).all(|offset| self.cell(start + offset) == CellState::Exact)
    }

    fn cell(&self, sample_index: u64) -> CellState {
        let index = ring_index(sample_index);
        if self.tags[index] == sample_index {
            self.cells[index]
        } else {
            CellState::Empty
        }
    }

    fn playout_cell(&mut self, sample_index: u64) -> CellState {
        let exact = self.cell(sample_index);
        if exact == CellState::Exact {
            return exact;
        }
        if let Some(active) = self.active_gap_cache {
            if (active.start..active.end).contains(&sample_index)
                && let RecordKind::Gap(reason) = active.kind
            {
                return CellState::Gap(reason);
            }
            if sample_index >= active.end {
                self.active_gap_cache = None;
            }
        }
        if let Some(next) = self.next_gap_cache {
            if sample_index < next.start {
                return CellState::Empty;
            }
            self.next_gap_cache = None;
            if sample_index < next.end
                && let RecordKind::Gap(reason) = next.kind
            {
                self.active_gap_cache = Some(next);
                return CellState::Gap(reason);
            }
        }
        let next = self
            .records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::Gap(_)) && record.end > sample_index)
            .min_by_key(|record| record.start)
            .copied();
        if let Some(record) = next {
            if record.start <= sample_index {
                self.active_gap_cache = Some(record);
                if let RecordKind::Gap(reason) = record.kind {
                    return CellState::Gap(reason);
                }
            } else {
                self.next_gap_cache = Some(record);
            }
        }
        CellState::Empty
    }

    fn sample(&self, sample_index: u64, channel: usize) -> f32 {
        let index = ring_index(sample_index) * usize::from(self.channels) + channel;
        self.samples[index]
    }

    fn target_duration(&self) -> rlb_clock::MonotonicDuration {
        let ticks = (u128::from(self.target_frames) * u128::from(self.ticks_per_second.get()))
            .div_ceil(u128::from(SAMPLE_RATE_HZ))
            .min(u128::from(u64::MAX)) as u64;
        rlb_clock::MonotonicDuration::from_ticks(ticks)
    }

    fn seconds_to_ticks(&self, seconds: u64) -> u64 {
        seconds.saturating_mul(self.ticks_per_second.get())
    }
}

fn ring_index(sample_index: u64) -> usize {
    (sample_index % SAMPLE_RING_FRAMES as u64) as usize
}

fn ranges_overlap(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> bool {
    left_start < right_end && right_start < left_end
}

fn saturating_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks() -> NonZeroU64 {
        NonZeroU64::new(1_000).unwrap()
    }

    fn unit(value: f32, channels: u8) -> Vec<f32> {
        vec![value; usize::from(MEDIA_UNIT_FRAMES) * usize::from(channels)]
    }

    #[test]
    fn preroll_uses_sample_anchor_and_callback_rounding() {
        let mut jitter = JitterBuffer::new(1, TargetMode::Automatic, ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                4_800,
                &unit(0.25, 1),
                MonotonicInstant::from_ticks(2),
            )
            .unwrap();
        assert_eq!(jitter.read_cursor(), Some(4_800));
        assert_eq!(jitter.metrics().preroll_anchor_sample, Some(4_800));
        assert_eq!(jitter.metrics().requested_start_ticks, Some(22));

        let mut output = [1.0; 480];
        jitter
            .render(MonotonicInstant::from_ticks(20), 480, &mut output)
            .unwrap();
        assert_eq!(jitter.state(), PlayoutState::Preroll);
        assert_eq!(output, [0.0; 480]);

        jitter
            .render(MonotonicInstant::from_ticks(30), 480, &mut output)
            .unwrap();
        assert_eq!(jitter.metrics().actual_start_ticks, Some(30));
        assert_eq!(jitter.metrics().callback_rounding_ticks, Some(8));
        assert!(output.iter().all(|sample| *sample == 0.25));
    }

    #[test]
    fn first_complete_unit_remains_the_reported_preroll_anchor() {
        let mut jitter = JitterBuffer::new(1, TargetMode::Automatic, ticks()).unwrap();
        let generation = jitter.enable();
        assert_eq!(
            jitter.insert_media(
                generation,
                1,
                9_600,
                &unit(0.5, 1),
                MonotonicInstant::from_ticks(2)
            ),
            Ok(InsertOutcome::Inserted)
        );
        assert_eq!(
            jitter.insert_media(
                generation,
                0,
                0,
                &unit(0.25, 1),
                MonotonicInstant::from_ticks(3)
            ),
            Ok(InsertOutcome::Stale)
        );
        let metrics = jitter.metrics();
        assert_eq!(metrics.preroll_anchor_sample, Some(9_600));
        assert_eq!(metrics.complete_units_total, 1);
        assert_eq!(metrics.stale_records_total, 1);
    }

    #[test]
    fn callback_entry_deadline_gates_recovery_before_cursor_advance() {
        let mut late = JitterBuffer::new(1, TargetMode::manual(480).unwrap(), ticks()).unwrap();
        let generation = late.enable();
        late.insert_media(
            generation,
            0,
            0,
            &unit(1.0, 1),
            MonotonicInstant::from_ticks(0),
        )
        .unwrap();
        let mut output = [0.0; 480];
        late.render(MonotonicInstant::from_ticks(10), 480, &mut output)
            .unwrap();
        assert_eq!(late.read_cursor(), Some(480));

        late.note_render_callback_entry(MonotonicInstant::from_ticks(20), 480);
        assert_eq!(
            late.insert_media_with_recovery(
                generation,
                1,
                480,
                &unit(0.5, 1),
                MonotonicInstant::from_ticks(21),
                true,
            ),
            Ok(InsertOutcome::Late)
        );
        assert_eq!(late.read_cursor(), Some(480));
        let late_metrics = late.metrics();
        assert_eq!(late_metrics.late_frames_total, 480);
        assert_eq!(late_metrics.complete_units_total, 1);
        assert_eq!(late_metrics.recovered_before_deadline_units_total, 0);
        assert_eq!(late_metrics.recovered_before_deadline_frames_total, 0);
        late.render(MonotonicInstant::from_ticks(20), 480, &mut output)
            .unwrap();
        assert_eq!(late.read_cursor(), Some(960));
        assert_eq!(late.metrics().exact_frames_total, 480);

        let mut on_time = JitterBuffer::new(1, TargetMode::manual(480).unwrap(), ticks()).unwrap();
        let generation = on_time.enable();
        on_time
            .insert_media(
                generation,
                0,
                0,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        on_time
            .render(MonotonicInstant::from_ticks(10), 480, &mut output)
            .unwrap();
        on_time.note_render_callback_entry(MonotonicInstant::from_ticks(20), 480);
        assert_eq!(
            on_time.insert_media_with_recovery(
                generation,
                1,
                480,
                &unit(0.5, 1),
                MonotonicInstant::from_ticks(20),
                true,
            ),
            Ok(InsertOutcome::Inserted)
        );
        let metrics = on_time.metrics();
        assert_eq!(metrics.late_frames_total, 0);
        assert_eq!(metrics.recovered_before_deadline_units_total, 1);
        assert_eq!(metrics.recovered_before_deadline_frames_total, 480);
    }

    #[test]
    fn timeline_detects_reorder_duplicate_stale_and_overlap() {
        let mut jitter = JitterBuffer::new(1, TargetMode::Automatic, ticks()).unwrap();
        let generation = jitter.enable();
        assert_eq!(
            jitter.insert_media(
                generation,
                1,
                480,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(0)
            ),
            Ok(InsertOutcome::Inserted)
        );
        assert_eq!(
            jitter.insert_media(
                generation,
                2,
                0,
                &unit(2.0, 1),
                MonotonicInstant::from_ticks(1)
            ),
            Ok(InsertOutcome::Stale)
        );
        assert_eq!(
            jitter.insert_media(
                generation,
                1,
                480,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(1)
            ),
            Ok(InsertOutcome::Duplicate)
        );

        let old_generation = generation;
        let new_generation = jitter.enable();
        assert_ne!(old_generation, new_generation);
        assert_eq!(
            jitter.insert_gap(old_generation, 3, 960, 480, GapKind::CaptureOverrun),
            Ok(InsertOutcome::StaleGeneration)
        );
        jitter
            .insert_media(
                new_generation,
                4,
                0,
                &unit(0.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        assert_eq!(
            jitter.insert_gap(new_generation, 5, 240, 480, GapKind::KnownAdapterGap),
            Err(JitterError::TimelineConflict)
        );
        assert_eq!(jitter.state(), PlayoutState::Faulted);
    }

    #[test]
    fn future_records_can_reorder_inside_the_fixed_window() {
        let mut jitter = JitterBuffer::new(1, TargetMode::Automatic, ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(0.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        jitter
            .insert_media(
                generation,
                2,
                960,
                &unit(2.0, 1),
                MonotonicInstant::from_ticks(1),
            )
            .unwrap();
        assert_eq!(
            jitter.insert_media(
                generation,
                1,
                480,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(2),
            ),
            Ok(InsertOutcome::Reordered)
        );
        assert_eq!(jitter.metrics().reordered_records_total, 1);
    }

    #[test]
    fn gap_concealment_preserves_time_and_merges_exact_audio() {
        let mut jitter = JitterBuffer::new(2, TargetMode::manual(480).unwrap(), ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(1.0, 2),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        jitter
            .insert_gap(generation, 1, 480, 480, GapKind::SendDeadlineDrop)
            .unwrap();
        jitter
            .insert_media(
                generation,
                2,
                960,
                &unit(0.5, 2),
                MonotonicInstant::from_ticks(1),
            )
            .unwrap();

        let mut exact = [0.0; 960];
        jitter
            .render(MonotonicInstant::from_ticks(10), 480, &mut exact)
            .unwrap();
        let mut concealed = [0.0; 960];
        jitter
            .render(MonotonicInstant::from_ticks(20), 480, &mut concealed)
            .unwrap();
        assert!(concealed[0] < 1.0 && concealed[0] > 0.99);
        assert!(concealed[95 * 2].abs() < 0.000_001);
        assert!(concealed[96 * 2..].iter().all(|sample| *sample == 0.0));

        let mut recovered = [0.0; 192];
        jitter
            .render(MonotonicInstant::from_ticks(30), 96, &mut recovered)
            .unwrap();
        assert!(recovered[0] > 0.0 && recovered[0] < 0.5);
        assert!((recovered[190] - 0.5).abs() < 0.000_001);
        assert_eq!(jitter.read_cursor(), Some(1_056));
        let metrics = jitter.metrics();
        assert_eq!(metrics.concealed_frames_total, 480);
        assert_eq!(
            metrics.sender_gap_frames_total[GapKind::SendDeadlineDrop.index()],
            480
        );
    }

    #[test]
    fn thirty_milliseconds_of_loss_enters_reprime_and_freezes_at_future_unit() {
        let mut jitter = JitterBuffer::new(1, TargetMode::manual(480).unwrap(), ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        let mut output = vec![0.0; 480];
        jitter
            .render(MonotonicInstant::from_ticks(10), 480, &mut output)
            .unwrap();
        output.resize(1_440, 0.0);
        jitter
            .render(MonotonicInstant::from_ticks(20), 1_440, &mut output)
            .unwrap();
        assert_eq!(jitter.state(), PlayoutState::ReprimeSeek);
        assert_eq!(jitter.read_cursor(), Some(1_920));

        jitter
            .insert_media(
                generation,
                4,
                2_400,
                &unit(0.5, 1),
                MonotonicInstant::from_ticks(21),
            )
            .unwrap();
        output.resize(480, 0.0);
        jitter
            .render(MonotonicInstant::from_ticks(30), 480, &mut output)
            .unwrap();
        assert_eq!(jitter.read_cursor(), Some(2_400));
        jitter
            .render(MonotonicInstant::from_ticks(40), 480, &mut output)
            .unwrap();
        assert_eq!(jitter.state(), PlayoutState::ReprimeFill);
        assert_eq!(jitter.read_cursor(), Some(2_400));
        jitter
            .render(MonotonicInstant::from_ticks(50), 480, &mut output)
            .unwrap();
        assert_eq!(jitter.state(), PlayoutState::Playing);
        assert_eq!(jitter.read_cursor(), Some(2_880));
        assert_eq!(jitter.metrics().reprime_count_total, 1);
    }

    #[test]
    fn reprime_seek_uses_a_ninety_six_frame_fade_to_zero() {
        let mut jitter = JitterBuffer::new(1, TargetMode::manual(480).unwrap(), ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        let mut exact = [0.0; 480];
        jitter
            .render(MonotonicInstant::from_ticks(10), 480, &mut exact)
            .unwrap();

        jitter.in_concealment = true;
        jitter.concealment_has_network_loss = true;
        jitter.conceal_position = 0;
        jitter.conceal_run_frames = REPRIME_AFTER_FRAMES - 1;
        jitter.conceal_base = [1.0, 0.0];
        jitter.last_output = [1.0, 0.0];
        let mut trigger = [0.0; 1];
        jitter
            .render(MonotonicInstant::from_ticks(20), 1, &mut trigger)
            .unwrap();
        assert_eq!(jitter.state(), PlayoutState::ReprimeSeek);

        let mut fade = [0.0; CONCEAL_RAMP_FRAMES as usize];
        jitter
            .render(
                MonotonicInstant::from_ticks(21),
                CONCEAL_RAMP_FRAMES,
                &mut fade,
            )
            .unwrap();
        assert!(fade[0] > 0.0);
        assert!(fade[0] < trigger[0]);
        assert!(
            fade.windows(2)
                .all(|samples| samples[1].abs() <= samples[0].abs())
        );
        assert!(fade[95].abs() < 0.000_001);
    }

    #[test]
    fn automatic_target_grows_and_shrinks_with_bounded_evidence() {
        let mut jitter = JitterBuffer::new(1, TargetMode::Automatic, ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(1.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        let mut exact = vec![0.0; 480];
        jitter
            .render(MonotonicInstant::from_ticks(20), 480, &mut exact)
            .unwrap();
        let mut loss = vec![0.0; 960];
        jitter
            .render(MonotonicInstant::from_ticks(30), 960, &mut loss)
            .unwrap();
        assert_eq!(jitter.target_frames(), 1_440);
        assert_eq!(
            jitter.metrics().adaptation_reason,
            Some(AdaptationReason::LongConcealment)
        );

        let clean = AutomaticSnapshot {
            minimum_ready_frames: 2_400,
            ..AutomaticSnapshot::default()
        };
        for second in 0..120 {
            jitter.observe_automatic_snapshot(
                MonotonicInstant::from_ticks(20_000 + second * 1_000),
                clean,
            );
        }
        assert_eq!(jitter.target_frames(), 960);
        assert_eq!(
            jitter.metrics().adaptation_reason,
            Some(AdaptationReason::CleanWindow)
        );
    }

    #[test]
    fn capacity_and_target_remain_independent() {
        let mut jitter = JitterBuffer::new(1, TargetMode::Automatic, ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(0.0, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        assert_eq!(
            jitter.insert_media(
                generation,
                1,
                LOGICAL_CAPACITY_FRAMES,
                &unit(0.0, 1),
                MonotonicInstant::from_ticks(1)
            ),
            Ok(InsertOutcome::CapacityDropped)
        );
        assert_eq!(jitter.target_frames(), 960);
        assert_eq!(LOGICAL_CAPACITY_FRAMES, 12_000);
    }

    #[test]
    fn epoch_end_stops_without_fabricated_tail() {
        let mut jitter = JitterBuffer::new(1, TargetMode::manual(480).unwrap(), ticks()).unwrap();
        let generation = jitter.enable();
        jitter
            .insert_media(
                generation,
                0,
                0,
                &unit(0.25, 1),
                MonotonicInstant::from_ticks(0),
            )
            .unwrap();
        jitter.insert_epoch_end(generation, 1, 480).unwrap();
        let mut output = [0.0; 960];
        jitter
            .render(MonotonicInstant::from_ticks(10), 960, &mut output)
            .unwrap();
        assert_eq!(jitter.state(), PlayoutState::Ended);
        assert_eq!(jitter.read_cursor(), Some(480));
        assert!(output[..480].iter().all(|sample| *sample == 0.25));
        assert!(output[480..].iter().all(|sample| *sample == 0.0));
    }
}
