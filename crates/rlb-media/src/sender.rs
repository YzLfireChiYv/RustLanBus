use rlb_clock::MonotonicInstant;
use rlb_codec::{CodecError, WireCodec, encode_interleaved_into, encoded_len};
use rlb_model::{MEDIA_UNIT_FRAMES, MemberId, SourceId, StreamEpoch};
use thiserror::Error;

use crate::wire::{
    EncodeError, EpochEndReason, EpochEndRecord, GapReason, GapRecord, MediaFragment,
    MediaIdentity, MediaUnitHeader, NackRecord, encode_epoch_end_into, encode_gap_into,
    encode_media_into, validate_nack_mask,
};
use crate::{
    FIXED_HEADER_BYTES, MAX_FRAGMENT_DATA_BYTES, MAX_FRAGMENTS, MAX_UDP_APPLICATION_BYTES,
    SENDER_DEADLINE_NS,
};

const HISTORY_MAX_UNITS: usize = 16;
const HISTORY_HORIZON_NS: u64 = 100_000_000;
const RESEND_INTERVAL_NS: u64 = 2_000_000;
const MAX_REMOTE_MEMBERS: usize = 62;
const MAX_RESEND_GATES: usize = HISTORY_MAX_UNITS * MAX_REMOTE_MEMBERS * MAX_FRAGMENTS as usize;

/// Wrapping record sequence allocator scoped to one stream epoch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordSequence {
    next: u32,
}

impl RecordSequence {
    /// Starts the epoch sequence at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Returns and consumes the next MEDIA or GAP sequence.
    pub const fn allocate(&mut self) -> u32 {
        let current = self.next;
        self.next = self.next.wrapping_add(1);
        current
    }

    /// Returns the value carried by EPOCH_END.
    #[must_use]
    pub const fn peek(self) -> u32 {
        self.next
    }
}

/// Immutable encoded and deterministically fragmented MEDIA datagram set.
#[derive(Debug)]
pub struct DatagramSet {
    identity: MediaIdentity,
    unit_sequence: u32,
    start_sample: u64,
    codec: WireCodec,
    channels: u8,
    datagrams: Box<[Box<[u8]>]>,
}

impl DatagramSet {
    /// Returns the source identity tuple.
    #[must_use]
    pub const fn identity(&self) -> MediaIdentity {
        self.identity
    }

    /// Returns the shared unit sequence.
    #[must_use]
    pub const fn unit_sequence(&self) -> u32 {
        self.unit_sequence
    }

    /// Returns the inclusive source sample start.
    #[must_use]
    pub const fn start_sample(&self) -> u64 {
        self.start_sample
    }

    /// Returns the once-selected wire codec.
    #[must_use]
    pub const fn codec(&self) -> WireCodec {
        self.codec
    }

    /// Returns the source channel count.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }

    /// Returns the exact fragment count.
    #[must_use]
    pub fn fragment_count(&self) -> u8 {
        self.datagrams.len() as u8
    }

    /// Borrows one immutable original datagram.
    #[must_use]
    pub fn datagram(&self, fragment_index: u8) -> Option<&[u8]> {
        self.datagrams
            .get(usize::from(fragment_index))
            .map(AsRef::as_ref)
    }

    /// Borrows all immutable initial datagrams in fragment order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        self.datagrams.iter().map(AsRef::as_ref)
    }

    /// Creates a zero-copy recipient/datagram iterator.
    #[must_use]
    pub fn fanout<'a>(&'a self, recipients: &'a [MemberId]) -> DatagramFanout<'a> {
        DatagramFanout {
            set: self,
            recipients,
            recipient_index: 0,
            fragment_index: 0,
        }
    }

    fn wire_bytes(&self) -> usize {
        self.datagrams.iter().map(|datagram| datagram.len()).sum()
    }
}

/// One zero-copy initial fanout item.
#[derive(Clone, Copy, Debug)]
pub struct FanoutDatagram<'a> {
    /// Exact target member.
    pub recipient: MemberId,
    /// Borrowed immutable application datagram.
    pub bytes: &'a [u8],
}

/// Iterator which reuses one immutable datagram set for every recipient.
pub struct DatagramFanout<'a> {
    set: &'a DatagramSet,
    recipients: &'a [MemberId],
    recipient_index: usize,
    fragment_index: usize,
}

