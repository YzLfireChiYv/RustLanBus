//! Device-free UDP entry point for the first peer implementation stage.

use std::error::Error;
use std::fs;
use std::io::{self, BufRead};
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};
use rlb_peer::{
    AdmittedOriginal, ChannelLayout, Intent, MAX_DATAGRAM_BYTES, PcmEncoding, PcmFormat, Peer,
    PeerStatus, SourceKind, SourcePublisher,
};
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const RATE: u32 = 48_000;
const SYSTEM_CHANNELS: u16 = 2;
const MICROPHONE_CHANNELS: u16 = 1;
const FRAMES: u64 = 128;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    member_id: MemberId,
    peer_id: MemberId,
    bind: SocketAddr,
    peer: SocketAddr,
    send: bool,
    receive: bool,
}

impl Config {
    fn intent(&self) -> Intent {
        Intent {
            send: self.send,
            receive: self.receive,
        }
    }
}

#[derive(Default)]
struct Counts {
    controls_sent: u64,
    media_datagrams_sent: u64,
    system_frames: u64,
    microphone_frames: u64,
    verified_samples: u64,
    mismatched_samples: u64,
    rejected_datagrams: u64,
    unexpected_endpoints: u64,
    send_failures: u64,
    receive_failures: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("init") => init(&args[1..]),
        Some("run") => run_peer(&args[1..]),
        Some("demo") => demo(),
        _ => Err("usage: rustlanbus-peer-lab init CONFIG --id ID --peer-id ID --bind ADDR --peer ADDR [--send] [--receive]\n       rustlanbus-peer-lab run CONFIG [--seconds N] [--fixture]\n       rustlanbus-peer-lab demo".into()),
    }
}

fn value(args: &[String], name: &str) -> Result<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
        .ok_or_else(|| format!("missing {name} value").into())
}

fn init(args: &[String]) -> Result<()> {
    let path = PathBuf::from(args.first().ok_or("missing config path")?);
    if path.exists() {
        return Err("config already exists; choose a new path".into());
    }
    let config = Config {
        member_id: value(args, "--id")?.parse()?,
        peer_id: value(args, "--peer-id")?.parse()?,
        bind: value(args, "--bind")?.parse()?,
        peer: value(args, "--peer")?.parse()?,
        send: args.iter().any(|arg| arg == "--send"),
        receive: args.iter().any(|arg| arg == "--receive"),
    };
    if config.member_id == config.peer_id {
        return Err("member and peer identities must differ".into());
    }
    save_config(&path, &config)?;
    println!("saved {}", path.display());
    Ok(())
}

fn save_config(path: &Path, config: &Config) -> Result<()> {
    let name = path
        .file_name()
        .ok_or("config needs a file name")?
        .to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(config)?)?;
    fs::rename(&temp, path)?;
    Ok(())
}

