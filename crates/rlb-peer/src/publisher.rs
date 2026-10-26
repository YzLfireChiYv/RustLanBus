//! Participant-owned original publication, independent of peers and devices.
//!
//! Packetize each original once, then fan its immutable fragments out to
//! currently ready peers. There is no destination queue here. Enqueue time is
//! an actual caller observation copied unchanged; acquisition timing and its
//! clock domain stay with the application's capture metadata.

use core::num::NonZeroU64;

use rlb_model::{RunId, SourceId, StreamEpoch};

use crate::source::{
    Catalogue, MAX_ORIGINAL_FRAMES, MAX_SOURCES, PcmFormat, PublicationEpoch,
    PublicationState, SourceEntry, SourceError, SourceKind,
};
use crate::{AdmittedPcm, MemberRun, PublicationInput, PublicationRef, UpstreamMapping};
#[cfg(test)]
use crate::PcmEncoding;

/// One immutable run-wide source-handoff identity for a completed or burned block.
///
/// The input owner allocates these values, strictly increasing within one local
/// run, before distribution. Captured and received inputs in the same run use
/// one allocator. This crate never mints them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CaptureTicket(NonZeroU64);

impl CaptureTicket {
    /// Accepts a nonzero ticket. Zero is not a ticket.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Highest ticket the local source-handoff allocator issued, including burned tickets.
///
/// Zero means no ticket has been issued. Callers pass the allocator's current
/// value at each interval replacement. A later call with a smaller value is rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureWatermark {
    pub allocated_through: u64,
}

