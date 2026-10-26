//! Independent native original owners and their bounded media handoff.
//! Capture tickets are allocated before either monitor or network distribution.

use super::output::{OutputHandle, OutputIngress};
use crate::participant::audio::render::{
    Permission, RenderSource, SourceActivity, SourceBlock, TimelineId,
};
use crate::participant::config::{LocalSource, ParticipantConfig, SourceKind};
use mio::Waker;
use rlb_audio_windows::{AudioWake, NativeCapturePacket, NativeF32Format};
use rlb_model::{RunId, SourceId, StreamEpoch};
use rlb_peer::{BindingRequest, CaptureTicket, CaptureWatermark, MAX_ORIGINAL_FRAMES, PcmFormat};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(not(test))]
use {
    super::devices::{BindingView, CaptureDeviceHandle, EndpointBinding},
    super::routes::RouteLease,
    rlb_audio::{CaptureBackend, EndpointDirection},
    rlb_audio_windows::{
        CaptureFlow, EndpointSelection, MmcssRegistration, NativeCaptureRead, NativeCaptureRequest,
        SharedPeriodPreference, WasapiCaptureStream, WindowsAudioError,
    },
};

const BLOCK_SAMPLES: usize = MAX_ORIGINAL_FRAMES * rlb_model::pcm::MAX_CHANNELS;
const QUEUE_BLOCKS: usize = 32;

/// Each source owns its reusable storage. Only the initialized sample prefix
/// travels with a block; the cold allocation covers the largest native chunk.
struct CaptureBuffers {
    available: Receiver<Box<[f64]>>,
    returned: SyncSender<Box<[f64]>>,
}

impl CaptureBuffers {
    fn new() -> Self {
        let (returned, available) = mpsc::sync_channel(QUEUE_BLOCKS + 1);
        for _ in 0..=QUEUE_BLOCKS {
            returned
                .try_send(vec![0.0; BLOCK_SAMPLES].into_boxed_slice())
                .expect("the cold capture pool has one slot for every buffer");
        }
        Self { available, returned }
    }

    fn try_copy(&self, samples: &[f64]) -> Option<CaptureSamples> {
        let mut storage = self.available.try_recv().ok()?;
        storage[..samples.len()].copy_from_slice(samples);
        Some(CaptureSamples {
            storage: Some(storage),
            initialized: samples.len(),
            returned: self.returned.clone(),
        })
    }
}

pub(crate) struct CaptureSamples {
    storage: Option<Box<[f64]>>,
    initialized: usize,
    returned: SyncSender<Box<[f64]>>,
}

impl std::ops::Deref for CaptureSamples {
    type Target = [f64];

    fn deref(&self) -> &Self::Target {
        &self.storage.as_deref().expect("a live capture lease owns storage")[..self.initialized]
    }
}

impl Drop for CaptureSamples {
    fn drop(&mut self) {
        if let Some(storage) = self.storage.take() {
            // A dropped Delivery disconnects this original pool. Its late
            // leases then free their own storage without reaching a new owner.
            let _ = self.returned.try_send(storage);
        }
    }
}

#[derive(Default)]
pub(crate) struct TicketClock(AtomicU64);

