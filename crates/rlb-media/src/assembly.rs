use rlb_clock::MonotonicInstant;
use rlb_codec::{CodecError, WireCodec, decode_interleaved_into};
use rlb_model::MEDIA_UNIT_FRAMES;
use thiserror::Error;

use crate::wire::{MediaDescriptor, MediaFragment, MediaIdentity, PacketError};
use crate::{MAX_FRAGMENTS, NACK_PUBLICATION_MARGIN_NS};

const MAX_UNIT_PAYLOAD_BYTES: usize = 3_840;
const MAX_ASSEMBLY_UNITS: usize =
    (LOGICAL_ASSEMBLY_CAPACITY_FRAMES / MEDIA_UNIT_FRAMES as u64) as usize + 1;
const DEFAULT_HOLE_AGE_NS: u64 = 2_000_000;
const DEFAULT_RTT_NS: u64 = 5_000_000;
const DEFAULT_RETRY_MIN_NS: u64 = 2_000_000;
const DEFAULT_RETRY_MAX_NS: u64 = 10_000_000;

/// Logical packet window enforced by each assembler.
pub const LOGICAL_ASSEMBLY_CAPACITY_FRAMES: u64 = 12_000;

/// Closed fragment-assembly failure set.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AssemblyError {
    /// A previous descriptor or payload conflict faulted the source.
    #[error("assembler is faulted")]
    Faulted,
    /// Datagram identity differed from this per-source assembler.
    #[error("fragment identity mismatch")]
    IdentityMismatch,
    /// One key carried conflicting fragment-invariant fields.
    #[error("fragment_descriptor_conflict")]
    DescriptorConflict,
    /// One record sequence mapped to a different sample start.
    #[error("record_sequence_conflict")]
    RecordSequenceConflict,
    /// A repeated fragment index carried different payload bytes.
    #[error("fragment_payload_conflict")]
    FragmentPayloadConflict,
    /// Caller decode output had the wrong interleaved sample count.
    #[error("decode output requires {expected} samples, got {actual}")]
    OutputLength {
        /// Exact required samples.
        expected: usize,
        /// Supplied samples.
        actual: usize,
    },
    /// Constructed fragment violated wire invariants.
    #[error("invalid fragment: {0}")]
    InvalidFragment(PacketError),
    /// PCM decode failed after complete assembly.
    #[error("decode failed: {0}")]
    Decode(CodecError),
}

/// Successful result of one fragment insertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssemblyOutcome {
    /// Fragment was stored and the unit remains incomplete.
    Stored,
    /// Fragment was stored after a newer `(start, index)` had been observed.
    Reordered,
    /// Byte-identical fragment was already present.
    Duplicate,
    /// Fragment belonged behind the current read cursor.
    Late,
    /// Fragment started beyond the independent 250 ms window.
    CapacityDropped,
    /// Unit completed and decoded exactly once.
    Complete(AssembledUnit),
}

/// Metadata for one decoded complete unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssembledUnit {
    /// Epoch record sequence.
    pub unit_sequence: u32,
    /// Inclusive source sample start.
    pub start_sample: u64,
    /// Fixed unit frame count.
    pub frames: u16,
    /// Payload codec.
    pub codec: WireCodec,
    /// Interleaved channels.
    pub channels: u8,
    /// At least one missing fragment was requested before completion.
    pub recovered_after_nack: bool,
    /// This insertion arrived behind a newer fragment key.
    pub reordered_completion: bool,
    /// Non-finite f32 payload values sanitized during decode.
    pub non_finite_samples: usize,
}

/// One bounded NACK produced into caller-owned storage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NackCandidate {
    /// Target unit sequence.
    pub unit_sequence: u32,
    /// Exact target source start.
    pub start_sample: u64,
    /// Missing fragments with remaining attempt budget.
    pub missing_fragment_mask: u32,
    /// True when at least one selected fragment was requested previously.
    pub retry: bool,
}