impl CaptureWatermark {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            allocated_through: 0,
        }
    }

    #[must_use]
    pub const fn through(ticket: CaptureTicket) -> Self {
        Self {
            allocated_through: ticket.get(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindingRequest {
    run: RunId,
    source: SourceId,
    epoch: PublicationEpoch,
    expected_acquisition: StreamEpoch,
    format: PcmFormat,
}

impl BindingRequest {
    #[must_use]
    pub const fn run(self) -> RunId {
        self.run
    }
    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }
    #[must_use]
    pub const fn epoch(self) -> PublicationEpoch {
        self.epoch
    }
    #[must_use]
    pub const fn expected_acquisition(self) -> StreamEpoch {
        self.expected_acquisition
    }
    #[must_use]
    pub const fn format(self) -> PcmFormat {
        self.format
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationBinding {
    request: BindingRequest,
    first_eligible_frame: u64,
}

impl PublicationBinding {
    #[must_use]
    pub const fn source(self) -> SourceId {
        self.request.source
    }
    #[must_use]
    pub const fn epoch(self) -> PublicationEpoch {
        self.request.epoch
    }
    #[must_use]
    pub const fn acquisition(self) -> StreamEpoch {
        self.request.expected_acquisition
    }
    #[must_use]
    pub const fn format(self) -> PcmFormat {
        self.request.format
    }
    #[must_use]
    pub const fn first_eligible_frame(self) -> u64 {
        self.first_eligible_frame
    }
}

/// An explicitly routed received publication, published under this run's own
/// source/epoch. Source frames keep their upstream positions, including gaps
/// and reordering; no local acquisition clock or playout queue is introduced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedBinding {
    run: RunId,
    source: SourceId,
    epoch: PublicationEpoch,
    input: PublicationRef,
    format: PcmFormat,
}

impl ReceivedBinding {
    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn epoch(self) -> PublicationEpoch {
        self.epoch
    }

    #[must_use]
    pub const fn input(self) -> PublicationRef {
        self.input
    }

    #[must_use]
    pub const fn format(self) -> PcmFormat {
        self.format
    }

    /// Local correspondence metadata; transmission of provenance is a separate
    /// control-protocol concern. Returning samples performs no DSP.
    #[must_use]
    pub const fn upstream_mapping(self) -> UpstreamMapping {
        UpstreamMapping {
            input: self.input,
            input_first_frame: 0,
            output_first_frame: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputBinding {
    Capture(PublicationBinding),
    Received(ReceivedBinding),
}

impl InputBinding {
    fn capture(self) -> Option<PublicationBinding> {
        match self {
            Self::Capture(binding) => Some(binding),
            Self::Received(_) => None,
        }
    }

    fn received(self) -> Option<ReceivedBinding> {
        match self {
            Self::Capture(_) => None,
            Self::Received(binding) => Some(binding),
        }
    }
}

/// Complete resulting catalogue and the affected source's current handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceUpdate {
    pub source: SourceId,
    pub catalogue: Catalogue,
    pub binding: Option<PublicationBinding>,
    pub received: Option<ReceivedBinding>,
    pub pending: Option<BindingRequest>,
}

struct PublishedSource {
    entry: SourceEntry,
    requested: bool,
    epoch_cursor: u64,
    binding: Option<InputBinding>,
    pending: Option<BindingRequest>,
    accepted_acquisition_end: Option<u64>,
    next_sequence: u64,
}

impl PublishedSource {
    fn clear_binding(&mut self) {
        self.binding = None;
        self.pending = None;
        self.accepted_acquisition_end = None;
        self.next_sequence = 1;
    }
}

pub struct SourcePublisher {
    run: RunId,
    sources: Vec<PublishedSource>,
    quiesced: bool,
}

impl SourcePublisher {
    #[must_use]
    pub fn new(run: RunId) -> Self {
        Self {
            run,
            sources: Vec::with_capacity(MAX_SOURCES),
            quiesced: false,
        }
    }

    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }

    /// Always check this after an operation, including an error result.
    /// Exhaustion clears all bindings and pending requests while retaining the
    /// last valid catalogue. The participant must close every admission using
    /// this publisher and create a fresh run before publishing again.
    #[must_use]
    pub const fn is_quiesced(&self) -> bool {
        self.quiesced
    }

    pub fn register(
        &mut self,
        id: SourceId,
        kind: SourceKind,
        requested: bool,
    ) -> Result<SourceUpdate, SourceError> {
        self.ensure_live()?;
        let index = match self
            .sources
            .binary_search_by_key(&id, |source| source.entry.id)
        {
            Ok(_) => return Err(SourceError::DuplicateSource),
            Err(index) => index,
        };
        if self.sources.len() == MAX_SOURCES {
            return Err(SourceError::Capacity);
        }
        self.sources.insert(
            index,
            PublishedSource {
                entry: SourceEntry {
                    id,
                    epoch: PublicationEpoch::new(1).expect("nonzero initial epoch"),
                    kind,
                    state: if requested {
                        PublicationState::Unavailable
                    } else {
                        PublicationState::Disabled
                    },
                    format: None,
                },
                requested,
                epoch_cursor: 1,
                binding: None,
                pending: None,
                accepted_acquisition_end: None,
                next_sequence: 1,
            },
        );
        Ok(self.update(index))
    }

    /// Returns the last valid catalogue, including while quiesced.
    #[must_use]
    pub fn catalogue(&self) -> Catalogue {
        Catalogue::from_sorted(self.sources.iter().map(|source| source.entry).collect())
            .expect("publisher maintains a validated sorted catalogue")
    }

    #[must_use]
    pub fn source(&self, id: SourceId) -> Option<&SourceEntry> {
        self.index(id).ok().map(|index| &self.sources[index].entry)
    }

    #[must_use]
    pub fn binding(&self, id: SourceId) -> Option<PublicationBinding> {
        self.index(id)
            .ok()
            .and_then(|index| self.sources[index].binding.and_then(InputBinding::capture))
    }

    #[must_use]
    pub fn received_binding(&self, id: SourceId) -> Option<ReceivedBinding> {
        self.index(id)
            .ok()
            .and_then(|index| self.sources[index].binding.and_then(InputBinding::received))
    }

    /// Current input association, derived from the active binding. A source
    /// without a committed input has no association to advertise.
    #[must_use]
    pub fn input(&self, id: SourceId) -> Option<PublicationInput> {
        match self.sources[self.index(id).ok()?].binding? {
            InputBinding::Capture(binding) => Some(PublicationInput::Capture {
                source: binding.source(),
                acquisition: binding.acquisition(),
            }),
            InputBinding::Received(binding) => Some(PublicationInput::Received {
                publication: binding.input(),
            }),
        }
    }

    /// Declares an explicit received-input route from its admitted descriptor,
    /// before audio arrives. The caller owns current upstream admission and
    /// route lifetime, and must retire this binding when that route closes.
    /// This creates no reverse route or automatic forwarding of other inputs.
    pub fn bind_received(
        &mut self,
        id: SourceId,
        input: PublicationRef,
        format: PcmFormat,
    ) -> Result<SourceUpdate, SourceError> {
        self.ensure_live()?;
        let index = self.index(id)?;
        let source = &self.sources[index];
        if !source.requested {
            return Err(SourceError::PublicationDisabled);
        }
        if let Some(binding) = source.binding.and_then(InputBinding::received) {
            if binding.input == input {
                if binding.format != format {
                    return Err(SourceError::InvalidFormat);
                }
                return Ok(self.update(index));
            }
        }
        let epoch = self.reserve_epochs(index, 1)?;
        let source = &mut self.sources[index];
        source.clear_binding();
        source.entry.epoch = epoch;
        source.entry.state = PublicationState::Active;
        source.entry.format = Some(format);
        source.binding = Some(InputBinding::Received(ReceivedBinding {
            run: self.run,
            source: id,
            epoch,
            input,
            format,
        }));
        Ok(self.update(index))
    }

    #[must_use]
    pub fn pending_binding(&self, id: SourceId) -> Option<BindingRequest> {
        self.index(id)
            .ok()
            .and_then(|index| self.sources[index].pending)
    }

    pub fn set_publication(
        &mut self,
        id: SourceId,
        enabled: bool,
    ) -> Result<SourceUpdate, SourceError> {
        self.ensure_live()?;
        let index = self.index(id)?;
        if self.sources[index].requested == enabled {
            return Ok(self.update(index));
        }
        let epoch = self.reserve_epochs(index, 1)?;
        let source = &mut self.sources[index];
        source.requested = enabled;
        source.clear_binding();
        source.entry.epoch = epoch;
        source.entry.state = if enabled {
            PublicationState::Unavailable
        } else {
            PublicationState::Disabled
        };
        source.entry.format = None;
        Ok(self.update(index))
    }

    pub fn mark_unavailable(&mut self, id: SourceId) -> Result<SourceUpdate, SourceError> {
        self.ensure_live()?;
        let index = self.index(id)?;
        if self.sources[index].entry.state == PublicationState::Active {
            let epoch = self.reserve_epochs(index, 1)?;
            self.sources[index].entry.epoch = epoch;
            self.sources[index].entry.state = PublicationState::Unavailable;
            self.sources[index].entry.format = None;
        }
        // An unavailable acquisition also invalidates an uncommitted request.
        // Its already reserved epoch remains burned even if no record changed.
        self.sources[index].clear_binding();
        Ok(self.update(index))
    }

    pub fn begin_binding(
        &mut self,
        id: SourceId,
        expected_acquisition: StreamEpoch,
        format: PcmFormat,
    ) -> Result<SourceUpdate, SourceError> {
        self.ensure_live()?;
        let index = self.index(id)?;
        let source = &self.sources[index];
        if !source.requested {
            return Err(SourceError::PublicationDisabled);
        }
        if source
            .binding
            .and_then(InputBinding::capture)
            .is_some_and(|binding| {
                binding.acquisition() == expected_acquisition && binding.format() == format
            })
            || source.pending.is_some_and(|request| {
                request.expected_acquisition == expected_acquisition && request.format == format
            })
        {
            return Ok(self.update(index));
        }

        let was_active = source.entry.state == PublicationState::Active;
        // Preflight the Unavailable transition and reservation together.
        let reserved_epoch = self.reserve_epochs(index, if was_active { 2 } else { 1 })?;
        let source = &mut self.sources[index];
        source.clear_binding();
        if was_active {
            source.entry.epoch = PublicationEpoch::new(reserved_epoch.get() - 1)
                .expect("preceding epoch is nonzero");
            source.entry.state = PublicationState::Unavailable;
            source.entry.format = None;
        }
        source.pending = Some(BindingRequest {
            run: self.run,
            source: id,
            epoch: reserved_epoch,
            expected_acquisition,
            format,
        });
        Ok(self.update(index))
    }

    /// The capture owner supplies a whole-block boundary from this request's
    /// acquisition. No old queued block may be used to choose that boundary.
    pub fn commit_binding(
        &mut self,
        request: BindingRequest,
        actual_acquisition: StreamEpoch,
        first_eligible_frame: u64,
    ) -> Result<SourceUpdate, SourceError> {
        self.ensure_live()?;
        let index = self.index(request.source)?;
        let source = &mut self.sources[index];
        if request.run != self.run
            || source.pending != Some(request)
            || request.expected_acquisition != actual_acquisition
            || !source.requested
            || source.entry.state != PublicationState::Unavailable
        {
            return Err(SourceError::StaleBinding);
        }
        source.entry.epoch = request.epoch;
        source.entry.state = PublicationState::Active;
        source.entry.format = Some(request.format);
        source.binding = Some(InputBinding::Capture(PublicationBinding {
            request,
            first_eligible_frame,
        }));
        source.pending = None;
        source.accepted_acquisition_end = None;
        source.next_sequence = 1;
        Ok(self.update(index))
    }

    /// Creates a local-original input borrowing only `samples`, so the caller
    /// can subsequently borrow this publisher mutably for preparation.
    pub fn local_original<'a>(
        &self,
        source: SourceId,
        acquisition: StreamEpoch,
        acquisition_start_frame: u64,
        samples: &'a [f64],
        enqueued_at_ms: u64,
    ) -> Result<LocalOriginal<'a>, SourceError> {
        self.ensure_live()?;
        let index = self.index(source)?;
        let binding = self.sources[index]
            .binding
            .and_then(InputBinding::capture)
            .ok_or(SourceError::StaleBinding)?;
        if binding.acquisition() != acquisition {
            return Err(SourceError::StaleBinding);
        }
        Ok(LocalOriginal {
            run: self.run,
            binding,
            acquisition_start_frame,
            samples,
            enqueued_at_ms,
        })
    }

    /// Allocating preparation. The result has no capture ticket.
    ///
    /// Production fanout uses [`Self::prepare_pcm_into`] with a ticket allocated
    /// before distribution. This wrapper remains for existing diagnostic callers.
    pub fn prepare_pcm(
        &mut self,
        original: LocalOriginal<'_>,
    ) -> Result<PreparedBatch, SourceError> {
        let plan = self.validate_pcm(&original)?;
        if plan.next_sequence.is_none() {
            self.quiesce();
            return Err(SourceError::CounterExhausted);
        }
        let channels = plan.channels;
        let mut packets = Vec::with_capacity(plan.fragment_count);
        for (offset, samples) in original
            .samples
            .chunks(plan.max_frames * channels)
            .enumerate()
        {
            let mut payload = vec![0_u8; samples.len() * usize::from(plan.format.sample_bytes())];
            rlb_model::pcm::encode_into(plan.format, samples, &mut payload)?;
            packets.push(PreparedOriginalPacket {
                run: self.run,
                source: original.binding.source(),
                epoch: original.binding.epoch(),
                sequence: plan.first_sequence + offset as u64,
                start_frame: plan.start_frame + (offset * plan.max_frames) as u64,
                frames: (samples.len() / channels) as u16,
                format: plan.format,
                enqueued_at_ms: original.enqueued_at_ms,
                ticket: None,
                payload,
            });
        }
        self.commit_plan(&plan)?;
        Ok(PreparedBatch { packets })
    }

    /// Encodes one ticketed original into caller storage that was sized before the call.
    ///
    /// Every fragment receives the same ticket and enqueue observation. Neither
    /// value is written into RLP4. A short final fragment is returned immediately.
    /// Storage that cannot hold the original leaves the source sequence and
    /// binding unchanged. On success the storage is replaced from its start, so
    /// an earlier [`PreparedView`] must be dropped before the next call.
    pub fn prepare_pcm_into<'a>(
        &mut self,
        original: TicketedOriginal<'_>,
        storage: &'a mut PreparedStorage,
    ) -> Result<PreparedView<'a>, SourceError> {
        let ticket = original.ticket;
        let enqueued_at_ms = original.original.enqueued_at_ms;
        let plan = self.validate_pcm(&original.original)?;
        self.prepare_into(
            plan,
            original.original.samples,
            ticket,
            enqueued_at_ms,
            storage,
        )
    }

    /// Prepares already admitted F32 for an explicit return/forward route.
    /// A fresh run-wide handoff ticket is allocated by the caller before
    /// distribution, using the same allocator as every other input in this run.
    /// Queue age starts at the original receiver arrival observation, copied
    /// from the admission metadata. The caller must recheck current upstream
    /// admission before calling, as with a render ingress.
    ///
    /// Input sequence order is immaterial: original frame positions and sample
    /// words survive unchanged. Ordinary Peer writers assign the local session
    /// header and check current destination permission and ticket floors.
    pub fn prepare_received_into<'a>(
        &mut self,
        binding: ReceivedBinding,
        original: AdmittedPcm<'_>,
        ticket: CaptureTicket,
        storage: &'a mut PreparedStorage,
    ) -> Result<PreparedView<'a>, SourceError> {
        self.ensure_live()?;
        let index = self.index(binding.source)?;
        let source = &self.sources[index];
        let metadata = original.metadata;
        if binding.run != self.run
            || source.binding != Some(InputBinding::Received(binding))
            || source.entry.state != PublicationState::Active
            || source.entry.epoch != binding.epoch
            || source.entry.format != Some(binding.format)
            || binding.input
                != (PublicationRef {
                    owner: MemberRun {
                        member: metadata.member_id,
                        run: metadata.remote_run,
                    },
                    source: metadata.entry.id,
                    epoch: metadata.entry.epoch,
                })
            || metadata.entry.state != PublicationState::Active
            || metadata.entry.format != Some(binding.format)
        {
            return Err(SourceError::StaleBinding);
        }
        let channels = usize::from(binding.format.channels());
        let frames = usize::from(metadata.frames);
        if frames == 0
            || frames > MAX_ORIGINAL_FRAMES
            || metadata.sequence == 0
            || metadata.start_frame.checked_add(frames as u64).is_none()
            || original.samples.len() != frames * channels
        {
            return Err(SourceError::InvalidOriginal);
        }
        if original.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(SourceError::NonFiniteSample);
        }
        let max_frames = crate::max_packet_frames(binding.format);
        let fragment_count = frames.div_ceil(max_frames);
        let plan = OriginalPlan {
            index,
            format: binding.format,
            frames,
            channels,
            max_frames,
            fragment_count,
            start_frame: metadata.start_frame,
            acquisition_end: None,
            first_sequence: source.next_sequence,
            next_sequence: source.next_sequence.checked_add(fragment_count as u64),
        };
        self.prepare_into(
            plan,
            original.samples,
            ticket,
            metadata.arrived_at_ms,
            storage,
        )
    }

    fn prepare_into<'a>(
        &mut self,
        plan: OriginalPlan,
        samples: &[f64],
        ticket: CaptureTicket,
        enqueued_at_ms: u64,
        storage: &'a mut PreparedStorage,
    ) -> Result<PreparedView<'a>, SourceError> {
        let entry = self.sources[plan.index].entry;
        let payload_bytes = plan.frames * plan.channels * usize::from(plan.format.sample_bytes());
        if plan.fragment_count > storage.fragment_limit || payload_bytes > storage.byte_limit {
            return Err(SourceError::StorageTooSmall);
        }
        if plan.next_sequence.is_none() {
            self.quiesce();
            return Err(SourceError::CounterExhausted);
        }
        storage.records.clear();
        storage.count = 0;
        let mut cursor = 0_usize;
        for (offset, samples) in samples.chunks(plan.max_frames * plan.channels).enumerate() {
            let bytes = samples.len() * usize::from(plan.format.sample_bytes());
            let end = cursor + bytes;
            rlb_model::pcm::encode_into(plan.format, samples, &mut storage.bytes[cursor..end])?;
            storage.records.push(FragmentRecord {
                run: self.run,
                source: entry.id,
                epoch: entry.epoch,
                sequence: plan.first_sequence + offset as u64,
                start_frame: plan.start_frame + (offset * plan.max_frames) as u64,
                frames: (samples.len() / plan.channels) as u16,
                format: plan.format,
                enqueued_at_ms,
                ticket,
                payload_at: cursor as u32,
                payload_len: bytes as u16,
            });
            cursor = end;
        }
        storage.count = storage.records.len() as u16;
        self.commit_plan(&plan)?;
        Ok(PreparedView { storage })
    }

    fn validate_pcm(&self, original: &LocalOriginal<'_>) -> Result<OriginalPlan, SourceError> {
        self.ensure_live()?;
        let index = self.index(original.binding.source())?;
        let source = &self.sources[index];
        if original.run != self.run
            || source.binding != Some(InputBinding::Capture(original.binding))
            || source.entry.state != PublicationState::Active
            || source.entry.epoch != original.binding.epoch()
            || source.entry.format != Some(original.binding.format())
        {
            return Err(SourceError::StaleBinding);
        }
        let format = original.binding.format();
        let channels = usize::from(format.channels());
        if original.samples.is_empty() || !original.samples.len().is_multiple_of(channels) {
            return Err(SourceError::InvalidOriginal);
        }
        let frames = original.samples.len() / channels;
        if frames > MAX_ORIGINAL_FRAMES {
            return Err(SourceError::InvalidOriginal);
        }
        if original.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(SourceError::NonFiniteSample);
        }
        let acquisition_end = original
            .acquisition_start_frame
            .checked_add(frames as u64)
            .ok_or(SourceError::InvalidOriginal)?;
        let start_frame = original
            .acquisition_start_frame
            .checked_sub(original.binding.first_eligible_frame())
            .ok_or(SourceError::TimelineWentBackwards)?;
        if source
            .accepted_acquisition_end
            .is_some_and(|end| original.acquisition_start_frame < end)
        {
            return Err(SourceError::TimelineWentBackwards);
        }
        let max_frames = crate::max_packet_frames(format);
        if max_frames == 0 {
            return Err(SourceError::InvalidFormat);
        }
        let fragment_count = frames.div_ceil(max_frames);
        let first_sequence = source.next_sequence;
        let next_sequence = first_sequence.checked_add(fragment_count as u64);
        Ok(OriginalPlan {
            index,
            format,
            frames,
            channels,
            max_frames,
            fragment_count,
            start_frame,
            acquisition_end: Some(acquisition_end),
            first_sequence,
            next_sequence,
        })
    }

    fn commit_plan(&mut self, plan: &OriginalPlan) -> Result<(), SourceError> {
        let Some(next_sequence) = plan.next_sequence else {
            self.quiesce();
            return Err(SourceError::CounterExhausted);
        };
        self.sources[plan.index].next_sequence = next_sequence;
        self.sources[plan.index].accepted_acquisition_end = plan.acquisition_end;
        Ok(())
    }

    fn index(&self, id: SourceId) -> Result<usize, SourceError> {
        self.sources
            .binary_search_by_key(&id, |source| source.entry.id)
            .map_err(|_| SourceError::UnknownSource)
    }

    fn ensure_live(&self) -> Result<(), SourceError> {
        if self.quiesced {
            Err(SourceError::Quiesced)
        } else {
            Ok(())
        }
    }

    fn reserve_epochs(
        &mut self,
        index: usize,
        count: u64,
    ) -> Result<PublicationEpoch, SourceError> {
        let Some(value) = self.sources[index].epoch_cursor.checked_add(count) else {
            self.quiesce();
            return Err(SourceError::CounterExhausted);
        };
        self.sources[index].epoch_cursor = value;
        PublicationEpoch::new(value)
    }

    fn quiesce(&mut self) {
        self.quiesced = true;
        for source in &mut self.sources {
            source.clear_binding();
        }
    }

    fn update(&self, index: usize) -> SourceUpdate {
        SourceUpdate {
            source: self.sources[index].entry.id,
            catalogue: self.catalogue(),
            binding: self.sources[index].binding.and_then(InputBinding::capture),
            received: self.sources[index].binding.and_then(InputBinding::received),
            pending: self.sources[index].pending,
        }
    }
}