impl TicketClock {
    pub(crate) fn allocate(&self) -> Result<CaptureTicket, String> {
        let previous = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "Capture ticket counter exhausted.")?;
        CaptureTicket::new(previous + 1).ok_or_else(|| "Capture ticket is zero.".into())
    }
    pub(crate) fn watermark(&self) -> CaptureWatermark {
        CaptureWatermark {
            allocated_through: self.0.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Acquisition {
    pub(crate) epoch: StreamEpoch,
    pub(crate) instance: u64,
    pub(crate) format: PcmFormat,
    pub(crate) native: NativeF32Format,
    pub(crate) period_frames: u32,
    pub(crate) buffer_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BindingAck {
    pub(crate) request: BindingRequest,
    pub(crate) acquisition: StreamEpoch,
    pub(crate) instance: u64,
    pub(crate) first_frame: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CaptureState {
    pub(crate) source: SourceId,
    pub(crate) revision: u64,
    pub(crate) phase: &'static str,
    pub(crate) endpoint: Option<String>,
    pub(crate) acquisition: Option<Acquisition>,
    pub(crate) ack: Option<BindingAck>,
    pub(crate) error: Option<String>,
    pub(crate) scheduling_error: Option<String>,
    pub(crate) captured_frames: u64,
    pub(crate) dropped_blocks: u64,
    pub(crate) last_packet: Option<NativeCapturePacket>,
    pub(crate) peak_abs: Option<f64>,
    pub(crate) peak_observed: Option<Instant>,
}

#[derive(Clone)]
struct Control {
    source: LocalSource,
    request: Option<BindingRequest>,
    revision: u64,
    refresh: u64,
    monitor_generation: u64,
}

#[derive(Clone)]
pub(crate) struct CaptureHandle {
    output: OutputHandle,
    control: Arc<Mutex<Control>>,
    wake: AudioWake,
    state: Arc<Mutex<CaptureState>>,
    stop: Arc<AtomicBool>,
    interest: Arc<AtomicBool>,
    demand: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    media_wake: Arc<Waker>,
    #[cfg(not(test))]
    device: CaptureDeviceHandle,
}

impl CaptureHandle {
    fn configure(&self, source: &LocalSource, force: bool) -> Result<(), String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.source != *source || force {
            let revision = control
                .revision
                .checked_add(1)
                .ok_or("Capture control revision exhausted.")?;
            if control.source.monitor != source.monitor
                || control.source.endpoint_id != source.endpoint_id
                || force
            {
                let generation = control
                    .monitor_generation
                    .checked_add(1)
                    .ok_or("Monitor generation exhausted.")?;
                // Preserve every accepted off/on boundary while this owner is busy.
                self.output.set_local_source(source.id, None)?;
                control.monitor_generation = generation;
            }
            if force {
                control.refresh = control
                    .refresh
                    .checked_add(1)
                    .ok_or("Capture refresh counter exhausted.")?;
            }
            control.source = source.clone();
            control.revision = revision;
        }
        drop(control);
        self.wake.signal().map_err(|error| error.to_string())?;
        #[cfg(not(test))]
        self.device.configure(
            source.endpoint_id.as_deref(),
            source.publish || source.monitor,
            force,
        )?;
        Ok(())
    }

    pub(crate) fn request_binding(&self, request: Option<BindingRequest>) -> Result<(), String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.request == request {
            return Ok(());
        }
        let revision = control
            .revision
            .checked_add(1)
            .ok_or("Capture control revision exhausted.")?;
        control.request = request;
        control.revision = revision;
        drop(control);
        self.wake.signal().map_err(|error| error.to_string())
    }

    pub(crate) fn set_interest(&self, interested: bool) {
        self.interest.store(interested, Ordering::Release);
    }

    pub(crate) fn set_demand(&self, demanded: bool) {
        if self.demand.swap(demanded, Ordering::AcqRel) != demanded {
            let _ = self.wake.signal();
        }
    }

    pub(crate) fn state_after(&self, revision: u64) -> Option<CaptureState> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (state.revision != revision).then(|| state.clone())
    }

    pub(crate) fn snapshot(&self) -> CaptureState {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        state.captured_frames = self.captured.load(Ordering::Relaxed);
        state.dropped_blocks = self.dropped.load(Ordering::Relaxed);
        state
    }

    fn publish(
        &self,
        phase: &'static str,
        acquisition: Option<Acquisition>,
        endpoint: Option<String>,
        error: Option<String>,
        scheduling: Option<String>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.phase == phase
            && state.acquisition == acquisition
            && state.endpoint == endpoint
            && state.error == error
            && state.scheduling_error == scheduling
        {
            return;
        }
        if state.acquisition != acquisition {
            state.ack = None;
            state.last_packet = None;
            state.peak_abs = None;
            state.peak_observed = None;
        }
        state.phase = phase;
        state.acquisition = acquisition;
        state.endpoint = endpoint;
        state.error = error;
        state.scheduling_error = scheduling;
        state.revision = state
            .revision
            .checked_add(1)
            .expect("capture status revision exhausted");
        drop(state);
        let _ = self.media_wake.wake();
    }
}

pub(crate) struct CaptureBlock {
    pub(crate) packet: NativeCapturePacket,
    pub(crate) instance: u64,
    pub(crate) request: BindingRequest,
    pub(crate) ticket: CaptureTicket,
    pub(crate) start_frame: u64,
    pub(crate) frames: usize,
    pub(crate) enqueued_at_ms: u64,
    pub(crate) deadline: Instant,
    pub(crate) samples: CaptureSamples,
}

pub(crate) struct CaptureRuntime {
    pub(crate) handles: Vec<CaptureHandle>,
    receiver: Receiver<CaptureBlock>,
    owners: Vec<JoinHandle<()>>,
}

impl CaptureRuntime {
    #[cfg(not(test))]
    pub(crate) fn start(
        config: &ParticipantConfig,
        run: RunId,
        origin: Instant,
        tickets: Arc<TicketClock>,
        output: OutputHandle,
        media_wake: Arc<Waker>,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_BLOCKS);
        let mut runtime = Self {
            handles: Vec::with_capacity(2),
            receiver,
            owners: Vec::with_capacity(2),
        };
        for source in &config.sources {
            let wake = AudioWake::new().map_err(|error| error.to_string())?;
            let device = output.devices().register_capture(
                source.id,
                source.kind == SourceKind::System,
                source.endpoint_id.clone(),
                source.publish || source.monitor,
                wake.clone(),
            )?;
            let handle = new_handle(
                source.clone(),
                wake,
                media_wake.clone(),
                output.clone(),
                device,
            );
            let delivery = Delivery {
                handle: handle.clone(),
                output: output.clone(),
                sender: sender.clone(),
                buffers: CaptureBuffers::new(),
                tickets: tickets.clone(),
                origin,
                run,
                acquisition: None,
                monitor: None,
                acknowledged: None,
            };
            let owner_handle = handle.clone();
            let thread = std::thread::Builder::new()
                .name(format!("rlb-capture-{}", source.kind.as_str()))
                .spawn(move || Owner::new(delivery).run())
                .map_err(|error| format!("Capture owner could not start: {error}"))?;
            runtime.handles.push(owner_handle);
            runtime.owners.push(thread);
        }
        Ok(runtime)
    }

    pub(crate) fn configure(&self, config: &ParticipantConfig, force: bool) -> Result<(), String> {
        for handle in &self.handles {
            let source = handle
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .source;
            handle.configure(
                config
                    .source(source)
                    .ok_or("Capture source identity changed.")?,
                force,
            )?;
        }
        Ok(())
    }

    pub(crate) fn next_block(&self) -> Option<CaptureBlock> {
        self.receiver.try_recv().ok()
    }
    pub(crate) fn snapshots(&self) -> Vec<CaptureState> {
        self.handles.iter().map(CaptureHandle::snapshot).collect()
    }

    pub(crate) fn stop(&mut self) {
        for handle in &self.handles {
            handle.stop.store(true, Ordering::Release);
            let _ = handle.wake.signal();
        }
        for owner in self.owners.drain(..) {
            let _ = owner.join();
        }
    }
}

