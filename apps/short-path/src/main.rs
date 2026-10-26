//! Short capture-to-play path.
//!
//! Endpoint loopback is sent from the capture thread to the local echo and
//! played on a different render device. The existing member engine is not used.

use std::collections::VecDeque;
use std::env;
use std::io::{self, ErrorKind};
use std::net::UdpSocket;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use rlb_audio::{
    CaptureBackend, CaptureProcessBuffer, CaptureProcessResult, EndpointDirection, EndpointState,
    RenderBackend,
};
use rlb_audio_windows::{
    CaptureFlow, CaptureRequest, EndpointEnumerator, EndpointRole, EndpointSelection, EventWait,
    RenderRequest, SharedPeriodPreference,
};
use rlb_model::{SourceId, StreamEpoch};

const ECHO: &str = "127.0.0.1:17110";
const HEADER_LEN: usize = 24;
const MAX_FRAMES: usize = 4_096;
const PLAY_TARGET_CAP: usize = 240;
const RUN_SECONDS: u64 = 6;

struct Packet {
    ticks: u64,
    start_sample: u64,
    frames: usize,
    samples: Vec<f32>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> io::Result<()> {
    let seconds = env::args()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(RUN_SECONDS);
    let enumerator = EndpointEnumerator::new().map_err(io_error)?;
    let catalog = enumerator.enumerate().map_err(io_error)?;
    let Some(loopback) = catalog.default_endpoint(EndpointDirection::Render, EndpointRole::Console)
    else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "no default playback device",
        ));
    };
    println!("render endpoints:");
    for endpoint in catalog.endpoints() {
        if endpoint.direction() == EndpointDirection::Render {
            println!(
                "  {} active={} id={}",
                endpoint.friendly_name(),
                endpoint.state() == EndpointState::Active,
                endpoint.endpoint_id()
            );
        }
    }
    let playback = catalog.endpoints().iter().find(|endpoint| {
        endpoint.direction() == EndpointDirection::Render
            && endpoint.state() == EndpointState::Active
            && endpoint.endpoint_id() != loopback.endpoint_id()
            && !endpoint.friendly_name().contains("CABLE")
    });
    let playback = playback.or_else(|| {
        catalog.endpoints().iter().find(|endpoint| {
            endpoint.direction() == EndpointDirection::Render
                && endpoint.state() == EndpointState::Active
                && endpoint.endpoint_id() != loopback.endpoint_id()
        })
    });
    let Some(playback) = playback else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "no second active playback device; refusing to play into the loopback device",
        ));
    };
    println!("loopback={}", loopback.friendly_name());
    println!("playback={}", playback.friendly_name());

    let stop = Arc::new(AtomicBool::new(false));
    let loopback_id = loopback.endpoint_id().to_owned();
    let playback_id = playback.endpoint_id().to_owned();
    let (ready_tx, ready_rx) = sync_channel(1);
    let (packet_tx, packet_rx) = sync_channel(8);
    let (pool_tx, pool_rx) = sync_channel(8);
    for _ in 0..8 {
        pool_tx
            .send(Packet {
                ticks: 0,
                start_sample: 0,
                frames: 0,
                samples: Vec::with_capacity(MAX_FRAMES * 2),
            })
            .expect("pool");
    }
    let capture_stop = Arc::clone(&stop);
    let tone_stop = Arc::clone(&stop);
    let tone_id = loopback.endpoint_id().to_owned();
    let tone = thread::Builder::new()
        .name("rlb-short-tone".to_owned())
        .spawn(move || tone_loop(tone_id, tone_stop))
        .map_err(io::Error::other)?;
    let capture = thread::Builder::new()
        .name("rlb-short-capture".to_owned())
        .spawn(move || {
            capture_loop(loopback_id, capture_stop, ready_tx, packet_tx, pool_rx)
        })
        .map_err(io::Error::other)?;
    let opened = ready_rx
        .recv()
        .map_err(|_| io::Error::other("capture thread ended before opening"))??;
    println!("capture_thread=rlb-short-capture");
    println!(
        "capture_period_frames={} capture_requested_period_frames={} capture_iaudioclient3={}",
        opened.period_frames, opened.requested_period_frames, opened.iaudioclient3
    );
    let play_target = PLAY_TARGET_CAP.min(opened.period_frames.max(1) as usize);
    println!("play_target_frames={play_target}");
    let play_stop = Arc::clone(&stop);
    let playback = thread::Builder::new()
        .name("rlb-short-play".to_owned())
        .spawn(move || play_loop(playback_id, play_target, play_stop, packet_rx, pool_tx))
        .map_err(io::Error::other)?;
    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let _ = capture.join();
    let _ = playback.join();
    let _ = tone.join();
    Ok(())
}