struct OriginalPlan {
    index: usize,
    format: PcmFormat,
    frames: usize,
    channels: usize,
    max_frames: usize,
    fragment_count: usize,
    start_frame: u64,
    acquisition_end: Option<u64>,
    first_sequence: u64,
    next_sequence: Option<u64>,
}

/// Constructed only through this publisher's local-original entry point.
pub struct LocalOriginal<'a> {
    run: RunId,
    binding: PublicationBinding,
    acquisition_start_frame: u64,
    samples: &'a [f64],
    enqueued_at_ms: u64,
}

impl<'a> LocalOriginal<'a> {
    /// Attaches the ticket allocated for this block before distribution.
    #[must_use]
    pub const fn with_ticket(self, ticket: CaptureTicket) -> TicketedOriginal<'a> {
        TicketedOriginal {
            original: self,
            ticket,
        }
    }
}

/// A local original plus the capture ticket chosen before distribution.
pub struct TicketedOriginal<'a> {
    original: LocalOriginal<'a>,
    ticket: CaptureTicket,
}

impl TicketedOriginal<'_> {
    #[must_use]
    pub const fn ticket(&self) -> CaptureTicket {
        self.ticket
    }

    #[must_use]
    pub const fn enqueued_at_ms(&self) -> u64 {
        self.original.enqueued_at_ms
    }
}