impl Drop for CaptureRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
pub(crate) struct FixtureSource {
    delivery: Delivery,
    acquisition: Acquisition,
    next_frame: u64,
}

#[cfg(test)]
impl FixtureSource {
    pub(crate) fn prepare(&mut self) -> Result<(), String> {
        let control = self.delivery.handle.control.lock().unwrap().clone();
        self.delivery
            .monitor(&control.source, control.monitor_generation)?;
        self.delivery.acknowledge(
            control.request.filter(|_| control.source.publish),
            self.next_frame,
        )?;
        Ok(())
    }

    pub(crate) fn emit(&mut self, samples: &[f64]) -> Result<(), String> {
        self.prepare()?;
        let control = self.delivery.handle.control.lock().unwrap().clone();
        self.delivery
            .monitor(&control.source, control.monitor_generation)?;
        let frames = samples.len() / usize::from(self.acquisition.native.channels);
        let packet = NativeCapturePacket {
            source_id: control.source.id,
            stream_epoch: self.acquisition.epoch,
            start_sample: self.next_frame,
            frames: frames as u32,
            format: self.acquisition.native,
            clock_domain: rlb_audio_windows::wasapi_clock_domain(),
            timing: rlb_audio::CaptureTiming::absent(),
            device_position: None,
            discontinuity: rlb_audio::CaptureDiscontinuity::Continuous,
            silent: false,
        };
        self.next_frame += frames as u64;
        self.delivery.packet(
            packet,
            samples,
            control.request.filter(|_| control.source.publish),
            Instant::now(),
        )
    }
}

#[cfg(test)]
impl Drop for FixtureSource {
    fn drop(&mut self) {
        let source = self.delivery.handle.snapshot().source;
        self.delivery.close(source);
        self.delivery
            .handle
            .publish("disabled", None, None, None, None);
    }
}