impl<'a> Iterator for DatagramFanout<'a> {
    type Item = FanoutDatagram<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.recipient_index >= self.recipients.len() {
            return None;
        }
        let recipient = self.recipients[self.recipient_index];
        let bytes = self.set.datagrams[self.fragment_index].as_ref();
        self.fragment_index += 1;
        if self.fragment_index == self.set.datagrams.len() {
            self.fragment_index = 0;
            self.recipient_index += 1;
        }
        Some(FanoutDatagram { recipient, bytes })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let complete_recipients = self
            .recipients
            .len()
            .saturating_sub(self.recipient_index + 1);
        let current = if self.recipient_index < self.recipients.len() {
            self.set.datagrams.len().saturating_sub(self.fragment_index)
        } else {
            0
        };
        let remaining = current + complete_recipients * self.set.datagrams.len();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for DatagramFanout<'_> {}

/// Owned packetization result for one sequence allocation.
#[derive(Debug)]
pub enum OutboundRecord {
    /// Immutable MEDIA set which must enter history before first submission.
    Media(DatagramSet),
    /// Exact sender-declared GAP datagram.
    Gap {
        /// Typed record.
        record: GapRecord,
        /// Immutable wire bytes.
        datagram: Box<[u8]>,
    },
    /// Exact normal epoch end datagram.
    EpochEnd {
        /// Typed record.
        record: EpochEndRecord,
        /// Immutable wire bytes.
        datagram: Box<[u8]>,
    },
}

/// Closed packetization failure set.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PacketizeError {
    /// Configured channels or payload shape was invalid.
    #[error("codec operation failed: {0}")]
    Codec(CodecError),
    /// Wire descriptor or output failed strict validation.
    #[error("wire encoding failed: {0}")]
    Wire(EncodeError),
    /// Source sample arithmetic overflowed.
    #[error("sample_range_overflow")]
    SampleRangeOverflow,
    /// GAP frame count was outside 1–48,000.
    #[error("gap_range_invalid")]
    GapRangeInvalid,
    /// A record overlapped or moved behind a prior published record.
    #[error("timeline_conflict")]
    TimelineConflict,
}

/// Cumulative per-source sender counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SenderMetrics {
    /// Last allocated MEDIA or GAP sequence.
    pub last_unit_sequence: Option<u32>,
    /// PCM units encoded exactly once.
    pub encoded_units_total: u64,
    /// Encoded complete payload bytes.
    pub encoded_payload_bytes_total: u64,
    /// Non-finite capture samples replaced by positive zero.
    pub capture_non_finite_samples_total: u64,
    /// Sender-declared frames indexed by [`GapReason`] minus one.
    pub gap_frames_total_by_reason: [u64; 4],
    /// Units converted to GAP at the sender deadline.
    pub send_deadline_dropped_units_total: u64,
    /// Initial immutable fragment datagrams created.
    pub initial_datagrams_total: u64,
    /// Initial immutable application bytes created.
    pub initial_wire_bytes_total: u64,
    /// EPOCH_END records encoded.
    pub epoch_end_sent_total: u64,
}

/// Encode-once packetizer for one source epoch.
pub struct MediaPacketizer {
    identity: MediaIdentity,
    codec: WireCodec,
    channels: u8,
    sender_deadline_ticks: u64,
    sequence: RecordSequence,
    last_record_end: Option<u64>,
    metrics: SenderMetrics,
}

impl MediaPacketizer {
    /// Creates a packetizer in a caller-defined monotonic tick domain.
    pub fn new(
        identity: MediaIdentity,
        codec: WireCodec,
        channels: u8,
        sender_deadline_ticks: u64,
    ) -> Result<Self, PacketizeError> {
        encoded_len(codec, channels, usize::from(MEDIA_UNIT_FRAMES))
            .map_err(PacketizeError::Codec)?;
        Ok(Self {
            identity,
            codec,
            channels,
            sender_deadline_ticks,
            sequence: RecordSequence::new(),
            last_record_end: None,
            metrics: SenderMetrics::default(),
        })
    }

