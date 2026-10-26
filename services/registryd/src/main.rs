use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;

use rustlanbus_registryd::{RegistryService, ServerConfig};

fn main() {
    match run() {
        Ok(()) => {}
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = ServerConfig::default();
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                config.listen = arguments
                    .next()
                    .ok_or("--listen requires HOST:PORT")?
                    .parse::<SocketAddr>()?;
            }
            "--state-dir" => {
                config.state_dir =
                    PathBuf::from(arguments.next().ok_or("--state-dir requires a path")?);
            }
            "--auto-approve" => config.auto_approve = true,
            "--room-name" => {
                config.room_name = arguments.next().ok_or("--room-name requires a value")?;
            }
            "--help" => {
                eprintln!(
                    "usage: rustlanbus-registryd [--listen HOST:PORT] [--state-dir PATH] [--auto-approve] [--room-name NAME]"
                );
                return Ok(());
            }
            other => {
                return Err(format!("unknown argument: {other}").into());
            }
        }
    }
    let service = RegistryService::start(config)?;
    eprintln!(
        "rustlanbus-registryd listening on {} room {}",
        service.local_addr(),
        service.room_id()
    );
    loop {
        std::thread::park();
    }
}