struct OpenedPeriod {
    period_frames: u32,
    requested_period_frames: u32,
    iaudioclient3: bool,
}

fn capture_loop(
    endpoint_id: String,
    stop: Arc<AtomicBool>,
    ready: SyncSender<io::Result<OpenedPeriod>>,
    packets: SyncSender<Packet>,
    pool: Receiver<Packet>,
) -> io::Result<()> {
    let opened = (|| -> io::Result<_> {
        let enumerator = EndpointEnumerator::new().map_err(io_error)?;
        let source_id = SourceId::generate().map_err(io_error)?;
        let stream_epoch = StreamEpoch::generate().map_err(io_error)?;
        let mut stream = enumerator
            .open_capture_with_period(
                CaptureRequest {
                    selection: EndpointSelection::fixed(endpoint_id),
                    flow: CaptureFlow::Loopback,
                    source_id,
                    stream_epoch,
                    channels: 2,
                },
                SharedPeriodPreference::Minimum,
            )
            .map_err(io_error)?;
        let diagnostics = stream.diagnostics();
        let capacity = (diagnostics.buffer_frames as usize)
            .clamp(diagnostics.period_frames.max(1) as usize, MAX_FRAMES);
        stream.start().map_err(io_error)?;
        Ok((stream, diagnostics, capacity))
    })();
    let (mut stream, _diagnostics, capacity) = match opened {
        Ok(value) => {
            let _ = ready.send(Ok(OpenedPeriod {
                period_frames: value.1.period_frames,
                requested_period_frames: value.1.requested_period_frames,
                iaudioclient3: value.1.iaudioclient3,
            }));
            value
        }
        Err(error) => {
            let _ = ready.send(Err(io::Error::other(error.to_string())));
            return Err(error);
        }
    };
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.set_nonblocking(true)?;
    let mut planar = vec![0.0_f32; capacity * 2];
    let mut outbound = vec![0_u8; HEADER_LEN + capacity * 2 * 4];
    let mut inbound = vec![0_u8; outbound.len()];
    let mut holding: VecDeque<Packet> = VecDeque::new();
    let mut sent = 0_u64;
    let mut echoed = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        match stream.wait(20) {
            Ok(EventWait::Signaled) | Ok(EventWait::Timeout) => {}
            Err(error) => return Err(io_error(error)),
        }
        loop {
            let mut buffer = CaptureProcessBuffer::new(&mut planar, capacity, 2).map_err(io_error)?;
            match stream.process(&mut buffer) {
                Ok(CaptureProcessResult::Captured(metadata)) => {
                    let frames = metadata.frames as usize;
                    if frames == 0 || frames > capacity {
                        continue;
                    }
                    let captured = buffer.captured(frames, metadata.channels).map_err(io_error)?;
                    let ticks = monotonic_nanos();
                    let bytes = encode_packet(
                        &mut outbound,
                        ticks,
                        metadata.start_sample,
                        frames,
                        metadata.channels,
                        captured.channel(0).unwrap_or(&[]),
                        captured.channel(1).unwrap_or(&[]),
                    );
                    let _ = socket.send_to(&outbound[..bytes], ECHO);
                    sent = sent.saturating_add(1);
                }
                Ok(CaptureProcessResult::Idle | CaptureProcessResult::EndOfStream) => break,
                Err(error) => return Err(io_error(error)),
            }
        }
        let drain_until = Instant::now() + Duration::from_millis(2);
        loop {
            match socket.recv_from(&mut inbound) {
                Ok((length, _)) => {
                    if let Some(parsed) = decode_packet(&inbound[..length]) {
                        let mut packet = match holding.pop_front() {
                            Some(packet) => packet,
                            None => match pool.try_recv() {
                                Ok(packet) => packet,
                                Err(_) => continue,
                            },
                        };
                        packet.ticks = parsed.0;
                        packet.start_sample = parsed.1;
                        packet.frames = parsed.2;
                        copy_samples(&inbound[..length], parsed.2, &mut packet.samples);
                        if let Err(TrySendError::Full(packet) | TrySendError::Disconnected(packet)) =
                            packets.try_send(packet)
                        {
                            holding.push_front(packet);
                        } else {
                            echoed = echoed.saturating_add(1);
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= drain_until {
                        break;
                    }
                    thread::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
        while let Ok(packet) = pool.try_recv() {
            holding.push_back(packet);
        }
    }
    let _ = stream.stop();
    println!("packets_sent={sent} packets_echoed={echoed}");
    Ok(())
}

fn tone_loop(endpoint_id: String, stop: Arc<AtomicBool>) -> io::Result<()> {
    let enumerator = EndpointEnumerator::new().map_err(io_error)?;
    let mut stream = enumerator
        .open_render(RenderRequest {
            selection: EndpointSelection::fixed(endpoint_id),
        })
        .map_err(io_error)?;
    stream.start().map_err(io_error)?;
    let mut phase = 0.0_f32;
    let step = std::f32::consts::TAU * 440.0 / 48_000.0;
    while !stop.load(Ordering::Relaxed) {
        match stream.wait(20) {
            Ok(EventWait::Signaled) | Ok(EventWait::Timeout) => {}
            Err(error) => return Err(io_error(error)),
        }
        let available = stream.available_frames().unwrap_or(0) as usize;
        if available == 0 {
            continue;
        }
        let frames = available.min(480);
        let mut interleaved = vec![0.0_f32; frames * 2];
        for frame in 0..frames {
            let sample = (phase.sin() * 0.2).clamp(-1.0, 1.0);
            interleaved[frame * 2] = sample;
            interleaved[frame * 2 + 1] = sample;
            phase = (phase + step) % std::f32::consts::TAU;
        }
        stream
            .render(&interleaved, frames)
            .map_err(io_error)?;
    }
    let _ = stream.stop();
    Ok(())
}

fn play_loop(
    endpoint_id: String,
    play_target: usize,
    stop: Arc<AtomicBool>,
    packets: Receiver<Packet>,
    pool: SyncSender<Packet>,
) -> io::Result<()> {
    let enumerator = EndpointEnumerator::new().map_err(io_error)?;
    let mut stream = enumerator
        .open_render_with_period(
            RenderRequest {
                selection: EndpointSelection::fixed(endpoint_id),
            },
            SharedPeriodPreference::Minimum,
        )
        .map_err(io_error)?;
    let playback_period = stream.diagnostics().period_frames;
    println!("playback_period_frames={playback_period}");
    stream.start().map_err(io_error)?;
    let mut queued: VecDeque<Packet> = VecDeque::new();
    let mut cursor = 0_usize;
    let mut reported = 0_u32;
    let mut last_rtt = 0_i64;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(packet) = packets.try_recv() {
            queued.push_back(packet);
        }
        match stream.wait(20) {
            Ok(EventWait::Signaled) | Ok(EventWait::Timeout) => {}
            Err(error) => return Err(io_error(error)),
        }
        let available = stream.available_frames().unwrap_or(0) as usize;
        let queued_frames = queued.iter().enumerate().fold(0_usize, |total, (index, packet)| {
            let pending = if index == 0 {
                packet.frames.saturating_sub(cursor)
            } else {
                packet.frames
            };
            total.saturating_add(pending)
        });
        if available == 0 || queued_frames < play_target {
            continue;
        }
        let frames = available.min(queued_frames);
        let mut interleaved = vec![0.0_f32; frames * 2];
        let mut filled = 0_usize;
        while filled < frames {
            let Some(packet) = queued.front() else {
                break;
            };
            if cursor == 0 && filled == 0 {
                let frames_delay = round_trip_frames(packet.ticks);
                last_rtt = frames_delay;
                if reported < 3 || reported % 100 == 0 {
                    println!("rtt_frames={frames_delay}");
                }
                reported = reported.saturating_add(1);
            }
            let remain = packet.frames.saturating_sub(cursor);
            if remain == 0 {
                if let Some(done) = queued.pop_front() {
                    cursor = 0;
                    let _ = pool.try_send(done);
                }
                continue;
            }
            let take = remain.min(frames - filled);
            let source = &packet.samples[cursor * 2..(cursor + take) * 2];
            interleaved[filled * 2..(filled + take) * 2].copy_from_slice(source);
            cursor += take;
            filled += take;
            if cursor >= packet.frames {
                if let Some(done) = queued.pop_front() {
                    cursor = 0;
                    let _ = pool.try_send(done);
                }
            }
        }
        if filled == 0 {
            continue;
        }
        stream
            .render(&interleaved[..filled * 2], filled)
            .map_err(io_error)?;
    }
    let _ = stream.stop();
    println!("played_packets={reported} rtt_frames_last={last_rtt}");
    Ok(())
}

fn encode_packet(
    buffer: &mut [u8],
    ticks: u64,
    start_sample: u64,
    frames: usize,
    channels: u8,
    left: &[f32],
    right: &[f32],
) -> usize {
    let samples = frames * 2;
    let bytes = HEADER_LEN + samples * 4;
    buffer[..8].copy_from_slice(&ticks.to_le_bytes());
    buffer[8..16].copy_from_slice(&start_sample.to_le_bytes());
    buffer[16..20].copy_from_slice(&(frames as u32).to_le_bytes());
    buffer[20..24].copy_from_slice(&u32::from(channels).to_le_bytes());
    for frame in 0..frames {
        let left_sample = left.get(frame).copied().unwrap_or(0.0);
        let right_sample = right.get(frame).copied().unwrap_or(left_sample);
        let offset = HEADER_LEN + frame * 8;
        buffer[offset..offset + 4].copy_from_slice(&left_sample.to_le_bytes());
        buffer[offset + 4..offset + 8].copy_from_slice(&right_sample.to_le_bytes());
    }
    bytes
}

fn decode_packet(buffer: &[u8]) -> Option<(u64, u64, usize)> {
    if buffer.len() < HEADER_LEN {
        return None;
    }
    let ticks = u64::from_le_bytes(buffer[0..8].try_into().ok()?);
    let start_sample = u64::from_le_bytes(buffer[8..16].try_into().ok()?);
    let frames = u32::from_le_bytes(buffer[16..20].try_into().ok()?) as usize;
    let bytes = HEADER_LEN + frames * 2 * 4;
    if buffer.len() < bytes || frames == 0 || frames > MAX_FRAMES {
        return None;
    }
    Some((ticks, start_sample, frames))
}

fn copy_samples(buffer: &[u8], frames: usize, into: &mut Vec<f32>) {
    into.clear();
    for frame in 0..frames {
        let offset = HEADER_LEN + frame * 8;
        let left = f32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap_or([0; 4]));
        let right =
            f32::from_le_bytes(buffer[offset + 4..offset + 8].try_into().unwrap_or([0; 4]));
        into.push(left);
        into.push(right);
    }
}

fn round_trip_frames(capture_ticks: u64) -> i64 {
    let elapsed = monotonic_nanos().saturating_sub(capture_ticks);
    ((elapsed as f64) * 48_000.0 / 1_000_000_000.0).round() as i64
}

fn monotonic_nanos() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
