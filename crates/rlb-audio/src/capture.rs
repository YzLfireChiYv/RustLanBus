use core::num::NonZeroU64;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::time::Duration;

use rlb_clock::{MonotonicClock, MonotonicDuration, MonotonicInstant};
use rlb_model::{MEDIA_UNIT_FRAMES, SAMPLE_RATE_HZ, SourceId, StreamEpoch};

use crate::{AudioError, AudioMetrics};

const MEDIA_UNIT_FRAMES_USIZE: usize = MEDIA_UNIT_FRAMES as usize;

/// Identity and frequency of one local monotonic clock domain.
///
/// The numeric identity only correlates observations made by this process. It
/// carries no cross-host timing meaning.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CaptureClockDomain {
    id: NonZeroU64,
    ticks_per_second: NonZeroU64,
}

impl CaptureClockDomain {
    /// Creates an opaque local domain description.
    #[must_use]
    pub const fn new(id: NonZeroU64, ticks_per_second: NonZeroU64) -> Self {
        Self {
            id,
            ticks_per_second,
        }
    }

    /// Describes an existing monotonic clock using a caller-owned domain ID.
    #[must_use]
    pub fn from_clock(id: NonZeroU64, clock: &impl MonotonicClock) -> Self {
        Self::new(id, clock.ticks_per_second())
    }

    /// Returns the process-local opaque domain identity.
    #[must_use]
    pub const fn id(self) -> NonZeroU64 {
        self.id
    }

    /// Returns the clock frequency used to interpret captured instants.
    #[must_use]
    pub const fn ticks_per_second(self) -> NonZeroU64 {
        self.ticks_per_second
    }
}

/// Timeline discontinuity immediately before a capture block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CaptureDiscontinuity {
    /// The block begins exactly where the prior block ended.
    #[default]
    Continuous,
    /// A known number of source frames are absent before this block.
    KnownGap {
        /// Missing source-frame duration.
        missing_frames: u64,
    },
}

/// Metadata attached to one variable-sized capture callback block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureMetadata {
    /// Stable product source identity.
    pub source_id: SourceId,
    /// Current continuous format and sample-timeline identity.
    pub stream_epoch: StreamEpoch,
    /// Zero-based first source frame in this epoch.
    pub start_sample: u64,
    /// Number of source frames in this block.
    pub frames: u32,
    /// Engine sample rate. V2 first release accepts 48 kHz.
    pub sample_rate_hz: u32,
    /// One or two planar channels.
    pub channels: u8,
    /// Opaque local clock domain for `capture_time`.
    pub clock_domain: CaptureClockDomain,
    /// Callback observation in `clock_domain`.
    pub capture_time: MonotonicInstant,
    /// Optional device sample position used for diagnostics.
    pub device_position: Option<u64>,
    /// Timeline state immediately before `start_sample`.
    pub discontinuity: CaptureDiscontinuity,
    /// The backend reported valid digital silence for the complete block.
    pub silent: bool,
}

impl CaptureMetadata {
    /// Validates first-release format and timeline bounds.
    pub fn validate(self) -> Result<(), AudioError> {
        validate_channels(self.channels)?;
        if self.sample_rate_hz != SAMPLE_RATE_HZ {
            return Err(AudioError::UnsupportedSampleRate {
                sample_rate_hz: self.sample_rate_hz,
            });
        }
        if self.frames == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "capture_frames",
            });
        }
        self.start_sample
            .checked_add(u64::from(self.frames))
            .ok_or(AudioError::SampleRangeOverflow)?;
        if matches!(
            self.discontinuity,
            CaptureDiscontinuity::KnownGap { missing_frames: 0 }
        ) {
            return Err(AudioError::EmptyDiscontinuity);
        }
        Ok(())
    }

    /// Returns the exclusive end of this source-frame interval.
    pub fn end_sample(self) -> Result<u64, AudioError> {
        self.start_sample
            .checked_add(u64::from(self.frames))
            .ok_or(AudioError::SampleRangeOverflow)
    }
}

/// Borrowed planar f32 samples with a fixed channel stride.
#[derive(Clone, Copy, Debug)]
pub struct PlanarSamples<'a> {
    samples: &'a [f32],
    frames: usize,
    channels: u8,
    channel_stride: usize,
}

impl<'a> PlanarSamples<'a> {
    /// Validates and creates a planar view.
    pub fn new(
        samples: &'a [f32],
        frames: usize,
        channels: u8,
        channel_stride: usize,
    ) -> Result<Self, AudioError> {
        validate_channels(channels)?;
        if channel_stride < frames {
            return Err(AudioError::InvalidPlanarStride {
                stride: channel_stride,
                frames,
            });
        }
        let required = required_planar_samples(frames, channels, channel_stride)?;
        if samples.len() < required {
            return Err(AudioError::PlanarStorageTooShort {
                required,
                actual: samples.len(),
            });
        }
        Ok(Self {
            samples,
            frames,
            channels,
            channel_stride,
        })
    }

    /// Returns the frame count in every plane.
    #[must_use]
    pub const fn frames(self) -> usize {
        self.frames
    }

    /// Returns the number of planes.
    #[must_use]
    pub const fn channels(self) -> u8 {
        self.channels
    }

    /// Returns one channel plane.
    #[must_use]
    pub fn channel(self, channel: u8) -> Option<&'a [f32]> {
        if channel >= self.channels {
            return None;
        }
        let start = usize::from(channel) * self.channel_stride;
        let end = start + self.frames;
        self.samples.get(start..end)
    }
}

