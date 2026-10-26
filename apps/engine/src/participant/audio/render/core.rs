//! Persistent render owner for one output generation at a time.
//!
//! Admission is applied at the start of a quantum. Source frames are closed
//! before conversion. Output frames count only after a matching successful
//! submission receipt.

use super::contract::{
    ChannelMatrix, ChannelRoute, CompletionReport, OutputSetup, Phase, PrepareOutcome,
    ProgressObservation, PushReport, QuantumHandle, RenderAdmission, RenderCapacities, RenderError,
    RenderSource, ServiceObservation, SourceActivity, SourceBlock, SourceGain, SourceStatus,
    SubmissionReceipt, TimelineId, TimingPolicy, block_frames, due_frames,
};
use super::control::{RatioSample, ReserveController};
use super::convert::RateConverter;
use super::ledger::Ledger;
use super::matrix::default_channel_matrix;
use super::mix::{GainRamp, finish_smoothed_f64, smoothing_frames};
use super::window::Window;
use rlb_peer::ChannelLayout;

struct OutputState {
    generation: u64,
    rate: u32,
    channels: usize,
    layout: ChannelLayout,
    quantum: u32,
    capacity: usize,
    submitted: u64,
    has_receipt: bool,
    available: bool,
    pending: Option<(u64, u32)>,
    mix: Vec<f64>,
    pcm: Vec<f64>,
}

struct Lane {
    ledger: Ledger,
    window: Window,
    phase: Phase,
    converter: RateConverter,
    controller: ReserveController,
    target_frames: u64,
    delivery_guard_frames: u64,
    active: bool,
    rendered_frames: usize,
    submitted_frames: u64,
    segment_restarts: u64,
    last_error: Option<&'static str>,
    gain: GainRamp,
    edge: EdgeBlend,
    segment_changed: bool,
    #[cfg(test)]
    fail_next_process: bool,
}

/// A single contribution sample per output channel closes discontinuities.
/// It never stores or replays a prior PCM block.
#[derive(Default)]
struct EdgeBlend {
    last: [f64; super::contract::ABSOLUTE_MAX_CHANNELS],
    from: [f64; super::contract::ABSOLUTE_MAX_CHANNELS],
    remaining: u32,
    total: u32,
    had_pcm: bool,
}

impl EdgeBlend {
    fn begin(&mut self, has_pcm: bool, changed: bool, frames: u32) {
        if has_pcm != self.had_pcm || (has_pcm && changed) {
            self.from = self.last;
            self.total = frames.max(1);
            self.remaining = self.total;
        }
        self.had_pcm = has_pcm;
    }

    fn apply(&mut self, frame: &mut [f64]) {
        let blend = if self.remaining == 0 {
            1.0
        } else {
            f64::from(self.total - self.remaining + 1) / f64::from(self.total)
        };
        for (channel, sample) in frame.iter_mut().enumerate() {
            if self.remaining != 0 {
                *sample = *sample * blend + self.from[channel] * (1.0 - blend);
            }
            self.last[channel] = *sample;
        }
        self.remaining = self.remaining.saturating_sub(1);
    }
}

pub struct RenderCore {
    capacities: RenderCapacities,
    policy: TimingPolicy,
    lanes: Vec<Lane>,
    queued: Option<RenderAdmission>,
    applied: Option<RenderAdmission>,
    applied_revision: u64,
    output: OutputState,
    routes: Vec<ChannelRoute>,
    matrices: Vec<ChannelMatrix>,
    master_gain: GainRamp,
    next_quantum: u64,
    unmapped: u16,
}

impl RenderCore {
    pub fn new(capacities: RenderCapacities, policy: TimingPolicy) -> Result<Self, RenderError> {
        capacities.validate(&policy)?;
        let channels = usize::from(capacities.output_channels);
        let capacity = capacities.max_quantum_frames;
        Ok(Self {
            capacities,
            policy,
            lanes: Vec::with_capacity(capacities.sources),
            queued: None,
            applied: None,
            applied_revision: 0,
            output: OutputState {
                generation: capacities.output_generation,
                rate: capacities.output_rate_hz,
                channels,
                layout: capacities.output_layout,
                quantum: capacities.quantum_frames,
                capacity,
                submitted: 0,
                has_receipt: false,
                available: true,
                pending: None,
                mix: vec![0.0; capacity * super::contract::ABSOLUTE_MAX_CHANNELS],
                pcm: vec![0.0; capacity * super::contract::ABSOLUTE_MAX_CHANNELS],
            },
            routes: Vec::with_capacity(capacities.sources),
            matrices: Vec::with_capacity(capacities.sources),
            master_gain: GainRamp::fixed(1.0),
            next_quantum: 1,
            unmapped: 0,
        })
    }

    pub fn queue_admission(&mut self, admission: RenderAdmission) -> Result<(), RenderError> {
        let newest = self.queued.as_ref().or(self.applied.as_ref());
        if let Some(newest) = newest {
            if admission.revision < newest.revision {
                return Err(RenderError::IncoherentAdmission(
                    "admission revision went backwards",
                ));
            }
            if admission.revision == newest.revision {
                return if &admission == newest {
                    Ok(())
                } else {
                    Err(RenderError::IncoherentAdmission(
                        "one admission revision has conflicting contents",
                    ))
                };
            }
        }
        self.queued = Some(admission);
        Ok(())
    }

