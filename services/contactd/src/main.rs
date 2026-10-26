//! Headless contact service for one room.
//!
//! Windows and OpenWrt can both run this program. It binds the requested socket,
//! prints the bound address, room name, and instance identity, and stops when the
//! console or process supervisor asks it to stop. The password is never printed.

mod config;
mod advertise;
mod signals;
mod status;

use std::env;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::Duration;

use rlb_contact::ContactService;
use rlb_return::{ReturnHost, ReturnHostConfig};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

struct StatusFile(Option<PathBuf>);

impl Drop for StatusFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            status::remove(&path);
        }
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match config::load(&arguments)? {
        config::Command::Help => {
            println!("{}", config::USAGE);
            Ok(())
        }
        config::Command::Run {
            config,
            return_bind,
            status,
        } => {
            let room_name = config.room_name().to_owned();
            signals::install().map_err(|error| error.to_string())?;
            let (service, mut return_host) = if let Some(bind) = return_bind {
                let password = config.password().to_owned();
                let (service, requests) = ContactService::start_with_returns(config)
                    .map_err(|error| error.to_string())?;
                let contact = local_contact_address(service.local_addr());
                let host = ReturnHost::start(
                    ReturnHostConfig {
                        contact,
                        bind,
                        room_name: room_name.clone(),
                        password,
                        local_scope: match bind {
                            SocketAddr::V4(_) => 0,
                            SocketAddr::V6(address) => address.scope_id(),
                        },
                    },
                    requests,
                )?;
                (service, Some(host))
            } else {
                (
                    ContactService::start(config).map_err(|error| error.to_string())?,
                    None,
                )
            };
            let return_address = return_host.as_ref().map(ReturnHost::local_addr);
            let info = service.room_info();
            let mut advertisement = advertise::Advertisement::new(&info, service.local_addrs().to_vec());
            advertisement.turn();
            let status_file = if let Some(path) = status {
                status::write(&path, service.local_addr(), &info, return_address)?;
                StatusFile(Some(path))
            } else {
                StatusFile(None)
            };
            eprintln!("listening {}", service.local_addr());
            if let Some(address) = return_address {
                eprintln!("audio return UDP {address}");
            }
            eprintln!("room {room_name}");
            eprintln!("instance {}", info.instance_id);
            let mut next_status = std::time::Instant::now();
            while !signals::requested() {
                if return_host.as_ref().is_some_and(|host| !host.is_running()) {
                    return Err("The virtual return host stopped unexpectedly.".into());
                }
                advertisement.turn();
                if std::time::Instant::now() >= next_status {
                    if let Some(path) = &status_file.0 {
                        let returns = return_host.as_ref().map(ReturnHost::local_addrs).unwrap_or_default();
                        let warnings = return_host.as_ref().map(ReturnHost::binding_warnings).unwrap_or_default();
                        status::write_runtime(path, service.local_addr(), &info, return_address,
                            service.local_addrs(), &returns, &warnings,
                            advertisement.active(), advertisement.error())?;
                    }
                    next_status = std::time::Instant::now() + Duration::from_secs(2);
                }
                thread::sleep(Duration::from_millis(50));
            }
            drop(advertisement);
            let return_result = return_host.as_mut().map_or(Ok(()), ReturnHost::stop);
            let service_result = service.stop().map_err(|error| error.to_string());
            return_result.and(service_result)
        }
    }
}

fn local_contact_address(address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        SocketAddr::new(
            match address {
                SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            },
            address.port(),
        )
    } else {
        address
    }
}
