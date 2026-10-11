use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use rlb_audio::{
    AudioMetrics, CaptureBackend, CaptureBlockConsumer, CaptureBlockPool, CaptureBlockProducer,
    CaptureInput, CaptureProcessBuffer, CaptureProcessResult, CaptureReblocker, MixGraphConsumer,
    RenderBackend, SharedRenderMixer, SoftLimiter,
};
use rlb_model::MEDIA_UNIT_FRAMES;

use crate::capture::{CaptureFlow, CaptureRequest, CaptureStreamDiagnostics, WasapiCaptureStream};
use crate::client::EventWait;
use crate::endpoint::EndpointEnumerator;
use crate::mmcss::MmcssRegistration;
use crate::render::{RenderRequest, RenderStreamDiagnostics, WasapiRenderStream};
use crate::{WasapiFormat, WindowsAudioError};

/// First-release mix graph: 32 members × two sources.
pub const RENDER_SOURCE_CAPACITY: usize = 64;

const WAIT_TIMEOUT_MS: u32 = 20;
const CAPTURE_POOL_BLOCKS: usize = 16;
const CAPTURE_READY_CAPACITY: usize = 8;

/// Negotiated capture stream published after the worker thread has started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureWorkerReady {
    /// Shared-engine mix format observed before initialize.
    pub mix_format: Option<WasapiFormat>,
    /// 48 kHz f32 format requested from WASAPI.
    pub engine_format: WasapiFormat,
    /// Whether Windows performed rate/channel conversion.
    pub autoconvert: bool,
    /// Whether `IAudioClient3::InitializeSharedAudioStream` succeeded.
    pub iaudioclient3: bool,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    /// Negotiated callback period in frames.
    pub period_frames: u32,
}

/// Capture counters returned after a normal worker stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureWorkerReport {
    /// Stream counters at stop.
    pub diagnostics: CaptureStreamDiagnostics,
}

/// Dedicated MMCSS capture thread that waits on the WASAPI event.
pub struct CaptureWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<CaptureWorkerReport, WindowsAudioError>>>,
    consumer: Option<CaptureBlockConsumer>,
    ready: CaptureWorkerReady,
    endpoint_id: String,
    friendly_name: String,
}

struct CaptureStartup {
    ready: CaptureWorkerReady,
    consumer: CaptureBlockConsumer,
    endpoint_id: String,
    friendly_name: String,
}

