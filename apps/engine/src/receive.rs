//! Render-thread jitter adapter with lock-free sample ingress.
//!
//! The coordinator thread decodes complete MEDIA units and pushes them through
//! a preallocated `sync_channel`. The WASAPI mix callback drains with
//! `try_recv`, inserts into [`JitterBuffer`], and converts interleaved playout
//! to planar mixer storage. A fixed atomic single-producer/single-consumer side
//! queue preserves commands rejected by a full ingress queue. Recycled sample
//! boxes return on a second channel so the render thread never drops the last
//! `Box`.

use std::array;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use rlb_audio::{SourceReadOutcome, SourceReader};
use rlb_clock::{MonotonicClock, StdMonotonic};
use rlb_jitter::{GapKind, JitterBuffer, TargetMode};
use rlb_model::{AUTOMATIC_TARGET_FRAMES, MEDIA_UNIT_FRAMES};

const INGRESS_CAPACITY: usize = 32;
const RECYCLE_CAPACITY: usize = 40;
const MAX_INGRESS_PER_READ: usize = 8;
const OVERFLOW_CAPACITY: usize = INGRESS_CAPACITY;
const MAX_GAP_FRAMES: u32 = 48_000;

const SLOT_EMPTY: u8 = 0;
const SLOT_WRITING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_READING: u8 = 3;

const COMMAND_GAP: u8 = 1;
const COMMAND_EPOCH_END: u8 = 2;
const COMMAND_TARGET_AUTOMATIC: u8 = 3;
const COMMAND_TARGET_MANUAL: u8 = 4;

const LIFECYCLE_NONE: u8 = 0;
const LIFECYCLE_ENABLE: u8 = 1;
const LIFECYCLE_DISABLE: u8 = 2;

/// Coordinator-to-render command for one remote source.
pub enum IngressCommand {
    /// Starts a fresh local playout generation after a matching subscription ACK.
    Enable,
    /// Clears and disables the current local playout generation.
    Disable,
    /// One decoded 480-frame unit.
    Media {
        sequence: u32,
        start_sample: u64,
        interleaved: Box<[f32]>,
        recovered_after_nack: bool,
    },
    /// Sender-declared GAP interval.
    Gap {
        sequence: u32,
        start_sample: u64,
        frame_count: u32,
        reason: GapKind,
    },
    /// Exact epoch end.
    EpochEnd {
        next_sequence: u32,
        final_sample: u64,
    },
    /// Follow `local.receive_buffer.set` without reallocating the timeline.
    SetTarget { mode: TargetMode },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverflowGap {
    sequence: u32,
    start_sample: u64,
    frame_count: u32,
}

impl OverflowGap {
    fn end_sample(self) -> Option<u64> {
        self.start_sample.checked_add(u64::from(self.frame_count))
    }