fn run_peer(args: &[String]) -> Result<()> {
    let path = PathBuf::from(args.first().ok_or("missing config path")?);
    let mut config: Config = serde_json::from_slice(&fs::read(&path)?)?;
    let fixture = args.iter().any(|arg| arg == "--fixture");
    let seconds = if args.iter().any(|arg| arg == "--seconds") {
        Some(value(args, "--seconds")?.parse::<u64>()?)
    } else {
        None
    };
    let socket = UdpSocket::bind(config.bind)?;
    socket.set_nonblocking(true)?;
    let run_id = RunId::generate()?;
    let mut sources = open_sources(run_id)?;
    let mut peer = Peer::new(
        config.member_id,
        config.peer_id,
        run_id,
        config.intent(),
        sources.publisher.catalogue(),
        0,
    )?;
    let (commands, input) = mpsc::sync_channel::<String>(16);
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if commands.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    println!(
        "listen={} peer={} fixture={fixture}",
        socket.local_addr()?,
        config.peer
    );
    println!("commands: send on/off, receive on/off, status, reconnect, quit");
    let origin = Instant::now();
    let mut counts = Counts::default();
    let mut last_report = Instant::now();
    let mut system_cursor = 0_u64;
    let mut microphone_cursor = 0_u64;
    loop {
        let now = millis(origin);
        if seconds.is_some_and(|limit| origin.elapsed() >= Duration::from_secs(limit)) {
            break;
        }
        for _ in 0..16 {
            let Ok(command) = input.try_recv() else {
                break;
            };
            let words: Vec<&str> = command.split_whitespace().collect();
            match words.as_slice() {
                ["quit"] => {
                    print_summary(&peer.status(), &counts);
                    return Ok(());
                }
                ["status"] => print_summary(&peer.status(), &counts),
                ["reconnect"] => {
                    peer.reconnect(now)?;
                }
                ["send", setting] | ["receive", setting] => {
                    let enabled = match *setting {
                        "on" => true,
                        "off" => false,
                        _ => {
                            println!("setting must be on or off");
                            continue;
                        }
                    };
                    if words[0] == "send" {
                        config.send = enabled;
                    } else {
                        config.receive = enabled;
                    }
                    peer.set_intent(config.intent(), now)?;
                    save_config(&path, &config)?;
                    print_summary(&peer.status(), &counts);
                }
                _ => println!("unknown command"),
            }
        }
        pump(
            &socket,
            config.peer,
            &mut peer,
            sources.system,
            sources.microphone,
            now,
            fixture,
            &mut counts,
        )?;
        if fixture && peer.status().send_ready {
            send_source(
                &socket,
                config.peer,
                &mut sources.publisher,
                &peer,
                sources.system,
                sources.system_acquisition,
                system_cursor,
                SYSTEM_CHANNELS,
                FRAMES,
                now,
                &mut counts,
            )?;
            send_source(
                &socket,
                config.peer,
                &mut sources.publisher,
                &peer,
                sources.microphone,
                sources.microphone_acquisition,
                microphone_cursor,
                MICROPHONE_CHANNELS,
                FRAMES,
                now,
                &mut counts,
            )?;
            system_cursor += FRAMES;
            microphone_cursor += FRAMES;
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            print_summary(&peer.status(), &counts);
            last_report = Instant::now();
        }
        // This diagnostic socket loop is not the production audio callback scheduler.
        thread::sleep(Duration::from_millis(1));
    }
    print_summary(&peer.status(), &counts);
    Ok(())
}

