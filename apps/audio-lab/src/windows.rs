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
    CaptureBackend, CaptureProcessBuffer, CaptureProcessResult, DirectPcmPlayout,
    DirectPlayoutConfig, EndpointDirection, EndpointState, PcmPacket, RenderBackend,
};
use rlb_audio_windows::{
    CaptureFlow, CaptureRequest, EndpointEnumerator, EndpointSelection, EventWait,
    MmcssRegistration, RenderRequest, SharedPeriodPreference, WasapiFormat, wasapi_qpc_100ns_now,
};
use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};
use rlb_peer::{Intent, MAX_DATAGRAM_BYTES, Peer};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const MAX_FRAMES: usize = 4096;
const POOL_BLOCKS: usize = 8;
const CAPTURE_MAX_AGE_HNS: u64 = 1_000_000; // 100 ms, a diagnostic stale cutoff.

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("devices") => devices(),
        Some("probe") => probe(&args[1..]),
        Some("loopback") => loopback(&args[1..]),
        _ => Err("usage: rustlanbus-audio-lab devices\n       rustlanbus-audio-lab probe --source N --output N\n       rustlanbus-audio-lab loopback --source N --output N --seconds N [--play remote|local|mix|mute] [--local-gain 0..1] [--remote-gain 0..1] [--buffer-periods 1..8]".into()),
    }
}

fn value<'a>(args: &'a [String], key: &str) -> Result<&'a str> {
    args.iter()
        .position(|arg| arg == key)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .ok_or_else(|| format!("missing {key}").into())
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

fn route(args: &[String]) -> Result<(SourceRoute, String)> {
    let source_index: usize = value(args, "--source")?.parse()?;
    let output_index: usize = value(args, "--output")?.parse()?;
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
    println!(
        "original source={}\nplayback output={}",
        source.friendly_name(),
        output.friendly_name()
    );
    Ok((
        SourceRoute {
            id: source.endpoint_id().to_owned(),
            flow,
        },
        output.endpoint_id().to_owned(),
    ))
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

fn probe(args: &[String]) -> Result<()> {
    let (source, output) = route(args)?;
    // Keep the COM apartment alive until both streams and their services drop.
    let enumerator = EndpointEnumerator::new()?;
    let capture = enumerator
        .open_capture_with_period(capture_request(source)?, SharedPeriodPreference::Minimum)?;
    let render = enumerator.open_render_with_period(
        RenderRequest {
            selection: EndpointSelection::fixed(output),
        },
        SharedPeriodPreference::Minimum,
    )?;
    println!(
        "capture={:?}\nrender={:?}\ninitial output padding={} frames",
        capture.diagnostics(),
        render.diagnostics(),
        render.callback_capacity_frames() - render.available_frames()?
    );
    println!("probe opened streams without starting capture or playback");
    Ok(())
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

fn loopback(args: &[String]) -> Result<()> {
    let (source, output) = route(args)?;
    let seconds: u64 = value(args, "--seconds")?.parse()?;
    if seconds == 0 || seconds > 300 {
        return Err("seconds must be 1..300 for this bounded experiment".into());
    }
    let playback = match args
        .iter()
        .position(|arg| arg == "--play")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
    {
        None | Some("remote") => Playback::Remote,
        Some("local") => Playback::Local,
        Some("mix") => Playback::Mix,
        Some("mute") => Playback::Mute,
        _ => return Err("play must be remote, local, mix or mute".into()),
    };
    let local_gain = gain(args, "--local-gain")?;
    let remote_gain = gain(args, "--remote-gain")?;
    let buffer_periods = if args.iter().any(|arg| arg == "--buffer-periods") {
        value(args, "--buffer-periods")?.parse::<usize>()?
    } else {
        2
    };
    if !(1..=8).contains(&buffer_periods) {
        return Err("buffer periods must be 1..8".into());
    }
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
    let mut a = Peer::new(
        a_id,
        b_id,
        RunId::generate()?,
        Intent {
            send: true,
            receive: false,
        },
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
        0,
    )?;
    let mut encoded_frames = 0_u64;
    let mut received_frames = 0_u64;
    let mut stale_capture_frames = 0_u64;
    let mut rejected = 0_u64;
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
            let stale = millis(origin).saturating_sub(block.arrival_ms) > 100
                || block.capture_hns.zip(wasapi_qpc_100ns_now()).is_some_and(
                    |(captured, current)| current.saturating_sub(captured) > CAPTURE_MAX_AGE_HNS,
                );
            if stale {
                stale_capture_frames += block.frames as u64;
            } else {
                if matches!(playback, Playback::Local | Playback::Mix) {
                    local_tx.copy(samples, block.epoch, block.start, block.arrival_ms);
                }
                if a.status().send_ready {
                    for packet in a.encode_original(RATE, CHANNELS, block.start, samples)? {
                        send(&a_socket, &packet)?;
                    }
                    encoded_frames += block.frames as u64;
                }
            }
            original_rx.release(block)?;
        }
        // This control/UDP loop is diagnostic polling; the audio owners wait on WASAPI events.
        thread::sleep(Duration::from_millis(1));
    }
    workers.finish()?;
    println!(
        "encoded_source_frames={encoded_frames} received_frames={received_frames} stale_capture_frames={stale_capture_frames} rejected_datagrams={rejected} local_queue_drops={} remote_queue_drops={}",
        local_tx.dropped, remote_tx.dropped
    );
    Ok(())
}

fn gain(args: &[String], key: &str) -> Result<f32> {
    let gain = if args.iter().any(|arg| arg == key) {
        value(args, key)?.parse()?
    } else {
        1.0_f32
    };
    if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
        return Err(format!("{key} must be finite in 0..1").into());
    }
    Ok(gain)
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
    for _ in 0..64 {
        match socket.recv(&mut bytes) {
            Ok(length) => match peer.ingest(&bytes[..length], now) {
                Ok(Some(packet)) if packet.sample_rate == RATE && packet.channels == CHANNELS => {
                    audio(
                        packet.source_epoch,
                        packet.start_frame,
                        &packet.samples,
                        now,
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
                            block.capture_hns = Some(metadata.capture_time.ticks());
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
            "render stopped: {:?}; own={:?}; returned={:?}; queue_expired_frames={queue_expired_frames}; over_unity_samples={over_unity_samples}",
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
}