impl CaptureWorker {
    /// Opens loopback or microphone capture on a new MTA worker thread.
    pub fn spawn(
        request: CaptureRequest,
        metrics: Arc<AudioMetrics>,
    ) -> Result<Self, WindowsAudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let (startup_tx, startup_rx) = sync_channel(1);
        let thread_name = match request.flow {
            CaptureFlow::Loopback => "rlb-wasapi-sys",
            CaptureFlow::Microphone => "rlb-wasapi-mic",
        };
        let join = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || capture_thread(request, metrics, stop_for_thread, startup_tx))
            .map_err(|error| WindowsAudioError::ThreadSpawn {
                message: error.to_string(),
            })?;
        match startup_rx.recv() {
            Ok(Ok(startup)) => Ok(Self {
                stop,
                join: Some(join),
                consumer: Some(startup.consumer),
                ready: startup.ready,
                endpoint_id: startup.endpoint_id,
                friendly_name: startup.friendly_name,
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => Err(join_startup_failure(join)),
        }
    }

    /// Encoder-side half of the preallocated 480-frame unit queue.
    pub fn consumer(&mut self) -> Option<&mut CaptureBlockConsumer> {
        self.consumer.as_mut()
    }

    /// Moves the encoder queue to a source-encoder worker.
    ///
    /// After this call the capture thread still produces units. Recycle and
    /// packetization belong to the encoder until it is stopped.
    pub fn take_consumer(&mut self) -> Option<CaptureBlockConsumer> {
        self.consumer.take()
    }

    /// Stream negotiation published before the first wait.
    #[must_use]
    pub const fn ready(&self) -> CaptureWorkerReady {
        self.ready
    }

    /// Stable endpoint identity captured at open.
    #[must_use]
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// User-visible endpoint label captured at open.
    #[must_use]
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Asks the worker to leave MMCSS and join the thread.
    pub fn stop(mut self) -> Result<CaptureWorkerReport, WindowsAudioError> {
        self.signal_stop();
        take_join(&mut self.join)
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn capture_thread(
    request: CaptureRequest,
    metrics: Arc<AudioMetrics>,
    stop: Arc<AtomicBool>,
    startup: SyncSender<Result<CaptureStartup, WindowsAudioError>>,
) -> Result<CaptureWorkerReport, WindowsAudioError> {
    let mut session = match CaptureSession::open(request, metrics) {
        Ok(session) => session,
        Err(error) => {
            let _ = startup.send(Err(error.clone()));
            return Err(error);
        }
    };
    let payload = session.take_startup();
    if startup.send(Ok(payload)).is_err() {
        let _ = session.stream.stop();
        return Err(WindowsAudioError::StartupChannelClosed);
    }
    session.run(&stop)
}

struct CaptureSession {
    stream: WasapiCaptureStream,
    producer: CaptureBlockProducer,
    reblocker: CaptureReblocker,
    storage: Vec<f32>,
    frame_capacity: usize,
    channels: u8,
    startup: Option<CaptureStartup>,
    _mmcss: MmcssRegistration,
    _enumerator: EndpointEnumerator,
}

impl CaptureSession {
    fn open(
        request: CaptureRequest,
        metrics: Arc<AudioMetrics>,
    ) -> Result<Self, WindowsAudioError> {
        let source_id = request.source_id;
        let stream_epoch = request.stream_epoch;
        let channels = request.channels;
        let enumerator = EndpointEnumerator::new()?;
        let mut stream = enumerator.open_capture(request)?;
        let endpoint_id = stream.endpoint().endpoint_id().to_owned();
        let friendly_name = stream.endpoint().friendly_name().to_owned();
        let diagnostics = stream.diagnostics();
        let ready = CaptureWorkerReady {
            mix_format: diagnostics.mix_format,
            engine_format: diagnostics.engine_format,
            autoconvert: diagnostics.autoconvert,
            iaudioclient3: diagnostics.iaudioclient3,
            buffer_frames: diagnostics.buffer_frames,
            period_frames: diagnostics.period_frames,
        };
        let pool = CaptureBlockPool::new(
            CAPTURE_POOL_BLOCKS,
            usize::from(MEDIA_UNIT_FRAMES),
            channels,
        )?;
        let (producer, consumer) = pool.into_spsc(CAPTURE_READY_CAPACITY)?;
        let reblocker = CaptureReblocker::new(
            source_id,
            stream_epoch,
            channels,
            stream.clock_domain(),
            metrics,
        )?;
        let frame_capacity = stream.callback_capacity_frames() as usize;
        let storage = vec![0.0_f32; frame_capacity.saturating_mul(usize::from(channels))];
        let mmcss = MmcssRegistration::register_pro_audio()?;
        stream.start()?;
        Ok(Self {
            stream,
            producer,
            reblocker,
            storage,
            frame_capacity,
            channels,
            startup: Some(CaptureStartup {
                ready,
                consumer,
                endpoint_id,
                friendly_name,
            }),
            _mmcss: mmcss,
            _enumerator: enumerator,
        })
    }

    fn take_startup(&mut self) -> CaptureStartup {
        self.startup
            .take()
            .expect("capture worker startup is taken once")
    }

    fn run(mut self, stop: &AtomicBool) -> Result<CaptureWorkerReport, WindowsAudioError> {
        let mut destination =
            CaptureProcessBuffer::new(&mut self.storage, self.frame_capacity, self.channels)?;
        while !stop.load(Ordering::Relaxed) {
            match self.stream.wait(WAIT_TIMEOUT_MS)? {
                EventWait::Timeout => continue,
                EventWait::Signaled => loop {
                    match self.stream.process(&mut destination)? {
                        CaptureProcessResult::Idle => break,
                        CaptureProcessResult::EndOfStream => {
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                        CaptureProcessResult::Captured(metadata) => {
                            let frames = usize::try_from(metadata.frames).map_err(|_| {
                                rlb_audio::AudioError::CapacityOverflow {
                                    field: "capture_frames",
                                }
                            })?;
                            let samples = destination.captured(frames, metadata.channels)?;
                            let input = CaptureInput::new(metadata, samples)?;
                            self.reblocker.process_spsc(input, &mut self.producer)?;
                        }
                    }
                },
            }
        }
        self.stream.stop()?;
        Ok(CaptureWorkerReport {
            diagnostics: self.stream.diagnostics(),
        })
    }
}

/// Negotiated render stream published after the worker thread has started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderWorkerReady {
    /// Shared-engine mix format observed before initialize.
    pub mix_format: Option<WasapiFormat>,
    /// 48 kHz stereo f32 format requested from WASAPI.
    pub engine_format: WasapiFormat,
    /// Whether Windows performed rate/channel conversion.
    pub autoconvert: bool,
    /// Whether `IAudioClient3::InitializeSharedAudioStream` succeeded.
    pub iaudioclient3: bool,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    /// Negotiated callback period in frames.
    pub period_frames: u32,
}

/// Render counters returned after a normal worker stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderWorkerReport {
    /// Stream counters at stop.
    pub diagnostics: RenderStreamDiagnostics,
}

/// Dedicated MMCSS render thread that waits on the WASAPI event.
pub struct RenderWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<RenderWorkerReport, WindowsAudioError>>>,
    ready: RenderWorkerReady,
    endpoint_id: String,
    friendly_name: String,
}

