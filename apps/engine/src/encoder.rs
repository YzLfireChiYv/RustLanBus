//! CaptureBlock → RLB2 packetizer worker.
//!
//! Each local source epoch owns one encoder thread. The thread waits on the
//! capture ready queue with a timeout so it can observe stop, then encodes a
//! complete 480-frame unit with [`MediaPacketizer::packetize`]. Known capture
//! gaps become explicit GAP records. Outbound records travel through a bounded
//! `sync_channel` so the coordinator can insert MEDIA into resend history
//! before the first UDP submit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rlb_audio::{AudioError, CaptureBlock, CaptureBlockConsumer, CaptureDiscontinuity};
use rlb_clock::{MonotonicClock, MonotonicInstant, StdMonotonic};
use rlb_codec::WireCodec;
use rlb_media::{
    DatagramSet, EpochEndReason, FIXED_HEADER_BYTES, GapReason, GapRecord, MediaIdentity,
    MediaPacketizer, OutboundRecord, PacketizeError, SENDER_DEADLINE_NS, SenderMetrics,
    encode_gap_into,
};
use rlb_model::{MEDIA_UNIT_FRAMES, SourceFormat};

use crate::error::EngineError;

const ENCODER_WAIT: Duration = Duration::from_millis(20);
const MAX_GAP_FRAMES: u32 = 48_000;
const OUTBOUND_CAPACITY: usize = 16;
const OUTBOUND_RETRY_WAIT: Duration = Duration::from_millis(1);
const OUTBOUND_MAX_WAIT: Duration = Duration::from_nanos(SENDER_DEADLINE_NS);
const PENDING_GAP_CAPACITY: usize = OUTBOUND_CAPACITY;

/// Counters published by one source encoder for the coordinator thread.
#[derive(Debug)]
pub struct EncoderShared {
    /// Cumulative packetizer snapshot.
    pub metrics: Mutex<SenderMetrics>,
    /// MEDIA units encoded since spawn.
    pub encoded_units: AtomicU64,
    /// GAP records encoded since spawn.
    pub gap_records: AtomicU64,
    /// Times the outbound queue was full and the encoder waited.
    pub outbound_blocked: AtomicU64,
    /// Records discarded only by abnormal Drop cleanup.
    pub outbound_dropped: AtomicU64,
    /// SourceEncoder values dropped without a successful normal stop.
    pub abnormal_drops: AtomicU64,
    additional_gap_frames: [AtomicU64; 4],
    additional_deadline_drops: AtomicU64,
}

impl EncoderShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            metrics: Mutex::new(SenderMetrics::default()),
            encoded_units: AtomicU64::new(0),
            gap_records: AtomicU64::new(0),
            outbound_blocked: AtomicU64::new(0),
            outbound_dropped: AtomicU64::new(0),
            abnormal_drops: AtomicU64::new(0),
            additional_gap_frames: std::array::from_fn(|_| AtomicU64::new(0)),
            additional_deadline_drops: AtomicU64::new(0),
        })
    }
}

/// Terminal report after a normal encoder stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceEncoderReport {
    /// MEDIA units encoded.
    pub encoded_units: u64,
    /// GAP records encoded.
    pub gap_records: u64,
    /// Last packetizer snapshot.
    pub metrics: SenderMetrics,
}

struct EncoderThreadExit {
    report: SourceEncoderReport,
    residual: Vec<OutboundRecord>,
}

/// One source-epoch packetizer running off the WASAPI callback thread.
pub struct SourceEncoder {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<EncoderThreadExit, EngineError>>>,
    completion: Option<Result<EncoderThreadExit, EngineError>>,
    normal_stop_complete: bool,
    shared: Arc<EncoderShared>,
    clock: StdMonotonic,
    outbound: Receiver<QueuedOutbound>,
}

impl SourceEncoder {
    /// Takes the capture consumer and starts packetizing on a helper thread.
    pub fn spawn(
        identity: MediaIdentity,
        format: SourceFormat,
        consumer: CaptureBlockConsumer,
        thread_name: &'static str,
    ) -> Result<Self, EngineError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let shared = EncoderShared::new();
        let shared_for_thread = Arc::clone(&shared);
        let clock = StdMonotonic::new();
        let clock_for_thread = clock.clone();
        let (outbound_tx, outbound_rx) = sync_channel(OUTBOUND_CAPACITY);
        let join = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                encoder_thread(
                    identity,
                    format,
                    consumer,
                    clock_for_thread,
                    stop_for_thread,
                    shared_for_thread,
                    outbound_tx,
                )
            })
            .map_err(|error| EngineError::ThreadSpawn(error.to_string()))?;
        Ok(Self {
            stop,
            join: Some(join),
            completion: None,
            normal_stop_complete: false,
            shared,
            clock,
            outbound: outbound_rx,
        })
    }

    /// Takes every currently queued outbound record without blocking.
    pub fn drain_outbound(&mut self) -> Vec<OutboundRecord> {
        self.drain_outbound_inner(false)
    }

    fn drain_outbound_inner(&mut self, stopping: bool) -> Vec<OutboundRecord> {
        self.collect_finished();
        let mut records = Vec::new();
        let now = self.clock.now();
        while let Ok(queued) = self.outbound.try_recv() {
            let record = materialize_queued(queued, now, stopping, &self.shared);
            append_coalesced(&mut records, record);
        }
        if let Some(Ok(exit)) = self.completion.as_mut() {
            for record in std::mem::take(&mut exit.residual) {
                append_coalesced(&mut records, record);
            }
        }
        records
    }

    fn collect_finished(&mut self) {
        if self.completion.is_none() && self.join.as_ref().is_some_and(JoinHandle::is_finished) {
            self.completion = Some(take_join(&mut self.join));
        }
    }

    /// Latest packetizer snapshot for [`rlb_session::CoordinatorEvent::LocalSenderMetrics`].
    #[must_use]
    pub fn metrics_snapshot(&self) -> SenderMetrics {
        let metrics = self
            .shared
            .metrics
            .lock()
            .map(|guard| *guard)
            .unwrap_or_default();
        decorate_sender_metrics(metrics, &self.shared)
    }

    /// MEDIA units encoded since spawn.
    #[must_use]
    pub fn encoded_units(&self) -> u64 {
        self.shared.encoded_units.load(Ordering::Relaxed)
    }

    /// GAP records encoded since spawn.
    #[must_use]
    pub fn gap_records(&self) -> u64 {
        self.shared.gap_records.load(Ordering::Relaxed)
    }

    /// Asks the encoder to emit EPOCH_END and join.
    pub fn stop(mut self) -> Result<(SourceEncoderReport, Vec<OutboundRecord>), EngineError> {
        self.stop.store(true, Ordering::Relaxed);
        let mut leftover = self.drain_outbound_inner(true);
        if self.completion.is_none() {
            self.completion = Some(take_join(&mut self.join));
        }
        leftover.extend(self.drain_outbound_inner(true));
        let mut exit = self
            .completion
            .take()
            .unwrap_or(Err(EngineError::ThreadPanicked))?;
        leftover.extend(std::mem::take(&mut exit.residual));
        let mut report = exit.report;
        report.gap_records = self.shared.gap_records.load(Ordering::Relaxed);
        report.metrics = decorate_sender_metrics(report.metrics, &self.shared);
        self.normal_stop_complete = true;
        Ok((report, leftover))
    }
}