/// One capture callback's metadata and borrowed planar samples.
#[derive(Clone, Copy, Debug)]
pub struct CaptureInput<'a> {
    /// Validated callback metadata.
    pub metadata: CaptureMetadata,
    /// Validated planar samples.
    pub samples: PlanarSamples<'a>,
}

impl<'a> CaptureInput<'a> {
    /// Creates an input after checking metadata and view agreement.
    pub fn new(metadata: CaptureMetadata, samples: PlanarSamples<'a>) -> Result<Self, AudioError> {
        metadata.validate()?;
        let metadata_frames =
            usize::try_from(metadata.frames).map_err(|_| AudioError::CapacityOverflow {
                field: "capture_frames",
            })?;
        if metadata_frames != samples.frames() {
            return Err(AudioError::CaptureFrameCountMismatch);
        }
        if metadata.channels != samples.channels() {
            return Err(AudioError::UnsupportedChannels {
                channels: samples.channels(),
            });
        }
        Ok(Self { metadata, samples })
    }
}

/// A pool-owned planar block moved through a bounded capture queue.
#[derive(Debug)]
pub struct CaptureBlock {
    metadata: Option<CaptureMetadata>,
    samples: Box<[f32]>,
    frame_capacity: usize,
    channel_capacity: u8,
}

impl CaptureBlock {
    fn new(frame_capacity: usize, channel_capacity: u8) -> Result<Self, AudioError> {
        let sample_capacity = frame_capacity
            .checked_mul(usize::from(channel_capacity))
            .ok_or(AudioError::CapacityOverflow {
                field: "capture_block_samples",
            })?;
        Ok(Self {
            metadata: None,
            samples: vec![0.0; sample_capacity].into_boxed_slice(),
            frame_capacity,
            channel_capacity,
        })
    }

    /// Returns metadata after a producer has prepared this block.
    #[must_use]
    pub const fn metadata(&self) -> Option<CaptureMetadata> {
        self.metadata
    }

    /// Returns the preallocated frame capacity per channel.
    #[must_use]
    pub const fn frame_capacity(&self) -> usize {
        self.frame_capacity
    }

    /// Returns the number of preallocated channel planes.
    #[must_use]
    pub const fn channel_capacity(&self) -> u8 {
        self.channel_capacity
    }

    /// Returns the active samples for one channel.
    #[must_use]
    pub fn channel(&self, channel: u8) -> Option<&[f32]> {
        let metadata = self.metadata?;
        if channel >= metadata.channels {
            return None;
        }
        let frames = usize::try_from(metadata.frames).ok()?;
        let start = usize::from(channel) * self.frame_capacity;
        self.samples.get(start..start + frames)
    }

    /// Returns prepared storage for one channel.
    ///
    /// A backend or reblocker must only write frames later covered by metadata.
    pub fn channel_capacity_mut(&mut self, channel: u8) -> Option<&mut [f32]> {
        if channel >= self.channel_capacity {
            return None;
        }
        let start = usize::from(channel) * self.frame_capacity;
        self.samples.get_mut(start..start + self.frame_capacity)
    }

    /// Attaches validated metadata without changing storage capacity.
    pub fn set_metadata(&mut self, metadata: CaptureMetadata) -> Result<(), AudioError> {
        metadata.validate()?;
        let frames =
            usize::try_from(metadata.frames).map_err(|_| AudioError::CapacityOverflow {
                field: "capture_frames",
            })?;
        if frames > self.frame_capacity || metadata.channels > self.channel_capacity {
            return Err(AudioError::CaptureCapacityExceeded);
        }
        self.metadata = Some(metadata);
        Ok(())
    }

    fn clear_metadata(&mut self) {
        self.metadata = None;
    }
}

/// Constructor-allocated collection of reusable capture blocks.
#[derive(Debug)]
pub struct CaptureBlockPool {
    free: Vec<CaptureBlock>,
    block_count: usize,
    frame_capacity: usize,
    channel_capacity: u8,
}