/// Receiver-side NACK timing in one local monotonic tick domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NackPlannerConfig {
    /// Smoothed recovery RTT estimate.
    pub recovery_rtt_ticks: u64,
    /// Decode and publication safety margin.
    pub publication_margin_ticks: u64,
    /// Incomplete-unit age which proves a hole.
    pub hole_age_ticks: u64,
    /// Minimum retry interval in the same local tick domain.
    pub retry_min_ticks: u64,
    /// Maximum retry interval in the same local tick domain.
    pub retry_max_ticks: u64,
    /// Whether retransmission requests are enabled.
    pub enabled: bool,
}

impl Default for NackPlannerConfig {
    fn default() -> Self {
        Self {
            recovery_rtt_ticks: DEFAULT_RTT_NS,
            publication_margin_ticks: NACK_PUBLICATION_MARGIN_NS,
            hole_age_ticks: DEFAULT_HOLE_AGE_NS,
            retry_min_ticks: DEFAULT_RETRY_MIN_NS,
            retry_max_ticks: DEFAULT_RETRY_MAX_NS,
            enabled: true,
        }
    }
}

impl NackPlannerConfig {
    /// Clamps the RTT estimate to the normative 1–100 ms nanosecond-domain range.
    #[must_use]
    pub fn nanosecond_domain(mut self) -> Self {
        self.recovery_rtt_ticks = self.recovery_rtt_ticks.clamp(1_000_000, 100_000_000);
        self
    }

    fn retry_interval(self) -> u64 {
        let minimum = self.retry_min_ticks.min(self.retry_max_ticks);
        let maximum = self.retry_min_ticks.max(self.retry_max_ticks);
        (self.recovery_rtt_ticks / 2).clamp(minimum, maximum)
    }
}

/// Cumulative bounded assembler counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AssemblerMetrics {
    /// Accepted fragment datagrams.
    pub received_fragments_total: u64,
    /// Complete units decoded and published.
    pub complete_units_total: u64,
    /// Byte-identical duplicate fragments.
    pub duplicate_fragments_total: u64,
    /// Fragments behind the maximum observed key.
    pub reordered_fragments_total: u64,
    /// Fragments rejected behind the read cursor.
    pub late_fragments_total: u64,
    /// Fragments rejected beyond the logical window.
    pub capacity_dropped_fragments_total: u64,
    /// Descriptor, sequence, or repeated-payload conflicts.
    pub conflicts_total: u64,
    /// NACK datagrams planned.
    pub nacks_sent_total: u64,
    /// Planned NACKs containing at least one retried fragment.
    pub nack_retries_total: u64,
    /// Non-finite decoded samples sanitized.
    pub receive_non_finite_samples_total: u64,
    /// Current occupied bounded slots.
    pub assembler_units: u16,
    /// Current payload bytes held by fragment masks.
    pub assembler_bytes: u32,
}

#[derive(Debug)]
struct AssemblySlot {
    descriptor: Option<MediaDescriptor>,
    payload: [u8; MAX_UNIT_PAYLOAD_BYTES],
    received_mask: u32,
    received_bytes: u32,
    first_seen_ticks: u64,
    highest_fragment_index: u8,
    published: bool,
    nack_sent: bool,
    attempts: [u8; MAX_FRAGMENTS as usize],
    last_nack_ticks: [Option<u64>; MAX_FRAGMENTS as usize],
}

impl AssemblySlot {
    fn empty() -> Self {
        Self {
            descriptor: None,
            payload: [0; MAX_UNIT_PAYLOAD_BYTES],
            received_mask: 0,
            received_bytes: 0,
            first_seen_ticks: 0,
            highest_fragment_index: 0,
            published: false,
            nack_sent: false,
            attempts: [0; MAX_FRAGMENTS as usize],
            last_nack_ticks: [None; MAX_FRAGMENTS as usize],
        }
    }

