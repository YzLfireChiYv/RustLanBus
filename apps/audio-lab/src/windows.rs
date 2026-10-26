//! Explicit-device, direct-rate diagnostic wiring. No device settings are changed.

use std::error::Error;
use std::io;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rlb_audio::{
    CaptureBackend, CaptureMetadata, CaptureProcessBuffer, CaptureProcessResult, DirectPcmPlayout,
    DirectPlayoutConfig, EndpointDirection, EndpointState, FrameTimeQuality, PcmPacket,
    RenderBackend,
};
use rlb_audio_windows::{
    CaptureFlow, CaptureRequest, EndpointCatalog, EndpointEnumerator, EndpointSelection, EventWait,
    MmcssRegistration, RenderRequest, SampleEncoding, SharedPeriodPreference, WasapiFormat,
    wasapi_clock_domain, wasapi_qpc_100ns_now,
};

use crate::cli::{self, Command, LoopRoute, ProbeRoute};
use crate::profile::{
    self, CatalogDirection, CatalogEndpoint, CatalogState, EndpointFormat, PlaybackMode,
    RouteCatalog, SourceKind,
};
use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};
use rlb_peer::{
    Catalogue, ChannelLayout, Intent, MAX_DATAGRAM_BYTES, PcmEncoding, PcmFormat, Peer,
    SourceKind as PublicationKind, SourcePublisher,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const MAX_FRAMES: usize = 4096;
const POOL_BLOCKS: usize = 8;
const CAPTURE_MAX_AGE_HNS: u64 = 1_000_000; // 100 ms, a diagnostic stale cutoff.

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args)? {
        Command::Devices => devices(),
        Command::Probe(route) => probe(route),
        Command::Loopback { route, seconds } => loopback(route, seconds),
        Command::ProfileInit {
            path,
            source_index,
            output_index,
            playback,
        } => profile_init(&path, source_index, output_index, playback),
        Command::ProfileInspect(path) => inspect_profile(&path),
        Command::ProfilePreflight(path) => preflight_profile(&path),
    }
}

fn devices() -> Result<()> {
    let enumerator = EndpointEnumerator::new()?;
    let catalog = enumerator.enumerate()?;
    for (index, endpoint) in catalog.endpoints().iter().enumerate() {
        println!(
            "{index}: {:?} {:?} {}\n  id={}\n  mix={:?}",
            endpoint.direction(),
            endpoint.state(),
            endpoint.friendly_name(),
            endpoint.endpoint_id(),
            endpoint.mix_format()
        );
    }
    Ok(())
}

struct SourceRoute {
    id: String,
    flow: CaptureFlow,
}

fn native_stereo(format: WasapiFormat) -> bool {
    format.is_engine_f32(CHANNELS) && matches!(format.channel_mask, 0 | 3)
}

struct IndexSelection {
    source: SourceRoute,
    output_id: String,
    source_label: String,
    output_label: String,
}

fn resolve_indexes(source_index: usize, output_index: usize) -> Result<IndexSelection> {
    let enumerator = EndpointEnumerator::new()?;
    let catalog = enumerator.enumerate()?;
    let source = catalog
        .endpoints()
        .get(source_index)
        .ok_or("source index absent")?;
    let output = catalog
        .endpoints()
        .get(output_index)
        .ok_or("output index absent")?;
    if source.endpoint_id() == output.endpoint_id() {
        return Err("source and output must be separate: endpoint loopback includes all playback on its source".into());
    }
    for endpoint in [source, output] {
        if endpoint.state() != EndpointState::Active {
            return Err(format!("{} is not active", endpoint.friendly_name()).into());
        }
        if !endpoint
            .mix_format()
            .is_some_and(|format| format.is_engine_f32(CHANNELS))
        {
            return Err(format!(
                "{}: this direct-rate slice requires native 48 kHz stereo F32; observed {:?}",
                endpoint.friendly_name(),
                endpoint.mix_format()
            )
            .into());
        }
        if !endpoint
            .mix_format()
            .is_some_and(|format| matches!(format.channel_mask, 0 | 3))
        {
            return Err(
                "this stereo slice requires an unspecified or front-left/front-right channel mask"
                    .into(),
            );
        }
    }
    if output.direction() != EndpointDirection::Render {
        return Err("output must be a render endpoint".into());
    }
    let flow = match source.direction() {
        EndpointDirection::Render => CaptureFlow::Loopback,
        EndpointDirection::Capture => CaptureFlow::Microphone,
        EndpointDirection::LoopbackCapture => {
            return Err("choose a physical catalog endpoint for loopback acquisition".into());
        }
    };
    Ok(IndexSelection {
        source: SourceRoute {
            id: source.endpoint_id().to_owned(),
            flow,
        },
        output_id: output.endpoint_id().to_owned(),
        source_label: source.friendly_name().to_owned(),
        output_label: output.friendly_name().to_owned(),
    })
}

fn print_index_selection(selection: &IndexSelection) {
    println!(
        "original source={}\nplayback output={}",
        selection.source_label, selection.output_label
    );
}