    pub fn apply_admission(&mut self) -> Result<u64, RenderError> {
        let Some(admission) = self.queued.take() else {
            return Ok(self.applied_revision);
        };
        if admission.revision < self.applied_revision {
            return Err(RenderError::IncoherentAdmission(
                "admission revision went backwards",
            ));
        }
        if let Some(applied) = &self.applied {
            if admission.revision == self.applied_revision {
                return if applied == &admission {
                    Ok(self.applied_revision)
                } else {
                    Err(RenderError::IncoherentAdmission(
                        "one admission revision has conflicting contents",
                    ))
                };
            }
        }
        if admission.sources.len() > self.capacities.sources {
            return Err(RenderError::Capacity(
                "the admission snapshot exceeds the prepared source slots",
            ));
        }
        self.reconcile(&admission)?;
        self.applied_revision = admission.revision;
        self.applied = Some(admission);
        Ok(self.applied_revision)
    }

    pub fn push_block(&mut self, block: SourceBlock<'_>) -> Result<PushReport, RenderError> {
        let frames = block_frames(block.format, block.samples)?;
        if frames > self.capacities.max_block_frames as u64 {
            return Err(RenderError::Capacity(
                "an original block exceeds the prepared block capacity",
            ));
        }
        if block.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(RenderError::NonFinite);
        }
        let lane = self
            .lanes
            .iter_mut()
            .find(|lane| lane.ledger.timeline == block.timeline)
            .ok_or(RenderError::IncoherentAdmission(
                "the block has no applied source timeline",
            ))?;
        if !lane.active
            || lane.ledger.permission != block.permission
            || lane.ledger.format != block.format
        {
            return Err(RenderError::IncoherentAdmission(
                "the block does not match the applied source context",
            ));
        }
        let end = lane.ledger.note_extent(block.start_frame, frames)?;
        let origin = end.saturating_sub(lane.window.capacity_frames() as u64);
        if origin > lane.window.origin_frame() {
            // A first join or large real gap may start far into source time.
            // Keep the bounded current window and permanently retire its old
            // prefix, instead of rejecting every future block as all-ahead.
            lane.ledger.discard_through(origin);
            lane.window.advance_origin(lane.ledger.closed_before);
            lane.reset_segment(Phase::AwaitCurrent);
        }
        let floor = lane.ledger.closed_before;
        let stored = lane
            .window
            .insert(block.start_frame, block.samples, floor)?;
        lane.window.advance_origin(lane.ledger.closed_before);
        Ok(PushReport {
            stored_frames: stored.stored,
            duplicate_frames: stored.duplicate,
            stale_frames: stored.stale,
            ahead_frames: stored.ahead,
            high_water: lane.ledger.high_water,
        })
    }

    pub fn observe_progress(
        &mut self,
        observation: ProgressObservation,
    ) -> Result<(), RenderError> {
        let lane = self
            .lanes
            .iter_mut()
            .find(|lane| lane.ledger.timeline == observation.timeline)
            .ok_or(RenderError::IncoherentAdmission(
                "the progress observation has no applied source timeline",
            ))?;
        if !lane.active
            || lane.ledger.permission != observation.permission
            || lane.ledger.format != observation.format
        {
            return Err(RenderError::IncoherentAdmission(
                "the progress observation does not match the applied source context",
            ));
        }
        if observation.end_frame > lane.ledger.high_water {
            lane.ledger.high_water = observation.end_frame;
        }
        if let Some(discard) = observation.discard_through {
            let old_floor = lane.ledger.closed_before;
            lane.ledger.discard_through(discard);
            lane.window.advance_origin(lane.ledger.closed_before);
            if lane.ledger.closed_before > old_floor {
                lane.reset_segment(Phase::AwaitCurrent);
            }
        }
        Ok(())
    }

    pub fn set_output(&mut self, setup: OutputSetup) -> Result<(), RenderError> {
        if !(8_000..=384_000).contains(&setup.sample_rate_hz)
            || setup.channels == 0
            || usize::from(setup.channels) > super::contract::ABSOLUTE_MAX_CHANNELS
            || setup.channels != setup.layout.channels()
            || setup.quantum_frames == 0
            || setup.quantum_frames as usize > self.output.capacity
            || setup.generation == 0
        {
            return Err(RenderError::InvalidConfig(
                "output setup exceeds the prepared rate/channel/quantum bounds",
            ));
        }
        if setup.generation < self.output.generation {
            return Err(RenderError::StaleCompletion);
        }
        if setup.generation == self.output.generation {
            return if self.output.available
                && setup.sample_rate_hz == self.output.rate
                && usize::from(setup.channels) == self.output.channels
                && setup.layout == self.output.layout
                && setup.quantum_frames == self.output.quantum
            {
                Ok(())
            } else {
                Err(RenderError::StaleCompletion)
            };
        }
        // Configuration-only preparation. A failed constructor leaves the
        // current generation, every kernel and all retirement untouched.
        let mut converters = Vec::with_capacity(self.lanes.len());
        for lane in &self.lanes {
            let source_policy = TimingPolicy {
                delivery_guard_frames: lane.delivery_guard_frames,
                ..self.policy
            };
            converters.push(prepare_converter(
                lane.ledger.format,
                &self.capacities,
                setup.sample_rate_hz,
                &source_policy,
            )?);
        }
        for (lane, converter) in self.lanes.iter_mut().zip(converters) {
            lane.converter = converter;
            lane.retire_queued();
            lane.edge = EdgeBlend::default();
            lane.gain = GainRamp::fixed(lane.gain.target());
            lane.submitted_frames = 0;
        }
        self.master_gain = GainRamp::fixed(self.master_gain.target());
        self.output.generation = setup.generation;
        self.output.rate = setup.sample_rate_hz;
        self.output.channels = usize::from(setup.channels);
        self.output.layout = setup.layout;
        self.output.quantum = setup.quantum_frames;
        self.output.pending = None;
        self.output.submitted = 0;
        self.output.has_receipt = false;
        self.output.available = true;
        Ok(())
    }

    pub fn set_master_gain(&mut self, gain: f64) -> Result<(), RenderError> {
        if !gain.is_finite() {
            return Err(RenderError::NumericalOverflow);
        }
        if self.output.has_receipt {
            self.master_gain
                .set_target(gain, smoothing_frames(self.output.rate));
        } else {
            self.master_gain = GainRamp::fixed(gain);
        }
        Ok(())
    }

    /// Configuration boundary after the peer owner has irreversibly replaced
    /// this member's remote run. Its old datagrams/admissions must already be
    /// rejected upstream. Same-run omitted ledgers remain retained.
    pub fn retire_obsolete_remote_runs(
        &mut self,
        member: rlb_model::MemberId,
        current: rlb_model::RunId,
    ) {
        self.lanes.retain(|lane| {
            !matches!(lane.ledger.timeline,
            TimelineId::Remote { member: owner, run, .. } if owner == member && run != current)
        });
        self.routes.retain(|route| {
            self.lanes
                .iter()
                .any(|lane| lane.ledger.timeline == route.timeline)
        });
        self.matrices.retain(|matrix| {
            self.lanes
                .iter()
                .any(|lane| lane.ledger.timeline == matrix.timeline)
        });
    }

    /// Cold endpoint-owner boundary after all detached producers have been
    /// irrevocably retired. Their queued packets must be rejected by binding
    /// identity before reaching this core. Ordinary apply_admission retains
    /// its same-run floor contract; the endpoint can release full DSP state
    /// once that upstream lifetime has ended.
    pub fn retire_detached_sources(&mut self, current: &[RenderSource]) {
        self.lanes.retain(|lane| {
            current
                .iter()
                .any(|source| source.timeline == lane.ledger.timeline)
        });
        self.routes.retain(|route| {
            current
                .iter()
                .any(|source| source.timeline == route.timeline)
        });
        self.matrices.retain(|matrix| {
            current
                .iter()
                .any(|source| source.timeline == matrix.timeline)
        });
    }

    /// Configuration boundary: the owner supplies the source's actual
    /// delivery-cadence guard in that source's frame units.
    pub fn set_source_delivery_guard(
        &mut self,
        timeline: TimelineId,
        frames: u64,
    ) -> Result<(), RenderError> {
        let lane = self
            .lanes
            .iter_mut()
            .find(|lane| lane.ledger.timeline == timeline)
            .ok_or(RenderError::IncoherentAdmission(
                "the guard has no current source timeline",
            ))?;
        if (lane.converter.max_input_frames() as u64)
            .checked_add(frames)
            .is_none_or(|needed| needed > self.capacities.window_frames as u64)
        {
            return Err(RenderError::Capacity(
                "source delivery guard exceeds its prepared window",
            ));
        }
        if lane.delivery_guard_frames != frames {
            lane.delivery_guard_frames = frames;
            lane.reset_segment(Phase::Priming);
        }
        Ok(())
    }

    pub fn set_source_gain(&mut self, gain: SourceGain) -> Result<(), RenderError> {
        if !gain.gain.is_finite() {
            return Err(RenderError::NumericalOverflow);
        }
        let lane = self
            .lanes
            .iter_mut()
            .find(|lane| lane.ledger.timeline == gain.timeline)
            .ok_or(RenderError::IncoherentAdmission(
                "the gain has no current source timeline",
            ))?;
        if lane.submitted_frames == 0 {
            lane.gain = GainRamp::fixed(gain.gain);
        } else {
            lane.gain
                .set_target(gain.gain, smoothing_frames(self.output.rate));
        }
        Ok(())
    }

    pub fn set_route(&mut self, route: ChannelRoute) -> Result<(), RenderError> {
        self.require_timeline(route.timeline)?;
        if route
            .output_for_source
            .iter()
            .flatten()
            .any(|index| usize::from(*index) >= super::contract::ABSOLUTE_MAX_CHANNELS)
        {
            return Err(RenderError::InvalidConfig(
                "channel route exceeds prepared output channels",
            ));
        }
        if let Some(existing) = self
            .routes
            .iter_mut()
            .find(|existing| existing.timeline == route.timeline)
        {
            if *existing != route {
                if let Some(lane) = self
                    .lanes
                    .iter_mut()
                    .find(|lane| lane.ledger.timeline == route.timeline)
                {
                    lane.segment_changed = true;
                }
            }
            *existing = route;
        } else {
            if self.routes.len() == self.capacities.sources {
                return Err(RenderError::Capacity(
                    "all prepared channel route slots are occupied",
                ));
            }
            self.routes.push(route);
        }
        Ok(())
    }

    /// Installs a finite matrix for a current source and the selected output
    /// shape. Validation completes before the previous matrix is replaced.
    pub fn set_channel_matrix(&mut self, matrix: ChannelMatrix) -> Result<(), RenderError> {
        self.require_timeline(matrix.timeline)?;
        if matrix.gains.iter().flatten().any(|gain| !gain.is_finite()) {
            return Err(RenderError::NumericalOverflow);
        }
        let source_channels = self
            .lanes
            .iter()
            .find(|lane| lane.ledger.timeline == matrix.timeline)
            .expect("the source timeline was checked")
            .source_channels();
        let source_layout = self
            .lanes
            .iter()
            .find(|lane| lane.ledger.timeline == matrix.timeline)
            .expect("the source timeline was checked")
            .ledger
            .format
            .layout();
        if matrix.source_layout != source_layout || matrix.output_layout != self.output.layout {
            return Err(RenderError::InvalidConfig(
                "channel matrix speaker layouts are stale",
            ));
        }
        if !matrix_fits(&matrix, source_channels, self.output.channels) {
            return Err(RenderError::InvalidConfig(
                "channel matrix exceeds current source or output channels",
            ));
        }
        if let Some(existing) = self
            .matrices
            .iter_mut()
            .find(|existing| existing.timeline == matrix.timeline)
        {
            if *existing != matrix {
                if let Some(lane) = self
                    .lanes
                    .iter_mut()
                    .find(|lane| lane.ledger.timeline == matrix.timeline)
                {
                    lane.segment_changed = true;
                }
            }
            *existing = matrix;
        } else {
            if self.matrices.len() == self.capacities.sources {
                return Err(RenderError::Capacity(
                    "all prepared channel matrix slots are occupied",
                ));
            }
            self.matrices.push(matrix);
        }
        Ok(())
    }

    pub fn prepare_quantum(
        &mut self,
        service: &ServiceObservation,
    ) -> Result<PrepareOutcome, RenderError> {
        self.apply_admission()?;
        if service.generation != self.output.generation {
            return Err(RenderError::StaleCompletion);
        }
        if !self.output.available {
            return Err(RenderError::OutputUnavailable);
        }
        if self.output.pending.is_some() {
            return Err(RenderError::Capacity(
                "the previous quantum is still waiting for its submission receipt",
            ));
        }
        if !service.selected {
            return Ok(PrepareOutcome::Waiting {
                generation: self.output.generation,
            });
        }
        let due = due_frames(service, self.output.quantum);
        if due == 0 {
            return Ok(PrepareOutcome::Waiting {
                generation: self.output.generation,
            });
        }
        let Some(next_quantum) = self.next_quantum.checked_add(1) else {
            return Err(RenderError::CounterExhausted);
        };
        let due = due as usize;
        self.unmapped = 0;
        let output_channels = self.output.channels;
        let output_rate = self.output.rate;
        let policy = self.policy;
        let missed = service.missed_service;
        for lane in &mut self.lanes {
            let mapped = mapping_for(
                &self.matrices,
                &self.routes,
                lane.ledger.timeline,
                lane.ledger.format.layout(),
                self.output.layout,
            )
            .is_some();
            lane.render(
                due,
                &policy,
                output_rate,
                self.output.quantum as usize,
                missed,
                mapped,
            );
        }
        let mix_len = due * output_channels;
        self.output.mix[..mix_len].fill(0.0);
        for lane in &mut self.lanes {
            let mapping = mapping_for(
                &self.matrices,
                &self.routes,
                lane.ledger.timeline,
                lane.ledger.format.layout(),
                self.output.layout,
            );
            lane.mix_into(
                &mut self.output.mix[..mix_len],
                output_channels,
                due,
                mapping,
                output_rate,
            );
        }
        let finished = finish_smoothed_f64(
            &mut self.output.mix[..mix_len],
            &mut self.output.pcm[..mix_len],
            output_channels,
            &mut self.master_gain,
        );
        let (peak, over_unity) = match finished {
            Ok(value) => value,
            Err(error) => {
                self.abandon_segments();
                return Err(error);
            }
        };
        let id = self.next_quantum;
        self.next_quantum = next_quantum;
        self.output.pending = Some((id, due as u32));
        Ok(PrepareOutcome::Prepared(QuantumHandle {
            id,
            generation: self.output.generation,
            frames: due as u32,
            channels: self.output.channels as u16,
            peak_abs: peak,
            over_unity,
        }))
    }

    pub fn complete(
        &mut self,
        receipt: SubmissionReceipt,
    ) -> Result<CompletionReport, RenderError> {
        let Some((id, frames)) = self.output.pending else {
            return Err(RenderError::StaleCompletion);
        };
        if receipt.generation != self.output.generation || receipt.quantum_id != id {
            return Err(RenderError::StaleCompletion);
        }
        if receipt.success {
            let submitted = receipt
                .end_frame
                .checked_sub(receipt.start_frame)
                .ok_or(RenderError::MalformedRange)?;
            if submitted != u64::from(frames) {
                return Err(RenderError::MalformedRange);
            }
            if self.output.has_receipt && receipt.start_frame != self.output.submitted {
                return Err(RenderError::MalformedRange);
            }
            self.output.submitted = receipt.end_frame;
            self.output.has_receipt = true;
            for lane in &mut self.lanes {
                lane.submitted_frames = lane
                    .submitted_frames
                    .saturating_add(lane.rendered_frames as u64);
            }
        } else {
            self.abandon_segments();
            self.output.available = false;
        }
        self.output.pending = None;
        Ok(CompletionReport {
            submitted_output_frames: self.output.submitted,
            accepted: receipt.success,
        })
    }

    #[must_use]
    pub fn output_interleaved(&self) -> &[f64] {
        let frames = self.output.pending.map(|(_, frames)| frames).unwrap_or(0);
        let length = frames as usize * self.output.channels;
        &self.output.pcm[..length]
    }

    #[must_use]
    pub fn submitted_frames(&self) -> u64 {
        self.output.submitted
    }

    fn require_timeline(&self, timeline: TimelineId) -> Result<(), RenderError> {
        if self
            .lanes
            .iter()
            .any(|lane| lane.ledger.timeline == timeline)
        {
            Ok(())
        } else {
            Err(RenderError::IncoherentAdmission(
                "the setting has no current source timeline",
            ))
        }
    }

    fn abandon_segments(&mut self) {
        for lane in &mut self.lanes {
            lane.retire_queued();
            lane.edge = EdgeBlend::default();
            lane.gain = GainRamp::fixed(lane.gain.target());
        }
        self.master_gain = GainRamp::fixed(self.master_gain.target());
    }

    #[must_use]
    pub fn applied_revision(&self) -> u64 {
        self.applied_revision
    }

    #[must_use]
    pub fn source_status(&self, timeline: TimelineId) -> Option<SourceStatus> {
        self.lanes
            .iter()
            .find(|lane| lane.ledger.timeline == timeline)
            .map(|lane| SourceStatus {
                closed_before: lane.ledger.closed_before,
                high_water: lane.ledger.high_water,
                phase: lane.phase,
                ratio: lane.converter.ratio(),
                cutoff: lane.converter.cutoff,
                active: lane.active,
                delivery_guard_frames: lane.delivery_guard_frames,
                submitted_frames: lane.submitted_frames,
                segment_restarts: lane.segment_restarts,
                last_error: lane.last_error,
            })
    }

    #[must_use]
    pub fn preset(&self) -> Preset {
        Preset {
            sinc_taps: super::contract::SINC_TAPS,
            phases: super::contract::SINC_PHASES,
            window: "BlackmanHarris2",
            interpolation: "cubic",
            cutoff: self.lanes.iter().find_map(|lane| lane.converter.cutoff),
            output_delay: self.lanes.first().map(|lane| lane.converter.output_delay()),
        }
    }

    #[cfg(test)]
    pub fn fail_next_process(&mut self, timeline: TimelineId) {
        if let Some(lane) = self
            .lanes
            .iter_mut()
            .find(|lane| lane.ledger.timeline == timeline)
        {
            lane.fail_next_process = true;
        }
    }

    #[cfg(test)]
    pub(super) fn exhaust_quantum_counter(&mut self) {
        self.next_quantum = u64::MAX;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub sinc_taps: usize,
    pub phases: usize,
    pub window: &'static str,
    pub interpolation: &'static str,
    pub cutoff: Option<f32>,
    pub output_delay: Option<usize>,
}