fn millis(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn open_sources(run_id: RunId) -> Result<Sources> {
    let system = SourceId::from_bytes([3; 16])?;
    let microphone = SourceId::from_bytes([4; 16])?;
    let system_acquisition = StreamEpoch::from_bytes([7; 16])?;
    let microphone_acquisition = StreamEpoch::from_bytes([8; 16])?;
    let mut publisher = SourcePublisher::new(run_id);
    publisher.register(system, SourceKind::System, true)?;
    publisher.register(microphone, SourceKind::Microphone, true)?;
    bind(&mut publisher, system, system_acquisition, stereo())?;
    bind(&mut publisher, microphone, microphone_acquisition, mono())?;
    Ok(Sources {
        publisher,
        system,
        microphone,
        system_acquisition,
        microphone_acquisition,
    })
}

struct Sources {
    publisher: SourcePublisher,
    system: SourceId,
    microphone: SourceId,
    system_acquisition: StreamEpoch,
    microphone_acquisition: StreamEpoch,
}

fn stereo() -> PcmFormat {
    PcmFormat::new(RATE, ChannelLayout::StereoLeftRight, PcmEncoding::F32Le, 32)
        .expect("stereo F32 is a supported descriptor")
}

fn mono() -> PcmFormat {
    PcmFormat::new(RATE, ChannelLayout::Mono, PcmEncoding::F32Le, 32)
        .expect("mono F32 is a supported descriptor")
}

fn bind(
    publisher: &mut SourcePublisher,
    source: SourceId,
    acquisition: StreamEpoch,
    format: PcmFormat,
) -> Result<()> {
    let request = publisher
        .begin_binding(source, acquisition, format)?
        .pending
        .ok_or("synthetic binding was not reserved")?;
    publisher.commit_binding(request, acquisition, 0)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn pump(
    socket: &UdpSocket,
    endpoint: SocketAddr,
    peer: &mut Peer,
    system: SourceId,
    microphone: SourceId,
    now: u64,
    verify: bool,
    counts: &mut Counts,
) -> Result<()> {
    peer.tick(now)?;
    let mut bytes = [0_u8; MAX_DATAGRAM_BYTES + 1];
    for _ in 0..64 {
        match socket.recv_from(&mut bytes) {
            Ok((length, source)) if source == endpoint => {
                match peer.ingest(&bytes[..length], now, now) {
                    Ok(Some(audio)) => receive_audio(&audio, system, microphone, verify, counts),
                    Ok(None) => {}
                    Err(_) => counts.rejected_datagrams += 1,
                }
            }
            Ok(_) => counts.unexpected_endpoints += 1,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if cfg!(windows) && error.raw_os_error() == Some(10040) => {
                counts.rejected_datagrams += 1;
            }
            Err(error) if transient_network_error(&error) => {
                counts.receive_failures += 1;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    for _ in 0..32 {
        let Some(bytes) = peer.take_control() else {
            break;
        };
        if transmit(socket, endpoint, &bytes, counts)? {
            counts.controls_sent += 1;
        }
    }
    Ok(())
}

fn transmit(
    socket: &UdpSocket,
    endpoint: SocketAddr,
    bytes: &[u8],
    counts: &mut Counts,
) -> Result<bool> {
    match socket.send_to(bytes, endpoint) {
        Ok(length) if length == bytes.len() => Ok(true),
        Ok(_) => {
            counts.send_failures += 1;
            Ok(false)
        }
        Err(error) if transient_network_error(&error) => {
            counts.send_failures += 1;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn transient_network_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
    )
}

fn pattern(frame: u64, channel: u8) -> f32 {
    match (frame + u64::from(channel)) % 8 {
        0 => 0.0,
        1 => -0.0,
        2 => 0.25,
        3 => -0.75,
        4 => 1.25,
        5 => f32::from_bits(1),
        6 => 1.0e-20,
        _ => -1.0e-20,
    }
}

#[allow(clippy::too_many_arguments)]
fn send_source(
    socket: &UdpSocket,
    endpoint: SocketAddr,
    publisher: &mut SourcePublisher,
    peer: &Peer,
    source: SourceId,
    acquisition: StreamEpoch,
    start: u64,
    channels: u16,
    frames: u64,
    now: u64,
    counts: &mut Counts,
) -> Result<()> {
    let mut samples = Vec::with_capacity(frames as usize * usize::from(channels));
    for frame in start..start + frames {
        for channel in 0..channels {
            samples.push(f64::from(pattern(frame, u8::try_from(channel).unwrap_or(0))));
        }
    }
    let original = publisher.local_original(source, acquisition, start, &samples, now)?;
    let prepared = publisher.prepare_pcm(original)?;
    for packet in prepared.as_slice() {
        if let Some(bytes) = peer.write_original(packet)? {
            if transmit(socket, endpoint, &bytes, counts)? {
                counts.media_datagrams_sent += 1;
            }
        }
    }
    Ok(())
}

fn receive_audio(
    audio: &AdmittedOriginal,
    system: SourceId,
    microphone: SourceId,
    verify: bool,
    counts: &mut Counts,
) {
    if audio.source_id == system {
        counts.system_frames += u64::from(audio.frames);
    } else if audio.source_id == microphone {
        counts.microphone_frames += u64::from(audio.frames);
    }
    if verify {
        for (index, sample) in audio.samples.iter().enumerate() {
            let channels = usize::from(audio.channels());
            let frame = audio.start_frame + (index / channels) as u64;
            let channel = (index % channels) as u8;
            counts.verified_samples += 1;
            if sample.to_bits() != f64::from(pattern(frame, channel)).to_bits() {
                counts.mismatched_samples += 1;
            }
        }
    }
}

fn print_summary(status: &PeerStatus, counts: &Counts) {
    println!(
        "contact={} send_ready={} receive_ready={} intent={}/{} controls={} sent={} system_frames={} microphone_frames={} verified_samples={} mismatches={} rejected={} unexpected={} send_failures={} receive_failures={}",
        status.connected,
        status.send_ready,
        status.receive_ready,
        status.local.send,
        status.local.receive,
        counts.controls_sent,
        counts.media_datagrams_sent,
        counts.system_frames,
        counts.microphone_frames,
        counts.verified_samples,
        counts.mismatched_samples,
        counts.rejected_datagrams,
        counts.unexpected_endpoints,
        counts.send_failures,
        counts.receive_failures,
    );
}

fn demo() -> Result<()> {
    let first = MemberId::from_bytes([1; 16])?;
    let second = MemberId::from_bytes([2; 16])?;
    let a_run = RunId::generate()?;
    let b_run = RunId::generate()?;
    let mut a_sources = open_sources(a_run)?;
    let mut b_sources = open_sources(b_run)?;
    let a_socket = UdpSocket::bind("127.0.0.1:0")?;
    let b_reservation = UdpSocket::bind("127.0.0.1:0")?;
    a_socket.set_nonblocking(true)?;
    let a_addr = a_socket.local_addr()?;
    let b_addr = b_reservation.local_addr()?;
    drop(b_reservation);
    let origin = Instant::now();
    let mut a = Peer::new(
        first,
        second,
        a_run,
        Intent {
            send: true,
            receive: true,
        },
        a_sources.publisher.catalogue(),
        0,
    )?;
    let mut a_counts = Counts::default();
    let until_b_starts = Instant::now() + Duration::from_millis(80);
    while Instant::now() < until_b_starts {
        pump(
            &a_socket,
            b_addr,
            &mut a,
            b_sources.system,
            b_sources.microphone,
            millis(origin),
            true,
            &mut a_counts,
        )?;
        thread::sleep(Duration::from_millis(1));
    }
    let b_socket = UdpSocket::bind(b_addr)?;
    b_socket.set_nonblocking(true)?;
    let mut b = Peer::new(
        second,
        first,
        b_run,
        Intent {
            send: true,
            receive: true,
        },
        b_sources.publisher.catalogue(),
        millis(origin),
    )?;
    let mut b_counts = Counts::default();
    wait_for(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
        |a, b| a.send_ready && a.receive_ready && b.send_ready && b.receive_ready,
    )?;
    send_both(
        &a_socket,
        b_addr,
        &mut a_sources,
        &a,
        0,
        480,
        millis(origin),
        &mut a_counts,
    )?;
    send_both(
        &b_socket,
        a_addr,
        &mut b_sources,
        &b,
        0,
        480,
        millis(origin),
        &mut b_counts,
    )?;
    pump_pair(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
    )?;
    if a_counts.system_frames != 480
        || a_counts.microphone_frames != 480
        || b_counts.system_frames != 480
        || b_counts.microphone_frames != 480
    {
        return Err("demo: both sources did not deliver 480 frames in each direction".into());
    }
    b.set_intent(
        Intent {
            send: true,
            receive: false,
        },
        millis(origin),
    )?;
    wait_for(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
        |a, b| !a.send_ready && !b.receive_ready && b.send_ready && a.receive_ready,
    )?;
    let system_before = a_counts.system_frames;
    let microphone_before = a_counts.microphone_frames;
    send_both(
        &b_socket,
        a_addr,
        &mut b_sources,
        &b,
        480,
        128,
        millis(origin),
        &mut b_counts,
    )?;
    pump_pair(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
    )?;
    if a_counts.system_frames <= system_before || a_counts.microphone_frames <= microphone_before {
        return Err("demo: receive refusal stopped the healthy reverse sources".into());
    }
    b.set_intent(
        Intent {
            send: true,
            receive: true,
        },
        millis(origin),
    )?;
    wait_for(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
        |a, b| a.send_ready && b.receive_ready,
    )?;
    a.reconnect(millis(origin))?;
    wait_for(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
        |a, b| a.send_ready && a.receive_ready && b.send_ready && b.receive_ready,
    )?;
    let fresh_run = RunId::generate()?;
    b_sources = open_sources(fresh_run)?;
    b = Peer::new(
        second,
        first,
        fresh_run,
        Intent {
            send: true,
            receive: true,
        },
        b_sources.publisher.catalogue(),
        millis(origin),
    )?;
    wait_for(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
        |a, b| a.send_ready && a.receive_ready && b.send_ready && b.receive_ready,
    )?;
    let before = b_counts.system_frames;
    send_both(
        &a_socket,
        b_addr,
        &mut a_sources,
        &a,
        480,
        128,
        millis(origin),
        &mut a_counts,
    )?;
    pump_pair(
        &a_socket,
        b_addr,
        &mut a,
        &a_sources,
        &mut a_counts,
        &b_socket,
        a_addr,
        &mut b,
        &b_sources,
        &mut b_counts,
        origin,
    )?;
    if b_counts.system_frames <= before
        || a_counts.mismatched_samples + b_counts.mismatched_samples != 0
    {
        return Err("demo: fresh-run recovery or finite F32 integrity failed".into());
    }
    println!(
        "demo passed: delayed startup, two-source RLP4 fanout, refusal, regrant, same-run reconnect, fresh run, exact finite F32 samples"
    );
    print_summary(&a.status(), &a_counts);
    print_summary(&b.status(), &b_counts);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_both(
    socket: &UdpSocket,
    endpoint: SocketAddr,
    sources: &mut Sources,
    peer: &Peer,
    start: u64,
    frames: u64,
    now: u64,
    counts: &mut Counts,
) -> Result<()> {
    send_source(
        socket,
        endpoint,
        &mut sources.publisher,
        peer,
        sources.system,
        sources.system_acquisition,
        start,
        SYSTEM_CHANNELS,
        frames,
        now,
        counts,
    )?;
    send_source(
        socket,
        endpoint,
        &mut sources.publisher,
        peer,
        sources.microphone,
        sources.microphone_acquisition,
        start,
        MICROPHONE_CHANNELS,
        frames,
        now,
        counts,
    )
}

#[allow(clippy::too_many_arguments)]
fn pump_pair(
    a_socket: &UdpSocket,
    b_addr: SocketAddr,
    a: &mut Peer,
    a_sources: &Sources,
    a_counts: &mut Counts,
    b_socket: &UdpSocket,
    a_addr: SocketAddr,
    b: &mut Peer,
    b_sources: &Sources,
    b_counts: &mut Counts,
    origin: Instant,
) -> Result<()> {
    for _ in 0..8 {
        let now = millis(origin);
        pump(
            a_socket,
            b_addr,
            a,
            b_sources.system,
            b_sources.microphone,
            now,
            true,
            a_counts,
        )?;
        pump(
            b_socket,
            a_addr,
            b,
            a_sources.system,
            a_sources.microphone,
            now,
            true,
            b_counts,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn wait_for(
    a_socket: &UdpSocket,
    b_addr: SocketAddr,
    a: &mut Peer,
    a_sources: &Sources,
    a_counts: &mut Counts,
    b_socket: &UdpSocket,
    a_addr: SocketAddr,
    b: &mut Peer,
    b_sources: &Sources,
    b_counts: &mut Counts,
    origin: Instant,
    predicate: impl Fn(&PeerStatus, &PeerStatus) -> bool,
) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        pump_pair(
            a_socket, b_addr, a, a_sources, a_counts, b_socket, a_addr, b, b_sources, b_counts,
            origin,
        )?;
        if predicate(&a.status(), &b.status()) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
    Err(format!(
        "demo: state did not settle: A={:?}; B={:?}",
        a.status(),
        b.status()
    )
    .into())
}