#[cfg(test)]
pub(crate) fn fixture(
    config: &ParticipantConfig,
    run: RunId,
    origin: Instant,
    tickets: Arc<TicketClock>,
    output: OutputHandle,
    wake: Arc<Waker>,
) -> Result<(CaptureRuntime, Vec<FixtureSource>), String> {
    let (sender, receiver) = mpsc::sync_channel(QUEUE_BLOCKS);
    let mut runtime = CaptureRuntime {
        handles: Vec::new(),
        receiver,
        owners: Vec::new(),
    };
    let mut fixtures = Vec::new();
    for (index, source) in config.sources.iter().enumerate() {
        let channels = if source.kind == SourceKind::System {
            2
        } else {
            1
        };
        let rate = if channels == 2 { 48_000 } else { 44_100 };
        let native = NativeF32Format {
            encoding: rlb_audio_windows::SampleEncoding::Float,
            sample_rate_hz: rate,
            channels,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            block_align: u16::from(channels) * 4,
            channel_mask: if channels == 2 { 3 } else { 4 },
        };
        let acquisition = Acquisition {
            epoch: StreamEpoch::from_bytes([index as u8 + 1; 16]).unwrap(),
            instance: 1,
            native,
            format: PcmFormat::new(
                rate,
                if channels == 2 {
                    rlb_peer::ChannelLayout::StereoLeftRight
                } else {
                    rlb_peer::ChannelLayout::Mono
                },
                rlb_peer::PcmEncoding::F32Le,
                32,
            )
            .unwrap(),
            period_frames: rate / 100,
            buffer_frames: 8_192,
        };
        let handle = new_handle(
            source.clone(),
            AudioWake::new().map_err(|error| error.to_string())?,
            wake.clone(),
            output.clone(),
        );
        handle.publish(
            "ready",
            Some(acquisition),
            source.endpoint_id.clone(),
            None,
            None,
        );
        let delivery = Delivery {
            handle: handle.clone(),
            output: output.clone(),
            sender: sender.clone(),
            buffers: CaptureBuffers::new(),
            tickets: tickets.clone(),
            origin,
            run,
            acquisition: Some(acquisition),
            monitor: None,
            acknowledged: None,
        };
        runtime.handles.push(handle);
        fixtures.push(FixtureSource {
            delivery,
            acquisition,
            next_frame: 100_000,
        });
    }
    Ok((runtime, fixtures))
}

fn new_handle(
    source: LocalSource,
    wake: AudioWake,
    media_wake: Arc<Waker>,
    output: OutputHandle,
    #[cfg(not(test))] device: CaptureDeviceHandle,
) -> CaptureHandle {
    let state = CaptureState {
        source: source.id,
        revision: 1,
        phase: "starting",
        endpoint: None,
        acquisition: None,
        ack: None,
        error: None,
        scheduling_error: None,
        captured_frames: 0,
        dropped_blocks: 0,
        last_packet: None,
        peak_abs: None,
        peak_observed: None,
    };
    CaptureHandle {
        output,
        control: Arc::new(Mutex::new(Control {
            source,
            request: None,
            revision: 1,
            refresh: 0,
            monitor_generation: 1,
        })),
        wake,
        state: Arc::new(Mutex::new(state)),
        stop: Arc::new(AtomicBool::new(false)),
        interest: Arc::new(AtomicBool::new(false)),
        demand: Arc::new(AtomicBool::new(false)),
        captured: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
        media_wake,
        #[cfg(not(test))]
        device,
    }
}

/// Common production handoff after one native whole packet was released.
struct Delivery {
    handle: CaptureHandle,
    output: OutputHandle,
    sender: SyncSender<CaptureBlock>,
    buffers: CaptureBuffers,
    tickets: Arc<TicketClock>,
    origin: Instant,
    run: RunId,
    acquisition: Option<Acquisition>,
    monitor: Option<OutputIngress>,
    acknowledged: Option<BindingRequest>,
}

impl Delivery {
    fn acknowledge(
        &mut self,
        request: Option<BindingRequest>,
        boundary: u64,
    ) -> Result<Option<BindingRequest>, String> {
        let Some(acquisition) = self.acquisition else {
            return Ok(None);
        };
        let source = self
            .handle
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .source;
        let request = request.filter(|request| {
            request.run() == self.run
                && request.source() == source
                && request.expected_acquisition() == acquisition.epoch
                && request.format() == acquisition.format
        });
        if request != self.acknowledged {
            let mut state = self
                .handle
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let revision = state
                .revision
                .checked_add(1)
                .ok_or("Capture acknowledgement revision exhausted.")?;
            state.ack = request.map(|request| BindingAck {
                request,
                acquisition: acquisition.epoch,
                instance: acquisition.instance,
                first_frame: boundary,
            });
            state.revision = revision;
            self.acknowledged = request;
            drop(state);
            let _ = self.handle.media_wake.wake();
        }
        Ok(request)
    }

    fn monitor(&mut self, source: &LocalSource, generation: u64) -> Result<(), String> {
        let wanted = source.monitor.then_some(self.acquisition).flatten();
        let unchanged = match (&self.monitor, wanted) {
            (None, None) => true,
            (Some(ingress), Some(acquisition)) => {
                matches!(ingress.source().timeline,
                TimelineId::Local { acquisition: epoch, .. } if epoch == acquisition.epoch)
                    && matches!(ingress.source().permission, Permission::Local { monitor_revision, .. } if monitor_revision == generation)
            }
            _ => false,
        };
        if unchanged {
            return Ok(());
        }
        let monitor = wanted.map(|acquisition| RenderSource {
            timeline: TimelineId::Local {
                run: self.run,
                source: source.id,
                acquisition: acquisition.epoch,
            },
            permission: Permission::Local {
                monitor_revision: generation,
                capture_instance: acquisition.instance,
            },
            activity: SourceActivity::Active,
            format: acquisition.format,
            kind: Some(match source.kind {
                SourceKind::System => rlb_peer::SourceKind::System,
                SourceKind::Microphone => rlb_peer::SourceKind::Microphone,
            }),
        });
        self.monitor = self.output.set_local_source(source.id, monitor)?;
        Ok(())
    }