impl Lane {
    fn create(
        source: &RenderSource,
        capacities: &RenderCapacities,
        output_rate: u32,
        policy: &TimingPolicy,
    ) -> Result<Self, RenderError> {
        let channels = usize::from(source.format.channels());
        if channels == 0 || channels > super::contract::ABSOLUTE_MAX_CHANNELS {
            return Err(RenderError::IncoherentAdmission(
                "the source channel count is outside the prepared bound",
            ));
        }
        let converter = prepare_converter(source.format, capacities, output_rate, policy)?;
        Ok(Self {
            ledger: Ledger::new(source.timeline, source.permission, source.format),
            window: Window::new(channels, capacities.window_frames)?,
            phase: Phase::Priming,
            converter,
            controller: ReserveController::new(),
            target_frames: 0,
            delivery_guard_frames: policy.delivery_guard_frames,
            active: source.activity == SourceActivity::Active,
            rendered_frames: 0,
            submitted_frames: 0,
            segment_restarts: 0,
            last_error: None,
            gain: GainRamp::fixed(1.0),
            edge: EdgeBlend::default(),
            segment_changed: true,
            #[cfg(test)]
            fail_next_process: false,
        })
    }

    fn source_channels(&self) -> usize {
        usize::from(self.ledger.format.channels())
    }