    fn merge(self, next: Self) -> Option<Self> {
        let self_end = self.end_sample()?;
        let next_end = next.end_sample()?;
        if self_end < next.start_sample || next_end < self.start_sample {
            return None;
        }
        let start_sample = self.start_sample.min(next.start_sample);
        let end_sample = self_end.max(next_end);
        let frame_count = u32::try_from(end_sample.checked_sub(start_sample)?).ok()?;
        if frame_count > MAX_GAP_FRAMES {
            return None;
        }
        let sequence = if next.start_sample < self.start_sample {
            next.sequence
        } else {
            self.sequence
        };
        Some(Self {
            sequence,
            start_sample,
            frame_count,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverflowCommand {
    Gap(OverflowGap),
    EpochEnd {
        next_sequence: u32,
        final_sample: u64,
    },
    SetTarget {
        mode: TargetMode,
    },
}

impl OverflowCommand {
    fn from_rejected(command: &IngressCommand) -> Option<Self> {
        match command {
            IngressCommand::Enable | IngressCommand::Disable => None,
            IngressCommand::Media {
                sequence,
                start_sample,
                ..
            } => Some(Self::Gap(OverflowGap {
                sequence: *sequence,
                start_sample: *start_sample,
                frame_count: u32::from(MEDIA_UNIT_FRAMES),
            })),
            IngressCommand::Gap {
                sequence,
                start_sample,
                frame_count,
                ..
            } => Some(Self::Gap(OverflowGap {
                sequence: *sequence,
                start_sample: *start_sample,
                frame_count: *frame_count,
            })),
            IngressCommand::EpochEnd {
                next_sequence,
                final_sample,
            } => Some(Self::EpochEnd {
                next_sequence: *next_sequence,
                final_sample: *final_sample,
            }),
            IngressCommand::SetTarget { mode } => Some(Self::SetTarget { mode: *mode }),
        }
    }

    fn merge(self, next: Self) -> Option<Self> {
        match (self, next) {
            (Self::Gap(current), Self::Gap(next)) => current.merge(next).map(Self::Gap),
            (Self::SetTarget { .. }, Self::SetTarget { mode }) => Some(Self::SetTarget { mode }),
            (
                Self::EpochEnd {
                    next_sequence: current_sequence,
                    final_sample: current_sample,
                },
                next @ Self::EpochEnd {
                    next_sequence,
                    final_sample,
                },
            ) if current_sequence == next_sequence && current_sample == final_sample => Some(next),
            _ => None,
        }
    }

    fn into_ingress(self) -> IngressCommand {
        match self {
            Self::Gap(gap) => IngressCommand::Gap {
                sequence: gap.sequence,
                start_sample: gap.start_sample,
                frame_count: gap.frame_count,
                reason: GapKind::SendQueueOverflow,
            },
            Self::EpochEnd {
                next_sequence,
                final_sample,
            } => IngressCommand::EpochEnd {
                next_sequence,
                final_sample,
            },
            Self::SetTarget { mode } => IngressCommand::SetTarget { mode },
        }
    }
}

enum SlotTake {
    Unavailable,
    Taken(Option<OverflowCommand>),
}

struct OverflowSlot {
    state: AtomicU8,
    kind: AtomicU8,
    sequence: AtomicU32,
    sample: AtomicU64,
    value: AtomicU32,
}

impl OverflowSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(SLOT_EMPTY),
            kind: AtomicU8::new(COMMAND_GAP),
            sequence: AtomicU32::new(0),
            sample: AtomicU64::new(0),
            value: AtomicU32::new(0),
        }
    }

    fn try_store(&self, command: OverflowCommand) -> bool {
        if self
            .state
            .compare_exchange(
                SLOT_EMPTY,
                SLOT_WRITING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        self.store(command);
        self.state.store(SLOT_READY, Ordering::Release);
        true
    }

    fn try_merge(&self, command: OverflowCommand) -> bool {
        if self
            .state
            .compare_exchange(
                SLOT_READY,
                SLOT_WRITING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        let merged = self.load().and_then(|current| current.merge(command));
        if let Some(merged) = merged {
            self.store(merged);
        }
        self.state.store(SLOT_READY, Ordering::Release);
        merged.is_some()
    }

    fn try_take(&self) -> SlotTake {
        if self
            .state
            .compare_exchange(
                SLOT_READY,
                SLOT_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return SlotTake::Unavailable;
        }
        let command = self.load();
        self.state.store(SLOT_EMPTY, Ordering::Release);
        SlotTake::Taken(command)
    }

    fn store(&self, command: OverflowCommand) {
        match command {
            OverflowCommand::Gap(gap) => {
                self.kind.store(COMMAND_GAP, Ordering::Relaxed);
                self.sequence.store(gap.sequence, Ordering::Relaxed);
                self.sample.store(gap.start_sample, Ordering::Relaxed);
                self.value.store(gap.frame_count, Ordering::Relaxed);
            }
            OverflowCommand::EpochEnd {
                next_sequence,
                final_sample,
            } => {
                self.kind.store(COMMAND_EPOCH_END, Ordering::Relaxed);
                self.sequence.store(next_sequence, Ordering::Relaxed);
                self.sample.store(final_sample, Ordering::Relaxed);
                self.value.store(0, Ordering::Relaxed);
            }
            OverflowCommand::SetTarget {
                mode: TargetMode::Automatic,
            } => {
                self.kind.store(COMMAND_TARGET_AUTOMATIC, Ordering::Relaxed);
                self.sequence.store(0, Ordering::Relaxed);
                self.sample.store(0, Ordering::Relaxed);
                self.value.store(0, Ordering::Relaxed);
            }
            OverflowCommand::SetTarget {
                mode: TargetMode::Manual(frames),
            } => {
                self.kind.store(COMMAND_TARGET_MANUAL, Ordering::Relaxed);
                self.sequence.store(0, Ordering::Relaxed);
                self.sample.store(0, Ordering::Relaxed);
                self.value.store(u32::from(frames), Ordering::Relaxed);
            }
        }
    }

    fn load(&self) -> Option<OverflowCommand> {
        let kind = self.kind.load(Ordering::Relaxed);
        let sequence = self.sequence.load(Ordering::Relaxed);
        let sample = self.sample.load(Ordering::Relaxed);
        let value = self.value.load(Ordering::Relaxed);
        match kind {
            COMMAND_GAP => Some(OverflowCommand::Gap(OverflowGap {
                sequence,
                start_sample: sample,
                frame_count: value,
            })),
            COMMAND_EPOCH_END => Some(OverflowCommand::EpochEnd {
                next_sequence: sequence,
                final_sample: sample,
            }),
            COMMAND_TARGET_AUTOMATIC => Some(OverflowCommand::SetTarget {
                mode: TargetMode::Automatic,
            }),
            COMMAND_TARGET_MANUAL => u16::try_from(value)
                .ok()
                .map(TargetMode::Manual)
                .map(|mode| OverflowCommand::SetTarget { mode }),
            _ => None,
        }
    }
}

struct OverflowQueue {
    slots: [OverflowSlot; OVERFLOW_CAPACITY],
    head: AtomicUsize,
    tail: AtomicUsize,
    consumer_connected: AtomicBool,
}

impl OverflowQueue {
    fn new() -> Self {
        Self {
            slots: array::from_fn(|_| OverflowSlot::new()),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            consumer_connected: AtomicBool::new(true),
        }
    }

    fn try_push(&self, command: OverflowCommand) -> bool {
        if !self.consumer_connected.load(Ordering::Acquire) {
            return false;
        }
        if self.try_merge_tail(command) {
            return true;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= OVERFLOW_CAPACITY {
            return false;
        }
        let slot = &self.slots[tail % OVERFLOW_CAPACITY];
        if !slot.try_store(command) {
            return false;
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    fn try_take(&self) -> Option<OverflowCommand> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        match self.slots[head % OVERFLOW_CAPACITY].try_take() {
            SlotTake::Unavailable => None,
            SlotTake::Taken(command) => {
                self.head.store(head.wrapping_add(1), Ordering::Release);
                command
            }
        }
    }

    fn disconnect_consumer(&self) {
        self.consumer_connected.store(false, Ordering::Release);
    }

    fn consumer_connected(&self) -> bool {
        self.consumer_connected.load(Ordering::Acquire)
    }

    fn try_merge_tail(&self, command: OverflowCommand) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if head == tail {
            return false;
        }
        self.slots[tail.wrapping_sub(1) % OVERFLOW_CAPACITY].try_merge(command)
    }
}

struct PendingOverflowMedia {
    armed: AtomicBool,
    sequence: AtomicU32,
    start_sample: AtomicU64,
}

impl PendingOverflowMedia {
    const fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            sequence: AtomicU32::new(0),
            start_sample: AtomicU64::new(0),
        }
    }

    fn arm(&self, sequence: u32, start_sample: u64) {
        self.sequence.store(sequence, Ordering::Relaxed);
        self.start_sample.store(start_sample, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }

    fn take_matching(&self, command: &IngressCommand) -> bool {
        if !self.armed.swap(false, Ordering::AcqRel) {
            return false;
        }
        let IngressCommand::Gap {
            sequence,
            start_sample,
            frame_count,
            reason: GapKind::SendQueueOverflow,
        } = command
        else {
            return false;
        };
        if *frame_count != u32::from(MEDIA_UNIT_FRAMES)
            || self.sequence.load(Ordering::Relaxed) != *sequence
            || self.start_sample.load(Ordering::Relaxed) != *start_sample
        {
            return false;
        }
        true
    }
}

/// Shared counters observed by the coordinator without touching the reader.
#[derive(Debug, Default)]
pub struct JitterReaderMetrics {
    pub exact_frames: AtomicU64,
    pub render_frames: AtomicU64,
    pub ingress_applied: AtomicU64,
    pub ingress_dropped: AtomicU64,
    pub insert_rejected: AtomicU64,
    pub read_cursor: AtomicU64,
    pub target_frames: AtomicU16,
    pub generation: AtomicU64,
    pub enabled: AtomicBool,
    pub retired: AtomicBool,
}

/// Coordinator half of one remote source timeline.
pub struct JitterIngress {
    commands: SyncSender<IngressCommand>,
    recycle: Receiver<Box<[f32]>>,
    overflow: Arc<OverflowQueue>,
    lifecycle: Arc<AtomicU8>,
    pending_overflow_media: PendingOverflowMedia,
    pub metrics: Arc<JitterReaderMetrics>,
    sample_count: usize,
}

impl JitterIngress {
    /// Allocates ingress, recycle, jitter storage, and the render reader.
    pub fn pair(
        channels: u8,
        clock: StdMonotonic,
        mode: TargetMode,
    ) -> Result<(Self, Box<dyn SourceReader>), crate::error::EngineError> {
        let (command_tx, command_rx) = sync_channel(INGRESS_CAPACITY);
        let (recycle_tx, recycle_rx) = sync_channel(RECYCLE_CAPACITY);
        let sample_count = usize::from(MEDIA_UNIT_FRAMES) * usize::from(channels);
        for _ in 0..RECYCLE_CAPACITY {
            let _ = recycle_tx.send(vec![0.0_f32; sample_count].into_boxed_slice());
        }
        let metrics = Arc::new(JitterReaderMetrics {
            read_cursor: AtomicU64::new(u64::MAX),
            target_frames: AtomicU16::new(target_frames(mode)),
            ..JitterReaderMetrics::default()
        });
        let overflow = Arc::new(OverflowQueue::new());
        let lifecycle = Arc::new(AtomicU8::new(LIFECYCLE_NONE));
        let reader = JitterSourceReader {
            jitter: JitterBuffer::new(channels, mode, clock.ticks_per_second())?,
            commands: command_rx,
            recycle: recycle_tx,
            pending_recycle: None,
            overflow: Arc::clone(&overflow),
            lifecycle: Arc::clone(&lifecycle),
            clock,
            metrics: Arc::clone(&metrics),
            interleaved: vec![0.0_f32; sample_count],
        };
        Ok((
            Self {
                commands: command_tx,
                recycle: recycle_rx,
                overflow,
                lifecycle,
                pending_overflow_media: PendingOverflowMedia::new(),
                metrics,
                sample_count,
            },
            Box::new(reader),
        ))
    }

    /// Returns a recycled interleaved buffer, allocating only when the pool is empty.
    pub fn acquire_samples(&self) -> Box<[f32]> {
        self.recycle
            .try_recv()
            .unwrap_or_else(|_| vec![0.0_f32; self.sample_count].into_boxed_slice())
    }

    /// Pushes one command. A full primary queue records the exact command in a
    /// fixed single-producer/single-consumer side queue and returns false.
    /// Consecutive sample ranges merge into one GAP, and the Media box is
    /// released on this thread.
    pub fn try_push(&self, command: IngressCommand) -> bool {
        match &command {
            IngressCommand::Enable => return self.try_enable(),
            IngressCommand::Disable => {
                self.retire();
                self.force_disable();
                return true;
            }
            IngressCommand::Media { .. }
            | IngressCommand::Gap { .. }
            | IngressCommand::EpochEnd { .. }
                if !self.timeline_enabled() =>
            {
                self.metrics.ingress_dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            IngressCommand::SetTarget { .. } if self.metrics.retired.load(Ordering::Acquire) => {
                self.metrics.ingress_dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            _ => {}
        }
        if self.pending_overflow_media.take_matching(&command) && self.overflow.consumer_connected()
        {
            return true;
        }
        match self.commands.try_send(command) {
            Ok(()) => true,
            Err(TrySendError::Full(command)) => {
                self.metrics.ingress_dropped.fetch_add(1, Ordering::Relaxed);
                let represented = OverflowCommand::from_rejected(&command)
                    .is_some_and(|overflow| self.overflow.try_push(overflow));
                if represented
                    && let IngressCommand::Media {
                        sequence,
                        start_sample,
                        ..
                    } = command
                {
                    self.pending_overflow_media.arm(sequence, start_sample);
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.metrics.ingress_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Permanently closes this session's coordinator-visible timeline gate.
    pub(crate) fn retire(&self) {
        self.metrics.retired.store(true, Ordering::Release);
        self.metrics.enabled.store(false, Ordering::Release);
        self.metrics.read_cursor.store(u64::MAX, Ordering::Relaxed);
        self.pending_overflow_media.disarm();
    }

    /// Forces a render-thread disable through the priority lifecycle slot.
    pub(crate) fn force_disable(&self) {
        self.lifecycle.store(LIFECYCLE_DISABLE, Ordering::Release);
    }

    fn try_enable(&self) -> bool {
        if self.metrics.retired.load(Ordering::Acquire)
            || self
                .metrics
                .enabled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        if self.metrics.retired.load(Ordering::Acquire) {
            self.metrics.enabled.store(false, Ordering::Release);
            return false;
        }
        self.lifecycle.store(LIFECYCLE_ENABLE, Ordering::Release);
        true
    }

    fn timeline_enabled(&self) -> bool {
        self.metrics.enabled.load(Ordering::Acquire)
            && !self.metrics.retired.load(Ordering::Acquire)
    }
}

struct JitterSourceReader {
    jitter: JitterBuffer,
    commands: Receiver<IngressCommand>,
    recycle: SyncSender<Box<[f32]>>,
    pending_recycle: Option<Box<[f32]>>,
    overflow: Arc<OverflowQueue>,
    lifecycle: Arc<AtomicU8>,
    clock: StdMonotonic,
    metrics: Arc<JitterReaderMetrics>,
    interleaved: Vec<f32>,
}

impl Drop for JitterSourceReader {
    fn drop(&mut self) {
        self.overflow.disconnect_consumer();
    }
}

impl JitterSourceReader {
    fn apply_ingress(&mut self) {
        let now = self.clock.now();
        match self.lifecycle.swap(LIFECYCLE_NONE, Ordering::AcqRel) {
            LIFECYCLE_ENABLE => self.apply_command(IngressCommand::Enable, now),
            LIFECYCLE_DISABLE => self.apply_command(IngressCommand::Disable, now),
            _ => {}
        }
        if let Some(command) = self.overflow.try_take() {
            self.apply_command(command.into_ingress(), now);
        }
        for _ in 0..MAX_INGRESS_PER_READ {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            self.apply_command(command, now);
        }
        self.metrics
            .target_frames
            .store(self.jitter.target_frames(), Ordering::Relaxed);
    }

    fn apply_command(&mut self, command: IngressCommand, now: rlb_clock::MonotonicInstant) {
        let applied = match command {
            IngressCommand::Enable => {
                if self.metrics.retired.load(Ordering::Acquire)
                    || !self.metrics.enabled.load(Ordering::Acquire)
                {
                    self.jitter.disable();
                    self.metrics.enabled.store(false, Ordering::Release);
                    return;
                }
                let generation = self.jitter.enable();
                self.metrics
                    .generation
                    .store(generation.get(), Ordering::Relaxed);
                self.metrics.enabled.store(true, Ordering::Release);
                self.metrics.read_cursor.store(u64::MAX, Ordering::Relaxed);
                true
            }
            IngressCommand::Disable => {
                self.jitter.disable();
                self.metrics.enabled.store(false, Ordering::Release);
                self.metrics.read_cursor.store(u64::MAX, Ordering::Relaxed);
                true
            }
            IngressCommand::Media {
                sequence,
                start_sample,
                interleaved,
                recovered_after_nack,
            } => {
                if !self.timeline_enabled() {
                    self.recycle_samples(interleaved);
                    return;
                }
                let generation = self.jitter.generation();
                let result = self.jitter.insert_media_with_recovery(
                    generation,
                    sequence,
                    start_sample,
                    &interleaved,
                    now,
                    recovered_after_nack,
                );
                self.recycle_samples(interleaved);
                match result {
                    Ok(_) => true,
                    Err(_) => {
                        self.metrics.insert_rejected.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                }
            }
            IngressCommand::Gap {
                sequence,
                start_sample,
                frame_count,
                reason,
            } => {
                if !self.timeline_enabled() {
                    false
                } else {
                    match self.jitter.insert_gap(
                        self.jitter.generation(),
                        sequence,
                        start_sample,
                        frame_count,
                        reason,
                    ) {
                        Ok(_) => true,
                        Err(_) => {
                            self.metrics.insert_rejected.fetch_add(1, Ordering::Relaxed);
                            false
                        }
                    }
                }
            }
            IngressCommand::EpochEnd {
                next_sequence,
                final_sample,
            } => {
                if !self.timeline_enabled() {
                    false
                } else {
                    match self.jitter.insert_epoch_end(
                        self.jitter.generation(),
                        next_sequence,
                        final_sample,
                    ) {
                        Ok(_) => true,
                        Err(_) => {
                            self.metrics.insert_rejected.fetch_add(1, Ordering::Relaxed);
                            false
                        }
                    }
                }
            }
            IngressCommand::SetTarget { mode } => {
                if self.metrics.retired.load(Ordering::Acquire) {
                    return;
                }
                self.jitter.set_target_mode(mode);
                self.metrics
                    .target_frames
                    .store(self.jitter.target_frames(), Ordering::Relaxed);
                true
            }
        };
        if applied {
            self.metrics.ingress_applied.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn timeline_enabled(&self) -> bool {
        self.metrics.enabled.load(Ordering::Acquire)
            && !self.metrics.retired.load(Ordering::Acquire)
    }

    fn recycle_samples(&mut self, samples: Box<[f32]>) {
        if let Some(pending) = self.pending_recycle.take() {
            match self.recycle.try_send(pending) {
                Ok(()) => {}
                Err(TrySendError::Full(pending) | TrySendError::Disconnected(pending)) => {
                    self.pending_recycle = Some(pending);
                    self.pending_recycle.get_or_insert(samples);
                    return;
                }
            }
        }
        match self.recycle.try_send(samples) {
            Ok(()) => {}
            Err(TrySendError::Full(samples) | TrySendError::Disconnected(samples)) => {
                self.pending_recycle = Some(samples);
            }
        }
    }
}

impl SourceReader for JitterSourceReader {
    fn read(
        &mut self,
        planar_output: &mut [f32],
        frames: usize,
        channels: u8,
    ) -> SourceReadOutcome {
        self.apply_ingress();
        let required = frames.saturating_mul(usize::from(channels));
        if planar_output.len() < required || frames == 0 {
            return SourceReadOutcome { frames_written: 0 };
        }
        let interleaved_len = required;
        if self.interleaved.len() < interleaved_len {
            return SourceReadOutcome { frames_written: 0 };
        }
        if !self.timeline_enabled() {
            self.jitter.disable();
            planar_output[..required].fill(0.0);
            return SourceReadOutcome {
                frames_written: frames,
            };
        }
        let Ok(report) = self.jitter.render(
            self.clock.now(),
            frames as u32,
            &mut self.interleaved[..interleaved_len],
        ) else {
            planar_output[..required].fill(0.0);
            return SourceReadOutcome { frames_written: 0 };
        };
        self.metrics
            .target_frames
            .store(self.jitter.target_frames(), Ordering::Relaxed);
        interleaved_to_planar(
            &self.interleaved[..interleaved_len],
            &mut planar_output[..required],
            frames,
            channels,
        );
        self.metrics
            .render_frames
            .fetch_add(u64::from(frames as u32), Ordering::Relaxed);
        self.metrics
            .exact_frames
            .fetch_add(u64::from(report.exact_frames), Ordering::Relaxed);
        if let Some(cursor) = self.jitter.read_cursor() {
            self.metrics.read_cursor.store(cursor, Ordering::Relaxed);
        }
        SourceReadOutcome {
            frames_written: frames,
        }
    }
}

fn interleaved_to_planar(interleaved: &[f32], planar: &mut [f32], frames: usize, channels: u8) {
    let channels = usize::from(channels);
    for channel in 0..channels {
        let plane = &mut planar[channel * frames..(channel + 1) * frames];
        for frame in 0..frames {
            plane[frame] = interleaved[frame * channels + channel];
        }
    }
}

const fn target_frames(mode: TargetMode) -> u16 {
    match mode {
        TargetMode::Automatic => AUTOMATIC_TARGET_FRAMES,
        TargetMode::Manual(frames) => frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::MEDIA_UNIT_FRAMES;
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn ingress_media_renders_planar_after_preroll() {
        let clock = StdMonotonic::new();
        let (ingress, mut reader) =
            JitterIngress::pair(2, clock, TargetMode::Automatic).expect("pair");
        assert!(ingress.try_push(IngressCommand::Enable));
        let mut samples = ingress.acquire_samples();
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = if index % 2 == 0 { 0.25 } else { -0.5 };
        }
        assert!(
            ingress.try_push(IngressCommand::Media {
                sequence: 0,
                start_sample: 0,
                interleaved: samples,
                recovered_after_nack: false,
            }),
            "first media command should queue"
        );
        let frames = usize::from(MEDIA_UNIT_FRAMES);
        let mut planar = vec![0.0_f32; frames * 2];
        let first = reader.read(&mut planar, frames, 2);
        assert_eq!(first.frames_written, frames);
        thread::sleep(Duration::from_millis(25));
        let second = reader.read(&mut planar, frames, 2);
        assert_eq!(second.frames_written, frames);
        assert!(
            ingress.metrics.ingress_applied.load(Ordering::Relaxed) >= 1
                || ingress.metrics.render_frames.load(Ordering::Relaxed)
                    >= u64::from(MEDIA_UNIT_FRAMES)
        );
        assert!(ingress.metrics.enabled.load(Ordering::Acquire));
        assert_eq!(ingress.metrics.generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn explicit_enable_and_disable_bound_playout_generation() {
        let clock = StdMonotonic::new();
        let (ingress, mut reader) =
            JitterIngress::pair(1, clock, TargetMode::manual(1_440).expect("target"))
                .expect("pair");
        let frames = usize::from(MEDIA_UNIT_FRAMES);
        let mut planar = vec![1.0_f32; frames];

        assert!(!ingress.try_push(IngressCommand::Media {
            sequence: 0,
            start_sample: 0,
            interleaved: ingress.acquire_samples(),
            recovered_after_nack: false,
        }));
        reader.read(&mut planar, frames, 1);
        assert!(!ingress.metrics.enabled.load(Ordering::Acquire));
        assert_eq!(ingress.metrics.generation.load(Ordering::Relaxed), 0);

        assert!(ingress.try_push(IngressCommand::Enable));
        reader.read(&mut planar, frames, 1);
        assert!(ingress.metrics.enabled.load(Ordering::Acquire));
        assert_eq!(ingress.metrics.generation.load(Ordering::Relaxed), 1);
        assert_eq!(ingress.metrics.target_frames.load(Ordering::Relaxed), 1_440);

        assert!(ingress.try_push(IngressCommand::Disable));
        assert!(ingress.metrics.retired.load(Ordering::Acquire));
        assert!(!ingress.try_push(IngressCommand::Enable));
        assert!(!ingress.try_push(IngressCommand::Gap {
            sequence: 1,
            start_sample: u64::from(MEDIA_UNIT_FRAMES),
            frame_count: u32::from(MEDIA_UNIT_FRAMES),
            reason: GapKind::KnownAdapterGap,
        }));
        reader.read(&mut planar, frames, 1);
        assert!(!ingress.metrics.enabled.load(Ordering::Acquire));
        assert_eq!(
            ingress.metrics.read_cursor.load(Ordering::Relaxed),
            u64::MAX
        );
    }

    #[test]
    fn subscription_withdrawal_retires_queued_media_before_render() {
        let clock = StdMonotonic::new();
        let (ingress, mut reader) =
            JitterIngress::pair(1, clock, TargetMode::Automatic).expect("pair");
        assert!(ingress.try_push(IngressCommand::Enable));
        let mut queued = ingress.acquire_samples();
        queued.fill(0.75);
        assert!(ingress.try_push(IngressCommand::Media {
            sequence: 0,
            start_sample: 0,
            interleaved: queued,
            recovered_after_nack: false,
        }));
        for _ in 1..INGRESS_CAPACITY {
            assert!(ingress.try_push(IngressCommand::SetTarget {
                mode: TargetMode::Automatic,
            }));
        }

        assert!(ingress.try_push(IngressCommand::Disable));
        assert!(ingress.metrics.retired.load(Ordering::Acquire));
        assert!(!ingress.metrics.enabled.load(Ordering::Acquire));
        assert!(!ingress.try_push(IngressCommand::Media {
            sequence: 1,
            start_sample: u64::from(MEDIA_UNIT_FRAMES),
            interleaved: ingress.acquire_samples(),
            recovered_after_nack: false,
        }));
        assert!(!ingress.try_push(IngressCommand::Gap {
            sequence: 1,
            start_sample: u64::from(MEDIA_UNIT_FRAMES),
            frame_count: u32::from(MEDIA_UNIT_FRAMES),
            reason: GapKind::KnownAdapterGap,
        }));
        assert!(!ingress.try_push(IngressCommand::EpochEnd {
            next_sequence: 1,
            final_sample: u64::from(MEDIA_UNIT_FRAMES),
        }));
        assert!(!ingress.try_push(IngressCommand::Enable));

        let frames = usize::from(MEDIA_UNIT_FRAMES);
        let mut planar = vec![1.0_f32; frames];
        let report = reader.read(&mut planar, frames, 1);
        assert_eq!(report.frames_written, frames);
        assert!(planar.iter().all(|sample| *sample == 0.0));
        assert_eq!(ingress.metrics.exact_frames.load(Ordering::Relaxed), 0);
        assert_eq!(
            ingress.metrics.read_cursor.load(Ordering::Relaxed),
            u64::MAX
        );
    }

    #[test]
    fn full_ingress_queue_stores_exact_overflow_gap() {
        let clock = StdMonotonic::new();
        let (ingress, _reader) =
            JitterIngress::pair(1, clock, TargetMode::Automatic).expect("pair");
        assert!(ingress.try_push(IngressCommand::Enable));
        for _ in 0..INGRESS_CAPACITY {
            assert!(
                ingress.try_push(IngressCommand::SetTarget {
                    mode: TargetMode::Automatic,
                }),
                "primary queue should fill completely"
            );
        }
        assert!(
            !ingress.try_push(IngressCommand::Gap {
                sequence: 73,
                start_sample: 91_337,
                frame_count: 1_440,
                reason: GapKind::KnownAdapterGap,
            }),
            "full primary queue must report rejection"
        );
        assert_eq!(
            ingress.overflow.try_take(),
            Some(OverflowCommand::Gap(OverflowGap {
                sequence: 73,
                start_sample: 91_337,
                frame_count: 1_440,
            }))
        );
        assert_eq!(ingress.metrics.ingress_dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn consecutive_media_overflow_merges_exact_range() {
        let clock = StdMonotonic::new();
        let (ingress, _reader) =
            JitterIngress::pair(1, clock, TargetMode::Automatic).expect("pair");
        assert!(ingress.try_push(IngressCommand::Enable));
        for _ in 0..INGRESS_CAPACITY {
            assert!(ingress.try_push(IngressCommand::SetTarget {
                mode: TargetMode::Automatic,
            }));
        }

        let unit_frames = u64::from(MEDIA_UNIT_FRAMES);
        for sequence in 32..36_u32 {
            let start_sample = u64::from(sequence) * unit_frames;
            assert!(
                !ingress.try_push(IngressCommand::Media {
                    sequence,
                    start_sample,
                    interleaved: ingress.acquire_samples(),
                    recovered_after_nack: false,
                }),
                "full primary queue must reject MEDIA {sequence}"
            );
            assert!(
                ingress.try_push(IngressCommand::Gap {
                    sequence,
                    start_sample,
                    frame_count: u32::from(MEDIA_UNIT_FRAMES),
                    reason: GapKind::SendQueueOverflow,
                }),
                "the caller's duplicate GAP must use the stored range"
            );
        }

        assert_eq!(
            ingress.overflow.try_take(),
            Some(OverflowCommand::Gap(OverflowGap {
                sequence: 32,
                start_sample: 32 * unit_frames,
                frame_count: 4 * u32::from(MEDIA_UNIT_FRAMES),
            }))
        );
        assert_eq!(ingress.overflow.try_take(), None);
        assert_eq!(ingress.metrics.ingress_dropped.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn overflow_queue_preserves_ranges_during_contention() {
        const GAP_COUNT: u32 = 240;
        const FIRST_SEQUENCE: u32 = 1_000;
        const FIRST_SAMPLE: u64 = 480_000;

        let queue = Arc::new(OverflowQueue::new());
        let barrier = Arc::new(Barrier::new(2));
        let producer_queue = Arc::clone(&queue);
        let producer_barrier = Arc::clone(&barrier);
        let producer = thread::spawn(move || {
            producer_barrier.wait();
            for offset in 0..GAP_COUNT {
                assert!(
                    producer_queue.try_push(OverflowCommand::Gap(OverflowGap {
                        sequence: FIRST_SEQUENCE + offset,
                        start_sample: FIRST_SAMPLE
                            + u64::from(offset) * u64::from(MEDIA_UNIT_FRAMES),
                        frame_count: u32::from(MEDIA_UNIT_FRAMES),
                    })),
                    "continuous overflow range {offset} must remain representable"
                );
                if offset % 2 == 0 {
                    thread::yield_now();
                }
            }
        });

        barrier.wait();
        let expected_frames = GAP_COUNT * u32::from(MEDIA_UNIT_FRAMES);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut consumed_frames = 0_u32;
        while consumed_frames < expected_frames {
            match queue.try_take() {
                Some(OverflowCommand::Gap(gap)) => {
                    let expected_start = FIRST_SAMPLE + u64::from(consumed_frames);
                    assert_eq!(gap.start_sample, expected_start);
                    assert_eq!(
                        gap.sequence,
                        FIRST_SEQUENCE + consumed_frames / u32::from(MEDIA_UNIT_FRAMES)
                    );
                    assert_eq!(gap.frame_count % u32::from(MEDIA_UNIT_FRAMES), 0);
                    consumed_frames += gap.frame_count;
                }
                Some(other) => panic!("unexpected overflow command: {other:?}"),
                None if Instant::now() < deadline => thread::yield_now(),
                None => panic!("timed out while consuming overflow ranges"),
            }
        }
        producer.join().expect("producer thread");
        assert_eq!(consumed_frames, expected_frames);
        assert_eq!(queue.try_take(), None);
    }

    #[test]
    fn disconnected_reader_rejects_without_recording_overflow() {
        let clock = StdMonotonic::new();
        let (ingress, reader) = JitterIngress::pair(1, clock, TargetMode::Automatic).expect("pair");
        assert!(ingress.try_push(IngressCommand::Enable));
        drop(reader);

        assert!(
            !ingress.try_push(IngressCommand::Media {
                sequence: 9,
                start_sample: 4_320,
                interleaved: ingress.acquire_samples(),
                recovered_after_nack: false,
            }),
            "a disconnected primary consumer must reject immediately"
        );
        assert_eq!(ingress.overflow.try_take(), None);
        assert_eq!(ingress.metrics.ingress_dropped.load(Ordering::Relaxed), 1);
    }
}