impl CaptureBlockPool {
    /// Allocates every block and sample plane needed before stream start.
    pub fn new(
        block_count: usize,
        frame_capacity: usize,
        channel_capacity: u8,
    ) -> Result<Self, AudioError> {
        if block_count == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "capture_block_count",
            });
        }
        if frame_capacity == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "capture_frame_capacity",
            });
        }
        validate_channels(channel_capacity)?;

        let mut free = Vec::with_capacity(block_count);
        for _ in 0..block_count {
            free.push(CaptureBlock::new(frame_capacity, channel_capacity)?);
        }
        Ok(Self {
            free,
            block_count,
            frame_capacity,
            channel_capacity,
        })
    }

    /// Obtains a block immediately, returning `None` when the pool is empty.
    pub fn try_acquire(&mut self) -> Option<CaptureBlock> {
        self.free.pop()
    }

    /// Returns a compatible block without allocation.
    pub fn release(&mut self, mut block: CaptureBlock) -> Result<(), AudioError> {
        if block.frame_capacity != self.frame_capacity
            || block.channel_capacity != self.channel_capacity
        {
            return Err(AudioError::ForeignCaptureBlock);
        }
        if self.free.len() == self.block_count {
            return Err(AudioError::CapturePoolFull);
        }
        block.clear_metadata();
        self.free.push(block);
        Ok(())
    }

    /// Returns the fixed number of blocks owned by the pool.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.block_count
    }

    /// Returns the number of blocks currently available.
    #[must_use]
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Returns the fixed per-channel frame capacity.
    #[must_use]
    pub const fn frame_capacity(&self) -> usize {
        self.frame_capacity
    }

    /// Returns the fixed number of channel planes.
    #[must_use]
    pub const fn channel_capacity(&self) -> u8 {
        self.channel_capacity
    }

    /// Converts the prepared pool into cross-thread bounded SPSC handles.
    ///
    /// Only non-waiting `try_*` operations are exposed by the handles. The
    /// standard synchronous channel allocates its bounded slots here, before
    /// either realtime owner starts.
    pub fn into_spsc(
        self,
        ready_capacity: usize,
    ) -> Result<(CaptureBlockProducer, CaptureBlockConsumer), AudioError> {
        if ready_capacity == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "capture_ready_capacity",
            });
        }

        let (free_sender, free_receiver) = sync_channel(self.block_count);
        let (ready_sender, ready_receiver) = sync_channel(ready_capacity);
        for block in self.free {
            if free_sender.try_send(block).is_err() {
                return Err(AudioError::BackendContractViolation);
            }
        }
        let free_count = Arc::new(AtomicUsize::new(self.block_count));
        let ready_count = Arc::new(AtomicUsize::new(0));
        Ok((
            CaptureBlockProducer {
                free_receiver,
                ready_sender,
                recycled_block: None,
                free_count: Arc::clone(&free_count),
                ready_count: Arc::clone(&ready_count),
                block_count: self.block_count,
                ready_capacity,
                frame_capacity: self.frame_capacity,
                channel_capacity: self.channel_capacity,
            },
            CaptureBlockConsumer {
                ready_receiver,
                free_sender,
                free_count,
                ready_count,
                block_count: self.block_count,
                ready_capacity,
                frame_capacity: self.frame_capacity,
                channel_capacity: self.channel_capacity,
            },
        ))
    }

    #[cfg(test)]
    fn storage_capacity(&self) -> usize {
        self.free.capacity()
    }
}

/// Capture-callback half of a preallocated block pool and ready queue.
///
/// The type is deliberately non-`Clone`, preserving one logical producer.
pub struct CaptureBlockProducer {
    free_receiver: Receiver<CaptureBlock>,
    ready_sender: SyncSender<CaptureBlock>,
    recycled_block: Option<CaptureBlock>,
    free_count: Arc<AtomicUsize>,
    ready_count: Arc<AtomicUsize>,
    block_count: usize,
    ready_capacity: usize,
    frame_capacity: usize,
    channel_capacity: u8,
}