    fn mix_into(
        &mut self,
        destination: &mut [f64],
        output_channels: usize,
        due: usize,
        mapping: Option<Mapping>,
        output_rate: u32,
    ) {
        // Closing permission or losing the speaker mapping remains immediate.
        // A brief blend is allowed only inside a still-admitted playback lane.
        if !self.active || mapping.is_none() {
            self.edge = EdgeBlend::default();
            return;
        }
        let source_channels = self.source_channels();
        let samples = self.converter.rendered(self.rendered_frames);
        self.edge.begin(
            self.rendered_frames != 0,
            self.segment_changed,
            smoothing_frames(output_rate),
        );
        self.segment_changed = false;
        for frame in 0..due {
            let gain = self.gain.next();
            let mut contribution = [0.0; super::contract::ABSOLUTE_MAX_CHANNELS];
            if frame < self.rendered_frames {
                match mapping.as_ref().expect("the speaker mapping was checked") {
                    Mapping::Matrix(matrix) => {
                        for (output, value) in
                            contribution.iter_mut().enumerate().take(output_channels)
                        {
                            for input in 0..source_channels {
                                *value += samples[frame * source_channels + input]
                                    * matrix.gains[output][input]
                                    * gain;
                            }
                        }
                    }
                    Mapping::Route(route) => {
                        for input in 0..source_channels {
                            if let Some(output) = route.output_for_source[input]
                                .map(usize::from)
                                .filter(|output| *output < output_channels)
                            {
                                contribution[output] +=
                                    samples[frame * source_channels + input] * gain;
                            }
                        }
                    }
                }
            }
            self.edge.apply(&mut contribution[..output_channels]);
            for output in 0..output_channels {
                destination[frame * output_channels + output] += contribution[output];
            }
        }
    }