struct RenderStartup {
    ready: RenderWorkerReady,
    endpoint_id: String,
    friendly_name: String,
}

impl RenderWorker {
    /// Opens shared-mode system playback on a new MTA worker thread.
    ///
    /// `mix` is drained with `try_recv` on this thread before every mixer
    /// callback. Reader drops travel back through the paired reclaimer.
    pub fn spawn(
        request: RenderRequest,
        metrics: Arc<AudioMetrics>,
        mix: MixGraphConsumer,
    ) -> Result<Self, WindowsAudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let (startup_tx, startup_rx) = sync_channel(1);
        let join = thread::Builder::new()
            .name("rlb-wasapi-rnd".to_owned())
            .spawn(move || render_thread(request, metrics, mix, stop_for_thread, startup_tx))
            .map_err(|error| WindowsAudioError::ThreadSpawn {
                message: error.to_string(),
            })?;
        match startup_rx.recv() {
            Ok(Ok(startup)) => Ok(Self {
                stop,
                join: Some(join),
                ready: startup.ready,
                endpoint_id: startup.endpoint_id,
                friendly_name: startup.friendly_name,
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => Err(join_startup_failure(join)),
        }
    }

    /// Stream negotiation published before the first wait.
    #[must_use]
    pub const fn ready(&self) -> RenderWorkerReady {
        self.ready
    }

    /// Stable endpoint identity captured at open.
    #[must_use]
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// User-visible endpoint label captured at open.
    #[must_use]
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Asks the worker to leave MMCSS and join the thread.
    pub fn stop(mut self) -> Result<RenderWorkerReport, WindowsAudioError> {
        self.signal_stop();
        take_join(&mut self.join)
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn render_thread(
    request: RenderRequest,
    metrics: Arc<AudioMetrics>,
    mix: MixGraphConsumer,
    stop: Arc<AtomicBool>,
    startup: SyncSender<Result<RenderStartup, WindowsAudioError>>,
) -> Result<RenderWorkerReport, WindowsAudioError> {
    let mut session = match RenderSession::open(request, metrics, mix) {
        Ok(session) => session,
        Err(error) => {
            let _ = startup.send(Err(error.clone()));
            return Err(error);
        }
    };
    let payload = session.take_startup();
    if startup.send(Ok(payload)).is_err() {
        let _ = session.stream.stop();
        return Err(WindowsAudioError::StartupChannelClosed);
    }
    session.run(&stop)
}

struct RenderSession {
    stream: WasapiRenderStream,
    mixer: SharedRenderMixer,
    mix: MixGraphConsumer,
    output: Vec<f32>,
    period_frames: u32,
    startup: Option<RenderStartup>,
    _mmcss: MmcssRegistration,
    _enumerator: EndpointEnumerator,
}

impl RenderSession {
    fn open(
        request: RenderRequest,
        metrics: Arc<AudioMetrics>,
        mix: MixGraphConsumer,
    ) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        let mut stream = enumerator.open_render(request)?;
        let endpoint_id = stream.endpoint().endpoint_id().to_owned();
        let friendly_name = stream.endpoint().friendly_name().to_owned();
        let diagnostics = stream.diagnostics();
        let ready = RenderWorkerReady {
            mix_format: diagnostics.mix_format,
            engine_format: diagnostics.engine_format,
            autoconvert: diagnostics.autoconvert,
            iaudioclient3: diagnostics.iaudioclient3,
            buffer_frames: diagnostics.buffer_frames,
            period_frames: diagnostics.period_frames,
        };
        let buffer_frames = stream.callback_capacity_frames() as usize;
        let output = vec![0.0_f32; buffer_frames.saturating_mul(2)];
        let mixer = SharedRenderMixer::new(
            RENDER_SOURCE_CAPACITY,
            buffer_frames,
            SoftLimiter::default(),
            metrics,
        )?;
        let period_frames = diagnostics.period_frames.max(1);
        let mmcss = MmcssRegistration::register_pro_audio()?;
        stream.start()?;
        Ok(Self {
            stream,
            mixer,
            mix,
            output,
            period_frames,
            startup: Some(RenderStartup {
                ready,
                endpoint_id,
                friendly_name,
            }),
            _mmcss: mmcss,
            _enumerator: enumerator,
        })
    }

    fn take_startup(&mut self) -> RenderStartup {
        self.startup
            .take()
            .expect("render worker startup is taken once")
    }

    fn run(mut self, stop: &AtomicBool) -> Result<RenderWorkerReport, WindowsAudioError> {
        while !stop.load(Ordering::Relaxed) {
            match self.stream.wait(WAIT_TIMEOUT_MS)? {
                EventWait::Timeout => continue,
                EventWait::Signaled => {
                    let available = self.stream.available_frames()?;
                    if available == 0 {
                        continue;
                    }
                    let frames = available.min(self.period_frames) as usize;
                    let required = frames.saturating_mul(2);
                    if required > self.output.len() {
                        continue;
                    }
                    let mixed = &mut self.output[..required];
                    self.mix.try_apply_all(&mut self.mixer);
                    self.mixer.render(mixed, frames)?;
                    self.stream.render(mixed, frames)?;
                }
            }
        }
        self.stream.stop()?;
        Ok(RenderWorkerReport {
            diagnostics: self.stream.diagnostics(),
        })
    }
}

fn join_startup_failure<T>(join: JoinHandle<Result<T, WindowsAudioError>>) -> WindowsAudioError {
    match join.join() {
        Ok(Err(error)) => error,
        Ok(Ok(_)) => WindowsAudioError::StartupChannelClosed,
        Err(_) => WindowsAudioError::ThreadPanicked,
    }
}

fn take_join<T>(
    join: &mut Option<JoinHandle<Result<T, WindowsAudioError>>>,
) -> Result<T, WindowsAudioError> {
    match join.take() {
        Some(join) => match join.join() {
            Ok(result) => result,
            Err(_) => Err(WindowsAudioError::ThreadPanicked),
        },
        None => Err(WindowsAudioError::StartupChannelClosed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointRole, EndpointSelection};
    use rlb_audio::EndpointDirection;
    use rlb_model::{SourceId, StreamEpoch};
    use std::time::{Duration, Instant};

    fn ids() -> (SourceId, StreamEpoch) {
        (
            SourceId::generate().expect("source"),
            StreamEpoch::generate().expect("epoch"),
        )
    }

    fn has_default_render() -> bool {
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        enumerator
            .enumerate()
            .expect("catalog")
            .default_endpoint(EndpointDirection::Render, EndpointRole::Console)
            .is_some()
    }

    fn has_default_capture() -> bool {
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        enumerator
            .enumerate()
            .expect("catalog")
            .default_endpoint(EndpointDirection::Capture, EndpointRole::Console)
            .is_some()
    }

    #[test]
    fn capture_worker_publishes_reblocked_units() {
        if !has_default_render() {
            return;
        }
        let (source_id, stream_epoch) = ids();
        let metrics = Arc::new(AudioMetrics::new());
        let mut worker = CaptureWorker::spawn(
            CaptureRequest {
                selection: EndpointSelection::follow_default(),
                flow: CaptureFlow::Loopback,
                source_id,
                stream_epoch,
                channels: 2,
            },
            Arc::clone(&metrics),
        )
        .expect("spawn capture worker");
        assert!(worker.ready().engine_format.is_engine_f32(2));
        assert!(!worker.endpoint_id().is_empty());

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut units = 0_u32;
        while Instant::now() < deadline {
            if let Some(consumer) = worker.consumer() {
                while let Some(block) = consumer.try_pop() {
                    let metadata = block.metadata().expect("unit metadata");
                    assert_eq!(metadata.source_id, source_id);
                    assert_eq!(metadata.stream_epoch, stream_epoch);
                    assert_eq!(metadata.frames, u32::from(MEDIA_UNIT_FRAMES));
                    assert_eq!(metadata.channels, 2);
                    units = units.saturating_add(1);
                    consumer.release(block).expect("recycle unit");
                }
            }
            if units > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let report = worker.stop().expect("stop capture worker");
        assert!(
            units > 0,
            "capture worker should publish at least one 480-frame unit"
        );
        assert!(report.diagnostics.captured_packets > 0);
        let snapshot = metrics.snapshot();
        assert!(snapshot.capture_frames_total > 0);
        assert!(snapshot.reblock_units_total > 0);
    }

    #[test]
    fn microphone_worker_publishes_mono_units() {
        if !has_default_capture() {
            return;
        }
        let (source_id, stream_epoch) = ids();
        let metrics = Arc::new(AudioMetrics::new());
        let mut worker = CaptureWorker::spawn(
            CaptureRequest {
                selection: EndpointSelection::follow_default(),
                flow: CaptureFlow::Microphone,
                source_id,
                stream_epoch,
                channels: 1,
            },
            Arc::clone(&metrics),
        )
        .expect("spawn microphone worker");
        assert!(worker.ready().engine_format.is_engine_f32(1));

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut units = 0_u32;
        while Instant::now() < deadline {
            if let Some(consumer) = worker.consumer() {
                while let Some(block) = consumer.try_pop() {
                    let metadata = block.metadata().expect("unit metadata");
                    assert_eq!(metadata.source_id, source_id);
                    assert_eq!(metadata.channels, 1);
                    assert_eq!(metadata.frames, u32::from(MEDIA_UNIT_FRAMES));
                    units = units.saturating_add(1);
                    consumer.release(block).expect("recycle unit");
                }
            }
            if units > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let report = worker.stop().expect("stop microphone worker");
        assert!(
            units > 0,
            "microphone worker should publish at least one 480-frame unit"
        );
        assert!(report.diagnostics.captured_packets > 0);
    }

    #[test]
    fn render_worker_mixes_empty_graph_into_shared_output() {
        if !has_default_render() {
            return;
        }
        let metrics = Arc::new(AudioMetrics::new());
        let (_publisher, mix, mut reclaimer) = rlb_audio::mix_graph_channel().expect("mix graph");
        let worker = RenderWorker::spawn(
            RenderRequest {
                selection: EndpointSelection::follow_default(),
            },
            Arc::clone(&metrics),
            mix,
        )
        .expect("spawn render worker");
        assert!(worker.ready().engine_format.is_engine_f32(2));
        thread::sleep(Duration::from_millis(250));
        let report = worker.stop().expect("stop render worker");
        reclaimer.drain();
        assert!(
            report.diagnostics.submitted_frames > 0,
            "render worker should submit at least one mixed callback"
        );
        assert!(metrics.snapshot().render_frames_total > 0);
    }

    #[test]
    fn missing_pin_fails_before_worker_runs() {
        let (source_id, stream_epoch) = ids();
        let error = CaptureWorker::spawn(
            CaptureRequest {
                selection: EndpointSelection::fixed("missing-endpoint"),
                flow: CaptureFlow::Microphone,
                source_id,
                stream_epoch,
                channels: 1,
            },
            Arc::new(AudioMetrics::new()),
        )
        .err()
        .expect("missing pin");
        assert!(matches!(error, WindowsAudioError::DeviceUnavailable { .. }));
        assert_eq!(error.stable_code(), "device_unavailable");
    }
}