/// Caller-owned fragment storage sized once for a format's largest original.
pub struct PreparedStorage {
    bytes: Vec<u8>,
    records: Vec<FragmentRecord>,
    count: u16,
    byte_limit: usize,
    fragment_limit: usize,
}

struct FragmentRecord {
    run: RunId,
    source: SourceId,
    epoch: PublicationEpoch,
    sequence: u64,
    start_frame: u64,
    frames: u16,
    format: PcmFormat,
    enqueued_at_ms: u64,
    ticket: CaptureTicket,
    payload_at: u32,
    payload_len: u16,
}

impl PreparedStorage {
    /// Sizes storage for `MAX_ORIGINAL_FRAMES` of this 32-bit float layout.
    pub fn for_format(format: PcmFormat) -> Result<Self, SourceError> {
        let max_frames = crate::max_packet_frames(format);
        if max_frames == 0 {
            return Err(SourceError::InvalidFormat);
        }
        let fragments = MAX_ORIGINAL_FRAMES.div_ceil(max_frames);
        let payload = max_frames
            .checked_mul(usize::from(format.channels()))
            .and_then(|samples| samples.checked_mul(usize::from(format.sample_bytes())))
            .ok_or(SourceError::InvalidFormat)?;
        let bytes = fragments
            .checked_mul(payload)
            .ok_or(SourceError::Capacity)?;
        Ok(Self::with_limits(fragments, bytes))
    }