    fn packet(
        &mut self,
        packet: NativeCapturePacket,
        samples: &[f64],
        request: Option<BindingRequest>,
        read_started: Instant,
    ) -> Result<(), String> {
        // A completed native block gets one ticket, retained by every split.
        let ticket = self.tickets.allocate()?;
        let acquisition = self
            .acquisition
            .ok_or("Capture has no current acquisition.")?;
        if packet.stream_epoch != acquisition.epoch || packet.format != acquisition.native {
            return Err("Native capture metadata changed without a new acquisition.".into());
        }
        let channels = usize::from(acquisition.format.channels());
        if samples.len() != packet.frames as usize * channels {
            return Err("Native packet length is inconsistent.".into());
        }
        self.handle
            .captured
            .fetch_add(u64::from(packet.frames), Ordering::Relaxed);
        {
            let peak = samples.iter().fold(0.0_f64, |peak, sample| peak.max(sample.abs()));
            let mut state = self.handle.state.lock().unwrap_or_else(|error| error.into_inner());
            state.last_packet = Some(packet);
            state.peak_abs = Some(peak);
            state.peak_observed = Some(Instant::now());
        }
        let now = Instant::now();
        #[cfg(not(test))]
        let current_qpc = rlb_audio_windows::wasapi_qpc_100ns_now();
        #[cfg(test)]
        let current_qpc = None;
        let processing_age = now.saturating_duration_since(read_started);
        let native_age = native_end_age(packet, current_qpc);
        let age = if current_qpc.is_some() {
            native_age.max(processing_age)
        } else {
            native_age.saturating_add(processing_age)
        };
        let budget = Duration::from_secs_f64(
            (2. * f64::from(acquisition.period_frames.max(1))
                / f64::from(acquisition.native.sample_rate_hz))
            .max(0.001),
        );
        if age > budget {
            self.handle.dropped.fetch_add(1, Ordering::Relaxed);
            if let Some(monitor) = &self.monitor {
                monitor.discard_through(packet.start_sample + u64::from(packet.frames));
            }
            return Ok(());
        }
        let observed = now.checked_sub(age).unwrap_or(now);
        let enqueued_at_ms =
            u64::try_from(now.duration_since(self.origin).as_millis()).unwrap_or(u64::MAX);
        let request = self.acknowledge(request, packet.start_sample)?;
        if let Some(monitor) = &self.monitor {
            let source = monitor.source();
            let _ = monitor.push_observed(
                SourceBlock {
                    timeline: source.timeline,
                    permission: source.permission,
                    format: source.format,
                    start_frame: packet.start_sample,
                    arrived_at_ms: enqueued_at_ms,
                    samples,
                },
                observed,
            );
        }
        if let Some(request) = request.filter(|_| self.handle.interest.load(Ordering::Acquire)) {
            let mut offset = 0usize;
            for chunk in samples.chunks(MAX_ORIGINAL_FRAMES * channels) {
                let frames = chunk.len() / channels;
                let Some(samples) = self.buffers.try_copy(chunk) else {
                    self.handle.dropped.fetch_add(1, Ordering::Relaxed);
                    offset += frames;
                    continue;
                };
                let block = CaptureBlock {
                    packet,
                    instance: acquisition.instance,
                    request,
                    ticket,
                    start_frame: packet.start_sample + offset as u64,
                    frames,
                    enqueued_at_ms,
                    deadline: observed + budget,
                    samples,
                };
                if self.sender.try_send(block).is_err() {
                    self.handle.dropped.fetch_add(1, Ordering::Relaxed);
                }
                offset += frames;
            }
            let _ = self.handle.media_wake.wake();
        }
        Ok(())
    }

    fn close(&mut self, source: SourceId) {
        let _ = self.output.set_local_source(source, None);
        self.monitor = None;
        self.acquisition = None;
        self.acknowledged = None;
    }
}

