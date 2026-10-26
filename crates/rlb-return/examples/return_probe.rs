//! Finite ordinary-peer round trip through a running return-enabled contactd.
//! No audio device or operating-system sound setting is opened or changed.

use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rlb_contact::{ContactClient, DEFAULT_IO_TIMEOUT, JoinParams, MemberContact, ReturnAction};
use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};
use rlb_peer::{
    CaptureTicket, CaptureWatermark, ChannelLayout, Intent, MAX_DATAGRAM_BYTES, PcmEncoding,
    PcmFormat, Peer, PeerOperation, PreparedStorage, SourceKind, SourcePublisher,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const FRAMES: usize = 64;
const SEGMENTS: u64 = 32;

fn main() {
    if let Err(error) = run() {
        eprintln!("return_probe failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut contact, mut room, mut password_file) = (None, None, None);
    while let Some(argument) = args.next() {
        if argument == "--help" || argument == "-h" {
            println!(
                "usage: return_probe --contact ADDRESS:PORT --room NAME [--password-file PATH]"
            );
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("Missing value for {argument}."))?;
        match argument.as_str() {
            "--contact" => contact = Some(value.parse::<SocketAddr>()?),
            "--room" => room = Some(value),
            "--password-file" => password_file = Some(PathBuf::from(value)),
            _ => return Err(format!("Unknown argument {argument}.").into()),
        }
    }
    let contact = contact.ok_or("--contact ADDRESS:PORT is required.")?;
    let room = room.ok_or("--room NAME is required.")?;
    let password = match password_file {
        Some(path) => std::fs::read_to_string(path)?
            .trim_end_matches(['\r', '\n'])
            .to_owned(),
        None => String::new(),
    };
    let bind = if contact.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    let member_id = MemberId::generate()?;
    let run_id = RunId::generate()?;
    let source_id = SourceId::generate()?;
    let mut client = ContactClient::connect(contact, DEFAULT_IO_TIMEOUT)?;
    client.join(JoinParams {
        room_name: room,
        password,
        member_id,
        run_id,
        display_name: "Return probe".to_owned(),
        media_port: socket.local_addr()?.port(),
        media_ip: None,
        media_scope_id: 0,
        media_alternates: vec![],
    })?;

    let mut started = false;
    let result = (|| -> Result<Report> {
        let state = client.return_action(ReturnAction::Start)?;
        if let Some(error) = state.error {
            return Err(error.into());
        }
        let remote = state.member.ok_or("Start returned no virtual member.")?;
        started = true;
        let scope = match contact {
            SocketAddr::V6(address) => address.scope_id(),
            _ => 0,
        };
        let address = remote.media.socket_addr_on(scope)?;
        let (report, epoch) = exchange(
            &socket,
            address,
            member_id,
            run_id,
            source_id,
            &remote,
            &mut client,
        )?;
        let status = client.return_action(ReturnAction::Status)?;
        if let Some(error) = status.error {
            return Err(error.into());
        }
        if !status.mappings.iter().any(|mapping| {
            mapping.upstream_member_id == member_id
                && mapping.upstream_run_id == run_id
                && mapping.upstream_source_id == source_id
                && mapping.upstream_epoch == epoch
                && Some(mapping.source_id) == report.returned_source
        }) {
            return Err(
                "Status did not report the observed upstream/returned source correspondence."
                    .into(),
            );
        }
        Ok(report)
    })();
    let stop = if started {
        Some(client.return_action(ReturnAction::Stop))
    } else {
        None
    };
    let leave = client.leave();
    let report = result?;
    if let Some(stop) = stop {
        if let Some(error) = stop?.error {
            return Err(error.into());
        }
    }
    leave?;
    println!(
        "sent_audio_packets={} returned_audio_packets={} verified_f32_words={} recent_packet_rtt_ms={:.3}",
        report.sent,
        report.returned,
        report.words,
        report
            .recent_rtt
            .ok_or("No returned packet timing was recorded.")?
            .as_secs_f64()
            * 1_000.0
    );
    println!("PCM words match. Timing is packet round trip, including probe scheduling.");
    Ok(())
}

struct Report {
    sent: u64,
    returned: u64,
    words: u64,
    recent_rtt: Option<Duration>,
    returned_source: Option<SourceId>,
}

fn sample(frame: u64, channel: usize) -> f32 {
    let value = ((frame * 2 + channel as u64) % 251 + 1) as f32 / 16_384.0;
    if (frame + channel as u64).is_multiple_of(2) {
        value
    } else {
        -value
    }
}

fn exchange(
    socket: &UdpSocket,
    address: SocketAddr,
    member: MemberId,
    run: RunId,
    source: SourceId,
    remote: &MemberContact,
    client: &mut ContactClient,
) -> Result<(Report, u64)> {
    let acquisition = StreamEpoch::generate()?;
    let format = PcmFormat::new(
        48_000,
        ChannelLayout::StereoLeftRight,
        PcmEncoding::F32Le,
        32,
    )?;
    let mut publisher = SourcePublisher::new(run);
    publisher.register(source, SourceKind::System, true)?;
    let request = publisher
        .begin_binding(source, acquisition, format)?
        .pending
        .ok_or("No capture binding request.")?;
    let binding = publisher
        .commit_binding(request, acquisition, 0)?
        .binding
        .ok_or("No active capture binding.")?;
    let mut peer = Peer::new(
        member,
        remote.member_id,
        run,
        Intent {
            send: true,
            receive: true,
        },
        publisher.catalogue(),
        0,
    )?;
    let mut storage = PreparedStorage::for_format(format)?;
    let mut input = [0_u8; MAX_DATAGRAM_BYTES + 1];
    let mut output = [0_u8; MAX_DATAGRAM_BYTES];
    let mut pcm = [0_f64; MAX_DATAGRAM_BYTES / 4];
    let mut samples = [0_f64; FRAMES * 2];
    let mut sent_at = HashMap::<u64, Instant>::new();
    let start = Instant::now();
    let mut last_send = start;
    let mut last_keepalive = start;
    let mut ticket = 0;
    let mut segment = 0;
    let mut report = Report {
        sent: 0,
        returned: 0,
        words: 0,
        recent_rtt: None,
        returned_source: None,
    };
    while start.elapsed() < Duration::from_millis(2_500) {
        let now = start.elapsed().as_millis() as u64;
        peer.tick(now)?;
        while let Some(control) = peer.take_control() {
            socket.send_to(&control, address)?;
        }
        for _ in 0..128 {
            let (length, from) = match socket.recv_from(&mut input) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if from != address {
                continue;
            }
            let now = start.elapsed().as_millis() as u64;
            let operation = PeerOperation {
                now_ms: now,
                watermark: CaptureWatermark {
                    allocated_through: ticket,
                },
            };
            let Some(audio) = peer.ingest_into(&input[..length], now, operation, &mut pcm)? else {
                continue;
            };
            if audio.metadata.remote_run != remote.run_id
                || audio.metadata.entry.format != Some(format)
                || usize::from(audio.metadata.frames) != FRAMES
            {
                return Err(
                    "Returned run, PCM format or frame count differs from the sent publication."
                        .into(),
                );
            }
            let first = audio.metadata.start_frame;
            let sent = sent_at
                .remove(&first)
                .ok_or("Returned frame has no corresponding sent packet.")?;
            for (index, word) in audio.samples.iter().enumerate() {
                if (*word as f32).to_bits()
                    != sample(first + (index / 2) as u64, index % 2).to_bits()
                {
                    return Err(format!(
                        "PCM word mismatch at frame {} channel {}.",
                        first + (index / 2) as u64,
                        index % 2
                    )
                    .into());
                }
            }
            report.returned += 1;
            report.words += audio.samples.len() as u64;
            report.recent_rtt = Some(sent.elapsed());
            report.returned_source = Some(audio.metadata.entry.id);
        }
        if segment < SEGMENTS
            && peer.source_send_ready(source)
            && last_send.elapsed() >= Duration::from_millis(10)
        {
            let first = segment * FRAMES as u64;
            for (index, value) in samples.iter_mut().enumerate() {
                *value = f64::from(sample(first + (index / 2) as u64, index % 2));
            }
            ticket += 1;
            let original = publisher
                .local_original(
                    source,
                    acquisition,
                    first,
                    &samples,
                    start.elapsed().as_millis() as u64,
                )?
                .with_ticket(CaptureTicket::new(ticket).ok_or("Invalid capture ticket.")?);
            let prepared = publisher.prepare_pcm_into(original, &mut storage)?;
            for index in 0..prepared.len() {
                let fragment = prepared.get(index).ok_or("Missing prepared fragment.")?;
                if let Some(packet) = peer.write_original_into(&fragment, &mut output)? {
                    let sent = Instant::now();
                    socket.send_to(packet.bytes, address)?;
                    sent_at.insert(fragment.start_frame(), sent);
                    report.sent += 1;
                }
            }
            segment += 1;
            last_send = Instant::now();
        }
        if last_keepalive.elapsed() >= Duration::from_secs(1) {
            client.keepalive()?;
            last_keepalive = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if report.sent == 0 || report.returned == 0 {
        return Err(format!(
            "Timed out after 2500 ms: sent={} returned={} connected={}",
            report.sent,
            report.returned,
            peer.status().connected
        )
        .into());
    }
    Ok((report, binding.epoch().get()))
}