    /// Allocates exact fragment and byte capacities. Later prepares do not grow them.
    #[must_use]
    pub fn with_limits(fragment_limit: usize, byte_limit: usize) -> Self {
        Self {
            bytes: vec![0; byte_limit],
            records: Vec::with_capacity(fragment_limit),
            count: 0,
            byte_limit,
            fragment_limit,
        }
    }
}

/// Borrow of one successful `prepare_pcm_into` result.
pub struct PreparedView<'a> {
    storage: &'a PreparedStorage,
}

impl<'a> PreparedView<'a> {
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.storage.count)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<PreparedFragment<'a>> {
        let record = self.storage.records.get(index)?;
        if index >= self.len() {
            return None;
        }
        let start = record.payload_at as usize;
        let end = start + usize::from(record.payload_len);
        Some(record.fragment(&self.storage.bytes[start..end]))
    }
}

impl FragmentRecord {
    fn fragment<'a>(&self, payload: &'a [u8]) -> PreparedFragment<'a> {
        PreparedFragment {
            run: self.run,
            source: self.source,
            epoch: self.epoch,
            sequence: self.sequence,
            start_frame: self.start_frame,
            frames: self.frames,
            format: self.format,
            enqueued_at_ms: self.enqueued_at_ms,
            ticket: Some(self.ticket),
            payload,
        }
    }
}