    /// Creates a packetizer for nanosecond monotonic timestamps.
    pub fn new_nanosecond_domain(
        identity: MediaIdentity,
        codec: WireCodec,
        channels: u8,
    ) -> Result<Self, PacketizeError> {
        Self::new(identity, codec, channels, SENDER_DEADLINE_NS)
    }

    /// Returns the exact next sequence without consuming it.
    #[must_use]
    pub const fn next_sequence(&self) -> u32 {
        self.sequence.peek()
    }

    /// Returns a cumulative sender snapshot.
    #[must_use]
    pub const fn metrics(&self) -> SenderMetrics {
        self.metrics
    }

    /// Encodes one unit once, or emits an equal-duration GAP at deadline.
    pub fn packetize_at_deadline(
        &mut self,
        unit_ready: MonotonicInstant,
        now: MonotonicInstant,
        start_sample: u64,
        interleaved_samples: &[f32],
    ) -> Result<OutboundRecord, PacketizeError> {
        let deadline = unit_ready
            .ticks()
            .saturating_add(self.sender_deadline_ticks);
        if now.ticks() >= deadline {
            self.metrics.send_deadline_dropped_units_total += 1;
            return self.gap(
                start_sample,
                u32::from(MEDIA_UNIT_FRAMES),
                GapReason::SendDeadlineDrop,
            );
        }
        self.packetize(start_sample, interleaved_samples)
            .map(OutboundRecord::Media)
    }

    /// Encodes and fragments one complete 480-frame source unit exactly once.
    pub fn packetize(
        &mut self,
        start_sample: u64,
        interleaved_samples: &[f32],
    ) -> Result<DatagramSet, PacketizeError> {
        let end_sample = start_sample
            .checked_add(u64::from(MEDIA_UNIT_FRAMES))
            .ok_or(PacketizeError::SampleRangeOverflow)?;
        self.validate_timeline_start(start_sample)?;
        let payload_len = encoded_len(self.codec, self.channels, usize::from(MEDIA_UNIT_FRAMES))
            .map_err(PacketizeError::Codec)?;
        let mut encoded = vec![0_u8; payload_len];
        let report = encode_interleaved_into(
            self.codec,
            self.channels,
            usize::from(MEDIA_UNIT_FRAMES),
            interleaved_samples,
            &mut encoded,
        )
        .map_err(PacketizeError::Codec)?;
        let sequence = self.sequence.allocate();
        let fragment_count = payload_len.div_ceil(MAX_FRAGMENT_DATA_BYTES) as u8;
        let mut datagrams = Vec::with_capacity(usize::from(fragment_count));
        for fragment_index in 0..fragment_count {
            let offset = usize::from(fragment_index) * MAX_FRAGMENT_DATA_BYTES;
            let end = (offset + MAX_FRAGMENT_DATA_BYTES).min(payload_len);
            let payload = &encoded[offset..end];
            let header = MediaUnitHeader {
                identity: self.identity,
                unit_sequence: sequence,
                start_sample,
                fragment_index,
                fragment_count,
                codec: self.codec,
                channels: self.channels,
                unit_payload_bytes: payload_len as u32,
            };
            let mut datagram = vec![0_u8; FIXED_HEADER_BYTES + payload.len()];
            encode_media_into(MediaFragment { header, payload }, &mut datagram)
                .map_err(PacketizeError::Wire)?;
            debug_assert!(datagram.len() <= MAX_UDP_APPLICATION_BYTES);
            datagrams.push(datagram.into_boxed_slice());
        }
        self.metrics.last_unit_sequence = Some(sequence);
        self.metrics.encoded_units_total += 1;
        self.metrics.encoded_payload_bytes_total = self
            .metrics
            .encoded_payload_bytes_total
            .saturating_add(payload_len as u64);
        self.metrics.capture_non_finite_samples_total = self
            .metrics
            .capture_non_finite_samples_total
            .saturating_add(report.non_finite_samples as u64);
        self.metrics.initial_datagrams_total = self
            .metrics
            .initial_datagrams_total
            .saturating_add(u64::from(fragment_count));
        self.metrics.initial_wire_bytes_total =
            self.metrics.initial_wire_bytes_total.saturating_add(
                datagrams
                    .iter()
                    .map(|datagram| datagram.len() as u64)
                    .sum::<u64>(),
            );
        self.last_record_end = Some(end_sample);
        Ok(DatagramSet {
            identity: self.identity,
            unit_sequence: sequence,
            start_sample,
            codec: self.codec,
            channels: self.channels,
            datagrams: datagrams.into_boxed_slice(),
        })
    }

