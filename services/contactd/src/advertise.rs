//! Optional contact-service advertisement. Failure never stops direct-IP entry.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, TryRecvError};
use rlb_contact::RoomInfo;
use rlb_contact::advertisement::{AdvertisedRoom, build_service_info_bound_all};

pub struct Advertisement {
    daemon: Option<ServiceDaemon>,
    events: Option<mdns_sd::Receiver<mdns_sd::DaemonEvent>>,
    fullname: Option<String>,
    room: AdvertisedRoom,
    listeners: Vec<SocketAddr>,
    retry_at: Instant,
    error: Option<String>,
}

impl Advertisement {
    pub fn new(info: &RoomInfo, listeners: Vec<SocketAddr>) -> Self {
        Self {
            daemon: None,
            events: None,
            fullname: None,
            room: AdvertisedRoom {
                instance_id: info.instance_id,
                display_name: info.room_name.clone(),
                port: info.port,
            },
            listeners,
            retry_at: Instant::now(),
            error: None,
        }
    }

    pub fn turn(&mut self) {
        if let Some(events) = &self.events {
            loop {
                match events.try_recv() {
                    Ok(mdns_sd::DaemonEvent::Error(error)) => {
                        let message = error.to_string();
                        self.stop();
                        self.error = Some(message);
                        self.retry_at = Instant::now() + Duration::from_secs(2);
                        break;
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.stop();
                        self.error = Some("Room discovery stopped; direct IP contact remains available.".into());
                        self.retry_at = Instant::now() + Duration::from_secs(2);
                        break;
                    }
                }
            }
        }
        if self.daemon.is_some() || Instant::now() < self.retry_at {
            return;
        }
        if self.listeners.iter().all(|address| address.ip().is_loopback()) {
            return;
        }
        self.retry_at = Instant::now() + Duration::from_secs(2);
        if let Err(error) = self.start() {
            if self.error.as_deref() != Some(error.as_str()) {
                eprintln!("room discovery: {error}");
            }
            self.error = Some(error);
        }
    }

    fn start(&mut self) -> Result<(), String> {
        let addresses: Vec<_> = self.listeners.iter().map(SocketAddr::ip).collect();
        let info = build_service_info_bound_all(&self.room, &[], &addresses)
            .map_err(|error| error.to_string())?;
        let fullname = info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new().map_err(|error| error.to_string())?;
        let result = (|| {
            let events = daemon.monitor().map_err(|error| error.to_string())?;
            daemon.register(info).map_err(|error| error.to_string())?;
            Ok(events)
        })();
        match result {
            Ok(events) => {
                self.daemon = Some(daemon);
                self.events = Some(events);
                self.fullname = Some(fullname);
                self.error = None;
                Ok(())
            }
            Err(error) => {
                let _ = daemon.shutdown();
                Err(error)
            }
        }
    }

    pub fn active(&self) -> bool { self.daemon.is_some() && self.error.is_none() }
    pub fn error(&self) -> Option<&str> { self.error.as_deref() }

    fn stop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            if let Some(fullname) = self.fullname.take() {
                let _ = daemon.unregister(&fullname);
            }
            let _ = daemon.shutdown();
        }
        self.events = None;
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) { self.stop(); }
}