/// One immutable fragment shared between fanout destinations. A future Peer
/// writer must recheck run/source/epoch/format before adding its own header.
#[derive(Debug)]
pub struct PreparedOriginalPacket {
    run: RunId,
    source: SourceId,
    epoch: PublicationEpoch,
    sequence: u64,
    start_frame: u64,
    frames: u16,
    format: PcmFormat,
    enqueued_at_ms: u64,
    ticket: Option<CaptureTicket>,
    payload: Vec<u8>,
}

impl PreparedOriginalPacket {
    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }
    #[must_use]
    pub const fn epoch(&self) -> PublicationEpoch {
        self.epoch
    }
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    #[must_use]
    pub const fn start_frame(&self) -> u64 {
        self.start_frame
    }
    #[must_use]
    pub const fn frames(&self) -> u16 {
        self.frames
    }
    #[must_use]
    pub const fn format(&self) -> PcmFormat {
        self.format
    }
    /// Caller-supplied queue observation, unrelated to a remote clock/anchor.
    #[must_use]
    pub const fn enqueued_at_ms(&self) -> u64 {
        self.enqueued_at_ms
    }
    /// Present only when preparation received a capture ticket.
    #[must_use]
    pub const fn ticket(&self) -> Option<CaptureTicket> {
        self.ticket
    }
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    #[must_use]
    pub fn fragment(&self) -> PreparedFragment<'_> {
        PreparedFragment {
            run: self.run,
            source: self.source,
            epoch: self.epoch,
            sequence: self.sequence,
            start_frame: self.start_frame,
            frames: self.frames,
            format: self.format,
            enqueued_at_ms: self.enqueued_at_ms,
            ticket: self.ticket,
            payload: &self.payload,
        }
    }
}

/// One immutable fragment. Payload bytes stay in the caller's prepared storage
/// or in an allocating [`PreparedOriginalPacket`].
#[derive(Clone, Copy, Debug)]
pub struct PreparedFragment<'a> {
    run: RunId,
    source: SourceId,
    epoch: PublicationEpoch,
    sequence: u64,
    start_frame: u64,
    frames: u16,
    format: PcmFormat,
    enqueued_at_ms: u64,
    ticket: Option<CaptureTicket>,
    payload: &'a [u8],
}

impl<'a> PreparedFragment<'a> {
    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }
    #[must_use]
    pub const fn epoch(&self) -> PublicationEpoch {
        self.epoch
    }
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    #[must_use]
    pub const fn start_frame(&self) -> u64 {
        self.start_frame
    }
    #[must_use]
    pub const fn frames(&self) -> u16 {
        self.frames
    }
    #[must_use]
    pub const fn format(&self) -> PcmFormat {
        self.format
    }
    #[must_use]
    pub const fn enqueued_at_ms(&self) -> u64 {
        self.enqueued_at_ms
    }
    #[must_use]
    pub const fn ticket(&self) -> Option<CaptureTicket> {
        self.ticket
    }
    #[must_use]
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