fn capture_request(source: SourceRoute) -> Result<CaptureRequest> {
    Ok(CaptureRequest {
        selection: EndpointSelection::fixed(source.id),
        flow: source.flow,
        source_id: SourceId::generate()?,
        stream_epoch: StreamEpoch::generate()?,
        channels: CHANNELS,
    })
}

fn probe(route: ProbeRoute) -> Result<()> {
    let (source, output) = match route {
        ProbeRoute::Indexes { source, output } => {
            let selection = resolve_indexes(source, output)?;
            print_index_selection(&selection);
            (selection.source, selection.output_id)
        }
        ProbeRoute::Profile(path) => {
            let loaded = profile::load_profile(&path)?;
            println!("probe_playback=not_started");
            resolve_saved_route(&loaded)?
        }
    };
    open_probe_streams(source, output)
}

/// Opens the existing capture and render streams and returns before start.
fn open_probe_streams(source: SourceRoute, output: String) -> Result<()> {
    // Keep the COM apartment alive until both streams and their services drop.
    let enumerator = EndpointEnumerator::new()?;
    let capture = enumerator
        .open_capture_with_period(capture_request(source)?, SharedPeriodPreference::Minimum)?;
    let capture_diagnostics = capture.diagnostics();
    if capture_diagnostics.autoconvert || !capture_diagnostics.mix_format.is_some_and(native_stereo)
    {
        return Err("capture format changed or requires Windows conversion".into());
    }
    let render = enumerator.open_render_with_period(
        RenderRequest {
            selection: EndpointSelection::fixed(output),
        },
        SharedPeriodPreference::Minimum,
    )?;
    let render_diagnostics = render.diagnostics();
    if render_diagnostics.autoconvert || !render_diagnostics.mix_format.is_some_and(native_stereo) {
        return Err("output format changed or requires Windows conversion".into());
    }
    println!(
        "capture={:?}\nrender={:?}\ninitial output padding={} frames",
        capture.diagnostics(),
        render.diagnostics(),
        render.callback_capacity_frames() - render.available_frames()?
    );
    println!("probe opened streams without starting capture or playback");
    Ok(())
}

/// Reads the profile file and prints its saved choices. No enumerator and no stream.
fn inspect_profile(path: &std::path::Path) -> Result<()> {
    let loaded = profile::load_profile(path)?;
    println!("{}", loaded.inspection_report());
    Ok(())
}

/// Compares the saved ids with the current catalog. This does not open or start a stream.
fn preflight_profile(path: &std::path::Path) -> Result<()> {
    let loaded = profile::load_profile(path)?;
    let enumerator = EndpointEnumerator::new()?;
    let catalog = route_catalog(&enumerator.enumerate()?);
    println!("{}", loaded.preflight_report(&catalog));
    Ok(())
}

fn profile_init(
    path: &std::path::Path,
    source_index: usize,
    output_index: usize,
    playback: profile::SavedPlayback,
) -> Result<()> {
    let selection = resolve_indexes(source_index, output_index)?;
    let kind = match selection.source.flow {
        CaptureFlow::Loopback => SourceKind::RenderLoopback,
        CaptureFlow::Microphone => SourceKind::CaptureEndpoint,
    };
    let profile = profile::RouteProfile::from_selection(
        &selection.source.id,
        kind,
        Some(&selection.source_label),
        &selection.output_id,
        Some(&selection.output_label),
        playback,
    )?;
    profile::create_profile(path, &profile)?;
    println!("{}", profile.saved_configuration());
    println!("profile_path={}", path.display());
    println!("Init wrote the profile. Loading the file starts no audio, and no stream was opened.");
    Ok(())
}

fn resolve_saved_route(profile: &profile::RouteProfile) -> Result<(SourceRoute, String)> {
    let enumerator = EndpointEnumerator::new()?;
    let catalog = route_catalog(&enumerator.enumerate()?);
    println!("{}", profile.preflight_report(&catalog));
    checked_saved_route(profile, &catalog)
}

fn checked_saved_route(
    profile: &profile::RouteProfile,
    catalog: &RouteCatalog,
) -> Result<(SourceRoute, String)> {
    profile.require_catalog_eligible(catalog)?;
    let flow = match profile.source_kind() {
        SourceKind::RenderLoopback => CaptureFlow::Loopback,
        SourceKind::CaptureEndpoint => CaptureFlow::Microphone,
    };
    Ok((
        SourceRoute {
            id: profile.source_id().to_owned(),
            flow,
        },
        profile.output_id().to_owned(),
    ))
}

fn playback_options(playback: profile::SavedPlayback) -> PlaybackOptions {
    PlaybackOptions {
        mode: match playback.mode {
            PlaybackMode::Remote => Playback::Remote,
            PlaybackMode::Local => Playback::Local,
            PlaybackMode::Mix => Playback::Mix,
            PlaybackMode::Mute => Playback::Mute,
        },
        local_gain: playback.local_gain,
        remote_gain: playback.remote_gain,
        buffer_periods: usize::from(playback.buffer_periods),
    }
}