impl CaptureBlockProducer {
    /// Acquires a free block immediately.
    pub fn try_acquire(&mut self) -> Option<CaptureBlock> {
        if let Some(block) = self.recycled_block.take() {
            self.free_count.fetch_sub(1, Ordering::Relaxed);
            return Some(block);
        }
        match self.free_receiver.try_recv() {
            Ok(block) => {
                self.free_count.fetch_sub(1, Ordering::Relaxed);
                Some(block)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Publishes a complete block immediately.
    ///
    /// The large error deliberately returns the preallocated block by value;
    /// boxing it would allocate on the realtime path.
    #[allow(clippy::result_large_err)]
    pub fn try_submit(&mut self, block: CaptureBlock) -> Result<(), TryPushError<CaptureBlock>> {
        self.ready_count.fetch_add(1, Ordering::Relaxed);
        match self.ready_sender.try_send(block) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(block) | TrySendError::Disconnected(block)) => {
                self.ready_count.fetch_sub(1, Ordering::Relaxed);
                Err(TryPushError(block))
            }
        }
    }

    /// Returns a block after a failed ready-queue submission.
    fn recycle(&mut self, mut block: CaptureBlock) -> Result<(), AudioError> {
        if block.frame_capacity != self.frame_capacity
            || block.channel_capacity != self.channel_capacity
        {
            return Err(AudioError::ForeignCaptureBlock);
        }
        block.clear_metadata();
        if self.recycled_block.is_some() {
            return Err(AudioError::CapturePoolFull);
        }
        self.recycled_block = Some(block);
        self.free_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Returns an instantaneous free-block count.
    #[must_use]
    pub fn available_blocks(&self) -> usize {
        self.free_count.load(Ordering::Relaxed)
    }

    /// Returns an instantaneous ready-block count.
    #[must_use]
    pub fn ready_blocks(&self) -> usize {
        self.ready_count.load(Ordering::Relaxed)
    }

    /// Returns the fixed pool size.
    #[must_use]
    pub const fn block_capacity(&self) -> usize {
        self.block_count
    }

    /// Returns the fixed ready-queue bound.
    #[must_use]
    pub const fn ready_capacity(&self) -> usize {
        self.ready_capacity
    }
}

/// Encoder-worker half of a preallocated block pool and ready queue.
///
/// The type is deliberately non-`Clone`, preserving one logical consumer.
pub struct CaptureBlockConsumer {
    ready_receiver: Receiver<CaptureBlock>,
    free_sender: SyncSender<CaptureBlock>,
    free_count: Arc<AtomicUsize>,
    ready_count: Arc<AtomicUsize>,
    block_count: usize,
    ready_capacity: usize,
    frame_capacity: usize,
    channel_capacity: u8,
}

impl CaptureBlockConsumer {
    /// Receives a complete block immediately.
    pub fn try_pop(&mut self) -> Option<CaptureBlock> {
        match self.ready_receiver.try_recv() {
            Ok(block) => {
                self.ready_count.fetch_sub(1, Ordering::Relaxed);
                Some(block)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Waits up to `timeout` for a complete block on the encoder thread.
    ///
    /// A timeout returns `Ok(None)`. A disconnected producer returns
    /// [`AudioError::BackendUnavailable`] so the encoder can end the epoch.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<CaptureBlock>, AudioError> {
        match self.ready_receiver.recv_timeout(timeout) {
            Ok(block) => {
                self.ready_count.fetch_sub(1, Ordering::Relaxed);
                Ok(Some(block))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(AudioError::BackendUnavailable),
        }
    }

    /// Returns a consumed block to the callback-side free ring immediately.
    pub fn release(&mut self, mut block: CaptureBlock) -> Result<(), AudioError> {
        if block.frame_capacity != self.frame_capacity
            || block.channel_capacity != self.channel_capacity
        {
            return Err(AudioError::ForeignCaptureBlock);
        }
        block.clear_metadata();
        self.free_count.fetch_add(1, Ordering::Relaxed);
        match self.free_sender.try_send(block) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.free_count.fetch_sub(1, Ordering::Relaxed);
                Err(AudioError::CapturePoolFull)
            }
        }
    }

    /// Returns an instantaneous ready-block count.
    #[must_use]
    pub fn ready_blocks(&self) -> usize {
        self.ready_count.load(Ordering::Relaxed)
    }

    /// Returns an instantaneous free-block count.
    #[must_use]
    pub fn available_blocks(&self) -> usize {
        self.free_count.load(Ordering::Relaxed)
    }

    /// Returns the fixed pool size.
    #[must_use]
    pub const fn block_capacity(&self) -> usize {
        self.block_count
    }

    /// Returns the fixed ready-queue bound.
    #[must_use]
    pub const fn ready_capacity(&self) -> usize {
        self.ready_capacity
    }
}

/// Value returned when a bounded queue has no writable slot.
#[derive(Debug, Eq, PartialEq)]
pub struct TryPushError<T>(pub T);

/// Fixed-storage, FIFO queue with non-waiting SPSC operations.
///
/// The safe implementation keeps exclusive mutation in one owner. A
/// composition layer may transfer that owner between its one producer and one
/// consumer; `try_push` and `try_pop` themselves never wait or grow storage.
#[derive(Debug)]
pub struct BoundedSpsc<T> {
    slots: Box<[Option<T>]>,
    head: usize,
    tail: usize,
    len: usize,
}

impl<T> BoundedSpsc<T> {
    /// Allocates all queue slots.
    pub fn new(capacity: usize) -> Result<Self, AudioError> {
        if capacity == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "spsc_capacity",
            });
        }
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || None);
        Ok(Self {
            slots: slots.into_boxed_slice(),
            head: 0,
            tail: 0,
            len: 0,
        })
    }

    /// Pushes immediately or returns ownership when the ring is full.
    pub fn try_push(&mut self, value: T) -> Result<(), TryPushError<T>> {
        if self.len == self.slots.len() {
            return Err(TryPushError(value));
        }
        self.slots[self.tail] = Some(value);
        self.tail += 1;
        if self.tail == self.slots.len() {
            self.tail = 0;
        }
        self.len += 1;
        Ok(())
    }

    /// Pops immediately or returns `None` when the ring is empty.
    pub fn try_pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let value = self.slots[self.head].take();
        self.head += 1;
        if self.head == self.slots.len() {
            self.head = 0;
        }
        self.len -= 1;
        value
    }

    /// Returns the constructor-fixed slot count.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Returns the occupied slot count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Reports whether no value is ready.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reports whether every slot is occupied.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len == self.slots.len()
    }

    #[cfg(test)]
    fn storage_address(&self) -> usize {
        self.slots.as_ptr() as usize
    }
}

/// Per-callback outcome from a 480-frame reblocker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReblockReport {
    /// Input source frames consumed.
    pub input_frames: usize,
    /// Complete units published to the output queue.
    pub emitted_units: usize,
    /// Complete units discarded because a bounded resource was full.
    pub dropped_units: usize,
    /// Captured frames discarded before becoming complete units.
    pub dropped_partial_frames: usize,
    /// Missing source frames observed before this input.
    pub gap_frames: u64,
    /// Samples sanitized to positive zero.
    pub non_finite_samples: u64,
    /// Frames retained for the next callback.
    pub pending_frames: usize,
}

/// Per-source callback reblocker producing fixed 480-frame planar blocks.
#[derive(Debug)]
pub struct CaptureReblocker {
    source_id: SourceId,
    stream_epoch: StreamEpoch,
    channels: u8,
    clock_domain: CaptureClockDomain,
    scratch: Box<[f32]>,
    scratch_frames: usize,
    scratch_start_sample: u64,
    scratch_capture_time: MonotonicInstant,
    scratch_device_position: Option<u64>,
    scratch_silent: bool,
    expected_input_start: u64,
    pending_gap_frames: u64,
    metrics: Arc<AudioMetrics>,
}

trait CaptureBlockFlow {
    fn frame_capacity(&self) -> usize;
    fn channel_capacity(&self) -> u8;
    fn try_acquire(&mut self) -> Option<CaptureBlock>;
    #[allow(clippy::result_large_err)]
    fn try_submit(&mut self, block: CaptureBlock) -> Result<(), TryPushError<CaptureBlock>>;
    fn recycle(&mut self, block: CaptureBlock) -> Result<(), AudioError>;
}