    /// Encodes one known sender GAP with a fresh record sequence.
    pub fn gap(
        &mut self,
        start_sample: u64,
        frame_count: u32,
        reason: GapReason,
    ) -> Result<OutboundRecord, PacketizeError> {
        if !(1..=48_000).contains(&frame_count) {
            return Err(PacketizeError::GapRangeInvalid);
        }
        let end_sample = start_sample
            .checked_add(u64::from(frame_count))
            .ok_or(PacketizeError::SampleRangeOverflow)?;
        self.validate_timeline_start(start_sample)?;
        let sequence = self.sequence.allocate();
        let record = GapRecord {
            identity: self.identity,
            record_sequence: sequence,
            start_sample,
            frame_count,
            reason,
        };
        let mut datagram = vec![0; FIXED_HEADER_BYTES];
        encode_gap_into(record, &mut datagram).map_err(PacketizeError::Wire)?;
        self.metrics.last_unit_sequence = Some(sequence);
        self.metrics.gap_frames_total_by_reason[gap_index(reason)] =
            self.metrics.gap_frames_total_by_reason[gap_index(reason)]
                .saturating_add(u64::from(frame_count));
        self.last_record_end = Some(end_sample);
        Ok(OutboundRecord::Gap {
            record,
            datagram: datagram.into_boxed_slice(),
        })
    }

    /// Encodes a normal EPOCH_END using the next unallocated sequence.
    pub fn epoch_end(
        &mut self,
        final_sample: u64,
        reason: EpochEndReason,
    ) -> Result<OutboundRecord, PacketizeError> {
        if self
            .last_record_end
            .is_some_and(|record_end| final_sample < record_end)
        {
            return Err(PacketizeError::TimelineConflict);
        }
        let record = EpochEndRecord {
            identity: self.identity,
            next_record_sequence: self.sequence.peek(),
            final_sample,
            reason,
        };
        let mut datagram = vec![0; FIXED_HEADER_BYTES];
        encode_epoch_end_into(record, &mut datagram).map_err(PacketizeError::Wire)?;
        self.metrics.epoch_end_sent_total += 1;
        Ok(OutboundRecord::EpochEnd {
            record,
            datagram: datagram.into_boxed_slice(),
        })
    }

    fn validate_timeline_start(&self, start_sample: u64) -> Result<(), PacketizeError> {
        if self
            .last_record_end
            .is_some_and(|previous_end| start_sample < previous_end)
        {
            Err(PacketizeError::TimelineConflict)
        } else {
            Ok(())
        }
    }
}

/// Exact immutable history key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct HistoryKey {
    /// Local source identity.
    pub source_id: SourceId,
    /// Active source epoch.
    pub stream_epoch: StreamEpoch,
    /// Wrapping unit sequence.
    pub unit_sequence: u32,
    /// Inclusive source sample start.
    pub start_sample: u64,
}

impl From<&DatagramSet> for HistoryKey {
    fn from(set: &DatagramSet) -> Self {
        Self {
            source_id: set.identity.source_id,
            stream_epoch: set.identity.stream_epoch,
            unit_sequence: set.unit_sequence,
            start_sample: set.start_sample,
        }
    }
}

#[derive(Debug)]
struct HistoryEntry {
    inserted_ticks: u64,
    set: DatagramSet,
}

#[derive(Clone, Copy, Debug)]
struct ResendGate {
    recipient: Option<MemberId>,
    key: Option<HistoryKey>,
    fragment_index: u8,
    attempts: u8,
    last_send_ticks: u64,
}

impl ResendGate {
    const EMPTY: Self = Self {
        recipient: None,
        key: None,
        fragment_index: 0,
        attempts: 0,
        last_send_ticks: 0,
    };
}