#[derive(Debug)]
pub struct PreparedBatch {
    packets: Vec<PreparedOriginalPacket>,
}

impl PreparedBatch {
    #[must_use]
    pub fn as_slice(&self) -> &[PreparedOriginalPacket] {
        &self.packets
    }
    #[must_use]
    pub fn into_packets(self) -> Vec<PreparedOriginalPacket> {
        self.packets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::ChannelLayout;

    fn source_id(byte: u8) -> SourceId {
        SourceId::from_bytes([byte; 16]).unwrap()
    }
    fn acquisition(byte: u8) -> StreamEpoch {
        StreamEpoch::from_bytes([byte; 16]).unwrap()
    }
    fn publisher() -> SourcePublisher {
        SourcePublisher::new(RunId::from_bytes([1; 16]).unwrap())
    }
    fn format(layout: ChannelLayout) -> PcmFormat {
        PcmFormat::new(48_000, layout, PcmEncoding::F32Le, 32).unwrap()
    }
    fn bind(
        publisher: &mut SourcePublisher,
        source: SourceId,
        acquisition: StreamEpoch,
        layout: ChannelLayout,
        first: u64,
    ) -> PublicationBinding {
        let request = publisher
            .begin_binding(source, acquisition, format(layout))
            .unwrap()
            .pending
            .unwrap();
        publisher
            .commit_binding(request, acquisition, first)
            .unwrap()
            .binding
            .unwrap()
    }

    #[test]
    fn independent_sources_burn_canceled_reservations_and_keep_system_running() {
        let mut publisher = publisher();
        let system = source_id(2);
        let mic = source_id(3);
        publisher
            .register(system, SourceKind::System, true)
            .unwrap();
        publisher
            .register(mic, SourceKind::Microphone, true)
            .unwrap();
        let system_binding = bind(
            &mut publisher,
            system,
            acquisition(4),
            ChannelLayout::StereoLeftRight,
            100,
        );
        let mic_request = publisher
            .begin_binding(mic, acquisition(5), format(ChannelLayout::Mono))
            .unwrap()
            .pending
            .unwrap();
        assert_eq!(
            publisher
                .begin_binding(mic, acquisition(5), format(ChannelLayout::Mono))
                .unwrap()
                .pending,
            Some(mic_request)
        );
        publisher.set_publication(mic, false).unwrap();
        publisher.set_publication(mic, true).unwrap();
        assert_eq!(
            publisher.commit_binding(mic_request, acquisition(5), 100),
            Err(SourceError::StaleBinding)
        );
        let current = publisher
            .begin_binding(mic, acquisition(5), format(ChannelLayout::Mono))
            .unwrap()
            .pending
            .unwrap();
        assert!(current.epoch() > mic_request.epoch());
        publisher.mark_unavailable(mic).unwrap();
        assert_eq!(
            publisher.commit_binding(current, acquisition(5), 100),
            Err(SourceError::StaleBinding)
        );
        let mic_binding = bind(
            &mut publisher,
            mic,
            acquisition(5),
            ChannelLayout::Mono,
            200,
        );
        assert!(mic_binding.epoch() > current.epoch());
        assert_eq!(publisher.binding(system), Some(system_binding));
        let samples = [0.25, -0.25];
        let original = publisher
            .local_original(system, acquisition(4), 105, &samples, 7)
            .unwrap();
        let prepared = publisher.prepare_pcm(original).unwrap();
        assert_eq!(prepared.as_slice()[0].start_frame(), 5);
        assert_eq!(prepared.as_slice()[0].sequence(), 1);
        let before = publisher.catalogue();
        assert!(
            publisher
                .begin_binding(mic, acquisition(5), format(ChannelLayout::Mono))
                .unwrap()
                .pending
                .is_none()
        );
        assert_eq!(publisher.catalogue(), before);
        let mic_original = publisher
            .local_original(mic, acquisition(5), 200, &[0.5], 8)
            .unwrap();
        let mic_packet = publisher.prepare_pcm(mic_original).unwrap();
        assert_eq!(mic_packet.as_slice()[0].sequence(), 1);
        let stale_original = publisher
            .local_original(mic, acquisition(5), 201, &[0.5], 8)
            .unwrap();
        let replacement = publisher
            .begin_binding(mic, acquisition(6), format(ChannelLayout::Mono))
            .unwrap();
        assert_eq!(
            publisher.source(mic).unwrap().state,
            PublicationState::Unavailable
        );
        assert!(replacement.pending.unwrap().epoch() > mic_binding.epoch());
        assert_eq!(publisher.binding(system), Some(system_binding));
        assert!(matches!(
            publisher.prepare_pcm(stale_original),
            Err(SourceError::StaleBinding)
        ));
    }

    #[test]
    fn complete_span_validation_precedes_all_sequence_and_timeline_mutation() {
        let mut publisher = publisher();
        let source = source_id(2);
        publisher
            .register(source, SourceKind::System, true)
            .unwrap();
        bind(
            &mut publisher,
            source,
            acquisition(3),
            ChannelLayout::Mono,
            100,
        );
        let mut samples = vec![0.25; 480];
        samples[479] = f64::NAN;
        let original = publisher
            .local_original(source, acquisition(3), 100, &samples, 9)
            .unwrap();
        assert!(matches!(
            publisher.prepare_pcm(original),
            Err(SourceError::NonFiniteSample)
        ));
        samples[479] = 0.5;
        let original = publisher
            .local_original(source, acquisition(3), 100, &samples, 9)
            .unwrap();
        let prepared = publisher.prepare_pcm(original).unwrap();
        assert_eq!(prepared.as_slice()[0].sequence(), 1);
        assert_eq!(
            prepared
                .as_slice()
                .iter()
                .map(|packet| packet.frames())
                .collect::<Vec<_>>(),
            vec![280, 200]
        );
        let original = publisher
            .local_original(source, acquisition(3), 579, &[0.75], 10)
            .unwrap();
        assert!(matches!(
            publisher.prepare_pcm(original),
            Err(SourceError::TimelineWentBackwards)
        ));
        let original = publisher
            .local_original(source, acquisition(3), 580, &[0.75], 10)
            .unwrap();
        let next = publisher.prepare_pcm(original).unwrap();
        assert_eq!(next.as_slice()[0].sequence(), 3);
        assert_eq!(next.as_slice()[0].start_frame(), 480);
    }

    #[test]
    fn stereo_microphone_fragments_keep_all_original_words_and_queue_observation() {
        let mut publisher = publisher();
        let source = source_id(2);
        publisher
            .register(source, SourceKind::Microphone, true)
            .unwrap();
        bind(
            &mut publisher,
            source,
            acquisition(3),
            ChannelLayout::StereoLeftRight,
            10,
        );
        let words = [0x8000_0000, 0x0000_0001, 0x7f7f_ffff, 0xc020_0000];
        let samples: Vec<f64> = words
            .into_iter()
            .cycle()
            .take(960)
            .map(f32::from_bits)
            .map(f64::from)
            .collect();
        let original = publisher
            .local_original(source, acquisition(3), 10, &samples, 0)
            .unwrap();
        let prepared = publisher.prepare_pcm(original).unwrap();
        assert_eq!(
            prepared
                .as_slice()
                .iter()
                .map(|packet| packet.frames())
                .collect::<Vec<_>>(),
            vec![140, 140, 140, 60]
        );
        let payload: Vec<u8> = prepared
            .as_slice()
            .iter()
            .flat_map(|packet| packet.payload().iter().copied())
            .collect();
        let expected: Vec<u8> = samples
            .iter()
            .flat_map(|sample| (*sample as f32).to_le_bytes())
            .collect();
        assert_eq!(payload, expected);
        assert!(
            prepared
                .as_slice()
                .iter()
                .all(|packet| packet.enqueued_at_ms() == 0)
        );
    }

    #[test]
    fn multi_fragment_legacy_exhaustion_quiesces_before_encoding() {
        let mut publisher = publisher();
        let source = source_id(2);
        publisher
            .register(source, SourceKind::System, true)
            .unwrap();
        bind(
            &mut publisher,
            source,
            acquisition(3),
            ChannelLayout::Mono,
            0,
        );
        let catalogue = publisher.catalogue();
        publisher.sources[0].next_sequence = u64::MAX;
        let samples = vec![0.25; crate::max_packet_frames(format(ChannelLayout::Mono)) + 1];
        let original = publisher
            .local_original(source, acquisition(3), 0, &samples, 4)
            .unwrap();
        assert!(matches!(
            publisher.prepare_pcm(original),
            Err(SourceError::CounterExhausted)
        ));
        assert!(publisher.is_quiesced());
        assert!(publisher.binding(source).is_none());
        assert!(publisher.pending_binding(source).is_none());
        assert_eq!(publisher.catalogue(), catalogue);
    }

    #[test]
    fn exhaustion_quiesces_without_relabeling_last_valid_catalogue() {
        let mut publisher = publisher();
        let source = source_id(2);
        publisher
            .register(source, SourceKind::System, true)
            .unwrap();
        bind(
            &mut publisher,
            source,
            acquisition(3),
            ChannelLayout::Mono,
            0,
        );
        let catalogue = publisher.catalogue();
        publisher.sources[0].next_sequence = u64::MAX;
        let original = publisher
            .local_original(source, acquisition(3), 0, &[0.25], 4)
            .unwrap();
        assert!(matches!(
            publisher.prepare_pcm(original),
            Err(SourceError::CounterExhausted)
        ));
        assert!(publisher.is_quiesced());
        assert!(publisher.binding(source).is_none());
        assert!(publisher.pending_binding(source).is_none());
        assert_eq!(publisher.catalogue(), catalogue);
        assert_eq!(
            publisher.set_publication(source, false),
            Err(SourceError::Quiesced)
        );

        let mut other = SourcePublisher::new(RunId::from_bytes([9; 16]).unwrap());
        other.register(source, SourceKind::System, true).unwrap();
        bind(&mut other, source, acquisition(3), ChannelLayout::Mono, 0);
        let catalogue = other.catalogue();
        other.sources[0].epoch_cursor = u64::MAX - 1;
        assert_eq!(
            other.begin_binding(source, acquisition(4), format(ChannelLayout::Mono)),
            Err(SourceError::CounterExhausted)
        );
        assert!(other.is_quiesced());
        assert_eq!(other.catalogue(), catalogue);
        assert!(other.binding(source).is_none());
    }
}