struct LocalCaptureBlockFlow<'a> {
    pool: &'a mut CaptureBlockPool,
    output: &'a mut BoundedSpsc<CaptureBlock>,
}

impl CaptureBlockFlow for LocalCaptureBlockFlow<'_> {
    fn frame_capacity(&self) -> usize {
        self.pool.frame_capacity()
    }

    fn channel_capacity(&self) -> u8 {
        self.pool.channel_capacity()
    }

    fn try_acquire(&mut self) -> Option<CaptureBlock> {
        self.pool.try_acquire()
    }

    fn try_submit(&mut self, block: CaptureBlock) -> Result<(), TryPushError<CaptureBlock>> {
        self.output.try_push(block)
    }

    fn recycle(&mut self, block: CaptureBlock) -> Result<(), AudioError> {
        self.pool.release(block)
    }
}

impl CaptureBlockFlow for CaptureBlockProducer {
    fn frame_capacity(&self) -> usize {
        self.frame_capacity
    }

    fn channel_capacity(&self) -> u8 {
        self.channel_capacity
    }

    fn try_acquire(&mut self) -> Option<CaptureBlock> {
        self.try_acquire()
    }

    fn try_submit(&mut self, block: CaptureBlock) -> Result<(), TryPushError<CaptureBlock>> {
        self.try_submit(block)
    }

    fn recycle(&mut self, block: CaptureBlock) -> Result<(), AudioError> {
        self.recycle(block)
    }
}

impl CaptureReblocker {
    /// Preallocates one media-unit scratch block for a source and epoch.
    pub fn new(
        source_id: SourceId,
        stream_epoch: StreamEpoch,
        channels: u8,
        clock_domain: CaptureClockDomain,
        metrics: Arc<AudioMetrics>,
    ) -> Result<Self, AudioError> {
        validate_channels(channels)?;
        let samples = MEDIA_UNIT_FRAMES_USIZE
            .checked_mul(usize::from(channels))
            .ok_or(AudioError::CapacityOverflow {
                field: "reblock_scratch_samples",
            })?;
        Ok(Self {
            source_id,
            stream_epoch,
            channels,
            clock_domain,
            scratch: vec![0.0; samples].into_boxed_slice(),
            scratch_frames: 0,
            scratch_start_sample: 0,
            scratch_capture_time: MonotonicInstant::from_ticks(0),
            scratch_device_position: None,
            scratch_silent: true,
            expected_input_start: 0,
            pending_gap_frames: 0,
            metrics,
        })
    }

    /// Copies one callback into fixed storage and publishes complete units.
    ///
    /// This realtime entry point performs no allocation, waiting, logging, or
    /// device operations.
    pub fn process(
        &mut self,
        input: CaptureInput<'_>,
        pool: &mut CaptureBlockPool,
        output: &mut BoundedSpsc<CaptureBlock>,
    ) -> Result<ReblockReport, AudioError> {
        let mut flow = LocalCaptureBlockFlow { pool, output };
        self.process_with(input, &mut flow)
    }

    /// Copies one callback into a cross-thread preallocated pool and SPSC ring.
    ///
    /// The paired [`CaptureBlockConsumer`] can run on the source encoder
    /// worker. Both halves expose only immediate `try_*` operations.
    pub fn process_spsc(
        &mut self,
        input: CaptureInput<'_>,
        producer: &mut CaptureBlockProducer,
    ) -> Result<ReblockReport, AudioError> {
        self.process_with(input, producer)
    }

    fn process_with(
        &mut self,
        input: CaptureInput<'_>,
        flow: &mut impl CaptureBlockFlow,
    ) -> Result<ReblockReport, AudioError> {
        input.metadata.validate()?;
        if input.metadata.source_id != self.source_id {
            return Err(AudioError::SourceMismatch);
        }
        if input.metadata.stream_epoch != self.stream_epoch {
            return Err(AudioError::EpochMismatch);
        }
        if input.metadata.channels != self.channels || input.samples.channels() != self.channels {
            return Err(AudioError::UnsupportedChannels {
                channels: input.metadata.channels,
            });
        }
        if input.metadata.clock_domain != self.clock_domain {
            return Err(AudioError::BackendContractViolation);
        }
        if flow.frame_capacity() < MEDIA_UNIT_FRAMES_USIZE
            || flow.channel_capacity() < self.channels
        {
            return Err(AudioError::CaptureCapacityExceeded);
        }

        let input_frames =
            usize::try_from(input.metadata.frames).map_err(|_| AudioError::CapacityOverflow {
                field: "capture_frames",
            })?;
        if input_frames != input.samples.frames() {
            return Err(AudioError::CaptureFrameCountMismatch);
        }
        let input_end = input.metadata.end_sample()?;
        let observed_gap = self.validate_timeline(input.metadata)?;

        let mut report = ReblockReport {
            input_frames,
            gap_frames: observed_gap,
            ..ReblockReport::default()
        };
        self.metrics.add_capture_block(input_frames);

        if observed_gap > 0 {
            if self.scratch_frames > 0 {
                report.dropped_partial_frames = self.scratch_frames;
                self.metrics.add_reblock_dropped_frames(self.scratch_frames);
                self.pending_gap_frames = self
                    .pending_gap_frames
                    .saturating_add(self.scratch_frames as u64);
                self.scratch_frames = 0;
            }
            self.pending_gap_frames = self.pending_gap_frames.saturating_add(observed_gap);
        }
        self.expected_input_start = input_end;

        let mut input_offset = 0;
        while input_offset < input_frames {
            if self.scratch_frames == 0 {
                self.begin_scratch(input.metadata, input_offset);
            }
            let writable = MEDIA_UNIT_FRAMES_USIZE - self.scratch_frames;
            let chunk = writable.min(input_frames - input_offset);
            self.copy_sanitized(input.samples, input_offset, chunk, &mut report);
            self.scratch_silent &= input.metadata.silent;
            self.scratch_frames += chunk;
            input_offset += chunk;

            if self.scratch_frames == MEDIA_UNIT_FRAMES_USIZE {
                self.publish_complete(flow, &mut report)?;
            }
        }

        report.pending_frames = self.scratch_frames;
        self.metrics
            .add_capture_non_finite(report.non_finite_samples);
        Ok(report)
    }