fn native_end_age(packet: NativeCapturePacket, current_qpc: Option<u64>) -> Duration {
    let (Some(first), Some(observed)) =
        (packet.timing.first_frame, packet.timing.observed_on_owner)
    else {
        return Duration::ZERO;
    };
    let Some(ticks) = current_qpc
        .unwrap_or(observed.ticks())
        .checked_sub(first.at.ticks())
    else {
        return Duration::ZERO;
    };
    // Both native observations are in the documented 10MHz WASAPI QPC domain.
    let age = Duration::from_nanos(ticks.saturating_mul(100));
    age.saturating_sub(Duration::from_secs_f64(
        f64::from(packet.frames) / f64::from(packet.format.sample_rate_hz),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::output::OutputRuntime;
    use super::*;
    use rlb_peer::SourcePublisher;

    #[test]
    fn capture_handoff_keeps_whole_packet_identity_and_burns_stale_monitor_ranges() {
        let mut config =
            ParticipantConfig::create("Capture fixture", "127.0.0.1:0".parse().unwrap()).unwrap();
        for (index, source) in config.sources.iter_mut().enumerate() {
            source.publish = true;
            source.monitor = true;
            source.endpoint_id = Some(format!("fixture-{index}"));
        }
        let mut output = OutputRuntime::start(config.clone()).unwrap();
        let poll = mio::Poll::new().unwrap();
        let wake = Arc::new(Waker::new(poll.registry(), mio::Token(1)).unwrap());
        let tickets = Arc::new(TicketClock::default());
        let run = RunId::from_bytes([42; 16]).unwrap();
        let (mut runtime, mut sources) = fixture(
            &config,
            run,
            Instant::now(),
            tickets.clone(),
            output.handle.clone(),
            wake,
        )
        .unwrap();
        let mut publisher = SourcePublisher::new(run);
        for source in &sources {
            let id = source.delivery.handle.snapshot().source;
            publisher
                .register(id, rlb_peer::SourceKind::System, true)
                .unwrap();
            let request = publisher
                .begin_binding(id, source.acquisition.epoch, source.acquisition.format)
                .unwrap()
                .pending;
            source.delivery.handle.request_binding(request).unwrap();
            source.delivery.handle.set_interest(true);
        }
        let mut pcm = vec![0.25; 5_000 * 2];
        pcm[1] = -0.;
        pcm[2] = f64::from(f32::from_bits(1));
        sources[0].emit(&pcm).unwrap();
        let ack = runtime.handles[0].snapshot().ack.unwrap();
        assert_eq!(ack.first_frame, 100_000);
        assert_eq!(ack.instance, sources[0].acquisition.instance);
        publisher
            .commit_binding(ack.request, ack.acquisition, ack.first_frame)
            .unwrap();
        let first = runtime.next_block().unwrap();
        let tail = runtime.next_block().unwrap();
        assert_eq!((first.frames, tail.frames), (4_096, 904));
        assert_eq!(first.ticket, tail.ticket);
        assert_eq!(tail.start_frame, first.start_frame + 4_096);
        assert_eq!(first.packet, tail.packet);
        assert_eq!((first.samples[1] as f32).to_bits(), (-0f32).to_bits());
        assert_eq!((first.samples[2] as f32).to_bits(), 1);
        sources[1].emit(&[0.125; 64]).unwrap();
        assert!(runtime.next_block().unwrap().ticket > first.ticket);

        let old_monitor = sources[0].delivery.monitor.clone().unwrap();
        let mut off = config.sources[0].clone();
        off.monitor = false;
        runtime.handles[0].configure(&off, false).unwrap();
        runtime.handles[0]
            .configure(&config.sources[0], false)
            .unwrap();
        sources[0].emit(&[0.25; 64]).unwrap();
        let current_monitor = sources[0].delivery.monitor.clone().unwrap();
        assert_ne!(
            old_monitor.source().permission,
            current_monitor.source().permission
        );
        let old = old_monitor.source();
        assert!(
            old_monitor
                .push(SourceBlock {
                    timeline: old.timeline,
                    permission: old.permission,
                    format: old.format,
                    start_frame: 7_000_000,
                    arrived_at_ms: 0,
                    samples: &[0.5; 32],
                })
                .is_err()
        );
        let timeline = current_monitor.source().timeline;
        let high = output
            .handle
            .inspect(move |core| core.source_status(timeline).unwrap().high_water);
        assert!(high < 7_000_000, "pre-off ingress reopened the monitor");
        while runtime.next_block().is_some() {}
        let mut stale = runtime.handles[0].snapshot().last_packet.unwrap();
        stale.start_sample += u64::from(stale.frames);
        let before = tickets.watermark().allocated_through;
        let request = sources[0].delivery.handle.control.lock().unwrap().request;
        sources[0]
            .delivery
            .packet(
                stale,
                &[0.25; 64],
                request,
                Instant::now() - Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(tickets.watermark().allocated_through, before + 1);
        assert!(
            runtime.next_block().is_none(),
            "stalled capture got a renewed deadline"
        );
        assert!(runtime.handles[0].snapshot().dropped_blocks > 0);
        drop(sources);
        runtime.stop();
        output.stop();
    }
}

#[cfg(not(test))]
struct NativeDevice {
    stream: WasapiCaptureStream,
    _scheduling: Option<MmcssRegistration>,
    // Drop order releases the native stream before its endpoint reservation.
    _route: Option<RouteLease>,
    acquisition: Acquisition,
    endpoint: String,
    scratch: Vec<f64>,
    scheduling_error: Option<String>,
}

#[cfg(not(test))]
impl NativeDevice {
    fn open(
        source: &LocalSource,
        binding: &EndpointBinding,
        output: &OutputHandle,
        instance: u64,
    ) -> Result<Self, String> {
        let loopback = binding.direction == EndpointDirection::Render;
        let route = if loopback {
            Some(output.routes().reserve_loopback(&binding.endpoint_id)?)
        } else {
            None
        };
        let epoch = StreamEpoch::generate().map_err(|error| error.to_string())?;
        let mut stream = WasapiCaptureStream::open_native_with_period(
            NativeCaptureRequest {
                selection: EndpointSelection::fixed(&binding.endpoint_id),
                flow: if loopback {
                    CaptureFlow::Loopback
                } else {
                    CaptureFlow::Microphone
                },
                source_id: source.id,
                stream_epoch: epoch,
            },
            SharedPeriodPreference::Minimum,
        )
        .map_err(|error| error.to_string())?;
        if stream.native_format() != Some(binding.format)
            || stream.endpoint().endpoint_id() != binding.endpoint_id
        {
            return Err(
                "The capture endpoint changed during open; waiting for a fresh observation.".into(),
            );
        }
        let format = binding.format.pcm_format();
        let diagnostics = stream.diagnostics();
        let scratch = vec![
            0.;
            stream.callback_capacity_frames() as usize
                * usize::from(binding.format.channels)
        ];
        let (scheduling, scheduling_error) = match MmcssRegistration::register_pro_audio() {
            Ok(guard) => (Some(guard), None),
            Err(error) => (None, Some(error.to_string())),
        };
        stream.start().map_err(|error| error.to_string())?;
        Ok(Self {
            stream,
            _scheduling: scheduling,
            _route: route,
            acquisition: Acquisition {
                epoch,
                instance,
                format,
                native: binding.format,
                period_frames: diagnostics.period_frames.max(1),
                buffer_frames: diagnostics.buffer_frames,
            },
            endpoint: binding.endpoint_id.clone(),
            scratch,
            scheduling_error,
        })
    }
}

#[cfg(not(test))]
impl Drop for NativeDevice {
    fn drop(&mut self) {
        let _ = self.stream.stop();
    }
}

#[cfg(not(test))]
struct Owner {
    delivery: Delivery,
    control: Control,
    binding: BindingView,
    device: Option<NativeDevice>,
    instance: u64,
    opened_refresh: u64,
    retry_at: Instant,
}

#[cfg(not(test))]
impl Owner {
    fn new(delivery: Delivery) -> Self {
        let control = delivery
            .handle
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        Self {
            delivery,
            control,
            binding: BindingView::default(),
            device: None,
            instance: 0,
            opened_refresh: 0,
            retry_at: Instant::now(),
        }
    }

    fn run(mut self) {
        while !self.delivery.handle.stop.load(Ordering::Acquire) {
            let immediate = match self.turn() {
                Ok(again) => again,
                Err(error) => {
                    self.close();
                    self.retry_at = Instant::now() + Duration::from_millis(500);
                    self.delivery
                        .handle
                        .publish("error", None, None, Some(error), None);
                    false
                }
            };
            if immediate {
                continue;
            }
            let waited = if let Some(device) = &self.device {
                device
                    .stream
                    .wait_with_control(&self.delivery.handle.wake, 250)
            } else {
                self.delivery.handle.wake.wait(500)
            };
            if let Err(error) = waited {
                self.close();
                self.delivery
                    .handle
                    .publish("error", None, None, Some(error.to_string()), None);
            }
        }
        self.close();
        self.delivery
            .handle
            .publish("disabled", None, None, None, None);
    }

    fn turn(&mut self) -> Result<bool, String> {
        let current = self
            .delivery
            .handle
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if current.revision != self.control.revision {
            let selection_changed = current.source.endpoint_id != self.control.source.endpoint_id;
            self.control = current.clone();
            drop(current);
            if selection_changed {
                self.close();
                self.delivery.handle.publish("starting", None, None, None, None);
                self.retry_at = Instant::now();
            }
        } else {
            drop(current);
        }
        if let Some(binding) = self
            .delivery
            .handle
            .device
            .snapshot_after(self.binding.revision)
        {
            let changed = self.binding.endpoint != binding.endpoint;
            self.binding = binding;
            if changed {
                self.close();
                self.delivery.handle.publish("starting", None, None, None, None);
                self.retry_at = Instant::now();
            }
        }
        let source = &self.control.source;
        if !source.publish && !source.monitor {
            self.close();
            self.delivery
                .handle
                .publish("disabled", None, None, None, None);
            return Ok(false);
        }
        if source.endpoint_id.is_none() {
            self.close();
            self.delivery.handle.publish(
                "missing",
                None,
                None,
                Some("Select a capture device for this source.".into()),
                None,
            );
            return Ok(false);
        }
        if self.binding.revision == 0 || self.binding.selection != source.endpoint_id {
            self.close();
            self.delivery
                .handle
                .publish("starting", None, None, None, None);
            return Ok(false);
        }
        if !source.monitor && !self.delivery.handle.demand.load(Ordering::Acquire) {
            // Retain only a last-known descriptor. No native stream or PCM
            // queue is held solely because publication is enabled. Receiver
            // subscriptions remain independent and can wake the acquisition.
            let previous = self.delivery.handle.snapshot();
            if self.device.is_some() {
                self.close();
            }
            self.delivery.handle.publish(
                "idle",
                previous.acquisition,
                self.binding.endpoint.as_ref().map(|binding| binding.endpoint_id.clone()),
                None,
                None,
            );
            return Ok(false);
        }
        if self.device.is_some() && self.opened_refresh != self.control.refresh {
            self.close();
            self.retry_at = Instant::now();
        }
        if self.device.is_none() && Instant::now() >= self.retry_at {
            let binding = self.binding.endpoint.as_ref().ok_or_else(|| {
                self.binding
                    .error
                    .clone()
                    .unwrap_or_else(|| "The capture device is unavailable.".into())
            })?;
            self.instance = self
                .instance
                .checked_add(1)
                .ok_or("Capture instance counter exhausted.")?;
            self.delivery
                .handle
                .publish("starting", None, None, None, None);
            let device = NativeDevice::open(
                &self.control.source,
                binding,
                &self.delivery.output,
                self.instance,
            )?;
            self.delivery.acquisition = Some(device.acquisition);
            self.delivery.handle.publish(
                "ready",
                Some(device.acquisition),
                Some(device.endpoint.clone()),
                None,
                device.scheduling_error.clone(),
            );
            self.device = Some(device);
            self.opened_refresh = self.control.refresh;
        }
        self.delivery
            .monitor(&self.control.source, self.control.monitor_generation)?;
        let Some(device) = self.device.as_mut() else {
            return Ok(false);
        };
        let (epoch, boundary) = device
            .stream
            .native_boundary()
            .ok_or("The native capture boundary is unavailable.")?;
        if epoch != device.acquisition.epoch {
            return Err("Capture boundary belongs to another acquisition.".into());
        }
        self.delivery.acknowledge(
            self.control.request.filter(|_| self.control.source.publish),
            boundary,
        )?;
        for _ in 0..32 {
            if self.delivery.handle.stop.load(Ordering::Acquire)
                || self
                    .delivery
                    .handle
                    .control
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .revision
                    != self.control.revision
            {
                return Ok(true);
            }
            let read_started = Instant::now();
            match device.stream.read_native_f64(&mut device.scratch) {
                Ok(NativeCaptureRead::Idle | NativeCaptureRead::Stopped) => return Ok(false),
                Ok(NativeCaptureRead::Packet(packet)) => {
                    let len = packet.frames as usize * usize::from(packet.format.channels);
                    self.delivery.packet(
                        packet,
                        &device.scratch[..len],
                        self.control.request.filter(|_| self.control.source.publish),
                        read_started,
                    )?;
                }
                Err(WindowsAudioError::NativeSamplesNonFinite {
                    start_sample,
                    frames,
                    ..
                }) => {
                    self.delivery.tickets.allocate()?;
                    self.delivery.handle.dropped.fetch_add(1, Ordering::Relaxed);
                    if let Some(monitor) = &self.delivery.monitor {
                        monitor.discard_through(start_sample + u64::from(frames));
                    }
                }
                Err(WindowsAudioError::CaptureCapacityExceeded) => {
                    self.delivery.tickets.allocate()?;
                    self.delivery.handle.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(WindowsAudioError::UnknownDiscontinuity) => {
                    self.delivery.tickets.allocate()?;
                    self.close();
                    self.retry_at = Instant::now();
                    self.delivery
                        .handle
                        .publish("starting", None, None, None, None);
                    return Ok(true);
                }
                Err(error) => {
                    self.delivery.tickets.allocate()?;
                    return Err(error.to_string());
                }
            }
        }
        Ok(true)
    }

    fn close(&mut self) {
        self.delivery.close(self.control.source.id);
        self.device = None;
    }
}