/// Result of one authorized or rejected NACK lookup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResendDecision {
    /// Exact history unit existed.
    pub history_found: bool,
    /// NACK mask selected an impossible fragment.
    pub invalid_mask: bool,
    /// Current send gates rejected this recipient.
    pub unauthorized: bool,
    /// Original fragments passed to the send callback.
    pub sent_fragments: u8,
    /// Fragments rejected by interval or attempt limits.
    pub rate_limited_fragments: u8,
    /// Send callback failures.
    pub send_failed_fragments: u8,
}

/// Bounded short-history counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryMetrics {
    /// Current retained unit count.
    pub history_units: u8,
    /// Current retained application bytes.
    pub history_bytes: u32,
    /// Oldest retained age in local ticks.
    pub history_oldest_age_ticks: u64,
    /// NACKs presented to the history gate.
    pub nacks_received_total: u64,
    /// NACKs rejected by current recipient eligibility.
    pub unauthorized_nacks_total: u64,
    /// Exact NACK misses.
    pub history_miss_total: u64,
    /// Invalid mask requests.
    pub invalid_nacks_total: u64,
    /// Original datagrams selected for retransmission.
    pub retransmitted_fragments_total: u64,
    /// Requests rejected by interval or attempt limits.
    pub retransmit_rate_limited_total: u64,
    /// Send callback failures.
    pub retransmit_send_errors_total: u64,
}

/// Preallocated 100 ms / 16-unit immutable resend history.
pub struct ResendHistory {
    entries: Box<[Option<HistoryEntry>]>,
    gates: Box<[ResendGate]>,
    horizon_ticks: u64,
    resend_interval_ticks: u64,
    metrics: HistoryMetrics,
}