impl Drop for SourceEncoder {
    fn drop(&mut self) {
        if self.normal_stop_complete {
            return;
        }
        // Exceptional fallback: normal ownership paths must call stop() and
        // receive the complete materialized queue plus worker residual.
        self.shared.abnormal_drops.fetch_add(1, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        let mut dropped = 0_u64;
        while self.outbound.try_recv().is_ok() {
            dropped = dropped.saturating_add(1);
        }
        if self.completion.is_none() && self.join.is_some() {
            self.completion = Some(take_join(&mut self.join));
        }
        while self.outbound.try_recv().is_ok() {
            dropped = dropped.saturating_add(1);
        }
        if let Some(Ok(exit)) = self.completion.as_mut() {
            dropped = dropped.saturating_add(exit.residual.len() as u64);
            exit.residual.clear();
        }
        self.shared
            .outbound_dropped
            .fetch_add(dropped, Ordering::Relaxed);
    }
}

fn encoder_thread(
    identity: MediaIdentity,
    format: SourceFormat,
    mut consumer: CaptureBlockConsumer,
    clock: StdMonotonic,
    stop: Arc<AtomicBool>,
    shared: Arc<EncoderShared>,
    outbound: SyncSender<QueuedOutbound>,
) -> Result<EncoderThreadExit, EngineError> {
    let codec = WireCodec::from(format.profile());
    let mut packetizer =
        MediaPacketizer::new_nanosecond_domain(identity, codec, format.channels())?;
    let mut interleaved =
        vec![0.0_f32; usize::from(MEDIA_UNIT_FRAMES) * usize::from(format.channels())];
    let mut last_end = 0_u64;
    let mut delivery = OutboundDelivery::new();
    let mut delivery_saturated = false;
    while !stop.load(Ordering::Relaxed) && !delivery_saturated {
        match consumer.recv_timeout(ENCODER_WAIT) {
            Ok(Some(block)) => {
                match encode_capture_block(&mut packetizer, &block, &mut interleaved, clock.now()) {
                    Ok((report, records)) => {
                        last_end = report.end_sample;
                        shared
                            .encoded_units
                            .fetch_add(report.encoded_units, Ordering::Relaxed);
                        shared
                            .gap_records
                            .fetch_add(report.gap_records, Ordering::Relaxed);
                        let mut records = records.into_iter();
                        while let Some(record) = records.next() {
                            if send_outbound(
                                &mut delivery,
                                &outbound,
                                &stop,
                                &shared,
                                record,
                                report.send_deadline,
                                &clock,
                            )? {
                                continue;
                            }
                            let reason = media_gap_reason(
                                DeferralCause::QueueTimeout,
                                report.send_deadline,
                                clock.now(),
                            );
                            for remaining in records {
                                delivery.capture_residual(remaining, reason, &shared)?;
                            }
                            delivery_saturated = true;
                            break;
                        }
                        if let Ok(mut metrics) = shared.metrics.lock() {
                            *metrics = packetizer.metrics();
                        }
                    }
                    Err(error) => {
                        let _ = consumer.release(block);
                        return Err(error);
                    }
                }
                let _ = consumer.release(block);
                if delivery_saturated {
                    break;
                }
            }
            Ok(None) => {
                flush_outbound(
                    &mut delivery,
                    &outbound,
                    &stop,
                    &shared,
                    deadline_from_now(clock.now()),
                    &clock,
                )?;
            }
            Err(AudioError::BackendUnavailable) => break,
            Err(error) => return Err(error.into()),
        }
    }
    if !delivery_saturated {
        flush_outbound(
            &mut delivery,
            &outbound,
            &stop,
            &shared,
            deadline_from_now(clock.now()),
            &clock,
        )?;
    }
    if last_end > 0 {
        if let Ok(record) = packetizer.epoch_end(last_end, EpochEndReason::SourceStopped) {
            if delivery_saturated {
                delivery.capture_residual(record, GapReason::SendQueueOverflow, &shared)?;
            } else {
                let _ = send_outbound(
                    &mut delivery,
                    &outbound,
                    &stop,
                    &shared,
                    record,
                    deadline_from_now(clock.now()),
                    &clock,
                )?;
            }
        }
    }
    let residual = delivery.into_residual()?;
    let metrics = packetizer.metrics();
    if let Ok(mut shared_metrics) = shared.metrics.lock() {
        *shared_metrics = metrics;
    }
    Ok(EncoderThreadExit {
        report: SourceEncoderReport {
            encoded_units: shared.encoded_units.load(Ordering::Relaxed),
            gap_records: shared.gap_records.load(Ordering::Relaxed),
            metrics,
        },
        residual,
    })
}

pub(crate) struct BlockEncodeReport {
    encoded_units: u64,
    gap_records: u64,
    end_sample: u64,
    send_deadline: MonotonicInstant,
}

/// Encodes one reblocked unit and any declared gap that precedes it.
pub(crate) fn encode_capture_block(
    packetizer: &mut MediaPacketizer,
    block: &CaptureBlock,
    interleaved: &mut [f32],
    now: MonotonicInstant,
) -> Result<(BlockEncodeReport, Vec<OutboundRecord>), EngineError> {
    let metadata = block
        .metadata()
        .ok_or(AudioError::BackendContractViolation)?;
    if metadata.frames != u32::from(MEDIA_UNIT_FRAMES)
        || metadata.channels != packetizer_channels(interleaved)
    {
        return Err(AudioError::BackendContractViolation.into());
    }
    let mut records = Vec::new();
    if let CaptureDiscontinuity::KnownGap { missing_frames } = metadata.discontinuity
        && missing_frames > 0
        && metadata.start_sample >= missing_frames
    {
        emit_gaps(
            packetizer,
            metadata.start_sample - missing_frames,
            missing_frames,
            &mut records,
        )?;
    }
    planar_to_interleaved(block, interleaved)?;
    // `capture_time` is the compatibility projection. This encoder still passes
    // those ticks into the packetizer and adds `SENDER_DEADLINE_NS` as its
    // sender-deadline offset. WASAPI QPC values are 100 ns ticks with another
    // origin, so this formula is not an acquisition age. `CaptureTiming` is the
    // successor observation and is intentionally unused on this legacy path.
    let record = packetizer.packetize_at_deadline(
        metadata.capture_time,
        now,
        metadata.start_sample,
        interleaved,
    )?;
    records.push(record);
    Ok((
        BlockEncodeReport {
            encoded_units: 1,
            gap_records: records
                .iter()
                .filter(|record| matches!(record, OutboundRecord::Gap { .. }))
                .count() as u64,
            end_sample: metadata
                .start_sample
                .saturating_add(u64::from(metadata.frames)),
            send_deadline: MonotonicInstant::from_ticks(
                metadata
                    .capture_time
                    .ticks()
                    .saturating_add(SENDER_DEADLINE_NS),
            ),
        },
        records,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeferralCause {
    QueueTimeout,
    SenderDeadline,
    Stopped,
}

#[derive(Debug)]
struct QueuedOutbound {
    record: OutboundRecord,
    sender_deadline: MonotonicInstant,
}

#[derive(Debug)]
struct UnsentRecord {
    record: QueuedOutbound,
    cause: Option<DeferralCause>,
}

struct DeliveryContext<'a> {
    outbound: &'a SyncSender<QueuedOutbound>,
    stop: &'a AtomicBool,
    shared: &'a EncoderShared,
    clock: &'a dyn MonotonicClock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlushStatus {
    Ready,
    Blocked(DeferralCause),
    Disconnected,
}

struct OutboundDelivery {
    pending_gaps: VecDeque<GapRecord>,
    residual: Vec<OutboundRecord>,
    saturated: bool,
}

impl OutboundDelivery {
    fn new() -> Self {
        Self {
            pending_gaps: VecDeque::with_capacity(PENDING_GAP_CAPACITY),
            residual: Vec::new(),
            saturated: false,
        }
    }

    fn send_with_wait<F>(
        &mut self,
        context: &DeliveryContext<'_>,
        record: OutboundRecord,
        sender_deadline: MonotonicInstant,
        mut wait: F,
    ) -> Result<bool, EngineError>
    where
        F: FnMut(Duration),
    {
        if self.saturated {
            let reason = media_gap_reason(
                DeferralCause::QueueTimeout,
                sender_deadline,
                context.clock.now(),
            );
            self.capture_residual(record, reason, context.shared)?;
            return Ok(false);
        }
        match self.flush_pending_with_wait(context, sender_deadline, &mut wait)? {
            FlushStatus::Ready => {}
            FlushStatus::Blocked(cause) => {
                let reason = media_gap_reason(cause, sender_deadline, context.clock.now());
                return self.defer_record(record, reason, context.shared);
            }
            FlushStatus::Disconnected => {
                let reason = GapReason::SendQueueOverflow;
                self.saturate_record(record, reason, context.shared)?;
                return Ok(false);
            }
        }
        match try_send_bounded(
            context,
            QueuedOutbound {
                record,
                sender_deadline,
            },
            &mut wait,
        ) {
            Ok(()) => Ok(true),
            Err(unsent) => match *unsent {
                UnsentRecord {
                    record:
                        QueuedOutbound {
                            record,
                            sender_deadline,
                        },
                    cause: Some(cause),
                } => {
                    let reason = media_gap_reason(cause, sender_deadline, context.clock.now());
                    self.defer_record(record, reason, context.shared)
                }
                UnsentRecord {
                    cause: None,
                    record: QueuedOutbound { record, .. },
                } => {
                    self.saturate_record(record, GapReason::SendQueueOverflow, context.shared)?;
                    Ok(false)
                }
            },
        }
    }

    fn flush_with_wait<F>(
        &mut self,
        context: &DeliveryContext<'_>,
        sender_deadline: MonotonicInstant,
        mut wait: F,
    ) -> Result<(), EngineError>
    where
        F: FnMut(Duration),
    {
        let _ = self.flush_pending_with_wait(context, sender_deadline, &mut wait)?;
        Ok(())
    }

    fn flush_pending_with_wait<F>(
        &mut self,
        context: &DeliveryContext<'_>,
        sender_deadline: MonotonicInstant,
        wait: &mut F,
    ) -> Result<FlushStatus, EngineError>
    where
        F: FnMut(Duration),
    {
        while let Some(gap) = self.pending_gaps.pop_front() {
            let outbound_gap = match encode_pending_gap(gap) {
                Ok(record) => record,
                Err(error) => {
                    self.pending_gaps.push_front(gap);
                    return Err(error);
                }
            };
            match try_send_bounded(
                context,
                QueuedOutbound {
                    record: outbound_gap,
                    sender_deadline,
                },
                wait,
            ) {
                Ok(()) => {}
                Err(unsent) => match *unsent {
                    UnsentRecord {
                        record:
                            QueuedOutbound {
                                record: OutboundRecord::Gap { record, .. },
                                ..
                            },
                        cause: Some(cause),
                    } => {
                        self.pending_gaps.push_front(record);
                        return Ok(FlushStatus::Blocked(cause));
                    }
                    UnsentRecord {
                        cause: Some(_),
                        record: _,
                    } => unreachable!("pending outbound record must remain a GAP"),
                    UnsentRecord {
                        cause: None,
                        record:
                            QueuedOutbound {
                                record: OutboundRecord::Gap { record, .. },
                                ..
                            },
                    } => {
                        self.pending_gaps.push_front(record);
                        return Ok(FlushStatus::Disconnected);
                    }
                    UnsentRecord {
                        cause: None,
                        record: _,
                    } => unreachable!("pending outbound record must remain a GAP"),
                },
            }
        }
        Ok(FlushStatus::Ready)
    }

    fn defer_record(
        &mut self,
        record: OutboundRecord,
        media_reason: GapReason,
        shared: &EncoderShared,
    ) -> Result<bool, EngineError> {
        match record {
            OutboundRecord::Media(media) => {
                let gap = gap_for_media(&media, media_reason);
                note_media_gap(shared, media_reason);
                match self.defer_gap(gap) {
                    Ok(()) => Ok(true),
                    Err(residual) => {
                        self.saturate_outbound(encode_pending_gap(residual)?)?;
                        Ok(false)
                    }
                }
            }
            OutboundRecord::Gap { record, .. } => match self.defer_gap(record) {
                Ok(()) => Ok(true),
                Err(residual) => {
                    self.saturate_outbound(encode_pending_gap(residual)?)?;
                    Ok(false)
                }
            },
            record @ OutboundRecord::EpochEnd { .. } => {
                self.saturate_outbound(record)?;
                Ok(false)
            }
        }
    }

    fn defer_gap(&mut self, mut gap: GapRecord) -> Result<(), GapRecord> {
        if let Some(previous) = self.pending_gaps.back_mut()
            && previous.identity == gap.identity
            && previous.reason == gap.reason
            && previous
                .start_sample
                .checked_add(u64::from(previous.frame_count))
                == Some(gap.start_sample)
            && previous.frame_count < MAX_GAP_FRAMES
        {
            let merged_frames = gap.frame_count.min(MAX_GAP_FRAMES - previous.frame_count);
            previous.frame_count += merged_frames;
            gap.start_sample = gap.start_sample.saturating_add(u64::from(merged_frames));
            gap.frame_count -= merged_frames;
            if gap.frame_count == 0 {
                return Ok(());
            }
        }
        if self.pending_gaps.len() < PENDING_GAP_CAPACITY {
            self.pending_gaps.push_back(gap);
            Ok(())
        } else {
            Err(gap)
        }
    }

    fn capture_residual(
        &mut self,
        record: OutboundRecord,
        media_reason: GapReason,
        shared: &EncoderShared,
    ) -> Result<(), EngineError> {
        let record = match record {
            OutboundRecord::Media(media) => {
                let gap = gap_for_media(&media, media_reason);
                note_media_gap(shared, media_reason);
                encode_pending_gap(gap)?
            }
            record => record,
        };
        self.residual.push(record);
        Ok(())
    }

    fn saturate_record(
        &mut self,
        record: OutboundRecord,
        media_reason: GapReason,
        shared: &EncoderShared,
    ) -> Result<(), EngineError> {
        self.spill_pending()?;
        self.capture_residual(record, media_reason, shared)?;
        self.saturated = true;
        Ok(())
    }

    fn saturate_outbound(&mut self, record: OutboundRecord) -> Result<(), EngineError> {
        self.spill_pending()?;
        self.residual.push(record);
        self.saturated = true;
        Ok(())
    }

    fn spill_pending(&mut self) -> Result<(), EngineError> {
        while let Some(gap) = self.pending_gaps.pop_front() {
            self.residual.push(encode_pending_gap(gap)?);
        }
        Ok(())
    }

    fn into_residual(mut self) -> Result<Vec<OutboundRecord>, EngineError> {
        self.spill_pending()?;
        Ok(self.residual)
    }
}

/// Delivers one record within both its sender deadline and a local wait bound.
///
/// A timed-out MEDIA record becomes an equal-duration GAP with the same record
/// sequence. GAP records use a bounded, coalescing pending queue.
fn send_outbound(
    delivery: &mut OutboundDelivery,
    outbound: &SyncSender<QueuedOutbound>,
    stop: &AtomicBool,
    shared: &EncoderShared,
    record: OutboundRecord,
    sender_deadline: MonotonicInstant,
    clock: &dyn MonotonicClock,
) -> Result<bool, EngineError> {
    let context = DeliveryContext {
        outbound,
        stop,
        shared,
        clock,
    };
    delivery.send_with_wait(&context, record, sender_deadline, thread::sleep)
}

fn flush_outbound(
    delivery: &mut OutboundDelivery,
    outbound: &SyncSender<QueuedOutbound>,
    stop: &AtomicBool,
    shared: &EncoderShared,
    sender_deadline: MonotonicInstant,
    clock: &dyn MonotonicClock,
) -> Result<(), EngineError> {
    let context = DeliveryContext {
        outbound,
        stop,
        shared,
        clock,
    };
    delivery.flush_with_wait(&context, sender_deadline, thread::sleep)
}

fn try_send_bounded<F>(
    context: &DeliveryContext<'_>,
    mut record: QueuedOutbound,
    wait: &mut F,
) -> Result<(), Box<UnsentRecord>>
where
    F: FnMut(Duration),
{
    let wait_started = Instant::now();
    loop {
        match context.outbound.try_send(record) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(returned)) => {
                return Err(Box::new(UnsentRecord {
                    record: returned,
                    cause: None,
                }));
            }
            Err(TrySendError::Full(returned)) => {
                record = returned;
                context
                    .shared
                    .outbound_blocked
                    .fetch_add(1, Ordering::Relaxed);
                let now = context.clock.now();
                let cause = if context.stop.load(Ordering::Relaxed) {
                    Some(DeferralCause::Stopped)
                } else if now >= record.sender_deadline {
                    Some(DeferralCause::SenderDeadline)
                } else if wait_started.elapsed() >= OUTBOUND_MAX_WAIT {
                    Some(DeferralCause::QueueTimeout)
                } else {
                    None
                };
                if let Some(cause) = cause {
                    return Err(Box::new(UnsentRecord {
                        record,
                        cause: Some(cause),
                    }));
                }
                let deadline_remaining = Duration::from_nanos(
                    record.sender_deadline.ticks().saturating_sub(now.ticks()),
                );
                let wait_duration = OUTBOUND_RETRY_WAIT
                    .min(deadline_remaining)
                    .min(OUTBOUND_MAX_WAIT.saturating_sub(wait_started.elapsed()));
                if !wait_duration.is_zero() {
                    wait(wait_duration);
                }
            }
        }
    }
}

fn materialize_queued(
    queued: QueuedOutbound,
    now: MonotonicInstant,
    stopping: bool,
    shared: &EncoderShared,
) -> OutboundRecord {
    if !stopping && now < queued.sender_deadline {
        return queued.record;
    }
    match queued.record {
        OutboundRecord::Media(media) => {
            let reason = if now >= queued.sender_deadline {
                GapReason::SendDeadlineDrop
            } else {
                GapReason::SendQueueOverflow
            };
            let gap = gap_for_media(&media, reason);
            note_media_gap(shared, reason);
            encode_pending_gap(gap).expect("MEDIA-derived GAP is wire-valid")
        }
        record => record,
    }
}

fn append_coalesced(records: &mut Vec<OutboundRecord>, record: OutboundRecord) {
    let merged = match (records.last(), &record) {
        (
            Some(OutboundRecord::Gap {
                record: previous, ..
            }),
            OutboundRecord::Gap {
                record: current, ..
            },
        ) if previous.identity == current.identity
            && previous.reason == current.reason
            && previous
                .start_sample
                .checked_add(u64::from(previous.frame_count))
                == Some(current.start_sample) =>
        {
            previous
                .frame_count
                .checked_add(current.frame_count)
                .filter(|frames| *frames <= MAX_GAP_FRAMES)
                .map(|frame_count| GapRecord {
                    frame_count,
                    ..*previous
                })
        }
        _ => None,
    };
    if let Some(merged) = merged
        && let Ok(encoded) = encode_pending_gap(merged)
        && let Some(previous) = records.last_mut()
    {
        *previous = encoded;
        return;
    }
    records.push(record);
}

fn gap_for_media(media: &DatagramSet, reason: GapReason) -> GapRecord {
    GapRecord {
        identity: media.identity(),
        record_sequence: media.unit_sequence(),
        start_sample: media.start_sample(),
        frame_count: u32::from(MEDIA_UNIT_FRAMES),
        reason,
    }
}

fn note_media_gap(shared: &EncoderShared, reason: GapReason) {
    shared.gap_records.fetch_add(1, Ordering::Relaxed);
    shared.additional_gap_frames[gap_reason_index(reason)]
        .fetch_add(u64::from(MEDIA_UNIT_FRAMES), Ordering::Relaxed);
    if reason == GapReason::SendDeadlineDrop {
        shared
            .additional_deadline_drops
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn decorate_sender_metrics(mut metrics: SenderMetrics, shared: &EncoderShared) -> SenderMetrics {
    for (published, additional) in metrics
        .gap_frames_total_by_reason
        .iter_mut()
        .zip(&shared.additional_gap_frames)
    {
        *published = published.saturating_add(additional.load(Ordering::Relaxed));
    }
    metrics.send_deadline_dropped_units_total = metrics
        .send_deadline_dropped_units_total
        .saturating_add(shared.additional_deadline_drops.load(Ordering::Relaxed));
    metrics
}

fn media_gap_reason(
    cause: DeferralCause,
    sender_deadline: MonotonicInstant,
    now: MonotonicInstant,
) -> GapReason {
    if cause == DeferralCause::SenderDeadline || now >= sender_deadline {
        GapReason::SendDeadlineDrop
    } else {
        GapReason::SendQueueOverflow
    }
}

fn encode_pending_gap(record: GapRecord) -> Result<OutboundRecord, EngineError> {
    let mut datagram = vec![0; FIXED_HEADER_BYTES];
    encode_gap_into(record, &mut datagram)
        .map_err(PacketizeError::Wire)
        .map_err(EngineError::from)?;
    Ok(OutboundRecord::Gap {
        record,
        datagram: datagram.into_boxed_slice(),
    })
}

const fn gap_reason_index(reason: GapReason) -> usize {
    match reason {
        GapReason::CaptureOverrun => 0,
        GapReason::SendQueueOverflow => 1,
        GapReason::SendDeadlineDrop => 2,
        GapReason::KnownAdapterGap => 3,
    }
}

fn deadline_from_now(now: MonotonicInstant) -> MonotonicInstant {
    MonotonicInstant::from_ticks(now.ticks().saturating_add(SENDER_DEADLINE_NS))
}

fn packetizer_channels(interleaved: &[f32]) -> u8 {
    (interleaved.len() / usize::from(MEDIA_UNIT_FRAMES)) as u8
}

fn emit_gaps(
    packetizer: &mut MediaPacketizer,
    mut start_sample: u64,
    mut remaining: u64,
    records: &mut Vec<OutboundRecord>,
) -> Result<(), EngineError> {
    while remaining > 0 {
        let chunk = remaining.min(u64::from(MAX_GAP_FRAMES));
        records.push(packetizer.gap(
            start_sample,
            u32::try_from(chunk).expect("bounded GAP chunk"),
            GapReason::KnownAdapterGap,
        )?);
        start_sample = start_sample.saturating_add(chunk);
        remaining -= chunk;
    }
    Ok(())
}

fn planar_to_interleaved(block: &CaptureBlock, interleaved: &mut [f32]) -> Result<(), EngineError> {
    let metadata = block
        .metadata()
        .ok_or(AudioError::BackendContractViolation)?;
    let frames = usize::from(MEDIA_UNIT_FRAMES);
    let channels = usize::from(metadata.channels);
    if interleaved.len() != frames * channels {
        return Err(AudioError::BackendContractViolation.into());
    }
    for channel in 0..metadata.channels {
        let plane = block
            .channel(channel)
            .ok_or(AudioError::BackendContractViolation)?;
        if plane.len() != frames {
            return Err(AudioError::BackendContractViolation.into());
        }
        for (frame, sample) in plane.iter().enumerate() {
            interleaved[frame * channels + usize::from(channel)] = *sample;
        }
    }
    Ok(())
}

fn take_join(
    join: &mut Option<JoinHandle<Result<EncoderThreadExit, EngineError>>>,
) -> Result<EncoderThreadExit, EngineError> {
    match join.take() {
        Some(join) => match join.join() {
            Ok(result) => result,
            Err(_) => Err(EngineError::ThreadPanicked),
        },
        None => Err(EngineError::ThreadPanicked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::num::NonZeroU64;
    use rlb_audio::{
        CaptureBlockPool, CaptureClockDomain, CaptureInput, CaptureMetadata, CaptureReblocker,
        CaptureTiming, PlanarSamples,
    };
    use rlb_clock::MonotonicInstant;
    use rlb_model::{MemberId, QualityProfile, RoomId, RunId, SourceId, StreamEpoch};

    fn identity() -> MediaIdentity {
        MediaIdentity {
            room_id: RoomId::from_bytes([1; 16]).expect("room"),
            sender_member_id: MemberId::from_bytes([2; 16]).expect("member"),
            sender_run_id: RunId::from_bytes([3; 16]).expect("run"),
            source_id: SourceId::from_bytes([4; 16]).expect("source"),
            stream_epoch: StreamEpoch::from_bytes([5; 16]).expect("epoch"),
        }
    }

    fn metadata(
        start_sample: u64,
        discontinuity: CaptureDiscontinuity,
        channels: u8,
    ) -> CaptureMetadata {
        CaptureMetadata {
            source_id: identity().source_id,
            stream_epoch: identity().stream_epoch,
            start_sample,
            frames: u32::from(MEDIA_UNIT_FRAMES),
            sample_rate_hz: 48_000,
            channels,
            clock_domain: CaptureClockDomain::new(
                NonZeroU64::new(1).expect("domain"),
                NonZeroU64::new(10_000_000).expect("qpc"),
            ),
            timing: CaptureTiming::absent(),
            capture_time: MonotonicInstant::from_ticks(0),
            device_position: None,
            discontinuity,
            silent: false,
        }
    }

    fn queued(record: OutboundRecord) -> QueuedOutbound {
        QueuedOutbound {
            record,
            sender_deadline: MonotonicInstant::from_ticks(u64::MAX),
        }
    }

    #[test]
    fn continuous_unit_becomes_one_media_datagram_set() {
        let format =
            SourceFormat::for_source(rlb_model::SourceKind::SystemAudio, QualityProfile::PcmF32V1);
        let mut packetizer = MediaPacketizer::new_nanosecond_domain(
            identity(),
            WireCodec::from(format.profile()),
            format.channels(),
        )
        .expect("packetizer");
        let mut pool = CaptureBlockPool::new(1, usize::from(MEDIA_UNIT_FRAMES), 2).expect("pool");
        let mut block = pool.try_acquire().expect("block");
        for channel in 0..2 {
            block
                .channel_capacity_mut(channel)
                .expect("plane")
                .fill(0.1 * f32::from(channel + 1));
        }
        block
            .set_metadata(metadata(0, CaptureDiscontinuity::Continuous, 2))
            .expect("metadata");
        let mut interleaved = vec![0.0; usize::from(MEDIA_UNIT_FRAMES) * 2];
        let (report, records) = encode_capture_block(
            &mut packetizer,
            &block,
            &mut interleaved,
            MonotonicInstant::from_ticks(0),
        )
        .expect("encode");
        assert_eq!(report.encoded_units, 1);
        assert_eq!(report.gap_records, 0);
        assert_eq!(records.len(), 1);
        assert_eq!(packetizer.metrics().encoded_units_total, 1);
        pool.release(block).expect("release");
    }

    #[test]
    fn known_gap_emits_gap_then_media() {
        let format =
            SourceFormat::for_source(rlb_model::SourceKind::Microphone, QualityProfile::PcmF32V1);
        let mut packetizer = MediaPacketizer::new_nanosecond_domain(
            identity(),
            WireCodec::from(format.profile()),
            1,
        )
        .expect("packetizer");
        let mut pool = CaptureBlockPool::new(1, usize::from(MEDIA_UNIT_FRAMES), 1).expect("pool");
        let mut block = pool.try_acquire().expect("block");
        block.channel_capacity_mut(0).expect("plane").fill(0.0);
        block
            .set_metadata(metadata(
                u64::from(MEDIA_UNIT_FRAMES),
                CaptureDiscontinuity::KnownGap {
                    missing_frames: u64::from(MEDIA_UNIT_FRAMES),
                },
                1,
            ))
            .expect("metadata");
        let mut interleaved = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let (report, records) = encode_capture_block(
            &mut packetizer,
            &block,
            &mut interleaved,
            MonotonicInstant::from_ticks(0),
        )
        .expect("encode");
        assert_eq!(report.encoded_units, 1);
        assert_eq!(report.gap_records, 1);
        assert_eq!(records.len(), 2);
        assert_eq!(
            packetizer.metrics().gap_frames_total_by_reason[3],
            u64::from(MEDIA_UNIT_FRAMES)
        );
        pool.release(block).expect("release");
    }

    #[test]
    fn reblocker_output_encodes_without_deadline_drop() {
        use rlb_audio::AudioMetrics;
        let identity = identity();
        let metrics = std::sync::Arc::new(AudioMetrics::new());
        let mut reblocker = CaptureReblocker::new(
            identity.source_id,
            identity.stream_epoch,
            1,
            CaptureClockDomain::new(
                NonZeroU64::new(1).expect("domain"),
                NonZeroU64::new(10_000_000).expect("qpc"),
            ),
            metrics,
        )
        .expect("reblocker");
        let pool = CaptureBlockPool::new(2, usize::from(MEDIA_UNIT_FRAMES), 1).expect("pool");
        let (mut producer, mut consumer) = pool.into_spsc(2).expect("spsc");
        let samples = vec![0.5_f32; usize::from(MEDIA_UNIT_FRAMES)];
        let planar = PlanarSamples::new(
            &samples,
            usize::from(MEDIA_UNIT_FRAMES),
            1,
            usize::from(MEDIA_UNIT_FRAMES),
        )
        .expect("planar");
        let input = CaptureInput::new(metadata(0, CaptureDiscontinuity::Continuous, 1), planar)
            .expect("input");
        reblocker
            .process_spsc(input, &mut producer)
            .expect("reblock");
        let block = consumer.try_pop().expect("unit");
        let mut packetizer = MediaPacketizer::new_nanosecond_domain(identity, WireCodec::F32Le, 1)
            .expect("packetizer");
        let mut interleaved = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        encode_capture_block(
            &mut packetizer,
            &block,
            &mut interleaved,
            MonotonicInstant::from_ticks(0),
        )
        .expect("encode");
        assert_eq!(packetizer.metrics().send_deadline_dropped_units_total, 0);
        consumer.release(block).expect("release");
    }

    #[test]
    fn full_queue_at_sender_deadline_converts_media_to_equal_gap() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("filler"));
        let blocked = OutboundRecord::Media(packetizer.packetize(480, &samples).expect("blocked"));
        let (outbound, receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(false);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let deadline = clock.now();
        let mut delivery = OutboundDelivery::new();
        delivery
            .send_with_wait(&context, blocked, deadline, |_| {
                panic!("an expired deadline must not wait")
            })
            .expect("defer media");
        assert_eq!(delivery.pending_gaps.len(), 1);
        let pending = *delivery.pending_gaps.front().expect("pending gap");
        assert_eq!(pending.record_sequence, 1);
        assert_eq!(pending.start_sample, 480);
        assert_eq!(pending.frame_count, u32::from(MEDIA_UNIT_FRAMES));
        assert_eq!(pending.reason, GapReason::SendDeadlineDrop);
        assert_eq!(
            shared
                .metrics
                .lock()
                .expect("metrics")
                .gap_frames_total_by_reason,
            [0; 4]
        );
        let decorated = decorate_sender_metrics(SenderMetrics::default(), &shared);
        assert_eq!(
            decorated.gap_frames_total_by_reason[2],
            u64::from(MEDIA_UNIT_FRAMES)
        );
        assert_eq!(decorated.send_deadline_dropped_units_total, 1);

        let _ = receiver.try_recv().expect("remove filler");
        delivery
            .flush_with_wait(&context, deadline_from_now(clock.now()), |_| {
                panic!("an empty queue must not wait")
            })
            .expect("flush gap");
        let OutboundRecord::Gap { record, .. } = receiver.try_recv().expect("explicit gap").record
        else {
            panic!("expected explicit GAP");
        };
        assert_eq!(record, pending);
    }

    #[test]
    fn continuously_full_queue_stops_at_local_wait_bound() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("filler"));
        let blocked = OutboundRecord::Media(packetizer.packetize(480, &samples).expect("blocked"));
        let (outbound, _receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(false);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let distant_deadline =
            MonotonicInstant::from_ticks(clock.now().ticks().saturating_add(1_000_000_000));
        let mut delivery = OutboundDelivery::new();
        delivery
            .send_with_wait(&context, blocked, distant_deadline, thread::sleep)
            .expect("bounded deferral");
        let pending = delivery.pending_gaps.front().expect("overflow gap");
        assert_eq!(pending.record_sequence, 1);
        assert_eq!(pending.start_sample, 480);
        assert_eq!(pending.frame_count, u32::from(MEDIA_UNIT_FRAMES));
        assert_eq!(pending.reason, GapReason::SendQueueOverflow);
        assert!(shared.outbound_blocked.load(Ordering::Relaxed) > 1);
        let metrics = decorate_sender_metrics(SenderMetrics::default(), &shared);
        assert_eq!(
            metrics.gap_frames_total_by_reason[1],
            u64::from(MEDIA_UNIT_FRAMES)
        );
        assert_eq!(metrics.send_deadline_dropped_units_total, 0);
    }

    #[test]
    fn short_queue_recovery_keeps_media() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("filler"));
        let recovering =
            OutboundRecord::Media(packetizer.packetize(480, &samples).expect("recovering"));
        let (outbound, receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(false);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let mut delivery = OutboundDelivery::new();
        let mut released = false;
        delivery
            .send_with_wait(&context, recovering, deadline_from_now(clock.now()), |_| {
                if !released {
                    let _ = receiver.try_recv().expect("coordinator drain");
                    released = true;
                }
            })
            .expect("deliver media");
        assert!(released);
        assert!(delivery.pending_gaps.is_empty());
        let OutboundRecord::Media(media) = receiver.try_recv().expect("recovered media").record
        else {
            panic!("short recovery must preserve MEDIA");
        };
        assert_eq!(media.unit_sequence(), 1);
        assert_eq!(media.start_sample(), 480);
        assert_eq!(shared.gap_records.load(Ordering::Relaxed), 0);
        assert!(shared.outbound_blocked.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn consecutive_pending_gaps_merge_with_first_sequence() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("filler"));
        let first_gap = packetizer
            .gap(480, 480, GapReason::SendQueueOverflow)
            .expect("first gap");
        let second_gap = packetizer
            .gap(960, 480, GapReason::SendQueueOverflow)
            .expect("second gap");
        let (outbound, receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(false);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let mut delivery = OutboundDelivery::new();
        let deadline = clock.now();
        for gap in [first_gap, second_gap] {
            delivery
                .send_with_wait(&context, gap, deadline, |_| {
                    panic!("an expired deadline must not wait")
                })
                .expect("defer gap");
        }
        assert_eq!(delivery.pending_gaps.len(), 1);
        let pending = *delivery.pending_gaps.front().expect("merged gap");
        assert_eq!(pending.record_sequence, 1);
        assert_eq!(pending.start_sample, 480);
        assert_eq!(pending.frame_count, 960);
        assert_eq!(pending.reason, GapReason::SendQueueOverflow);

        let _ = receiver.try_recv().expect("remove filler");
        delivery
            .flush_with_wait(&context, deadline_from_now(clock.now()), |_| {
                panic!("an empty queue must not wait")
            })
            .expect("flush merged gap");
        let OutboundRecord::Gap { record, .. } = receiver.try_recv().expect("merged GAP").record
        else {
            panic!("expected merged GAP");
        };
        assert_eq!(record, pending);
    }

    #[test]
    fn coordinator_drain_converts_and_merges_expired_media() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let shared = EncoderShared::new();
        let deadline = MonotonicInstant::from_ticks(10);
        let now = MonotonicInstant::from_ticks(10);
        let mut records = Vec::new();
        for start_sample in [0, 480] {
            let queued = QueuedOutbound {
                record: OutboundRecord::Media(
                    packetizer.packetize(start_sample, &samples).expect("media"),
                ),
                sender_deadline: deadline,
            };
            let record = materialize_queued(queued, now, false, &shared);
            append_coalesced(&mut records, record);
        }
        assert_eq!(records.len(), 1);
        let OutboundRecord::Gap { record, .. } = &records[0] else {
            panic!("expected merged deadline GAP");
        };
        assert_eq!(record.record_sequence, 0);
        assert_eq!(record.start_sample, 0);
        assert_eq!(record.frame_count, 960);
        assert_eq!(record.reason, GapReason::SendDeadlineDrop);
        assert_eq!(shared.gap_records.load(Ordering::Relaxed), 2);
        let metrics = decorate_sender_metrics(SenderMetrics::default(), &shared);
        assert_eq!(metrics.gap_frames_total_by_reason[2], 960);
        assert_eq!(metrics.send_deadline_dropped_units_total, 2);
    }

    #[test]
    fn stop_with_full_queue_returns_with_preserved_gap() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("filler"));
        let stopping =
            OutboundRecord::Media(packetizer.packetize(480, &samples).expect("stopping"));
        let (outbound, receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(true);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let mut delivery = OutboundDelivery::new();
        delivery
            .send_with_wait(&context, stopping, deadline_from_now(clock.now()), |_| {
                panic!("stop must cancel queue waiting")
            })
            .expect("stop deferral");
        let pending = *delivery.pending_gaps.front().expect("shutdown gap");
        assert_eq!(pending.record_sequence, 1);
        assert_eq!(pending.start_sample, 480);
        assert_eq!(pending.frame_count, u32::from(MEDIA_UNIT_FRAMES));
        assert_eq!(pending.reason, GapReason::SendQueueOverflow);

        let _ = receiver.try_recv().expect("stop pre-drain");
        delivery
            .flush_with_wait(&context, deadline_from_now(clock.now()), |_| {
                panic!("pre-drained shutdown queue must not wait")
            })
            .expect("flush shutdown gap");
        let OutboundRecord::Gap { record, .. } = receiver.try_recv().expect("shutdown GAP").record
        else {
            panic!("expected shutdown GAP");
        };
        assert_eq!(record, pending);
        assert!(delivery.pending_gaps.is_empty());
    }

    #[test]
    fn nonadjacent_gaps_beyond_pending_capacity_become_residual() {
        let mut filler_packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("filler packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler =
            OutboundRecord::Media(filler_packetizer.packetize(0, &samples).expect("filler"));
        let (outbound, _receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(false);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let mut delivery = OutboundDelivery::new();
        let deadline = clock.now();
        for index in 0..=PENDING_GAP_CAPACITY {
            let gap = packetizer
                .gap(
                    (index as u64).saturating_mul(960),
                    u32::from(MEDIA_UNIT_FRAMES),
                    GapReason::SendQueueOverflow,
                )
                .expect("nonadjacent gap");
            let accepted = delivery
                .send_with_wait(&context, gap, deadline, |_| {
                    panic!("an expired deadline must not wait")
                })
                .expect("bounded delivery");
            assert_eq!(accepted, index < PENDING_GAP_CAPACITY);
        }
        assert!(delivery.saturated);
        assert_eq!(shared.outbound_dropped.load(Ordering::Relaxed), 0);
        let residual = delivery.into_residual().expect("residual");
        assert_eq!(residual.len(), PENDING_GAP_CAPACITY + 1);
        for (index, record) in residual.iter().enumerate() {
            let OutboundRecord::Gap { record, .. } = record else {
                panic!("expected residual GAP");
            };
            assert_eq!(record.record_sequence, index as u32);
            assert_eq!(record.start_sample, (index as u64).saturating_mul(960));
        }
    }

    #[test]
    fn pending_gap_partial_merge_returns_only_unrepresented_tail() {
        let mut delivery = OutboundDelivery::new();
        for index in 0..PENDING_GAP_CAPACITY - 1 {
            delivery.pending_gaps.push_back(GapRecord {
                identity: identity(),
                record_sequence: index as u32,
                start_sample: (index as u64).saturating_mul(96_000),
                frame_count: 480,
                reason: GapReason::SendQueueOverflow,
            });
        }
        let previous_start = 2_000_000;
        delivery.pending_gaps.push_back(GapRecord {
            identity: identity(),
            record_sequence: 100,
            start_sample: previous_start,
            frame_count: MAX_GAP_FRAMES - 240,
            reason: GapReason::SendQueueOverflow,
        });
        let incoming = GapRecord {
            identity: identity(),
            record_sequence: 101,
            start_sample: previous_start + u64::from(MAX_GAP_FRAMES - 240),
            frame_count: 480,
            reason: GapReason::SendQueueOverflow,
        };
        let residual = delivery.defer_gap(incoming).expect_err("bounded tail");
        let merged = delivery.pending_gaps.back().expect("full merged GAP");
        assert_eq!(merged.frame_count, MAX_GAP_FRAMES);
        assert_eq!(residual.record_sequence, 101);
        assert_eq!(
            residual.start_sample,
            previous_start + u64::from(MAX_GAP_FRAMES)
        );
        assert_eq!(residual.frame_count, 240);
    }

    #[test]
    fn epoch_end_backpressure_preserves_pending_gap_and_terminal_record() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let filler = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("filler"));
        let pending_gap = packetizer
            .gap(480, 480, GapReason::SendQueueOverflow)
            .expect("gap");
        let epoch_end = packetizer
            .epoch_end(960, EpochEndReason::SourceStopped)
            .expect("epoch end");
        let (outbound, _receiver) = sync_channel(1);
        outbound.try_send(queued(filler)).expect("fill queue");
        let shared = EncoderShared::new();
        let stop = AtomicBool::new(true);
        let clock = StdMonotonic::new();
        let context = DeliveryContext {
            outbound: &outbound,
            stop: &stop,
            shared: &shared,
            clock: &clock,
        };
        let mut delivery = OutboundDelivery::new();
        assert!(
            delivery
                .send_with_wait(&context, pending_gap, clock.now(), |_| {
                    panic!("stop must cancel queue waiting")
                })
                .expect("pending gap")
        );
        assert!(
            !delivery
                .send_with_wait(&context, epoch_end, clock.now(), |_| {
                    panic!("stop must cancel terminal waiting")
                })
                .expect("terminal residual")
        );
        let residual = delivery.into_residual().expect("residual");
        assert_eq!(residual.len(), 2);
        assert!(matches!(residual[0], OutboundRecord::Gap { .. }));
        assert!(matches!(residual[1], OutboundRecord::EpochEnd { .. }));
        assert_eq!(shared.outbound_dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn normal_stop_returns_materialized_queue_pending_gap_and_epoch_end() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let media = OutboundRecord::Media(packetizer.packetize(0, &samples).expect("media"));
        let pending_gap = packetizer
            .gap(480, 480, GapReason::SendQueueOverflow)
            .expect("gap");
        let epoch_end = packetizer
            .epoch_end(960, EpochEndReason::SourceStopped)
            .expect("epoch end");
        let report = SourceEncoderReport {
            encoded_units: 1,
            gap_records: 1,
            metrics: packetizer.metrics(),
        };
        let exit = EncoderThreadExit {
            report,
            residual: vec![pending_gap, epoch_end],
        };
        let (outbound, receiver) = sync_channel(1);
        outbound
            .try_send(QueuedOutbound {
                record: media,
                sender_deadline: MonotonicInstant::from_ticks(u64::MAX),
            })
            .expect("fill queue");
        drop(outbound);
        let shared = EncoderShared::new();
        shared.gap_records.store(1, Ordering::Relaxed);
        let shared_after_stop = Arc::clone(&shared);
        let encoder = SourceEncoder {
            stop: Arc::new(AtomicBool::new(false)),
            join: Some(thread::spawn(move || Ok(exit))),
            completion: None,
            normal_stop_complete: false,
            shared,
            clock: StdMonotonic::new(),
            outbound: receiver,
        };
        let (report, records) = encoder.stop().expect("normal stop");
        assert_eq!(records.len(), 3);
        let OutboundRecord::Gap { record, .. } = &records[0] else {
            panic!("queued MEDIA must become GAP");
        };
        assert_eq!(record.record_sequence, 0);
        assert_eq!(record.start_sample, 0);
        assert_eq!(record.frame_count, u32::from(MEDIA_UNIT_FRAMES));
        assert_eq!(record.reason, GapReason::SendQueueOverflow);
        assert!(matches!(records[1], OutboundRecord::Gap { .. }));
        assert!(matches!(records[2], OutboundRecord::EpochEnd { .. }));
        assert_eq!(report.gap_records, 2);
        assert_eq!(shared_after_stop.abnormal_drops.load(Ordering::Relaxed), 0);
        assert_eq!(
            shared_after_stop.outbound_dropped.load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn drop_records_abnormal_fallback_and_discarded_records() {
        let mut packetizer =
            MediaPacketizer::new_nanosecond_domain(identity(), WireCodec::F32Le, 1)
                .expect("packetizer");
        let samples = vec![0.0; usize::from(MEDIA_UNIT_FRAMES)];
        let queued_media =
            OutboundRecord::Media(packetizer.packetize(0, &samples).expect("queued media"));
        let residual_gap = packetizer
            .gap(480, 480, GapReason::SendQueueOverflow)
            .expect("residual gap");
        let exit = EncoderThreadExit {
            report: SourceEncoderReport {
                encoded_units: 1,
                gap_records: 1,
                metrics: packetizer.metrics(),
            },
            residual: vec![residual_gap],
        };
        let (outbound, receiver) = sync_channel(1);
        outbound.try_send(queued(queued_media)).expect("fill queue");
        drop(outbound);
        let shared = EncoderShared::new();
        let observed = Arc::clone(&shared);
        let encoder = SourceEncoder {
            stop: Arc::new(AtomicBool::new(false)),
            join: Some(thread::spawn(move || Ok(exit))),
            completion: None,
            normal_stop_complete: false,
            shared,
            clock: StdMonotonic::new(),
            outbound: receiver,
        };
        drop(encoder);
        assert_eq!(observed.abnormal_drops.load(Ordering::Relaxed), 1);
        assert_eq!(observed.outbound_dropped.load(Ordering::Relaxed), 2);
    }
}
