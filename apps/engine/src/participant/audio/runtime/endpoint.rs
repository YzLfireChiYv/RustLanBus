//! One resolved output endpoint. Native COM, MMCSS and its render core stay here.
//!
//! Producers publish current admission separately from the bounded PCM queue.
//! Queue pressure burns the affected range; it never holds up a UDP producer.

use super::super::render::{
    self, OutputSetup, PrepareOutcome, ProgressObservation, RenderAdmission, RenderCapacities,
    RenderCore, RenderSource, ServiceObservation, SourceBlock, SourceGain, SubmissionReceipt,
    TimelineId, TimingPolicy,
};
#[cfg(not(test))]
use super::devices::{BindingView, DeviceHandle, EndpointBinding, OutputDeviceHandle};
#[cfg(not(test))]
use super::routes::RouteGuard;
#[cfg(not(test))]
use super::routes::RouteLease;
use crate::participant::config::{MixIntent, ParticipantConfig};
#[cfg(not(test))]
use rlb_audio::RenderBackend;
use rlb_audio::SoftLimiter;
use rlb_audio_windows::{AudioWake, NativeF32Format, RenderClockFailure, RenderClockObservation};
#[cfg(not(test))]
use rlb_audio_windows::{
    EndpointSelection, MmcssRegistration, NativeRenderRequest, SharedPeriodPreference,
    WasapiRenderStream,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PCM_SAMPLES: usize = 280;
const PCM_QUEUE: usize = 512;
const SOURCES: usize = render::ABSOLUTE_MAX_SOURCES;
// Processing capacity, never an accumulation threshold. Large native service
// periods are submitted in immediately available bounded pieces. This also
// fits the full native rate-ratio range in the prepared source window.
const OUTPUT_CHUNK: usize = 128;
const RETRY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub(crate) struct OutputStatus {
    pub(crate) outputs: Vec<(String, OutputStatus)>,
    pub(crate) main_endpoint_id: Option<String>,
    pub(crate) state: &'static str,
    pub(crate) generation: u64,
    pub(crate) endpoint_id: Option<String>,
    pub(crate) rate_hz: Option<u32>,
    pub(crate) channels: Option<u8>,
    pub(crate) channel_mask: Option<u32>,
    pub(crate) period_frames: Option<u32>,
    pub(crate) target_padding_frames: Option<u32>,
    pub(crate) buffer_frames: Option<u32>,
    pub(crate) submitted_frames: u64,
    pub(crate) admission_revision: u64,
    pub(crate) overloaded: Option<bool>,
    pub(crate) peak_abs: Option<f64>,
    pub(crate) peak_observed: Option<Instant>,
    pub(crate) dropped_blocks: u64,
    pub(crate) last_error: Option<String>,
    pub(crate) source_errors: Vec<crate::participant::snapshot::SourceOutputError>,
    pub(crate) sources: Vec<crate::participant::snapshot::SourcePlayoutState>,
    pub(crate) clock: Option<RenderClockObservation>,
    pub(crate) clock_error: Option<String>,
    pub(crate) scheduling_error: Option<String>,
}

impl OutputStatus {
    pub(crate) fn presentation(&self) -> crate::participant::snapshot::OutputState {
        crate::participant::snapshot::OutputState {
            phase: self.state,
            generation: self.generation,
            actual_endpoint_id: self.endpoint_id.clone(),
            sample_rate_hz: self.rate_hz,
            channels: self.channels,
            channel_mask: self.channel_mask,
            period_frames: self.period_frames,
            target_padding_frames: self.target_padding_frames,
            buffer_frames: self.buffer_frames,
            dropped_blocks: self.dropped_blocks,
            last_error: self.last_error.clone(),
            source_errors: self.source_errors.clone(),
            sources: self.sources.clone(),
            clock_position_ticks: self.clock.map(|clock| clock.position_ticks),
            clock_ticks_per_second: self.clock.map(|clock| clock.ticks_per_second.get()),
            clock_correlated_qpc_100ns: self.clock.map(|clock| clock.correlated_qpc_100ns),
            clock_result_code: self.clock.map(|clock| clock.result_code),
            clock_error: self.clock_error.clone(),
            scheduling_error: self.scheduling_error.clone(),
            peak_db: (self.state == "running")
                .then(|| {
                    crate::participant::snapshot::recent_peak_db(self.peak_abs, self.peak_observed)
                })
                .flatten(),
        }
    }
}

struct Progress {
    active: AtomicBool,
    origin: Instant,
    last_arrival_us: AtomicU64,
    gap_us: AtomicU64,
    gap_window: AtomicU64,
    window_gap_us: AtomicU64,
    previous_gap_us: AtomicU64,
    end: AtomicU64,
    discard: AtomicU64,
    block_frames: AtomicU64,
    dropped: AtomicU64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(true),
            origin: Instant::now(),
            last_arrival_us: AtomicU64::new(0),
            gap_us: AtomicU64::new(0),
            gap_window: AtomicU64::new(0),
            window_gap_us: AtomicU64::new(0),
            previous_gap_us: AtomicU64::new(0),
            end: AtomicU64::new(0),
            discard: AtomicU64::new(0),
            block_frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

#[derive(Clone)]
pub(crate) struct EndpointIngress {
    source: RenderSource,
    binding: u64,
    progress: Arc<Progress>,
    sender: SyncSender<Packet>,
    wake: AudioWake,
}

impl EndpointIngress {
    pub(crate) fn source(&self) -> RenderSource {
        self.source
    }

    pub(crate) fn same_binding(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.progress, &other.progress)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.progress.active.load(Ordering::Acquire)
    }

    pub(crate) fn retire(&self) {
        self.progress.active.store(false, Ordering::Release);
        let _ = self.wake.signal();
    }

    pub(crate) fn begin_at(&self, end: u64) {
        self.progress.end.fetch_max(end, Ordering::Release);
        self.progress.discard.fetch_max(end, Ordering::Release);
        let _ = self.wake.signal();
    }

    /// Split immediately into prepared queue slots; never wait to aggregate.
    #[cfg(test)]
    pub(crate) fn push(&self, block: SourceBlock<'_>) -> Result<(), String> {
        self.push_observed(block, Instant::now())
    }

    pub(crate) fn discard_through(&self, end: u64) {
        if !self.progress.active.load(Ordering::Acquire) {
            return;
        }
        self.progress.end.fetch_max(end, Ordering::Release);
        self.progress.discard.fetch_max(end, Ordering::Release);
        self.progress.dropped.fetch_add(1, Ordering::Relaxed);
        let _ = self.wake.signal();
    }

    pub(crate) fn push_observed(
        &self,
        block: SourceBlock<'_>,
        observed: Instant,
    ) -> Result<(), String> {
        if !self.progress.active.load(Ordering::Acquire) {
            return Err("This playback ingress has been retired.".into());
        }
        if block.timeline != self.source.timeline
            || block.permission != self.source.permission
            || block.format != self.source.format
        {
            return Err("PCM has a superseded output admission.".into());
        }
        let channels = usize::from(block.format.channels());
        if block.samples.is_empty()
            || !block.samples.len().is_multiple_of(channels)
            || block.samples.iter().any(|sample| !sample.is_finite())
        {
            return Err("Output PCM is empty, malformed or non-finite.".into());
        }
        let frames = block.samples.len() / channels;
        let end = block
            .start_frame
            .checked_add(frames as u64)
            .ok_or("Output source frame coordinate exhausted.")?;
        self.progress.end.fetch_max(end, Ordering::Release);
        self.progress
            .block_frames
            .fetch_max(frames as u64, Ordering::Release);
        let arrival_us = u64::try_from(self.progress.origin.elapsed().as_micros())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let previous = self
            .progress
            .last_arrival_us
            .swap(arrival_us, Ordering::AcqRel);
        let gap = arrival_us.saturating_sub(previous);
        // Observe delivery cadence, including gaps between native callback
        // bursts. Long outages are current-sound recovery, never a new target.
        let window = arrival_us / 2_000_000;
        let old_window = self.progress.gap_window.swap(window, Ordering::AcqRel);
        if window != old_window {
            let old_peak = self.progress.window_gap_us.swap(0, Ordering::AcqRel);
            self.progress.previous_gap_us.store(
                if window == old_window.saturating_add(1) {
                    old_peak
                } else {
                    0
                },
                Ordering::Release,
            );
        }
        if previous != 0 && gap <= 50_000 {
            self.progress
                .window_gap_us
                .fetch_max(gap, Ordering::Release);
        }
        // Two short observation windows let a past jitter spike age away.
        self.progress.gap_us.store(
            self.progress
                .window_gap_us
                .load(Ordering::Acquire)
                .max(self.progress.previous_gap_us.load(Ordering::Acquire)),
            Ordering::Release,
        );
        let mut offset = 0;
        let mut dropped = false;
        for samples in block.samples.chunks((PCM_SAMPLES / channels) * channels) {
            if !self.progress.active.load(Ordering::Acquire) {
                return Err("This playback ingress has been retired.".into());
            }
            let mut packet = Packet {
                source: self.source,
                binding: self.binding,
                start: block.start_frame + offset,
                arrived_at_ms: block.arrived_at_ms,
                observed,
                len: samples.len(),
                samples: [0.0; PCM_SAMPLES],
            };
            packet.samples[..samples.len()].copy_from_slice(samples);
            if self.sender.try_send(packet).is_err() {
                dropped = true;
                break;
            }
            offset += (samples.len() / channels) as u64;
        }
        if dropped {
            // Close the complete original block, including any prefix queued
            // above. The consumer observes this fence before its next service.
            self.progress.discard.fetch_max(end, Ordering::Release);
            self.progress.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.wake.signal().map_err(|error| error.to_string())?;
        if dropped {
            Err("The bounded playback queue dropped an expired range.".into())
        } else {
            Ok(())
        }
    }
}

struct Packet {
    source: RenderSource,
    binding: u64,
    start: u64,
    arrived_at_ms: u64,
    observed: Instant,
    len: usize,
    samples: [f64; PCM_SAMPLES],
}

struct Control {
    revision: u64,
    remote: Vec<EndpointIngress>,
    local: Vec<EndpointIngress>,
    config: ParticipantConfig,
    refresh: u64,
    #[cfg(test)]
    software_selected: bool,
}

#[derive(Clone)]
pub(crate) struct EndpointHandle {
    control_revision: Arc<AtomicU64>,
    #[cfg(not(test))]
    routes: RouteGuard,
    #[cfg(not(test))]
    devices: OutputDeviceHandle,
    control: Arc<Mutex<Control>>,
    sender: SyncSender<Packet>,
    wake: AudioWake,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<OutputStatus>>,
    #[cfg(test)]
    inspect: SyncSender<Inspection>,
}

pub(crate) struct EndpointRuntime {
    pub(crate) handle: EndpointHandle,
    owner: Option<JoinHandle<()>>,
}

#[cfg(test)]
type Inspection = Box<dyn FnOnce(&RenderCore) + Send>;

impl EndpointRuntime {
    pub(crate) fn start(
        config: ParticipantConfig,
        #[cfg(not(test))] devices: &DeviceHandle,
        #[cfg(not(test))] routes: RouteGuard,
    ) -> Result<Self, String> {
        let wake = AudioWake::new().map_err(|error| error.to_string())?;
        #[cfg(not(test))]
        let device = devices.register_output(
            config
                .output_endpoint_id
                .clone()
                .ok_or("Endpoint output requires a resolved fixed device ID.")?,
            wake.clone(),
        )?;
        let (sender, receiver) = mpsc::sync_channel(PCM_QUEUE);
        let handle = EndpointHandle {
            control_revision: Arc::new(AtomicU64::new(1)),
            #[cfg(not(test))]
            routes,
            #[cfg(not(test))]
            devices: device,
            control: Arc::new(Mutex::new(Control {
                revision: 1,
                remote: Vec::new(),
                local: Vec::new(),
                config,
                refresh: 0,
                #[cfg(test)]
                software_selected: false,
            })),
            sender,
            wake,
            stop: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(OutputStatus {
                state: "idle",
                ..OutputStatus::default()
            })),
            #[cfg(test)]
            inspect: {
                let (tx, _) = mpsc::sync_channel(1);
                tx
            },
        };
        #[cfg(test)]
        let (handle, inspections) = {
            let (tx, rx) = mpsc::sync_channel(16);
            let mut handle = handle;
            handle.inspect = tx;
            (handle, rx)
        };
        let owner_handle = handle.clone();
        let (ready, boot) = mpsc::sync_channel(1);
        let owner = thread::Builder::new()
            .name("rlb-output".into())
            .spawn(move || match Owner::new(owner_handle, receiver) {
                Ok(mut owner) => {
                    let _ = ready.send(Ok(()));
                    owner.run(
                        #[cfg(test)]
                        inspections,
                    );
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| format!("The output owner could not start: {error}"))?;
        match boot.recv() {
            Ok(Ok(())) => Ok(Self {
                handle,
                owner: Some(owner),
            }),
            outcome => {
                let _ = owner.join();
                Err(outcome
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| "The output owner stopped at startup.".into()))
            }
        }
    }

    pub(crate) fn stop(&mut self) {
        self.handle.stop.store(true, Ordering::Release);
        let _ = self.handle.wake.signal();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

impl Drop for EndpointRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

impl EndpointHandle {
    pub(crate) fn configure(&self, config: &ParticipantConfig) -> Result<(), String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.config != *config {
            let audio_changed = control.config.output_endpoint_id != config.output_endpoint_id
                || control.config.master_gain_db != config.master_gain_db
                || control.config.limiter_enabled != config.limiter_enabled
                || control.remote.iter().chain(&control.local).any(|input| {
                    mix_for(&control.config, input.source.timeline)
                        != mix_for(config, input.source.timeline)
                });
            if audio_changed {
                let revision = control
                    .revision
                    .checked_add(1)
                    .ok_or("Output control revision exhausted.")?;
                control.revision = revision;
            }
            control.config = config.clone();
            self.control_revision
                .store(control.revision, Ordering::Release);
        }
        drop(control);
        self.wake.signal().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) fn replace_remote(
        &self,
        sources: Vec<RenderSource>,
    ) -> Result<Vec<EndpointIngress>, String> {
        self.replace(sources, false)
    }

    /// Local monitor admission is independent of remote room admission.
    pub(crate) fn replace_local(
        &self,
        sources: Vec<RenderSource>,
    ) -> Result<Vec<EndpointIngress>, String> {
        self.replace(sources, true)
    }

    fn replace(
        &self,
        sources: Vec<RenderSource>,
        local: bool,
    ) -> Result<Vec<EndpointIngress>, String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = if local {
            &control.local
        } else {
            &control.remote
        };
        if sources
            .iter()
            .eq(previous.iter().map(|ingress| &ingress.source))
            && previous.iter().all(EndpointIngress::is_active)
        {
            return Ok(previous.clone());
        }
        if sources.len()
            + if local {
                control.remote.len()
            } else {
                control.local.len()
            }
            > SOURCES
        {
            return Err("Output admission exceeds the prepared dual-source group capacity.".into());
        }
        let revision = control
            .revision
            .checked_add(1)
            .ok_or("Output admission revision exhausted.")?;
        let next: Vec<_> = sources
            .into_iter()
            .map(|source| {
                previous
                    .iter()
                    .find(|ingress| ingress.source == source && ingress.is_active())
                    .cloned()
                    .unwrap_or_else(|| EndpointIngress {
                        source,
                        binding: revision,
                        progress: Arc::new(Progress::default()),
                        sender: self.sender.clone(),
                        wake: self.wake.clone(),
                    })
            })
            .collect();
        for previous in previous {
            if !next
                .iter()
                .any(|current| Arc::ptr_eq(&current.progress, &previous.progress))
            {
                previous.progress.active.store(false, Ordering::Release);
            }
        }
        if local {
            control.local = next.clone();
        } else {
            control.remote = next.clone();
        }
        control.revision = revision;
        self.control_revision.store(revision, Ordering::Release);
        drop(control);
        self.wake.signal().map_err(|error| error.to_string())?;
        Ok(next)
    }

    pub(crate) fn refresh(&self) -> Result<(), String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let refresh = control
            .refresh
            .checked_add(1)
            .ok_or("Output refresh counter exhausted.")?;
        let revision = control
            .revision
            .checked_add(1)
            .ok_or("Output control revision exhausted.")?;
        control.refresh = refresh;
        control.revision = revision;
        self.control_revision.store(revision, Ordering::Release);
        drop(control);
        #[cfg(not(test))]
        self.devices.refresh()?;
        self.wake.signal().map_err(|error| error.to_string())
    }

    pub(crate) fn status(&self) -> OutputStatus {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn select_software(&self) {
        let mut control = self.control.lock().unwrap();
        if control.software_selected {
            return;
        }
        control.software_selected = true;
        control.revision += 1;
        self.control_revision
            .store(control.revision, Ordering::Release);
        drop(control);
        self.wake.signal().unwrap();
    }

    #[cfg(test)]
    pub(crate) fn inspect<T: Send + 'static>(
        &self,
        inspect: impl FnOnce(&RenderCore) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::sync_channel(1);
        self.inspect
            .send(Box::new(move |core| {
                let _ = tx.send(inspect(core));
            }))
            .unwrap();
        self.wake.signal().unwrap();
        rx.recv().unwrap()
    }
}

struct Owner {
    handle: EndpointHandle,
    receiver: Receiver<Packet>,
    core: RenderCore,
    revision: u64,
    sources: Vec<EndpointIngress>,
    config: ParticipantConfig,
    refresh: u64,
    opened_refresh: u64,
    generation: u64,
    stream: Option<Device>,
    retry_at: Instant,
    #[cfg(not(test))]
    binding: BindingView,
    last_service: Instant,
    pcm: Vec<f64>,
    status: OutputStatus,
    clock_failure: Option<RenderClockFailure>,
    #[cfg(test)]
    software_selected: bool,
}

impl Owner {
    fn new(handle: EndpointHandle, receiver: Receiver<Packet>) -> Result<Self, String> {
        let config = handle.control.lock().unwrap().config.clone();
        let core = RenderCore::new(
            RenderCapacities {
                sources: SOURCES,
                window_frames: render::ABSOLUTE_MAX_WINDOW_FRAMES,
                max_block_frames: render::ABSOLUTE_MAX_BLOCK_FRAMES,
                max_quantum_frames: OUTPUT_CHUNK,
                output_channels: 2,
                output_layout: rlb_peer::ChannelLayout::StereoLeftRight,
                output_rate_hz: 48_000,
                output_generation: 1,
                quantum_frames: 64,
            },
            TimingPolicy {
                delivery_guard_frames: 0,
                ..TimingPolicy::provisional()
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            handle,
            receiver,
            core,
            revision: 0,
            sources: Vec::new(),
            config,
            refresh: 0,
            opened_refresh: 0,
            generation: 1,
            stream: None,
            retry_at: Instant::now(),
            #[cfg(not(test))]
            binding: BindingView::default(),
            last_service: Instant::now(),
            pcm: vec![0.; OUTPUT_CHUNK * rlb_model::pcm::MAX_CHANNELS],
            status: OutputStatus {
                state: "idle",
                ..OutputStatus::default()
            },
            clock_failure: None,
            #[cfg(test)]
            software_selected: false,
        })
    }

    fn run(&mut self, #[cfg(test)] inspections: Receiver<Inspection>) {
        while !self.handle.stop.load(Ordering::Acquire) {
            if let Err(error) = self.turn() {
                self.fail(error);
            }
            #[cfg(test)]
            if let Ok(inspect) = inspections.try_recv() {
                inspect(&self.core);
            }
            self.publish();
            let wait = match &self.stream {
                Some(stream) => stream.wait(&self.handle.wake, 250),
                None => self
                    .handle
                    .wake
                    .wait(if self.sources.is_empty() {
                        u32::MAX
                    } else {
                        500
                    })
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
            };
            if let Err(error) = wait {
                self.fail(error);
                self.publish();
            }
        }
        self.close();
        self.status.state = "stopped";
        self.publish();
    }

    fn turn(&mut self) -> Result<(), String> {
        self.sync_control()?;
        self.ensure_stream()?;
        self.sync_progress()?;
        for _ in 0..PCM_QUEUE {
            match self.receiver.try_recv() {
                Ok(packet) => self.ingest(packet),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        // Controls are authoritative even when the PCM queue stays full.
        self.sync_control()?;
        self.ensure_stream()?;
        self.sync_progress()?;
        if self.stream.is_some() && !self.service()? {
            // A newer control arrived during preparation. Its boundary wins;
            // the abandoned quantum requires a fresh output generation.
            self.close();
            self.retry_at = Instant::now();
            self.status.state = "reconfiguring";
            let _ = self.handle.wake.signal();
        }
        Ok(())
    }

    fn ensure_stream(&mut self) -> Result<(), String> {
        let binding_changed = self.binding_changed()?;
        let selected = self
            .sources
            .iter()
            .any(|ingress| ingress.source.activity == render::SourceActivity::Active);
        #[cfg(test)]
        let selected = selected && self.software_selected;
        if !selected {
            self.close();
            self.status.state = "idle";
            self.status.last_error = None;
        } else {
            if self.stream.is_some() && (self.opened_refresh != self.refresh || binding_changed) {
                self.close();
                self.retry_at = Instant::now();
            }
            if self.stream.is_none() && Instant::now() >= self.retry_at {
                self.open()?;
            }
        }
        Ok(())
    }

    fn sync_control(&mut self) -> Result<(), String> {
        if self.revision == self.handle.control_revision.load(Ordering::Acquire)
            && self.sources.iter().all(EndpointIngress::is_active)
        {
            return Ok(());
        }
        let control = self
            .handle
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.refresh = control.refresh;
        #[cfg(test)]
        {
            self.software_selected = control.software_selected;
        }
        if self.revision == control.revision && self.sources.iter().all(EndpointIngress::is_active)
        {
            return Ok(());
        }
        let config = control.config.clone();
        let sources: Vec<_> = control
            .remote
            .iter()
            .chain(&control.local)
            .filter(|ingress| ingress.is_active())
            .cloned()
            .collect();
        let revision = control.revision;
        drop(control);
        if self.config.output_endpoint_id != config.output_endpoint_id {
            self.close();
            self.retry_at = Instant::now();
        }
        for ingress in &sources {
            if let TimelineId::Remote { member, run, .. } = ingress.source.timeline {
                self.core.retire_obsolete_remote_runs(member, run);
            }
        }
        for prior in &self.sources {
            if sources.iter().any(|current| {
                current.source.timeline == prior.source.timeline && !current.same_binding(prior)
            }) {
                if let Some(status) = self.core.source_status(prior.source.timeline) {
                    let _ = self.core.observe_progress(ProgressObservation {
                        timeline: prior.source.timeline,
                        permission: prior.source.permission,
                        format: prior.source.format,
                        end_frame: status.high_water,
                        discard_through: Some(status.high_water),
                    });
                }
            }
        }
        let admission_revision = self
            .core
            .applied_revision()
            .checked_add(1)
            .ok_or("Output render admission revision exhausted.")?;
        // EndpointHandle::replace retired every superseded binding before
        // publishing this source set. ingest checks the current binding and
        // active flag, so detached full DSP resources can end here without
        // another lifetime-wide source-history registry.
        let current: Vec<_> = sources.iter().map(|ingress| ingress.source).collect();
        self.core.retire_detached_sources(&current);
        self.core
            .queue_admission(RenderAdmission {
                revision: admission_revision,
                sources: current,
            })
            .map_err(|error| error.to_string())?;
        self.core
            .apply_admission()
            .map_err(|error| error.to_string())?;
        self.sources = sources;
        self.config = config;
        self.revision = revision;
        self.configure_mix()?;
        Ok(())
    }

    fn configure_mix(&mut self) -> Result<(), String> {
        self.status.source_errors.clear();
        let output_layout = rlb_peer::ChannelLayout::from_mask(
            u16::from(self.status.channels.unwrap_or(2)),
            self.status.channel_mask.unwrap_or(3),
        )
        .map_err(|error| error.to_string())?;
        let has_solo = self
            .sources
            .iter()
            .any(|ingress| mix_for(&self.config, ingress.source.timeline).solo);
        self.core
            .set_master_gain(db_gain(self.config.master_gain_db))
            .map_err(|error| error.to_string())?;
        for ingress in &self.sources {
            let source = ingress.source;
            if source.activity != render::SourceActivity::Active {
                continue;
            }
            let mix = mix_for(&self.config, source.timeline);
            let gain = if mix.mute || (has_solo && !mix.solo) {
                0.
            } else {
                db_gain(mix.gain_db)
            };
            self.core
                .set_source_gain(SourceGain {
                    timeline: source.timeline,
                    gain,
                })
                .map_err(|error| error.to_string())?;
            let matrix = match render::default_channel_matrix(
                source.timeline,
                source.format.layout(),
                output_layout,
                f64::from(mix.pan),
            ) {
                Ok(matrix) => matrix,
                Err(error @ render::RenderError::UnsupportedLayout { .. }) => {
                    self.core
                        .set_source_gain(SourceGain {
                            timeline: source.timeline,
                            gain: 0.0,
                        })
                        .map_err(|error| error.to_string())?;
                    let (member_id, source_id) = match source.timeline {
                        TimelineId::Remote { member, source, .. } => (Some(member), source),
                        TimelineId::Local { source, .. } => (None, source),
                    };
                    self.status.source_errors.push(
                        crate::participant::snapshot::SourceOutputError {
                            member_id,
                            source_id,
                            detail: error.to_string(),
                        },
                    );
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            };
            self.core
                .set_channel_matrix(matrix)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn sync_progress(&mut self) -> Result<(), String> {
        let mut dropped = 0u64;
        for ingress in &self.sources {
            if !ingress.progress.active.load(Ordering::Acquire) {
                continue;
            }
            let source = ingress.source;
            if source.activity != render::SourceActivity::Active {
                continue;
            }
            let end = ingress.progress.end.load(Ordering::Acquire);
            let discard = ingress.progress.discard.load(Ordering::Acquire);
            dropped = dropped.saturating_add(ingress.progress.dropped.load(Ordering::Relaxed));
            self.core
                .observe_progress(ProgressObservation {
                    timeline: source.timeline,
                    permission: source.permission,
                    format: source.format,
                    end_frame: end,
                    discard_through: (discard > 0).then_some(discard),
                })
                .map_err(|error| error.to_string())?;
            let period_guard = u64::from(self.status.period_frames.unwrap_or(1))
                * u64::from(source.format.sample_rate_hz())
                / u64::from(self.status.rate_hz.unwrap_or(48_000));
            let block_guard = ingress
                .progress
                .block_frames
                .load(Ordering::Acquire)
                .max(period_guard)
                .max(1);
            let gap_guard = ingress
                .progress
                .gap_us
                .load(Ordering::Acquire)
                .saturating_mul(u64::from(source.format.sample_rate_hz()))
                .div_ceil(1_000_000);
            // A cadence-sized step avoids reinitializing a running converter
            // for every microsecond of harmless arrival variation.
            let step = period_guard.max(block_guard / 4).max(1);
            let guard = block_guard
                .max(gap_guard.div_ceil(step).saturating_mul(step))
                .min((render::ABSOLUTE_MAX_WINDOW_FRAMES / 4) as u64);
            if self
                .core
                .source_status(source.timeline)
                .is_some_and(|status| status.delivery_guard_frames != guard)
            {
                self.core
                    .set_source_delivery_guard(source.timeline, guard)
                    .map_err(|error| error.to_string())?;
            }
        }
        self.status.dropped_blocks = dropped;
        Ok(())
    }

    fn ingest(&mut self, packet: Packet) {
        let Some(ingress) = self
            .sources
            .iter()
            .find(|ingress| ingress.source == packet.source && ingress.binding == packet.binding)
        else {
            return;
        };
        if !ingress.progress.active.load(Ordering::Acquire) {
            return;
        }
        // Use the selected endpoint cadence as the queue budget. A sustained
        // delayed consumer discards its backlog instead of replaying the past.
        let rate = self.status.rate_hz.unwrap_or(48_000);
        let period = self.status.period_frames.unwrap_or(480);
        let age_limit =
            Duration::from_secs_f64((f64::from(period) * 2.0 / f64::from(rate)).max(0.001));
        if packet.observed.elapsed() > age_limit {
            let frames = packet.len / usize::from(packet.source.format.channels());
            ingress
                .progress
                .discard
                .fetch_max(packet.start + frames as u64, Ordering::Release);
            ingress.progress.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Err(error) = self.core.push_block(SourceBlock {
            timeline: packet.source.timeline,
            permission: packet.source.permission,
            format: packet.source.format,
            start_frame: packet.start,
            arrived_at_ms: packet.arrived_at_ms,
            samples: &packet.samples[..packet.len],
        }) {
            self.status.last_error = Some(error.to_string());
        }
    }

    fn open(&mut self) -> Result<(), String> {
        self.status.state = "opening";
        #[cfg(not(test))]
        let binding = {
            if self.binding.revision == 0
                || self.binding.selection != self.config.output_endpoint_id
            {
                self.status.state = "awaiting-device";
                return Ok(());
            }
            match self.binding.endpoint.clone() {
                Some(binding) => binding,
                None => {
                    return Err(self
                        .binding
                        .error
                        .clone()
                        .unwrap_or_else(|| "The selected output is unavailable.".into()));
                }
            }
        };
        let mut device = Device::open(
            #[cfg(not(test))]
            &binding,
            #[cfg(not(test))]
            &self.handle.routes,
            #[cfg(test)]
            self.config.output_endpoint_id.as_deref(),
        )?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or("Output generation exhausted.")?;
        let format = device.format();
        let period = device.period().max(1).min(device.capacity());
        let quantum = period.min(OUTPUT_CHUNK as u32);
        self.core
            .set_output(OutputSetup {
                generation: self.generation,
                sample_rate_hz: format.sample_rate_hz,
                channels: u16::from(format.channels),
                layout: format.pcm_format().layout(),
                quantum_frames: quantum,
            })
            .map_err(|error| error.to_string())?;
        self.status.generation = self.generation;
        self.status.endpoint_id = Some(device.endpoint().to_owned());
        self.status.rate_hz = Some(format.sample_rate_hz);
        self.status.channels = Some(format.channels);
        self.status.channel_mask = Some(format.channel_mask);
        self.status.period_frames = Some(device.period());
        self.status.target_padding_frames = Some(period);
        self.status.buffer_frames = Some(device.capacity());
        self.configure_mix()?;
        device.prime_and_start(period)?;
        self.status.scheduling_error = device.scheduling_error();
        self.opened_refresh = self.refresh;
        self.last_service = Instant::now();
        self.status.last_error = None;
        self.status.state = "running";
        self.stream = Some(device);
        Ok(())
    }

    fn service(&mut self) -> Result<bool, String> {
        let stream = self.stream.as_mut().expect("open output");
        let mut padding = stream.padding()?;
        let period = self
            .status
            .target_padding_frames
            .expect("opened reserve target");
        let rate = self.status.rate_hz.expect("opened rate");
        let elapsed = self.last_service.elapsed();
        let missed = padding == 0
            && elapsed
                > Duration::from_secs_f64(
                    f64::from(stream.capacity().max(period.saturating_mul(2))) / f64::from(rate),
                );
        let writable = stream.capacity().saturating_sub(padding);
        let mut remaining = writable;
        let mut missed_service = missed;
        while remaining > 0 && padding < period {
            let observation = ServiceObservation {
                generation: self.generation,
                selected: true,
                quantum_frames: period.min(OUTPUT_CHUNK as u32),
                target_padding_frames: period,
                writable_frames: remaining,
                padding_frames: padding,
                missed_service,
            };
            let PrepareOutcome::Prepared(quantum) = self
                .core
                .prepare_quantum(&observation)
                .map_err(|error| error.to_string())?
            else {
                break;
            };
            let pcm = self.core.output_interleaved();
            self.pcm[..pcm.len()].copy_from_slice(pcm);
            if self.config.limiter_enabled {
                for sample in &mut self.pcm[..pcm.len()] {
                    *sample = SoftLimiter::default().process_f64(*sample);
                }
            }
            if self.handle.stop.load(Ordering::Acquire)
                || self.sources.iter().any(|ingress| !ingress.is_active())
            {
                self.core
                    .complete(SubmissionReceipt {
                        generation: quantum.generation,
                        quantum_id: quantum.id,
                        success: false,
                        start_frame: 0,
                        end_frame: 0,
                    })
                    .map_err(|error| error.to_string())?;
                return Ok(false);
            }
            let result = stream.submit(&self.pcm[..pcm.len()], quantum.frames);
            let receipt = match result {
                Ok((start_frame, end_frame)) => SubmissionReceipt {
                    generation: quantum.generation,
                    quantum_id: quantum.id,
                    success: true,
                    start_frame,
                    end_frame,
                },
                Err(error) => {
                    self.core
                        .complete(SubmissionReceipt {
                            generation: quantum.generation,
                            quantum_id: quantum.id,
                            success: false,
                            start_frame: 0,
                            end_frame: 0,
                        })
                        .map_err(|failure| failure.to_string())?;
                    return Err(error);
                }
            };
            self.core
                .complete(receipt)
                .map_err(|error| error.to_string())?;
            self.status.overloaded = Some(quantum.over_unity);
            self.status.peak_abs = Some(quantum.peak_abs);
            self.status.peak_observed = Some(Instant::now());
            self.last_service = Instant::now();
            padding += quantum.frames;
            remaining -= quantum.frames;
            missed_service = false;
            if self.handle.control_revision.load(Ordering::Acquire) != self.revision {
                let _ = self.handle.wake.signal();
                break;
            }
        }
        match stream.clock() {
            Ok(clock) => {
                self.status.clock = clock;
                self.status.clock_error = None;
                self.clock_failure = None;
            }
            Err(error) => {
                self.status.clock = None;
                if self.clock_failure != Some(error) {
                    self.status.clock_error = Some(error.to_string());
                    self.clock_failure = Some(error);
                }
            }
        }
        Ok(true)
    }

    fn binding_changed(&mut self) -> Result<bool, String> {
        #[cfg(test)]
        return Ok(false);
        #[cfg(not(test))]
        {
            let Some(view) = self.handle.devices.snapshot_after(self.binding.revision) else {
                return Ok(false);
            };
            let changed = self.binding.endpoint != view.endpoint;
            self.binding = view;
            Ok(changed)
        }
    }

    fn close(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.stop();
        }
        self.status.overloaded = None;
        self.status.source_errors.clear();
        self.status.peak_abs = None;
        self.status.peak_observed = None;
        self.status.clock = None;
        self.status.clock_error = None;
        self.clock_failure = None;
    }

    fn fail(&mut self, error: String) {
        self.close();
        self.status.state = "unavailable";
        self.status.last_error = Some(error);
        self.retry_at = Instant::now() + RETRY;
    }

    fn publish(&mut self) {
        self.status.submitted_frames = self.core.submitted_frames();
        self.status.admission_revision = self.core.applied_revision();
        self.status.sources.clear();
        let has_solo = self
            .sources
            .iter()
            .any(|ingress| mix_for(&self.config, ingress.source.timeline).solo);
        for ingress in &self.sources {
            if !ingress.is_active() {
                continue;
            }
            let source = ingress.source;
            let (member_id, source_id) = match source.timeline {
                TimelineId::Remote { member, source, .. } => (Some(member), source),
                TimelineId::Local { source, .. } => (None, source),
            };
            let mix = mix_for(&self.config, source.timeline);
            let effective_gain = if mix.mute || (has_solo && !mix.solo) {
                0.0
            } else {
                db_gain(mix.gain_db) * db_gain(self.config.master_gain_db)
            };
            let state = self.core.source_status(source.timeline);
            let unsupported = self
                .status
                .source_errors
                .iter()
                .any(|error| error.member_id == member_id && error.source_id == source_id);
            let phase = if self.status.state != "running" {
                "awaiting_output"
            } else if unsupported {
                "unsupported"
            } else if effective_gain == 0.0 {
                "muted"
            } else {
                match state.as_ref().map(|state| state.phase) {
                    Some(render::Phase::Running) => "running",
                    Some(render::Phase::Priming) => "priming",
                    _ => "awaiting_current",
                }
            };
            let arrival = ingress.progress.last_arrival_us.load(Ordering::Acquire);
            let now = u64::try_from(ingress.progress.origin.elapsed().as_micros())
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            self.status
                .sources
                .push(crate::participant::snapshot::SourcePlayoutState {
                    member_id,
                    source_id,
                    phase,
                    last_arrival_age_ms: (arrival != 0)
                        .then(|| now.saturating_sub(arrival) / 1_000),
                    closed_before: state.as_ref().map_or(0, |state| state.closed_before),
                    high_water: state.as_ref().map_or(0, |state| state.high_water),
                    submitted_frames: state.as_ref().map_or(0, |state| state.submitted_frames),
                    dropped_blocks: ingress.progress.dropped.load(Ordering::Relaxed),
                    effective_gain,
                    segment_restarts: state.as_ref().map_or(0, |state| state.segment_restarts),
                    last_error: state.as_ref().and_then(|state| state.last_error),
                });
        }
        let mut view = self
            .handle
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        view.state = self.status.state;
        view.generation = self.status.generation;
        view.endpoint_id.clone_from(&self.status.endpoint_id);
        view.rate_hz = self.status.rate_hz;
        view.channels = self.status.channels;
        view.channel_mask = self.status.channel_mask;
        view.period_frames = self.status.period_frames;
        view.target_padding_frames = self.status.target_padding_frames;
        view.buffer_frames = self.status.buffer_frames;
        view.submitted_frames = self.status.submitted_frames;
        view.admission_revision = self.status.admission_revision;
        view.overloaded = self.status.overloaded;
        view.peak_abs = self.status.peak_abs;
        view.peak_observed = self.status.peak_observed;
        view.dropped_blocks = self.status.dropped_blocks;
        view.last_error.clone_from(&self.status.last_error);
        view.source_errors.clone_from(&self.status.source_errors);
        view.sources.clone_from(&self.status.sources);
        view.clock = self.status.clock;
        view.clock_error.clone_from(&self.status.clock_error);
        view.scheduling_error
            .clone_from(&self.status.scheduling_error);
    }
}

fn db_gain(db: f32) -> f64 {
    10f64.powf(f64::from(db) / 20.)
}

fn mix_for(config: &ParticipantConfig, timeline: TimelineId) -> MixIntent {
    match timeline {
        TimelineId::Local { source, .. } => config
            .source(source)
            .map(|source| source.playback)
            .unwrap_or_else(MixIntent::unity),
        TimelineId::Remote { member, source, .. } => config
            .member(member)
            .map(|member| {
                let mut mix = member
                    .source_overrides
                    .iter()
                    .find(|entry| entry.source_id == source)
                    .map(|entry| entry.playback)
                    .unwrap_or_else(MixIntent::unity);
                mix.gain_db += member.playback.gain_db;
                mix.mute |= member.playback.mute;
                mix.solo |= member.playback.solo;
                mix.pan = (mix.pan + member.playback.pan).clamp(-1., 1.);
                mix
            })
            .unwrap_or_else(MixIntent::unity),
    }
}

struct Device {
    #[cfg(not(test))]
    native: WasapiRenderStream,
    #[cfg(not(test))]
    scheduling: Result<MmcssRegistration, String>,
    // Native client is declared first and is released before the route lease.
    #[cfg(not(test))]
    _route: RouteLease,
    #[cfg(test)]
    committed: u64,
    #[cfg(test)]
    endpoint: String,
}

impl Device {
    #[cfg(not(test))]
    fn open(binding: &EndpointBinding, routes: &RouteGuard) -> Result<Self, String> {
        let route = routes.reserve_output(&binding.endpoint_id)?;
        let native = WasapiRenderStream::open_native_with_period(
            NativeRenderRequest {
                selection: EndpointSelection::fixed(&binding.endpoint_id),
            },
            SharedPeriodPreference::Minimum,
        )
        .map_err(|error| error.to_string())?;
        if native.native_format() != Some(binding.format)
            || native.endpoint().endpoint_id() != binding.endpoint_id
        {
            return Err(
                "The output format changed while opening; waiting for a fresh device observation."
                    .into(),
            );
        }
        let scheduling = MmcssRegistration::register_pro_audio().map_err(|error| error.to_string());
        Ok(Self {
            native,
            scheduling,
            _route: route,
        })
    }
    #[cfg(test)]
    fn open(endpoint: Option<&str>) -> Result<Self, String> {
        let endpoint = endpoint.unwrap_or("software-fixture");
        if endpoint.starts_with("missing:") {
            return Err("The fixture output is disconnected.".into());
        }
        Ok(Self {
            committed: 0,
            endpoint: endpoint.to_owned(),
        })
    }

    #[cfg(not(test))]
    fn format(&self) -> NativeF32Format {
        self.native.native_format().expect("native output")
    }
    #[cfg(test)]
    fn format(&self) -> NativeF32Format {
        NativeF32Format {
            encoding: rlb_audio_windows::SampleEncoding::Float,
            sample_rate_hz: if self.endpoint.contains("44100") {
                44_100
            } else {
                48_000
            },
            channels: if self.endpoint.contains("stereo") {
                2
            } else {
                1
            },
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            block_align: if self.endpoint.contains("stereo") {
                8
            } else {
                4
            },
            channel_mask: if self.endpoint.contains("stereo") {
                3
            } else {
                4
            },
        }
    }
    #[cfg(not(test))]
    fn endpoint(&self) -> &str {
        self.native.endpoint().endpoint_id()
    }
    #[cfg(test)]
    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    #[cfg(not(test))]
    fn period(&self) -> u32 {
        self.native.diagnostics().period_frames
    }
    #[cfg(test)]
    fn period(&self) -> u32 {
        if self.endpoint.contains("44100") {
            96
        } else {
            64
        }
    }
    #[cfg(not(test))]
    fn capacity(&self) -> u32 {
        self.native.callback_capacity_frames()
    }
    #[cfg(test)]
    fn capacity(&self) -> u32 {
        self.period() * 2
    }
    #[cfg(not(test))]
    fn prime_and_start(&mut self, frames: u32) -> Result<(), String> {
        self.native
            .prime_native_silence(frames as usize)
            .map_err(|error| error.to_string())?;
        self.native.start().map_err(|error| error.to_string())
    }
    #[cfg(test)]
    fn prime_and_start(&mut self, frames: u32) -> Result<(), String> {
        self.committed = u64::from(frames);
        Ok(())
    }
    #[cfg(not(test))]
    fn padding(&self) -> Result<u32, String> {
        self.native
            .padding_frames()
            .map_err(|error| error.to_string())
    }
    #[cfg(test)]
    fn padding(&self) -> Result<u32, String> {
        Ok(0)
    }
    #[cfg(not(test))]
    fn submit(&mut self, samples: &[f64], frames: u32) -> Result<(u64, u64), String> {
        self.native
            .submit_native_f64(samples, frames as usize)
            .map(|receipt| (receipt.start_frame, receipt.end_frame))
            .map_err(|error| error.to_string())
    }
    #[cfg(test)]
    fn submit(&mut self, _: &[f64], frames: u32) -> Result<(u64, u64), String> {
        let start = self.committed;
        self.committed += u64::from(frames);
        Ok((start, self.committed))
    }
    #[cfg(not(test))]
    fn clock(&self) -> Result<Option<RenderClockObservation>, RenderClockFailure> {
        self.native.clock_observation().map(Some)
    }
    #[cfg(test)]
    fn clock(&self) -> Result<Option<RenderClockObservation>, RenderClockFailure> {
        Ok(None)
    }
    #[cfg(not(test))]
    fn scheduling_error(&self) -> Option<String> {
        self.scheduling.as_ref().err().cloned()
    }
    #[cfg(test)]
    fn scheduling_error(&self) -> Option<String> {
        None
    }
    #[cfg(not(test))]
    fn wait(&self, wake: &AudioWake, timeout: u32) -> Result<(), String> {
        self.native
            .wait_with_control(wake, timeout)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    #[cfg(test)]
    fn wait(&self, wake: &AudioWake, _: u32) -> Result<(), String> {
        wake.wait(1).map(|_| ()).map_err(|error| error.to_string())
    }
    #[cfg(not(test))]
    fn stop(&mut self) {
        let _ = self.native.stop();
    }
    #[cfg(test)]
    fn stop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::{RunId, StreamEpoch};
    use rlb_peer::{ChannelLayout, PcmEncoding, PcmFormat};

    #[test]
    fn coalesced_destination_replacement_retires_old_packets_and_window() {
        let config =
            ParticipantConfig::create("Binding fixture", "127.0.0.1:0".parse().unwrap()).unwrap();
        let source = RenderSource {
            timeline: TimelineId::Local {
                run: RunId::from_bytes([1; 16]).unwrap(),
                source: config.sources[0].id,
                acquisition: StreamEpoch::from_bytes([2; 16]).unwrap(),
            },
            permission: render::Permission::Local {
                monitor_revision: 1,
                capture_instance: 1,
            },
            activity: render::SourceActivity::Active,
            format: PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap(),
            kind: Some(rlb_peer::SourceKind::System),
        };
        let mut runtime = EndpointRuntime::start(config).unwrap();
        let original = runtime
            .handle
            .replace_local(vec![source])
            .unwrap()
            .remove(0);
        original
            .push(SourceBlock {
                timeline: source.timeline,
                permission: source.permission,
                format: source.format,
                start_frame: 0,
                arrived_at_ms: 0,
                samples: &[0.25; 120],
            })
            .unwrap();
        runtime.handle.inspect(move |core| {
            assert_eq!(core.source_status(source.timeline).unwrap().high_water, 120)
        });
        let (started, entered) = mpsc::sync_channel(1);
        let (resume, blocked) = mpsc::sync_channel(1);
        let handle = runtime.handle.clone();
        let inspection = std::thread::spawn(move || {
            handle.inspect(move |_| {
                started.send(()).unwrap();
                blocked.recv().unwrap();
            })
        });
        entered.recv().unwrap();
        original
            .push(SourceBlock {
                timeline: source.timeline,
                permission: source.permission,
                format: source.format,
                start_frame: 5_000,
                arrived_at_ms: 0,
                samples: &[0.5],
            })
            .unwrap();
        runtime.handle.replace_local(Vec::new()).unwrap();
        let current = runtime
            .handle
            .replace_local(vec![source])
            .unwrap()
            .remove(0);
        assert!(!current.same_binding(&original));
        current
            .push(SourceBlock {
                timeline: source.timeline,
                permission: source.permission,
                format: source.format,
                start_frame: 120,
                arrived_at_ms: 0,
                samples: &[0.75; 2],
            })
            .unwrap();
        resume.send(()).unwrap();
        inspection.join().unwrap();
        let status = runtime
            .handle
            .inspect(move |core| core.source_status(source.timeline).unwrap());
        assert_eq!(
            status.high_water, 122,
            "old queued binding must not extend the new source extent"
        );
        assert!(
            status.closed_before >= 120,
            "the old buffered window must stay retired"
        );
        runtime.stop();
    }

    #[test]
    fn bounded_pcm_loss_closes_the_range_and_remote_leave_keeps_local_admission() {
        let config =
            ParticipantConfig::create("Output fixture", "127.0.0.1:0".parse().unwrap()).unwrap();
        let source_id = config.sources[0].id;
        let mut runtime = EndpointRuntime::start(config).unwrap();
        let source = RenderSource {
            timeline: TimelineId::Local {
                run: RunId::from_bytes([1; 16]).unwrap(),
                source: source_id,
                acquisition: StreamEpoch::from_bytes([2; 16]).unwrap(),
            },
            permission: render::Permission::Local {
                monitor_revision: 1,
                capture_instance: 1,
            },
            activity: render::SourceActivity::Active,
            format: PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap(),
            kind: Some(rlb_peer::SourceKind::System),
        };
        let ingress = runtime
            .handle
            .replace_local(vec![source])
            .unwrap()
            .remove(0);
        runtime
            .handle
            .inspect(move |core| assert!(core.source_status(source.timeline).unwrap().active));
        let (arrived, blocked) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        runtime
            .handle
            .inspect
            .send(Box::new(move |_| {
                arrived.send(()).unwrap();
                resume.recv().unwrap();
            }))
            .unwrap();
        runtime.handle.wake.signal().unwrap();
        blocked.recv().unwrap();
        for index in 0..=PCM_QUEUE {
            let result = ingress.push(SourceBlock {
                timeline: source.timeline,
                permission: source.permission,
                format: source.format,
                start_frame: (index * PCM_SAMPLES) as u64,
                arrived_at_ms: 0,
                samples: &[0.25; PCM_SAMPLES],
            });
            assert_eq!(result.is_err(), index == PCM_QUEUE);
        }
        runtime.handle.replace_remote(Vec::new()).unwrap();
        release.send(()).unwrap();
        let status = runtime
            .handle
            .inspect(move |core| core.source_status(source.timeline).unwrap());
        assert!(status.active);
        assert!(status.closed_before >= ((PCM_QUEUE + 1) * PCM_SAMPLES) as u64);
        assert!(ingress.progress.dropped.load(Ordering::Relaxed) >= 1);
        runtime.stop();
        assert_eq!(runtime.handle.status().state, "stopped");
    }
}