    fn retire_queued(&mut self) {
        self.ledger.discard_through(self.ledger.high_water);
        self.window.advance_origin(self.ledger.closed_before);
        self.reset_segment(Phase::AwaitCurrent);
    }

    fn reset_segment(&mut self, phase: Phase) {
        self.converter.recover_nominal();
        self.controller = ReserveController::new();
        self.target_frames = 0;
        self.rendered_frames = 0;
        self.phase = phase;
        self.segment_changed = true;
        self.segment_restarts = self.segment_restarts.saturating_add(1);
    }

    fn fail_segment(&mut self, message: &'static str) {
        self.last_error = Some(message);
        self.reset_segment(Phase::AwaitCurrent);
    }

    fn render(
        &mut self,
        due: usize,
        policy: &TimingPolicy,
        output_rate: u32,
        full_quantum: usize,
        missed: bool,
        mapped: bool,
    ) {
        let source_policy = TimingPolicy {
            delivery_guard_frames: self.delivery_guard_frames,
            ..*policy
        };
        let policy = &source_policy;
        self.rendered_frames = 0;
        if !self.active || !mapped {
            self.retire_queued();
            return;
        }
        if missed {
            // No age clock is available here. Retire all known material and
            // await later PCM instead of replaying an old "newest" tail.
            self.retire_queued();
            return;
        }
        let nominal = self.converter.nominal();
        let was_running = self.phase == Phase::Running;
        if !was_running {
            let Ok((full_needed, _)) = self.converter.select(full_quantum, nominal) else {
                self.fail_segment("The rate converter could not prepare the output quantum.");
                return;
            };
            let Ok((needed, _)) = self.converter.select(due, nominal) else {
                self.fail_segment("The rate converter could not prepare the current output span.");
                return;
            };
            let back = (full_needed as u64).saturating_add(policy.delivery_guard_frames);
            if !self.try_latch(needed, back) {
                return;
            }
        }
        let reserve = self.ledger.reserve_frames();
        let target = self.target_frames;
        if i128::from(reserve) > i128::from(target) + i128::from(policy.excess_discard_frames) {
            self.reprime(policy, full_quantum);
            return;
        }
        let sample = RatioSample {
            nominal,
            reserve_frames: reserve,
            target_frames: target,
            sample_rate: self.ledger.format.sample_rate_hz(),
            dt_seconds: due as f64 / f64::from(output_rate),
            allow: false,
        };
        let held_ratio = self.controller.ratio(policy, sample);
        let Ok((mut needed, mut produced)) = self.converter.select(due, held_ratio) else {
            self.fail_segment("The rate converter could not select the current rate.");
            return;
        };
        let start = self.ledger.closed_before;
        let mut missing = self.window.missing(start, needed);
        if was_running && due == full_quantum && missing == 0 {
            // Stage a scalar controller candidate. If its requirement crosses
            // a hole, keep the previous rate and controller state for this loss.
            let mut candidate = self.controller.clone();
            let ratio = candidate.ratio(
                policy,
                RatioSample {
                    allow: true,
                    ..sample
                },
            );
            let Ok((new_needed, new_produced)) = self.converter.select(due, ratio) else {
                self.fail_segment("The rate converter could not select the corrected rate.");
                return;
            };
            if self.window.missing(start, new_needed) == 0 {
                self.controller = candidate;
                needed = new_needed;
                produced = new_produced;
                missing = 0;
            } else {
                let Ok((held_needed, held_produced)) = self.converter.select(due, held_ratio)
                else {
                    self.fail_segment("The rate converter could not retain the previous rate.");
                    return;
                };
                needed = held_needed;
                produced = held_produced;
                missing = self.window.missing(start, needed);
            }
        }
        if produced != due || needed > self.window.capacity_frames() {
            self.fail_segment("The rate converter exceeded its prepared span.");
            return;
        }
        if missing > policy.brief_missing_frames as usize {
            self.reset_segment(Phase::AwaitCurrent);
            return;
        }
        if was_running && due == full_quantum {
            self.remember_target(needed, policy);
        }
        let Some(end) = start.checked_add(needed as u64) else {
            self.fail_segment("The source frame coordinate overflowed.");
            return;
        };
        if self.stage(start, needed).is_err() {
            self.fail_segment("The original PCM window could not stage the planned input.");
            return;
        }
        self.ledger.close_through(end);
        self.window.advance_origin(self.ledger.closed_before);
        #[cfg(test)]
        if self.fail_next_process {
            self.fail_next_process = false;
            self.reset_segment(Phase::AwaitCurrent);
            return;
        }
        match self.converter.process() {
            Ok(()) => {
                self.rendered_frames = produced;
                self.phase = Phase::Running;
                self.last_error = None;
            }
            Err(_) => {
                self.fail_segment("The rate converter failed and is awaiting a fresh segment.");
            }
        }
    }