    /// Discards a final incomplete unit and returns its captured frame count.
    ///
    /// A sender can turn the returned duration into its final explicit GAP.
    pub fn discard_partial(&mut self) -> usize {
        let frames = self.scratch_frames;
        if frames > 0 {
            self.metrics.add_reblock_dropped_frames(frames);
            self.pending_gap_frames = self.pending_gap_frames.saturating_add(frames as u64);
            self.scratch_frames = 0;
        }
        frames
    }

    /// Returns the number of source frames retained across callbacks.
    #[must_use]
    pub const fn pending_frames(&self) -> usize {
        self.scratch_frames
    }

    /// Returns the shared metrics bank.
    #[must_use]
    pub fn metrics(&self) -> Arc<AudioMetrics> {
        Arc::clone(&self.metrics)
    }

    fn validate_timeline(&self, metadata: CaptureMetadata) -> Result<u64, AudioError> {
        if metadata.start_sample < self.expected_input_start {
            return Err(AudioError::NonMonotonicCapture);
        }
        let observed_gap = metadata.start_sample - self.expected_input_start;
        match metadata.discontinuity {
            CaptureDiscontinuity::Continuous => Ok(observed_gap),
            CaptureDiscontinuity::KnownGap { missing_frames } if missing_frames == observed_gap => {
                Ok(observed_gap)
            }
            CaptureDiscontinuity::KnownGap { .. } => Err(AudioError::DiscontinuityMismatch),
        }
    }

    fn begin_scratch(&mut self, metadata: CaptureMetadata, input_offset: usize) {
        self.scratch_start_sample = metadata.start_sample.saturating_add(input_offset as u64);
        self.scratch_capture_time = offset_capture_time(
            metadata.capture_time,
            input_offset,
            metadata.clock_domain.ticks_per_second(),
        );
        self.scratch_device_position = metadata
            .device_position
            .and_then(|position| position.checked_add(input_offset as u64));
        self.scratch_silent = true;
    }

    fn copy_sanitized(
        &mut self,
        input: PlanarSamples<'_>,
        input_offset: usize,
        frames: usize,
        report: &mut ReblockReport,
    ) {
        for channel in 0..self.channels {
            let Some(source) = input.channel(channel) else {
                self.metrics.add_callback_contract_violation();
                return;
            };
            let destination_start =
                usize::from(channel) * MEDIA_UNIT_FRAMES_USIZE + self.scratch_frames;
            let source_start = input_offset;
            for frame in 0..frames {
                let sample = source[source_start + frame];
                if sample.is_finite() {
                    self.scratch[destination_start + frame] = sample;
                } else {
                    self.scratch[destination_start + frame] = 0.0;
                    report.non_finite_samples += 1;
                }
            }
        }
    }

    fn publish_complete(
        &mut self,
        flow: &mut impl CaptureBlockFlow,
        report: &mut ReblockReport,
    ) -> Result<(), AudioError> {
        let Some(mut block) = flow.try_acquire() else {
            report.dropped_units += 1;
            self.metrics.add_pool_exhaustion();
            self.metrics
                .add_reblock_dropped_frames(MEDIA_UNIT_FRAMES_USIZE);
            self.pending_gap_frames = self
                .pending_gap_frames
                .saturating_add(MEDIA_UNIT_FRAMES_USIZE as u64);
            self.scratch_frames = 0;
            return Ok(());
        };

        let discontinuity = if self.pending_gap_frames == 0 {
            CaptureDiscontinuity::Continuous
        } else {
            CaptureDiscontinuity::KnownGap {
                missing_frames: self.pending_gap_frames,
            }
        };
        if let Err(error) = self.prepare_complete_block(&mut block, discontinuity) {
            self.metrics.add_callback_contract_violation();
            self.scratch_frames = 0;
            return match flow.recycle(block) {
                Ok(()) => Err(error),
                Err(recycle_error) => Err(recycle_error),
            };
        }

        match flow.try_submit(block) {
            Ok(()) => {
                report.emitted_units += 1;
                self.metrics.add_reblock_unit();
                self.pending_gap_frames = 0;
            }
            Err(TryPushError(block)) => {
                flow.recycle(block)?;
                report.dropped_units += 1;
                self.metrics.add_queue_overflow();
                self.metrics
                    .add_reblock_dropped_frames(MEDIA_UNIT_FRAMES_USIZE);
                self.pending_gap_frames = self
                    .pending_gap_frames
                    .saturating_add(MEDIA_UNIT_FRAMES_USIZE as u64);
            }
        }
        self.scratch_frames = 0;
        Ok(())
    }