fn project_format(format: WasapiFormat) -> EndpointFormat {
    EndpointFormat {
        sample_rate_hz: format.sample_rate_hz,
        channels: format.channels,
        bits_per_sample: format.bits_per_sample,
        valid_bits_per_sample: format.valid_bits_per_sample,
        block_align: format.block_align,
        channel_mask: format.channel_mask,
        float32: matches!(format.encoding, SampleEncoding::Float),
    }
}

fn route_catalog(catalog: &EndpointCatalog) -> RouteCatalog {
    RouteCatalog::from_endpoints(
        catalog
            .endpoints()
            .iter()
            .map(|endpoint| CatalogEndpoint {
                id: endpoint.endpoint_id().to_owned(),
                label: endpoint.friendly_name().to_owned(),
                direction: match endpoint.direction() {
                    EndpointDirection::Render => CatalogDirection::Render,
                    EndpointDirection::Capture => CatalogDirection::Capture,
                    EndpointDirection::LoopbackCapture => CatalogDirection::Other,
                },
                state: match endpoint.state() {
                    EndpointState::Active => CatalogState::Active,
                    EndpointState::Disabled => CatalogState::Disabled,
                    EndpointState::Unplugged => CatalogState::Unplugged,
                    EndpointState::NotPresent => CatalogState::NotPresent,
                },
                format: endpoint.mix_format().map(project_format),
            })
            .collect(),
    )
}

#[derive(Clone, Copy, Debug)]
enum Playback {
    Remote,
    Local,
    Mix,
    Mute,
}

#[derive(Clone, Copy)]
struct PlaybackOptions {
    mode: Playback,
    local_gain: f32,
    remote_gain: f32,
    buffer_periods: usize,
}

struct Block {
    samples: Box<[f32]>,
    frames: usize,
    epoch: u64,
    start: u64,
    arrival_ms: u64,
    capture_hns: Option<u64>,
}

struct Producer {
    free: Receiver<Block>,
    ready: SyncSender<Block>,
    spare: Option<Block>,
    dropped: u64,
}

struct Consumer {
    ready: Receiver<Block>,
    free: SyncSender<Block>,
}

fn pipe() -> (Producer, Consumer) {
    let (free_tx, free_rx) = sync_channel(POOL_BLOCKS);
    let (ready_tx, ready_rx) = sync_channel(POOL_BLOCKS);
    for _ in 0..POOL_BLOCKS {
        free_tx
            .send(Block {
                samples: vec![0.0; MAX_FRAMES * 2].into_boxed_slice(),
                frames: 0,
                epoch: 0,
                start: 0,
                arrival_ms: 0,
                capture_hns: None,
            })
            .expect("new empty pool");
    }
    (
        Producer {
            free: free_rx,
            ready: ready_tx,
            spare: None,
            dropped: 0,
        },
        Consumer {
            ready: ready_rx,
            free: free_tx,
        },
    )
}

impl Producer {
    fn acquire(&mut self) -> Option<Block> {
        let block = self.spare.take().or_else(|| self.free.try_recv().ok());
        if block.is_none() {
            self.dropped += 1;
        }
        block
    }
    fn submit(&mut self, block: Block) {
        if let Err(error) = self.ready.try_send(block) {
            let (TrySendError::Full(block) | TrySendError::Disconnected(block)) = error;
            self.spare = Some(block);
            self.dropped += 1;
        }
    }
    fn copy(&mut self, samples: &[f32], epoch: u64, start: u64, arrival_ms: u64) {
        if let Some(mut block) = self.acquire() {
            block.samples[..samples.len()].copy_from_slice(samples);
            block.frames = samples.len() / 2;
            block.epoch = epoch;
            block.start = start;
            block.arrival_ms = arrival_ms;
            block.capture_hns = None;
            self.submit(block);
        }
    }
}

impl Consumer {
    fn pop(&self) -> Option<Block> {
        self.ready.try_recv().ok()
    }
    fn release(&self, block: Block) -> Result<()> {
        self.free
            .try_send(block)
            .map_err(|_| "preallocated block pool ownership lost".into())
    }
}

struct Workers {
    stop: Arc<AtomicBool>,
    capture: Option<JoinHandle<Result<()>>>,
    render: Option<JoinHandle<Result<()>>>,
}