    fn try_latch(&mut self, needed: usize, back: u64) -> bool {
        let floor = self.ledger.closed_before;
        let extent = self.ledger.high_water;
        let mut candidate = extent.saturating_sub(back);
        if candidate < floor {
            candidate = floor;
        }
        let Some(end) = candidate.checked_add(needed as u64) else {
            return false;
        };
        if extent < end || extent.saturating_sub(candidate) < back {
            return false;
        }
        if !self.window.span_present(candidate, needed) {
            return false;
        }
        if candidate > floor {
            self.ledger.discard_through(candidate);
            self.window.advance_origin(self.ledger.closed_before);
        }
        self.target_frames = back;
        self.phase = Phase::Running;
        true
    }

    fn remember_target(&mut self, needed: usize, policy: &TimingPolicy) {
        let proposed = needed as u64 + policy.delivery_guard_frames;
        if self.target_frames == 0 || self.target_frames.abs_diff(proposed) > 4 {
            self.target_frames = proposed;
        }
    }

    fn reprime(&mut self, policy: &TimingPolicy, full_quantum: usize) {
        self.reset_segment(Phase::Priming);
        let Ok((needed, _)) = self
            .converter
            .select(full_quantum, self.converter.nominal())
        else {
            self.phase = Phase::AwaitCurrent;
            return;
        };
        let back = (needed as u64).saturating_add(policy.delivery_guard_frames);
        let mut candidate = self.ledger.high_water.saturating_sub(back);
        if candidate < self.ledger.closed_before {
            candidate = self.ledger.closed_before;
        }
        self.ledger.discard_through(candidate);
        self.window.advance_origin(self.ledger.closed_before);
    }