    fn prepare_complete_block(
        &self,
        block: &mut CaptureBlock,
        discontinuity: CaptureDiscontinuity,
    ) -> Result<(), AudioError> {
        for channel in 0..self.channels {
            let source_start = usize::from(channel) * MEDIA_UNIT_FRAMES_USIZE;
            let source = &self.scratch[source_start..source_start + MEDIA_UNIT_FRAMES_USIZE];
            let destination = block
                .channel_capacity_mut(channel)
                .ok_or(AudioError::CaptureCapacityExceeded)?;
            destination[..MEDIA_UNIT_FRAMES_USIZE].copy_from_slice(source);
        }
        block.set_metadata(CaptureMetadata {
            source_id: self.source_id,
            stream_epoch: self.stream_epoch,
            start_sample: self.scratch_start_sample,
            frames: u32::from(MEDIA_UNIT_FRAMES),
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: self.channels,
            clock_domain: self.clock_domain,
            capture_time: self.scratch_capture_time,
            device_position: self.scratch_device_position,
            discontinuity,
            silent: self.scratch_silent,
        })
    }

    #[cfg(test)]
    fn scratch_address(&self) -> usize {
        self.scratch.as_ptr() as usize
    }
}

fn validate_channels(channels: u8) -> Result<(), AudioError> {
    if matches!(channels, 1 | 2) {
        Ok(())
    } else {
        Err(AudioError::UnsupportedChannels { channels })
    }
}

fn required_planar_samples(
    frames: usize,
    channels: u8,
    channel_stride: usize,
) -> Result<usize, AudioError> {
    usize::from(channels - 1)
        .checked_mul(channel_stride)
        .and_then(|prefix| prefix.checked_add(frames))
        .ok_or(AudioError::CapacityOverflow {
            field: "planar_samples",
        })
}