impl Workers {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let mut failure = None;
        for handle in [&mut self.capture, &mut self.render] {
            if let Some(handle) = handle.take() {
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err("audio worker panicked".into()));
                if failure.is_none() {
                    failure = result.err();
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn millis(origin: Instant) -> u64 {
    origin.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Source age uses a reported first-frame QPC instant in the WASAPI domain.
///
/// Owner observations, estimated times, timestamp errors, and other clock
/// domains stay absent here. Absence is separate from a measured zero.
fn reported_first_frame_hns(metadata: &CaptureMetadata) -> Option<u64> {
    if metadata.clock_domain != wasapi_clock_domain() {
        return None;
    }
    let frame = metadata.timing.first_frame?;
    if frame.quality != FrameTimeQuality::ReportedCorrelated {
        return None;
    }
    Some(frame.at.ticks())
}

fn publication_stale(queue_age_ms: u64, source_age_hns: Option<u64>) -> bool {
    queue_age_ms > 100 || source_age_hns.is_some_and(|age| age > CAPTURE_MAX_AGE_HNS)
}

fn capture_age_hns(captured: Option<u64>, current: Option<u64>) -> Option<u64> {
    current?.checked_sub(captured?)
}

fn loopback(route: LoopRoute, seconds: u64) -> Result<()> {
    let (source, output, options) = match route {
        LoopRoute::Indexes {
            source,
            output,
            playback,
        } => {
            let selection = resolve_indexes(source, output)?;
            print_index_selection(&selection);
            (
                selection.source,
                selection.output_id,
                playback_options(playback),
            )
        }
        LoopRoute::Profile(path) => {
            let loaded = profile::load_profile(&path)?;
            println!(
                "This command applies the saved source, output, playback mode, gains, and buffer period."
            );
            println!("Peer identity and bilateral consent stay outside the saved route.");
            let (source, output) = resolve_saved_route(&loaded)?;
            (source, output, playback_options(loaded.playback()))
        }
    };
    run_bounded_loopback(source, output, seconds, options)
}

fn run_bounded_loopback(
    source: SourceRoute,
    output: String,
    seconds: u64,
    options: PlaybackOptions,
) -> Result<()> {
    if seconds == 0 || seconds > 300 {
        return Err("seconds must be 1..300 for this bounded experiment".into());
    }
    let playback = options.mode;
    let local_gain = options.local_gain;
    let remote_gain = options.remote_gain;
    let buffer_periods = options.buffer_periods;
    println!(
        "UDP self-loop: play={playback:?} local_gain={local_gain} remote_gain={remote_gain}; no limiter or ASRC"
    );
    let (original_tx, original_rx) = pipe();
    let (mut local_tx, local_rx) = pipe();
    let (mut remote_tx, remote_rx) = pipe();
    let origin = Instant::now();
    let mut workers = Workers {
        stop: Arc::new(AtomicBool::new(false)),
        capture: None,
        render: None,
    };
    let stop = Arc::clone(&workers.stop);
    workers.render = Some(thread::Builder::new().name("rlb-lab-render".into()).spawn(
        move || {
            render_thread(
                output,
                local_rx,
                remote_rx,
                stop,
                origin,
                PlaybackOptions {
                    mode: playback,
                    local_gain,
                    remote_gain,
                    buffer_periods,
                },
            )
        },
    )?);
    let selected_kind = match source.flow {
        CaptureFlow::Loopback => PublicationKind::System,
        CaptureFlow::Microphone => PublicationKind::Microphone,
    };
    let stop = Arc::clone(&workers.stop);
    workers.capture = Some(
        thread::Builder::new()
            .name("rlb-lab-capture".into())
            .spawn(move || capture_thread(source, original_tx, stop, origin))?,
    );

    // Both peer identities live in this process; this loop has no external address or registry.
    let a_socket = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let b_socket = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    a_socket.connect(b_socket.local_addr()?)?;
    b_socket.connect(a_socket.local_addr()?)?;
    a_socket.set_nonblocking(true)?;
    b_socket.set_nonblocking(true)?;
    let a_id = MemberId::generate()?;
    let b_id = MemberId::generate()?;
    let selected = SourceId::generate()?;
    let unbound = SourceId::generate()?;
    let unbound_kind = match selected_kind {
        PublicationKind::System => PublicationKind::Microphone,
        PublicationKind::Microphone => PublicationKind::System,
    };
    let sender_run = RunId::generate()?;
    let mut publisher = SourcePublisher::new(sender_run);
    publisher.register(selected, selected_kind, true)?;
    publisher.register(unbound, unbound_kind, false)?;
    let format = PcmFormat::new(RATE, ChannelLayout::StereoLeftRight, PcmEncoding::F32Le, 32)?;
    let acquisition = StreamEpoch::generate()?;
    let mut pending = Some(
        publisher
            .begin_binding(selected, acquisition, format)?
            .pending
            .ok_or("the selected capture binding was not reserved")?,
    );
    println!(
        "Publication binds only the selected capture. The other saved source stays disabled and no second device is opened."
    );
    let mut a = Peer::new(
        a_id,
        b_id,
        sender_run,
        Intent {
            send: true,
            receive: false,
        },
        publisher.catalogue(),
        0,
    )?;
    let mut b = Peer::new(
        b_id,
        a_id,
        RunId::generate()?,
        Intent {
            send: false,
            receive: true,
        },
        Catalogue::new(Vec::new())?,
        0,
    )?;
    let mut committed = false;
    let mut encoded_frames = 0_u64;
    let mut received_frames = 0_u64;
    let mut stale_capture_frames = 0_u64;
    let mut source_age_unknown_frames = 0_u64;
    let mut rejected = 0_u64;
    // This legacy diagnostic captures F32. Widen exactly for the shared PCM API.
    let mut original_pcm = Vec::with_capacity(MAX_FRAMES * usize::from(CHANNELS));
    while origin.elapsed() < Duration::from_secs(seconds) && !workers.stop.load(Ordering::Relaxed) {
        let now = millis(origin);
        pump(&a_socket, &mut a, now, |_, _, _, _| {}, &mut rejected)?;
        pump(
            &b_socket,
            &mut b,
            now,
            |epoch, start, samples, now| {
                received_frames += (samples.len() / 2) as u64;
                remote_tx.copy(samples, epoch, start, now);
            },
            &mut rejected,
        )?;
        for _ in 0..POOL_BLOCKS {
            let Some(block) = original_rx.pop() else {
                break;
            };
            let samples = &block.samples[..block.frames * 2];
            let source_age = capture_age_hns(block.capture_hns, wasapi_qpc_100ns_now());
            if source_age.is_none() {
                source_age_unknown_frames += block.frames as u64;
            }
            let stale =
                publication_stale(millis(origin).saturating_sub(block.arrival_ms), source_age);
            if stale {
                stale_capture_frames += block.frames as u64;
            } else {
                if matches!(playback, Playback::Local | Playback::Mix) {
                    local_tx.copy(samples, block.epoch, block.start, block.arrival_ms);
                }
                if a.status().send_ready {
                    if !committed {
                        let request = pending
                            .take()
                            .ok_or("the selected capture binding was already consumed")?;
                        publisher.commit_binding(request, acquisition, block.start)?;
                        a.install_catalogue(&publisher.catalogue(), now)?;
                        committed = true;
                    }
                    original_pcm.clear();
                    original_pcm.extend(samples.iter().copied().map(f64::from));
                    let original = publisher.local_original(
                        selected,
                        acquisition,
                        block.start,
                        &original_pcm,
                        block.arrival_ms,
                    )?;
                    let prepared = publisher.prepare_pcm(original)?;
                    let mut forwarded = false;
                    for packet in prepared.as_slice() {
                        if let Some(bytes) = a.write_original(packet)? {
                            send(&a_socket, &bytes)?;
                            forwarded = true;
                        }
                    }
                    if forwarded {
                        encoded_frames += block.frames as u64;
                    }
                }
            }
            original_rx.release(block)?;
        }
        // This control/UDP loop is diagnostic polling; the audio owners wait on WASAPI events.
        thread::sleep(Duration::from_millis(1));
    }
    workers.finish()?;
    println!(
        "encoded_source_frames={encoded_frames} received_frames={received_frames} stale_capture_frames={stale_capture_frames} source_age_unknown_frames={source_age_unknown_frames} rejected_datagrams={rejected} local_queue_drops={} remote_queue_drops={}",
        local_tx.dropped, remote_tx.dropped
    );
    Ok(())
}

fn send(socket: &UdpSocket, packet: &[u8]) -> Result<()> {
    match socket.send(packet) {
        Ok(length) if length == packet.len() => Ok(()),
        Ok(_) => Err("partial UDP datagram".into()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn pump(
    socket: &UdpSocket,
    peer: &mut Peer,
    now: u64,
    mut audio: impl FnMut(u64, u64, &[f32], u64),
    rejected: &mut u64,
) -> Result<()> {
    peer.tick(now)?;
    let mut bytes = [0_u8; MAX_DATAGRAM_BYTES + 1];
    let mut f32_samples = Vec::with_capacity(MAX_DATAGRAM_BYTES / 4);
    for _ in 0..64 {
        match socket.recv(&mut bytes) {
            Ok(length) => match peer.ingest(&bytes[..length], now, now) {
                Ok(Some(packet))
                    if packet.sample_rate() == RATE && packet.channels() == u16::from(CHANNELS)
                        && packet.format.encoding() == PcmEncoding::F32Le =>
                {
                    // Only original F32 publications enter this F32-only lab sink.
                    f32_samples.clear();
                    f32_samples.extend(packet.samples.iter().map(|sample| *sample as f32));
                    audio(
                        packet.epoch.get(),
                        packet.start_frame,
                        &f32_samples,
                        packet.arrived_at_ms,
                    )
                }
                Ok(Some(_)) | Err(_) => *rejected += 1,
                Ok(None) => {}
            },
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.raw_os_error() == Some(10040) => *rejected += 1,
            Err(error) => return Err(error.into()),
        }
    }
    for _ in 0..8 {
        let Some(packet) = peer.take_control() else {
            break;
        };
        send(socket, &packet)?;
    }
    Ok(())
}

fn capture_thread(
    source: SourceRoute,
    mut output: Producer,
    stop: Arc<AtomicBool>,
    origin: Instant,
) -> Result<()> {
    let result = (|| {
        let enumerator = EndpointEnumerator::new()?;
        let mut stream = enumerator
            .open_capture_with_period(capture_request(source)?, SharedPeriodPreference::Minimum)?;
        let diagnostics = stream.diagnostics();
        if diagnostics.autoconvert || !diagnostics.mix_format.is_some_and(native_stereo) {
            return Err("capture format changed or requires Windows conversion".into());
        }
        let capacity = stream.callback_capacity_frames() as usize;
        if capacity > MAX_FRAMES {
            return Err("capture callback exceeds this lab's fixed capacity".into());
        }
        let mut storage = vec![0.0; capacity * 2];
        let mut destination = CaptureProcessBuffer::new(&mut storage, capacity, CHANNELS)?;
        let _mmcss = MmcssRegistration::register_pro_audio()?;
        println!("capture opened: {diagnostics:?}");
        stream.start()?;
        while !stop.load(Ordering::Relaxed) {
            if stream.wait(20)? == EventWait::Timeout {
                continue;
            }
            for _ in 0..32 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match stream.process(&mut destination)? {
                    CaptureProcessResult::Idle => break,
                    CaptureProcessResult::EndOfStream => return Ok(()),
                    CaptureProcessResult::Captured(metadata) => {
                        if stream.diagnostics().non_finite_samples != 0 {
                            return Err("capture returned non-finite samples; original PCM publication stopped".into());
                        }
                        let frames = metadata.frames as usize;
                        let samples = destination.captured(frames, CHANNELS)?;
                        if let Some(mut block) = output.acquire() {
                            let left = samples.channel(0).expect("stereo plane");
                            let right = samples.channel(1).expect("stereo plane");
                            for frame in 0..frames {
                                block.samples[frame * 2] = left[frame];
                                block.samples[frame * 2 + 1] = right[frame];
                            }
                            block.frames = frames;
                            block.start = metadata.start_sample;
                            block.epoch = 0;
                            block.arrival_ms = millis(origin);
                            block.capture_hns = reported_first_frame_hns(&metadata);
                            output.submit(block);
                        }
                    }
                }
            }
        }
        stream.stop()?;
        if stream.diagnostics().non_finite_samples != 0 {
            return Err("capture returned non-finite samples; precision check failed".into());
        }
        println!(
            "capture stopped: {:?}; queue_drops={}",
            stream.diagnostics(),
            output.dropped
        );
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    result
}

fn render_thread(
    output: String,
    local: Consumer,
    remote: Consumer,
    stop: Arc<AtomicBool>,
    origin: Instant,
    options: PlaybackOptions,
) -> Result<()> {
    let result = (|| {
        let enumerator = EndpointEnumerator::new()?;
        let mut stream = enumerator.open_render_with_period(
            RenderRequest {
                selection: EndpointSelection::fixed(output),
            },
            SharedPeriodPreference::Minimum,
        )?;
        let diagnostics = stream.diagnostics();
        if diagnostics.autoconvert || !diagnostics.mix_format.is_some_and(native_stereo) {
            return Err("output format changed or requires Windows conversion".into());
        }
        let capacity = stream.callback_capacity_frames() as usize;
        if capacity > MAX_FRAMES {
            return Err("render callback exceeds this lab's fixed capacity".into());
        }
        let render_quantum = (diagnostics.period_frames as usize).clamp(1, capacity);
        let target_backlog = render_quantum * options.buffer_periods;
        if target_backlog > 8192 {
            return Err("selected buffer target exceeds fixed timeline capacity".into());
        }
        if target_backlog as u64 * 1000 >= u64::from(RATE) * 100 {
            return Err("selected buffer target reaches the 100 ms freshness cutoff; choose fewer buffer periods".into());
        }
        // Policy values are exposed diagnostics, not a latency result or automatic tuning.
        let config = DirectPlayoutConfig {
            sample_rate: RATE,
            channels: CHANNELS,
            capacity_frames: 8192,
            target_backlog_frames: target_backlog,
            max_backlog_frames: (target_backlog * 4).min(8192),
            max_operation_frames: MAX_FRAMES,
            arrival_timeout_ms: 100,
        };
        let mut own = DirectPcmPlayout::new(config)?;
        let mut returned = DirectPcmPlayout::new(config)?;
        let mut own_samples = vec![0.0; capacity * 2];
        let mut remote_samples = vec![0.0; capacity * 2];
        let mut mixed = vec![0.0; capacity * 2];
        let _mmcss = MmcssRegistration::register_pro_audio()?;
        let mut over_unity_samples = 0_u64;
        let mut queue_expired_frames = 0_u64;
        let mut clock_observations = 0_u64;
        let mut degraded_clock_observations = 0_u64;
        let mut clock_observation_failures = 0_u64;
        let mut last_clock_observation = None;
        let mut last_clock_failure = None;
        println!("render opened: {diagnostics:?}; playout={config:?}");
        stream.start()?;
        while !stop.load(Ordering::Relaxed) {
            if stream.wait(20)? == EventWait::Timeout {
                continue;
            }
            ingest_playout(&local, &mut own, origin, &mut queue_expired_frames)?;
            ingest_playout(&remote, &mut returned, origin, &mut queue_expired_frames)?;
            let now = millis(origin);
            // Refill one event quantum, keeping the PCM window consistent with
            // the source backlog. A fully empty device buffer must not make us
            // consume future source frames before they could have arrived.
            // These adjacent clock/padding calls form separate observations;
            // no equation between clock progress and padding is assumed here.
            match stream.clock_observation() {
                Ok(observation) => {
                    clock_observations += 1;
                    degraded_clock_observations += u64::from(observation.is_degraded());
                    last_clock_observation = Some(observation);
                }
                Err(error) => {
                    clock_observation_failures += 1;
                    last_clock_failure = Some(error);
                }
            }
            let frames = (stream.available_frames()? as usize).min(render_quantum);
            if frames == 0 {
                continue;
            }
            let samples = frames * 2;
            own.render(now, &mut own_samples[..samples])?;
            returned.render(now, &mut remote_samples[..samples])?;
            mix(
                options.mode,
                options.local_gain,
                options.remote_gain,
                &own_samples[..samples],
                &remote_samples[..samples],
                &mut mixed[..samples],
            )?;
            over_unity_samples += mixed[..samples]
                .iter()
                .filter(|sample| sample.abs() > 1.0)
                .count() as u64;
            stream.render(&mixed[..samples], frames)?;
        }
        stream.stop()?;
        println!(
            "render stopped: {:?}; own={:?}; returned={:?}; queue_expired_frames={queue_expired_frames}; over_unity_samples={over_unity_samples}; clock_observations={clock_observations}; degraded_clock_observations={degraded_clock_observations}; clock_observation_failures={clock_observation_failures}; last_clock_observation={last_clock_observation:?}; last_clock_failure={last_clock_failure:?}",
            stream.diagnostics(),
            own.stats(),
            returned.stats()
        );
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    result
}

fn ingest_playout(
    input: &Consumer,
    playout: &mut DirectPcmPlayout,
    origin: Instant,
    expired: &mut u64,
) -> Result<()> {
    for _ in 0..POOL_BLOCKS {
        let Some(block) = input.pop() else {
            break;
        };
        // Preserve the actual arrival, while observing current time on this owner.
        let result = playout
            .push_received(
                PcmPacket {
                    epoch: block.epoch,
                    start_frame: block.start,
                    sample_rate: RATE,
                    channels: CHANNELS,
                    samples: &block.samples[..block.frames * 2],
                },
                block.arrival_ms,
                millis(origin),
            )
            .map(|report| {
                *expired += report.expired_input_frames as u64;
            });
        input.release(block)?;
        result?;
    }
    Ok(())
}

fn mix(
    playback: Playback,
    local_gain: f32,
    remote_gain: f32,
    own: &[f32],
    remote: &[f32],
    output: &mut [f32],
) -> Result<()> {
    match playback {
        Playback::Mute => output.fill(0.0),
        Playback::Local if local_gain == 1.0 => output.copy_from_slice(own),
        Playback::Remote if remote_gain == 1.0 => output.copy_from_slice(remote),
        _ => {
            for ((out, own), remote) in output.iter_mut().zip(own).zip(remote) {
                *out = match playback {
                    Playback::Local => *own * local_gain,
                    Playback::Remote => *remote * remote_gain,
                    Playback::Mix => {
                        (f64::from(*own) * f64::from(local_gain)
                            + f64::from(*remote) * f64::from(remote_gain))
                            as f32
                    }
                    Playback::Mute => 0.0,
                };
                if !out.is_finite() {
                    return Err(
                        "mixed PCM exceeds finite F32 range; reduce explicit source gains".into(),
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_audio::{CaptureClockDomain, CaptureTiming, FrameTimestamp};

    fn reported(at: u64) -> CaptureTiming {
        CaptureTiming {
            first_frame: Some(FrameTimestamp::reported(at)),
            observed_on_owner: Some(FrameTimestamp::reported(at.saturating_add(1)).at),
        }
    }

    fn age(timing: CaptureTiming, domain: CaptureClockDomain, current: Option<u64>) -> Option<u64> {
        let metadata = CaptureMetadata {
            source_id: SourceId::from_bytes([1; 16]).expect("source"),
            stream_epoch: StreamEpoch::from_bytes([2; 16]).expect("epoch"),
            start_sample: 0,
            frames: 1,
            sample_rate_hz: RATE,
            channels: CHANNELS,
            clock_domain: domain,
            timing,
            capture_time: timing.compatibility_capture_time(),
            device_position: None,
            discontinuity: rlb_audio::CaptureDiscontinuity::Continuous,
            silent: false,
        };
        capture_age_hns(reported_first_frame_hns(&metadata), current)
    }

    #[test]
    fn callback_fallback_cannot_claim_a_fresh_first_frame() {
        let domain = wasapi_clock_domain();
        assert_eq!(age(reported(10), domain, Some(100)), Some(90));
        assert_eq!(age(reported(0), domain, Some(100)), Some(100));

        let callback_only = CaptureTiming {
            first_frame: None,
            observed_on_owner: Some(FrameTimestamp::reported(100).at),
        };
        assert_eq!(callback_only.compatibility_capture_time().ticks(), 100);
        assert_eq!(age(callback_only, domain, Some(100)), None);

        let estimated = CaptureTiming {
            first_frame: Some(FrameTimestamp::estimated(0)),
            observed_on_owner: None,
        };
        assert_eq!(age(estimated, domain, Some(100)), None);
        assert_eq!(age(reported(110), domain, Some(100)), None);
        assert_eq!(age(reported(10), domain, None), None);

        let other = CaptureClockDomain::new(
            core::num::NonZeroU64::new(2).expect("domain"),
            core::num::NonZeroU64::new(10_000_000).expect("ticks"),
        );
        assert_eq!(age(reported(10), other, Some(100)), None);

        assert!(publication_stale(101, None));
        assert!(!publication_stale(100, None));
        assert!(publication_stale(0, Some(CAPTURE_MAX_AGE_HNS + 1)));
        assert!(!publication_stale(0, Some(CAPTURE_MAX_AGE_HNS)));
    }

    #[test]
    fn saved_execution_rejects_ineligible_catalog_before_open() {
        let profile = profile::RouteProfile::from_selection(
            "source-fixed",
            SourceKind::RenderLoopback,
            None,
            "output-fixed",
            None,
            profile::SavedPlayback::remote_defaults(),
        )
        .unwrap();
        let format = profile::EndpointFormat {
            sample_rate_hz: 48_000,
            channels: 2,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            block_align: 8,
            channel_mask: 3,
            float32: true,
        };
        let source = CatalogEndpoint {
            id: "source-fixed".into(),
            label: "source".into(),
            direction: CatalogDirection::Render,
            state: CatalogState::Active,
            format: Some(format),
        };
        let output = CatalogEndpoint {
            id: "output-fixed".into(),
            label: "output".into(),
            ..source.clone()
        };
        let eligible = RouteCatalog::from_endpoints(vec![output.clone(), source.clone()]);
        let (route, output_id) = checked_saved_route(&profile, &eligible).unwrap();
        assert_eq!(route.id, "source-fixed");
        assert_eq!(route.flow, CaptureFlow::Loopback);
        assert_eq!(output_id, "output-fixed");

        let incompatible_output = CatalogEndpoint {
            format: Some(profile::EndpointFormat {
                sample_rate_hz: 44_100,
                ..format
            }),
            ..output.clone()
        };
        let unsupported = RouteCatalog::from_endpoints(vec![source.clone(), incompatible_output]);
        assert!(checked_saved_route(&profile, &unsupported).is_err());
        assert!(
            profile
                .preflight_report(&unsupported)
                .contains("route_status=unsupported")
        );

        let wrong_kind = CatalogEndpoint {
            direction: CatalogDirection::Capture,
            ..source
        };
        let unsupported = RouteCatalog::from_endpoints(vec![wrong_kind, output]);
        assert!(checked_saved_route(&profile, &unsupported).is_err());
        assert!(checked_saved_route(&profile, &RouteCatalog::default()).is_err());
    }

    #[test]
    fn unity_source_selection_preserves_bits_and_mix_is_explicit() {
        let own = [0.0, -0.0, f32::from_bits(1), 1.25];
        let remote = [-0.75, 0.25, -0.0, -1.25];
        let mut output = [0.0; 4];
        mix(Playback::Local, 1.0, 0.5, &own, &remote, &mut output).unwrap();
        assert_eq!(output.map(f32::to_bits), own.map(f32::to_bits));
        mix(Playback::Remote, 0.5, 1.0, &own, &remote, &mut output).unwrap();
        assert_eq!(output.map(f32::to_bits), remote.map(f32::to_bits));
        mix(Playback::Mix, 0.5, 0.5, &own, &remote, &mut output).unwrap();
        assert_eq!(output[0], -0.375);
        assert_eq!(output[3], 0.0);
        mix(Playback::Mute, 1.0, 1.0, &own, &remote, &mut output).unwrap();
        assert_eq!(output, [0.0; 4]);
    }

    #[test]
    fn projected_catalog_format_matches_the_open_path_native_check() {
        let format = WasapiFormat {
            sample_rate_hz: 48_000,
            channels: 2,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            block_align: 8,
            channel_mask: 3,
            encoding: SampleEncoding::Float,
        };
        assert!(native_stereo(format));
        assert!(profile::is_native_lab_format(&project_format(format)));

        let mask = WasapiFormat {
            channel_mask: 1,
            ..format
        };
        assert!(!native_stereo(mask));
        assert!(!profile::is_native_lab_format(&project_format(mask)));

        let rate = WasapiFormat {
            sample_rate_hz: 44_100,
            ..format
        };
        assert!(!native_stereo(rate));
        assert!(!profile::is_native_lab_format(&project_format(rate)));

        let integer = WasapiFormat {
            encoding: SampleEncoding::IntegerPcm,
            ..format
        };
        assert!(!native_stereo(integer));
        assert!(!profile::is_native_lab_format(&project_format(integer)));
    }
}