    fn stage(&mut self, start: u64, frames: usize) -> Result<(), RenderError> {
        let channels = self.source_channels();
        let length = frames * channels;
        self.window
            .stage_f64(start, frames, &mut self.converter.input_mut()[..length])
            .map(|_| ())
    }
}

impl RenderCore {
    fn reconcile(&mut self, admission: &RenderAdmission) -> Result<(), RenderError> {
        // Preflight every field, replacement and constructor before changing a
        // live lane. Only configuration changes construct windows/filters.
        for (index, source) in admission.sources.iter().enumerate() {
            if admission.sources[..index]
                .iter()
                .any(|prior| prior.timeline.same_logical_source(source.timeline))
            {
                return Err(RenderError::IncoherentAdmission(
                    "duplicate logical source in one admission",
                ));
            }
            match (source.timeline, source.permission) {
                (TimelineId::Remote { .. }, super::contract::Permission::Remote { fence, .. })
                    if fence > 0 => {}
                (
                    TimelineId::Local { .. },
                    super::contract::Permission::Local {
                        monitor_revision,
                        capture_instance,
                    },
                ) if monitor_revision > 0 && capture_instance > 0 => {}
                _ => {
                    return Err(RenderError::IncoherentAdmission(
                        "source timeline and permission are inconsistent",
                    ));
                }
            }
            if let Some(lane) = self
                .lanes
                .iter()
                .find(|lane| lane.ledger.timeline.same_logical_source(source.timeline))
            {
                if source.timeline == lane.ledger.timeline && source.format != lane.ledger.format {
                    return Err(RenderError::IncoherentAdmission(
                        "format changed without a new source timeline",
                    ));
                }
                if let (
                    TimelineId::Remote { run, epoch, .. },
                    TimelineId::Remote {
                        run: old_run,
                        epoch: old_epoch,
                        ..
                    },
                ) = (source.timeline, lane.ledger.timeline)
                {
                    if run == old_run && epoch < old_epoch {
                        return Err(RenderError::IncoherentAdmission(
                            "publication epoch went backwards",
                        ));
                    }
                }
                if source.timeline == lane.ledger.timeline {
                    match (source.permission, lane.ledger.permission) {
                        (
                            super::contract::Permission::Remote {
                                peer_instance,
                                fence,
                                ..
                            },
                            super::contract::Permission::Remote {
                                peer_instance: old_instance,
                                fence: old_fence,
                                ..
                            },
                        ) if peer_instance == old_instance && fence < old_fence => {
                            return Err(RenderError::IncoherentAdmission(
                                "source admission fence went backwards",
                            ));
                        }
                        (
                            super::contract::Permission::Local {
                                monitor_revision, ..
                            },
                            super::contract::Permission::Local {
                                monitor_revision: old_revision,
                                ..
                            },
                        ) if monitor_revision < old_revision => {
                            return Err(RenderError::IncoherentAdmission(
                                "monitor revision went backwards",
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        let kept = self
            .lanes
            .iter()
            .filter(|lane| {
                !admission.sources.iter().any(|source| {
                    source.timeline.same_logical_source(lane.ledger.timeline)
                        && source.timeline != lane.ledger.timeline
                })
            })
            .count();
        let added = admission
            .sources
            .iter()
            .filter(|source| {
                !self
                    .lanes
                    .iter()
                    .any(|lane| lane.ledger.timeline == source.timeline)
            })
            .count();
        if kept
            .checked_add(added)
            .is_none_or(|total| total > self.capacities.sources)
        {
            return Err(RenderError::Capacity(
                "still-reusable source ledgers occupy the prepared slots",
            ));
        }
        let mut prepared = Vec::with_capacity(added);
        for source in &admission.sources {
            if !self
                .lanes
                .iter()
                .any(|lane| lane.ledger.timeline == source.timeline)
            {
                prepared.push(Lane::create(
                    source,
                    &self.capacities,
                    self.output.rate,
                    &self.policy,
                )?);
            }
        }
        self.lanes.retain(|lane| {
            !admission.sources.iter().any(|source| {
                source.timeline.same_logical_source(lane.ledger.timeline)
                    && source.timeline != lane.ledger.timeline
            })
        });
        for lane in &mut self.lanes {
            if let Some(source) = admission
                .sources
                .iter()
                .find(|source| source.timeline == lane.ledger.timeline)
            {
                let was_active = lane.active;
                lane.active = source.activity == SourceActivity::Active;
                if source.permission != lane.ledger.permission {
                    lane.ledger.permission = source.permission;
                    lane.retire_queued();
                } else if was_active && !lane.active {
                    lane.retire_queued();
                } else if !was_active && lane.active {
                    lane.reset_segment(Phase::AwaitCurrent);
                }
            } else if lane.active {
                lane.active = false;
                lane.retire_queued();
            }
        }
        self.lanes.extend(prepared);
        self.routes.retain(|route| {
            self.lanes
                .iter()
                .any(|lane| lane.ledger.timeline == route.timeline)
        });
        self.matrices.retain(|matrix| {
            self.lanes
                .iter()
                .any(|lane| lane.ledger.timeline == matrix.timeline)
        });
        Ok(())
    }
}

fn prepare_converter(
    format: rlb_peer::PcmFormat,
    capacities: &RenderCapacities,
    output_rate: u32,
    policy: &TimingPolicy,
) -> Result<RateConverter, RenderError> {
    let converter = RateConverter::prepare(
        format.sample_rate_hz(),
        output_rate,
        usize::from(format.channels()),
        capacities.max_quantum_frames,
        policy.max_ratio_relative,
    )?;
    if (converter.max_input_frames() as u64)
        .checked_add(policy.delivery_guard_frames)
        .is_none_or(|needed| needed > capacities.window_frames as u64)
    {
        return Err(RenderError::Capacity(
            "converter input plus delivery guard exceeds the source window",
        ));
    }
    Ok(converter)
}

enum Mapping {
    Matrix(ChannelMatrix),
    Route(ChannelRoute),
}

fn matrix_fits(matrix: &ChannelMatrix, source_channels: usize, output_channels: usize) -> bool {
    matrix.gains.iter().enumerate().all(|(output, row)| {
        row.iter().enumerate().all(|(source, gain)| {
            (output < output_channels && source < source_channels) || *gain == 0.0
        })
    })
}

fn mapping_for(
    matrices: &[ChannelMatrix],
    routes: &[ChannelRoute],
    timeline: TimelineId,
    source_layout: ChannelLayout,
    output_layout: ChannelLayout,
) -> Option<Mapping> {
    let source_channels = usize::from(source_layout.channels());
    let output_channels = usize::from(output_layout.channels());
    if let Some(matrix) = matrices.iter().find(|matrix| matrix.timeline == timeline) {
        // A previously accepted matrix can become unusable when the output
        // format changes. Its explicit mapping still takes precedence.
        let usable = matrix.source_layout == source_layout
            && matrix.output_layout == output_layout
            && matrix_fits(matrix, source_channels, output_channels)
            && matrix.gains.iter().flatten().any(|gain| *gain != 0.0);
        return usable.then_some(Mapping::Matrix(*matrix));
    }
    if let Some(route) = routes.iter().find(|route| route.timeline == timeline) {
        let usable = route
            .output_for_source
            .iter()
            .take(source_channels)
            .any(|channel| channel.is_some_and(|index| usize::from(index) < output_channels));
        return usable.then_some(Mapping::Route(*route));
    }
    default_channel_matrix(timeline, source_layout, output_layout, 0.0)
        .ok()
        .map(Mapping::Matrix)
}

#[cfg(test)]
mod matrix_tests {
    use super::*;
    use rlb_model::{RunId, SourceId, StreamEpoch};
    use rlb_peer::{ChannelLayout, PcmEncoding, PcmFormat};

    #[test]
    fn invalid_matrix_keeps_the_current_configuration_and_explicit_precedence() {
        let timeline = TimelineId::Local {
            run: RunId::from_bytes([1; 16]).unwrap(),
            source: SourceId::from_bytes([2; 16]).unwrap(),
            acquisition: StreamEpoch::from_bytes([3; 16]).unwrap(),
        };
        let mut core = RenderCore::new(
            RenderCapacities {
                sources: 1,
                window_frames: 4_096,
                max_block_frames: 1_024,
                max_quantum_frames: 64,
                output_channels: 1,
                output_layout: ChannelLayout::Mono,
                output_rate_hz: 48_000,
                output_generation: 1,
                quantum_frames: 64,
            },
            TimingPolicy::provisional(),
        )
        .unwrap();
        core.queue_admission(RenderAdmission {
            revision: 1,
            sources: vec![RenderSource {
                timeline,
                permission: super::super::Permission::Local {
                    monitor_revision: 1,
                    capture_instance: 1,
                },
                activity: SourceActivity::Active,
                format: PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32)
                    .unwrap(),
                kind: None,
            }],
        })
        .unwrap();
        core.apply_admission().unwrap();
        // The legacy route is deliberately unmapped; the matrix must win.
        core.set_route(ChannelRoute {
            timeline,
            output_for_source: [None; super::super::ABSOLUTE_MAX_CHANNELS],
        })
        .unwrap();
        let original = ChannelMatrix {
            timeline,
            source_layout: ChannelLayout::Mono,
            output_layout: ChannelLayout::Mono,
            gains: expanded([[0.75, 0.0], [0.0, 0.0]]),
        };
        core.set_channel_matrix(original).unwrap();
        for gains in [
            [[f64::NAN, 0.0], [0.0, 0.0]],
            [[f64::INFINITY, 0.0], [0.0, 0.0]],
            [[1.0, 0.25], [0.0, 0.0]],
            [[1.0, 0.0], [0.25, 0.0]],
        ] {
            assert!(
                core.set_channel_matrix(ChannelMatrix {
                    gains: expanded(gains),
                    ..original
                })
                .is_err()
            );
            assert_eq!(core.matrices, vec![original]);
            assert!(matches!(
                mapping_for(&core.matrices, &core.routes, timeline, ChannelLayout::Mono, ChannelLayout::Mono),
                Some(Mapping::Matrix(matrix)) if matrix == original
            ));
        }
    }

    fn expanded(
        values: [[f64; 2]; 2],
    ) -> [[f64; super::super::ABSOLUTE_MAX_CHANNELS]; super::super::ABSOLUTE_MAX_CHANNELS] {
        let mut gains =
            [[0.0; super::super::ABSOLUTE_MAX_CHANNELS]; super::super::ABSOLUTE_MAX_CHANNELS];
        for row in 0..2 {
            gains[row][..2].copy_from_slice(&values[row]);
        }
        gains
    }
}
