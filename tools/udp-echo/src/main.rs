//! UDP echo for the step-1 round trip.
//!
//! `serve` returns each datagram unchanged. `probe` sends an 8-byte
//! little-endian tick count and prints the round trip in milliseconds.

use std::env;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::sync::OnceLock;
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_LISTEN: &str = "127.0.0.1:17110";
const TICK_LEN: usize = 8;
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

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
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {
            let listen = args.next().unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
            serve(&listen)
        }
        Some("probe") => {
            let target = args.next().unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
            let target: SocketAddr = target
                .parse()
                .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
            let milliseconds = probe(&target)?;
            println!("rtt_ms {milliseconds:.3}");
            Ok(())
        }
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "usage: rustlanbus-echo serve [ADDR:PORT] | rustlanbus-echo probe [ADDR:PORT]",
        )),
    }
}

fn serve(listen: &str) -> io::Result<()> {
    let socket = UdpSocket::bind(listen)?;
    eprintln!("listening {}", socket.local_addr()?);
    let mut buffer = [0_u8; 65_536];
    loop {
        let (length, source) = socket.recv_from(&mut buffer)?;
        socket.send_to(&buffer[..length], source)?;
    }
}

fn probe(target: &SocketAddr) -> io::Result<f64> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.set_read_timeout(Some(PROBE_TIMEOUT))?;
    measure(&socket, target)
}

fn measure(socket: &UdpSocket, target: &SocketAddr) -> io::Result<f64> {
    let sent = monotonic_nanos();
    socket.send_to(&sent.to_le_bytes(), target)?;
    let mut received = [0_u8; 65_536];
    let (length, _) = socket.recv_from(&mut received)?;
    if length != TICK_LEN {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "echo payload was not 8 bytes",
        ));
    }
    let echoed = u64::from_le_bytes(received[..TICK_LEN].try_into().expect("8 bytes"));
    if echoed != sent {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "echo payload was not returned unchanged",
        ));
    }
    let elapsed_nanos = monotonic_nanos().saturating_sub(sent);
    Ok(elapsed_nanos as f64 / 1_000_000.0)
}

fn monotonic_nanos() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_echo_rtt_is_positive() {
        let server = UdpSocket::bind("127.0.0.1:0").expect("bind echo");
        let address = server.local_addr().expect("server address");
        thread::spawn(move || {
            let mut buffer = [0_u8; 65_536];
            let (length, source) = server.recv_from(&mut buffer).expect("recv");
            server.send_to(&buffer[..length], source).expect("send");
        });
        let milliseconds = probe(&address).expect("probe");
        assert!(milliseconds > 0.0, "rtt_ms {milliseconds}");
    }
}