    fn clear(&mut self) {
        self.descriptor = None;
        self.received_mask = 0;
        self.received_bytes = 0;
        self.first_seen_ticks = 0;
        self.highest_fragment_index = 0;
        self.published = false;
        self.nack_sent = false;
        self.attempts.fill(0);
        self.last_nack_ticks.fill(None);
    }

    fn expected_mask(&self) -> u32 {
        self.descriptor.map_or(0, |descriptor| {
            if descriptor.fragment_count == 32 {
                u32::MAX
            } else {
                (1_u32 << descriptor.fragment_count) - 1
            }
        })
    }
}

/// Fixed-capacity fragment assembler for one active source epoch.
pub struct FragmentAssembler {
    identity: MediaIdentity,
    slots: Box<[AssemblySlot]>,
    window_base: Option<u64>,
    read_cursor: Option<u64>,
    maximum_key: Option<(u64, u8)>,
    faulted: bool,
    nack: NackPlannerConfig,
    metrics: AssemblerMetrics,
}

impl FragmentAssembler {
    /// Allocates every fragment slab and metadata slot.
    #[must_use]
    pub fn new(identity: MediaIdentity, nack: NackPlannerConfig) -> Self {
        let slots = (0..MAX_ASSEMBLY_UNITS)
            .map(|_| AssemblySlot::empty())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            identity,
            slots,
            window_base: None,
            read_cursor: None,
            maximum_key: None,
            faulted: false,
            nack,
            metrics: AssemblerMetrics::default(),
        }
    }

    /// Establishes or advances the logical window to the source read cursor.
    pub fn set_read_cursor(&mut self, cursor: u64) {
        self.read_cursor = Some(cursor);
        self.window_base = Some(cursor);
        for slot in &mut self.slots {
            if slot.descriptor.is_some_and(|descriptor| {
                descriptor
                    .start_sample
                    .saturating_add(u64::from(MEDIA_UNIT_FRAMES))
                    <= cursor
            }) {
                slot.clear();
            }
        }
        self.refresh_storage_metrics();
    }

    /// Sets a provisional pre-playout anchor before a read cursor exists.
    pub fn set_preroll_anchor(&mut self, anchor: u64) {
        if self.read_cursor.is_none() {
            self.window_base = Some(anchor);
            for slot in &mut self.slots {
                if slot
                    .descriptor
                    .is_some_and(|descriptor| descriptor.start_sample < anchor)
                {
                    slot.clear();
                }
            }
            self.refresh_storage_metrics();
        }
    }

    /// Returns a bounded cumulative metrics snapshot.
    #[must_use]
    pub const fn metrics(&self) -> AssemblerMetrics {
        self.metrics
    }

    /// Inserts one validated fragment and decodes a newly complete unit.
    pub fn push(
        &mut self,
        fragment: MediaFragment<'_>,
        arrival: MonotonicInstant,
        decoded_output: &mut [f32],
    ) -> Result<AssemblyOutcome, AssemblyError> {
        if self.faulted {
            return Err(AssemblyError::Faulted);
        }
        self.validate_fragment(fragment)?;
        let descriptor = fragment.header.descriptor_without_index();
        let start = descriptor.start_sample;
        if self.read_cursor.is_some_and(|cursor| start < cursor) {
            self.metrics.late_fragments_total += 1;
            return Ok(AssemblyOutcome::Late);
        }
        let base = *self.window_base.get_or_insert(start);
        if start < base {
            self.metrics.late_fragments_total += 1;
            return Ok(AssemblyOutcome::Late);
        }
        if start >= base.saturating_add(LOGICAL_ASSEMBLY_CAPACITY_FRAMES) {
            self.metrics.capacity_dropped_fragments_total += 1;
            return Ok(AssemblyOutcome::CapacityDropped);
        }

        let mut existing_index = None;
        let mut empty_index = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(existing) = slot.descriptor else {
                empty_index.get_or_insert(index);
                continue;
            };
            if existing.unit_sequence == descriptor.unit_sequence
                && existing.start_sample != descriptor.start_sample
            {
                return self.fail(AssemblyError::RecordSequenceConflict);
            }
            if existing.start_sample == descriptor.start_sample {
                if existing != descriptor {
                    return self.fail(AssemblyError::DescriptorConflict);
                }
                existing_index = Some(index);
                break;
            }
        }
        let slot_index = if let Some(index) = existing_index {
            index
        } else {
            let Some(index) = empty_index else {
                self.metrics.capacity_dropped_fragments_total += 1;
                return Ok(AssemblyOutcome::CapacityDropped);
            };
            let slot = &mut self.slots[index];
            slot.descriptor = Some(descriptor);
            slot.first_seen_ticks = arrival.ticks();
            slot.highest_fragment_index = fragment.header.fragment_index;
            index
        };

        let reordered = self.note_order(start, fragment.header.fragment_index);
        let bit = 1_u32 << fragment.header.fragment_index;
        let offset = fragment.header.fragment_offset();
        let end = offset + fragment.payload.len();
        if self.slots[slot_index].received_mask & bit != 0 {
            if self.slots[slot_index].payload[offset..end] == *fragment.payload {
                self.metrics.duplicate_fragments_total += 1;
                return Ok(AssemblyOutcome::Duplicate);
            }
            return self.fail(AssemblyError::FragmentPayloadConflict);
        }
        let (complete, published, received_bytes) = {
            let slot = &mut self.slots[slot_index];
            slot.payload[offset..end].copy_from_slice(fragment.payload);
            slot.received_mask |= bit;
            slot.received_bytes = slot
                .received_bytes
                .saturating_add(fragment.payload.len() as u32);
            slot.highest_fragment_index = slot
                .highest_fragment_index
                .max(fragment.header.fragment_index);
            (
                slot.received_mask == slot.expected_mask(),
                slot.published,
                slot.received_bytes,
            )
        };
        self.metrics.received_fragments_total += 1;
        self.refresh_storage_metrics();

        if !complete || published {
            return Ok(if reordered {
                AssemblyOutcome::Reordered
            } else {
                AssemblyOutcome::Stored
            });
        }
        let expected_samples = usize::from(MEDIA_UNIT_FRAMES) * usize::from(descriptor.channels);
        if decoded_output.len() != expected_samples {
            return Err(AssemblyError::OutputLength {
                expected: expected_samples,
                actual: decoded_output.len(),
            });
        }
        if received_bytes != descriptor.unit_payload_bytes {
            return self.fail(AssemblyError::DescriptorConflict);
        }
        let report = {
            let slot = &self.slots[slot_index];
            decode_interleaved_into(
                descriptor.codec,
                descriptor.channels,
                usize::from(MEDIA_UNIT_FRAMES),
                &slot.payload[..descriptor.unit_payload_bytes as usize],
                decoded_output,
            )
            .map_err(AssemblyError::Decode)?
        };
        let recovered_after_nack = self.slots[slot_index].nack_sent;
        self.slots[slot_index].published = true;
        self.metrics.complete_units_total += 1;
        self.metrics.receive_non_finite_samples_total = self
            .metrics
            .receive_non_finite_samples_total
            .saturating_add(report.non_finite_samples as u64);
        Ok(AssemblyOutcome::Complete(AssembledUnit {
            unit_sequence: descriptor.unit_sequence,
            start_sample: descriptor.start_sample,
            frames: MEDIA_UNIT_FRAMES,
            codec: descriptor.codec,
            channels: descriptor.channels,
            recovered_after_nack,
            reordered_completion: reordered,
            non_finite_samples: report.non_finite_samples,
        }))
    }

    /// Plans bounded NACKs into caller-owned storage.
    ///
    /// `deadline_for_start` returns the predicted callback-entry deadline for a
    /// unit start. Units without a known deadline produce no request.
    pub fn poll_nacks(
        &mut self,
        now: MonotonicInstant,
        output: &mut [NackCandidate],
        mut deadline_for_start: impl FnMut(u64) -> Option<MonotonicInstant>,
    ) -> usize {
        if !self.nack.enabled || self.faulted {
            return 0;
        }
        let retry_interval = self.nack.retry_interval();
        let mut written = 0;
        for slot in &mut self.slots {
            if written == output.len() {
                break;
            }
            let Some(descriptor) = slot.descriptor else {
                continue;
            };
            if slot.published || slot.received_mask == slot.expected_mask() {
                continue;
            }
            let higher_index_proves_hole = (0..slot.highest_fragment_index)
                .any(|index| slot.received_mask & (1_u32 << index) == 0);
            let age_proves_hole =
                now.ticks().saturating_sub(slot.first_seen_ticks) >= self.nack.hole_age_ticks;
            let future_start_proves_hole = self
                .maximum_key
                .is_some_and(|(start, _)| start > descriptor.start_sample);
            if !(higher_index_proves_hole || age_proves_hole || future_start_proves_hole) {
                continue;
            }
            let Some(deadline) = deadline_for_start(descriptor.start_sample) else {
                continue;
            };
            let budget_end = now
                .ticks()
                .saturating_add(self.nack.recovery_rtt_ticks)
                .saturating_add(self.nack.publication_margin_ticks);
            if budget_end >= deadline.ticks() {
                continue;
            }
            let mut mask = 0_u32;
            let mut retry = false;
            for fragment_index in 0..descriptor.fragment_count {
                let index = usize::from(fragment_index);
                let bit = 1_u32 << fragment_index;
                if slot.received_mask & bit != 0 || slot.attempts[index] >= 2 {
                    continue;
                }
                if slot.last_nack_ticks[index]
                    .is_some_and(|last| now.ticks().saturating_sub(last) < retry_interval)
                {
                    continue;
                }
                retry |= slot.attempts[index] > 0;
                slot.attempts[index] += 1;
                slot.last_nack_ticks[index] = Some(now.ticks());
                mask |= bit;
            }
            if mask == 0 {
                continue;
            }
            slot.nack_sent = true;
            output[written] = NackCandidate {
                unit_sequence: descriptor.unit_sequence,
                start_sample: descriptor.start_sample,
                missing_fragment_mask: mask,
                retry,
            };
            written += 1;
            self.metrics.nacks_sent_total += 1;
            if retry {
                self.metrics.nack_retries_total += 1;
            }
        }
        written
    }

    fn validate_fragment(&self, fragment: MediaFragment<'_>) -> Result<(), AssemblyError> {
        if fragment.header.identity != self.identity {
            return Err(AssemblyError::IdentityMismatch);
        }
        let expected = fragment
            .header
            .expected_fragment_payload_bytes()
            .map_err(AssemblyError::InvalidFragment)?;
        if expected != fragment.payload.len() {
            return Err(AssemblyError::InvalidFragment(
                PacketError::FragmentLengthMismatch,
            ));
        }
        if fragment.header.unit_payload_bytes as usize > MAX_UNIT_PAYLOAD_BYTES {
            return Err(AssemblyError::InvalidFragment(
                PacketError::UnitPayloadMismatch,
            ));
        }
        Ok(())
    }

    fn note_order(&mut self, start: u64, fragment_index: u8) -> bool {
        let key = (start, fragment_index);
        if self.maximum_key.is_some_and(|maximum| key < maximum) {
            self.metrics.reordered_fragments_total += 1;
            true
        } else {
            self.maximum_key = Some(self.maximum_key.map_or(key, |maximum| maximum.max(key)));
            false
        }
    }

    fn refresh_storage_metrics(&mut self) {
        self.metrics.assembler_units = self
            .slots
            .iter()
            .filter(|slot| slot.descriptor.is_some())
            .count() as u16;
        self.metrics.assembler_bytes = self.slots.iter().map(|slot| slot.received_bytes).sum();
    }

    fn fail<T>(&mut self, error: AssemblyError) -> Result<T, AssemblyError> {
        self.faulted = true;
        self.metrics.conflicts_total += 1;
        Err(error)
    }
}