impl ResendHistory {
    /// Allocates fixed history metadata in an arbitrary monotonic tick domain.
    #[must_use]
    pub fn new(horizon_ticks: u64, resend_interval_ticks: u64) -> Self {
        let entries = (0..HISTORY_MAX_UNITS)
            .map(|_| None)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            entries,
            gates: vec![ResendGate::EMPTY; MAX_RESEND_GATES].into_boxed_slice(),
            horizon_ticks,
            resend_interval_ticks,
            metrics: HistoryMetrics::default(),
        }
    }

    /// Allocates the normative history for nanosecond timestamps.
    #[must_use]
    pub fn new_nanosecond_domain() -> Self {
        Self::new(HISTORY_HORIZON_NS, RESEND_INTERVAL_NS)
    }

    /// Inserts before first socket submission and returns the exact key.
    pub fn insert(&mut self, set: DatagramSet, now: MonotonicInstant) -> HistoryKey {
        self.prune(now);
        let key = HistoryKey::from(&set);
        let slot = self
            .entries
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        entry.as_ref().map(|entry| (index, entry.inserted_ticks))
                    })
                    .min_by_key(|(_, ticks)| *ticks)
                    .map_or(0, |(index, _)| index)
            });
        if let Some(replaced) = self.entries[slot].take() {
            self.clear_gates_for(HistoryKey::from(&replaced.set));
        }
        self.entries[slot] = Some(HistoryEntry {
            inserted_ticks: now.ticks(),
            set,
        });
        self.refresh_metrics(now);
        key
    }

    /// Borrows an immutable set for initial fanout after history insertion.
    #[must_use]
    pub fn get(&self, key: HistoryKey) -> Option<&DatagramSet> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| HistoryKey::from(&entry.set) == key)
            .map(|entry| &entry.set)
    }

    /// Handles one NACK using exact history bytes and bounded resend gates.
    pub fn process_nack(
        &mut self,
        recipient: MemberId,
        authorized: bool,
        nack: NackRecord,
        now: MonotonicInstant,
        mut send: impl FnMut(&[u8]) -> bool,
    ) -> ResendDecision {
        self.prune(now);
        self.metrics.nacks_received_total += 1;
        if !authorized {
            self.metrics.unauthorized_nacks_total += 1;
            return ResendDecision {
                unauthorized: true,
                ..ResendDecision::default()
            };
        }
        let key = HistoryKey {
            source_id: nack.identity.source_id,
            stream_epoch: nack.identity.stream_epoch,
            unit_sequence: nack.target_unit_sequence,
            start_sample: nack.unit_start_sample,
        };
        let Some(entry_index) = self.entries.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|entry| HistoryKey::from(&entry.set) == key)
        }) else {
            self.metrics.history_miss_total += 1;
            return ResendDecision::default();
        };
        let fragment_count = self.entries[entry_index]
            .as_ref()
            .map_or(0, |entry| entry.set.fragment_count());
        if validate_nack_mask(nack, fragment_count).is_err() {
            self.metrics.invalid_nacks_total += 1;
            return ResendDecision {
                history_found: true,
                invalid_mask: true,
                ..ResendDecision::default()
            };
        }
        let mut decision = ResendDecision {
            history_found: true,
            ..ResendDecision::default()
        };
        for fragment_index in 0..fragment_count {
            if nack.missing_fragment_mask & (1_u32 << fragment_index) == 0 {
                continue;
            }
            let gate_index = self.gate_index(recipient, key, fragment_index);
            let Some(gate_index) = gate_index else {
                decision.rate_limited_fragments += 1;
                self.metrics.retransmit_rate_limited_total += 1;
                continue;
            };
            let gate = &self.gates[gate_index];
            if gate.attempts >= 2
                || (gate.attempts > 0
                    && now.ticks().saturating_sub(gate.last_send_ticks)
                        < self.resend_interval_ticks)
            {
                decision.rate_limited_fragments += 1;
                self.metrics.retransmit_rate_limited_total += 1;
                continue;
            }
            let bytes = self.entries[entry_index]
                .as_ref()
                .and_then(|entry| entry.set.datagram(fragment_index))
                .expect("validated history fragment");
            {
                let gate = &mut self.gates[gate_index];
                gate.attempts += 1;
                gate.last_send_ticks = now.ticks();
            }
            if send(bytes) {
                decision.sent_fragments += 1;
                self.metrics.retransmitted_fragments_total += 1;
            } else {
                decision.send_failed_fragments += 1;
                self.metrics.retransmit_send_errors_total += 1;
            }
        }
        decision
    }

    /// Removes entries older than the strict horizon.
    pub fn prune(&mut self, now: MonotonicInstant) {
        let mut removed = [None; HISTORY_MAX_UNITS];
        let mut removed_count = 0;
        for entry in &mut self.entries {
            if entry.as_ref().is_some_and(|entry| {
                now.ticks().saturating_sub(entry.inserted_ticks) > self.horizon_ticks
            }) {
                if let Some(expired) = entry.take() {
                    removed[removed_count] = Some(HistoryKey::from(&expired.set));
                    removed_count += 1;
                }
            }
        }
        for key in removed.into_iter().flatten() {
            self.clear_gates_for(key);
        }
        self.refresh_metrics(now);
    }

    /// Returns bounded cumulative and current metrics.
    #[must_use]
    pub const fn metrics(&self) -> HistoryMetrics {
        self.metrics
    }

    fn gate_index(
        &mut self,
        recipient: MemberId,
        key: HistoryKey,
        fragment_index: u8,
    ) -> Option<usize> {
        if let Some(index) = self.gates.iter().position(|gate| {
            gate.recipient == Some(recipient)
                && gate.key == Some(key)
                && gate.fragment_index == fragment_index
        }) {
            return Some(index);
        }
        let index = self
            .gates
            .iter()
            .position(|gate| gate.recipient.is_none())?;
        self.gates[index] = ResendGate {
            recipient: Some(recipient),
            key: Some(key),
            fragment_index,
            attempts: 0,
            last_send_ticks: 0,
        };
        Some(index)
    }

    fn clear_gates_for(&mut self, key: HistoryKey) {
        for gate in &mut self.gates {
            if gate.key == Some(key) {
                *gate = ResendGate::EMPTY;
            }
        }
    }

    fn refresh_metrics(&mut self, now: MonotonicInstant) {
        self.metrics.history_units = self.entries.iter().flatten().count() as u8;
        self.metrics.history_bytes = self
            .entries
            .iter()
            .flatten()
            .map(|entry| entry.set.wire_bytes() as u32)
            .sum();
        self.metrics.history_oldest_age_ticks = self
            .entries
            .iter()
            .flatten()
            .map(|entry| now.ticks().saturating_sub(entry.inserted_ticks))
            .max()
            .unwrap_or(0);
    }
}

fn gap_index(reason: GapReason) -> usize {
    reason as usize - 1
}