fn offset_capture_time(
    capture_time: MonotonicInstant,
    frame_offset: usize,
    ticks_per_second: NonZeroU64,
) -> MonotonicInstant {
    let ticks = (frame_offset as u128).saturating_mul(u128::from(ticks_per_second.get()))
        / u128::from(SAMPLE_RATE_HZ);
    let Ok(ticks) = u64::try_from(ticks) else {
        return capture_time;
    };
    match capture_time.checked_add(MonotonicDuration::from_ticks(ticks)) {
        Some(adjusted) => adjusted,
        None => capture_time,
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU64;
    use std::sync::Arc;

    use rlb_clock::MonotonicInstant;
    use rlb_model::{SourceId, StreamEpoch};

    use super::*;

    fn source_id() -> SourceId {
        SourceId::from_bytes([1; 16]).expect("nonzero source")
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_bytes([2; 16]).expect("nonzero epoch")
    }

    fn domain() -> CaptureClockDomain {
        CaptureClockDomain::new(
            NonZeroU64::new(1).expect("nonzero"),
            NonZeroU64::new(1_000_000_000).expect("nonzero"),
        )
    }

    fn metadata(start_sample: u64, frames: u32) -> CaptureMetadata {
        CaptureMetadata {
            source_id: source_id(),
            stream_epoch: epoch(),
            start_sample,
            frames,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: 2,
            clock_domain: domain(),
            capture_time: MonotonicInstant::from_ticks(10),
            device_position: Some(start_sample),
            discontinuity: CaptureDiscontinuity::Continuous,
            silent: false,
        }
    }

    #[test]
    fn bounded_queue_preserves_fifo_and_fixed_storage() {
        let mut queue = BoundedSpsc::new(2).expect("queue");
        let address = queue.storage_address();
        assert_eq!(queue.try_push(10), Ok(()));
        assert_eq!(queue.try_push(20), Ok(()));
        assert!(matches!(queue.try_push(30), Err(TryPushError(30))));
        assert_eq!(queue.try_pop(), Some(10));
        assert_eq!(queue.try_push(30), Ok(()));
        assert_eq!(queue.try_pop(), Some(20));
        assert_eq!(queue.try_pop(), Some(30));
        assert_eq!(queue.storage_address(), address);
        assert_eq!(queue.capacity(), 2);
    }

    #[test]
    fn split_pool_moves_blocks_between_one_producer_and_consumer() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut reblocker =
            CaptureReblocker::new(source_id(), epoch(), 1, domain(), metrics).expect("reblocker");
        let pool = CaptureBlockPool::new(2, MEDIA_UNIT_FRAMES_USIZE, 1).expect("pool");
        let (mut producer, consumer) = pool.into_spsc(2).expect("split pool");
        let samples = vec![0.25; MEDIA_UNIT_FRAMES_USIZE];
        let view = PlanarSamples::new(
            &samples,
            MEDIA_UNIT_FRAMES_USIZE,
            1,
            MEDIA_UNIT_FRAMES_USIZE,
        )
        .expect("view");
        reblocker
            .process_spsc(
                CaptureInput::new(
                    CaptureMetadata {
                        channels: 1,
                        ..metadata(0, u32::from(MEDIA_UNIT_FRAMES))
                    },
                    view,
                )
                .expect("input"),
                &mut producer,
            )
            .expect("process");
        assert_eq!(producer.ready_blocks(), 1);

        let consumer = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let mut consumer = consumer;
                    let block = consumer.try_pop().expect("ready block");
                    assert_eq!(block.channel(0).expect("mono")[0], 0.25);
                    consumer.release(block).expect("release");
                    consumer
                })
                .join()
                .expect("consumer thread")
        });

        assert_eq!(consumer.ready_blocks(), 0);
        assert_eq!(producer.available_blocks(), 2);
        assert!(producer.try_acquire().is_some());
    }

    #[test]
    fn reblocker_emits_480_frames_without_capacity_change() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut reblocker =
            CaptureReblocker::new(source_id(), epoch(), 2, domain(), Arc::clone(&metrics))
                .expect("reblocker");
        let scratch_address = reblocker.scratch_address();
        let mut pool = CaptureBlockPool::new(2, MEDIA_UNIT_FRAMES_USIZE, 2).expect("pool");
        let pool_storage_capacity = pool.storage_capacity();
        let mut queue = BoundedSpsc::new(2).expect("queue");
        let queue_address = queue.storage_address();

        let first = vec![0.25; 128 * 2];
        let first_view = PlanarSamples::new(&first, 128, 2, 128).expect("view");
        let report = reblocker
            .process(
                CaptureInput::new(metadata(0, 128), first_view).expect("input"),
                &mut pool,
                &mut queue,
            )
            .expect("process");
        assert_eq!(report.pending_frames, 128);
        assert_eq!(report.emitted_units, 0);

        let mut second = vec![0.5; 352 * 2];
        second[17] = f32::NAN;
        let second_view = PlanarSamples::new(&second, 352, 2, 352).expect("view");
        let report = reblocker
            .process(
                CaptureInput::new(metadata(128, 352), second_view).expect("input"),
                &mut pool,
                &mut queue,
            )
            .expect("process");
        assert_eq!(report.emitted_units, 1);
        assert_eq!(report.non_finite_samples, 1);

        let block = queue.try_pop().expect("complete block");
        assert_eq!(block.metadata().expect("metadata").start_sample, 0);
        assert_eq!(block.channel(0).expect("left")[128 + 17], 0.0);
        pool.release(block).expect("release");

        assert_eq!(pool.capacity(), 2);
        assert_eq!(pool.storage_capacity(), pool_storage_capacity);
        assert_eq!(queue.storage_address(), queue_address);
        assert_eq!(reblocker.scratch_address(), scratch_address);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.capture_frames_total, 480);
        assert_eq!(snapshot.capture_non_finite_samples_total, 1);
        assert_eq!(snapshot.reblock_units_total, 1);
    }

    #[test]
    fn queue_overflow_preserves_source_time_and_marks_next_gap() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut reblocker =
            CaptureReblocker::new(source_id(), epoch(), 1, domain(), Arc::clone(&metrics))
                .expect("reblocker");
        let mut pool = CaptureBlockPool::new(3, MEDIA_UNIT_FRAMES_USIZE, 1).expect("pool");
        let mut queue = BoundedSpsc::new(1).expect("queue");
        let samples = vec![0.25; MEDIA_UNIT_FRAMES_USIZE * 3];

        for unit in 0..3 {
            let start = (unit * MEDIA_UNIT_FRAMES_USIZE) as u64;
            let slice_start = unit * MEDIA_UNIT_FRAMES_USIZE;
            let view = PlanarSamples::new(
                &samples[slice_start..slice_start + MEDIA_UNIT_FRAMES_USIZE],
                MEDIA_UNIT_FRAMES_USIZE,
                1,
                MEDIA_UNIT_FRAMES_USIZE,
            )
            .expect("view");
            let report = reblocker
                .process(
                    CaptureInput::new(
                        CaptureMetadata {
                            channels: 1,
                            ..metadata(start, u32::from(MEDIA_UNIT_FRAMES))
                        },
                        view,
                    )
                    .expect("input"),
                    &mut pool,
                    &mut queue,
                )
                .expect("process");
            if unit == 1 {
                assert_eq!(report.dropped_units, 1);
                let first = queue.try_pop().expect("first unit");
                pool.release(first).expect("release");
            }
        }

        let third = queue.try_pop().expect("third unit");
        assert_eq!(
            third.metadata().expect("metadata").discontinuity,
            CaptureDiscontinuity::KnownGap {
                missing_frames: u64::from(MEDIA_UNIT_FRAMES)
            }
        );
        pool.release(third).expect("release");
        assert_eq!(metrics.snapshot().capture_queue_overflows_total, 1);
    }

    #[test]
    fn known_gap_must_match_sample_index() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut reblocker =
            CaptureReblocker::new(source_id(), epoch(), 1, domain(), metrics).expect("reblocker");
        let mut pool = CaptureBlockPool::new(1, MEDIA_UNIT_FRAMES_USIZE, 1).expect("pool");
        let mut queue = BoundedSpsc::new(1).expect("queue");
        let samples = vec![0.0; 10];
        let view = PlanarSamples::new(&samples, 10, 1, 10).expect("view");
        let input = CaptureInput::new(
            CaptureMetadata {
                channels: 1,
                discontinuity: CaptureDiscontinuity::KnownGap { missing_frames: 4 },
                ..metadata(5, 10)
            },
            view,
        )
        .expect("input");
        assert_eq!(
            reblocker.process(input, &mut pool, &mut queue),
            Err(AudioError::DiscontinuityMismatch)
        );
    }
}
