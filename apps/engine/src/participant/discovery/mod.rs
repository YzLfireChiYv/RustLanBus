//! LAN room discovery over DNS-SD.
//!
//! Browses and advertises `_rlbroom._tcp.local.` on a dedicated worker.
//! Candidates are [`rooms::DiscoveredRoom`] values for a later selector.
//! This module does not read or write room profiles, choose a room, join a
//! room, evaluate member rules, or touch capture and playback.
//!
//! C16 reads [`DiscoverySnapshot::candidates`] and passes them to
//! `rooms::choose_plan`. A visible [`DiscoverySnapshot::failure`] leaves the
//! explicit-address probe available: an empty candidate list still produces
//! `SelectionPlan::ProbeAddress` when the saved card has an address.
//! Removal of a browsed room updates only this candidate pool.
//!
//! C14 supplies [`AdvertisedRoom::instance_id`], [`AdvertisedRoom::display_name`],
//! and the bound [`AdvertisedRoom::port`]. Interface addresses come from the
//! mDNS library, optionally limited by [`DiscoveryConfig`].

mod contract;
mod map;
mod service;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mdns_sd::{
    DaemonEvent, DaemonStatus, Receiver, RecvTimeoutError, ServiceDaemon, ServiceEvent,
    TryRecvError, UnregisterStatus,
};

use contract::validate_config;
use map::{BrowseSignal, CandidatePool};
use service::{
    build_service_info, build_service_info_bound, build_service_info_bound_all,
    is_our_service_type, record_from_resolved, to_if_kind,
};

pub use super::rooms::DiscoveredRoom;
pub use contract::{
    AdvertisedRoom, DiscoveryConfig, DiscoveryError, InterfaceSelector, PROTOCOL_VERSION,
    SERVICE_TYPE, TXT_INSTANCE, TXT_NAME, TXT_VERSION,
};

const COMMAND_WAIT: Duration = Duration::from_secs(1);
const EVENT_POLL: Duration = Duration::from_millis(200);

/// Latest conflict rename reported by the mDNS library.
///
/// The room caption stays in the text record and in [`DiscoveredRoom::name`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DnsLabelChange {
    pub original: String,
    pub current: String,
}

/// Coalesced discovery view. Each generation replaces the previous view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoverySnapshot {
    pub generation: u64,
    pub stopped: bool,
    pub browsing: bool,
    pub failure: Option<String>,
    pub advertised: Option<AdvertisedRoom>,
    pub advertised_dns_name: Option<String>,
    pub dns_label_change: Option<DnsLabelChange>,
    pub candidates: Vec<DiscoveredRoom>,
}

/// Result of waiting for a newer snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaitResult {
    /// At least one browse or status update happened. Intermediate updates
    /// during the wait are collapsed into this snapshot.
    Changed(DiscoverySnapshot),
    /// The generation was unchanged when the wait elapsed.
    TimedOut,
    /// Discovery is stopped and the caller has already observed this generation.
    Stopped(DiscoverySnapshot),
}

/// Owner of one mDNS daemon and one discovery worker.
pub struct RoomDiscovery {
    session: Mutex<Option<Session>>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    enable_interfaces: Vec<InterfaceSelector>,
}

struct Session {
    daemon: ServiceDaemon,
    worker: JoinHandle<()>,
    advertised_fullname: Option<String>,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

#[derive(Clone)]
struct Inner {
    generation: u64,
    stopped: bool,
    browsing: bool,
    failure: Option<String>,
    advertised: Option<AdvertisedRoom>,
    advertised_dns_name: Option<String>,
    dns_label_change: Option<DnsLabelChange>,
    candidates: Vec<DiscoveredRoom>,
}

impl Inner {
    fn browsing() -> Self {
        Self {
            generation: 0,
            stopped: false,
            browsing: true,
            failure: None,
            advertised: None,
            advertised_dns_name: None,
            dns_label_change: None,
            candidates: Vec::new(),
        }
    }

    fn snapshot(&self) -> DiscoverySnapshot {
        DiscoverySnapshot {
            generation: self.generation,
            stopped: self.stopped,
            browsing: self.browsing,
            failure: self.failure.clone(),
            advertised: self.advertised.clone(),
            advertised_dns_name: self.advertised_dns_name.clone(),
            dns_label_change: self.dns_label_change.clone(),
            candidates: self.candidates.clone(),
        }
    }
}

impl Shared {
    fn browsing() -> Self {
        Self {
            inner: Mutex::new(Inner::browsing()),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut Inner)) {
        let mut inner = self.lock();
        let before = inner.clone();
        change(&mut inner);
        if view_changed(&before, &inner) {
            inner.generation = inner.generation.wrapping_add(1);
            self.changed.notify_all();
        }
    }

    fn mark_stopped(&self, failure: Option<String>) {
        self.update(|inner| {
            inner.stopped = true;
            inner.browsing = false;
            inner.candidates.clear();
            inner.advertised = None;
            inner.advertised_dns_name = None;
            inner.failure = failure;
        });
    }
}

fn view_changed(before: &Inner, after: &Inner) -> bool {
    before.stopped != after.stopped
        || before.browsing != after.browsing
        || before.failure != after.failure
        || before.advertised != after.advertised
        || before.advertised_dns_name != after.advertised_dns_name
        || before.dns_label_change != after.dns_label_change
        || before.candidates != after.candidates
}

struct DaemonGuard {
    daemon: Option<ServiceDaemon>,
}

impl DaemonGuard {
    fn arm(daemon: ServiceDaemon) -> Self {
        Self {
            daemon: Some(daemon),
        }
    }

    fn daemon(&self) -> &ServiceDaemon {
        self.daemon
            .as_ref()
            .expect("the discovery daemon is still armed")
    }

    fn disarm(mut self) -> ServiceDaemon {
        self.daemon
            .take()
            .expect("the discovery daemon is still armed")
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            let _ = shutdown_daemon(&daemon);
        }
    }
}

impl RoomDiscovery {
    /// Start browsing. Advertisement is a separate owner call.
    pub fn start(config: DiscoveryConfig) -> Result<Self, DiscoveryError> {
        validate_config(&config)?;
        let guard = DaemonGuard::arm(call(ServiceDaemon::new)?);
        apply_interfaces(guard.daemon(), &config)?;
        let monitor = call(|| guard.daemon().monitor())?;
        let browse = call(|| guard.daemon().browse(SERVICE_TYPE))?;
        let shared = Arc::new(Shared::browsing());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("rlb-room-discovery".to_owned())
            .spawn(move || worker_loop(browse, monitor, worker_shared, worker_stop))
            .map_err(|error| {
                DiscoveryError::Service(format!("The discovery worker did not start: {error}"))
            })?;
        let enable_interfaces = config.enable_interfaces;
        Ok(Self {
            session: Mutex::new(Some(Session {
                daemon: guard.disarm(),
                worker,
                advertised_fullname: None,
            })),
            shared,
            stop,
            enable_interfaces,
        })
    }

    /// Publish one running contact service.
    ///
    /// A second call withdraws the previous advertisement first. The DNS
    /// instance label is the instance marker. The text record keeps
    /// `display_name`.
    pub fn advertise(&self, room: AdvertisedRoom) -> Result<(), DiscoveryError> {
        let info = build_service_info(&room, &self.enable_interfaces)?;
        self.publish_advertisement(room, info)
    }

    /// Concrete listeners advertise only their actual IP. Wildcards retain
    /// automatic address selection within the configured interface scope.
    pub fn advertise_bound(
        &self,
        room: AdvertisedRoom,
        bind_ip: std::net::IpAddr,
    ) -> Result<(), DiscoveryError> {
        if bind_ip.is_unspecified() {
            return self.advertise(room);
        }
        let info = build_service_info_bound(&room, &self.enable_interfaces, Some(bind_ip))?;
        self.publish_advertisement(room, info)
    }

    /// Publish exactly the bound TCP address families/addresses, including
    /// dual wildcard listeners. A single concrete address retains TXT_ADDRESS.
    pub fn advertise_bound_all(
        &self,
        room: AdvertisedRoom,
        bind_ips: &[std::net::IpAddr],
    ) -> Result<(), DiscoveryError> {
        let info = build_service_info_bound_all(&room, &self.enable_interfaces, bind_ips)?;
        self.publish_advertisement(room, info)
    }

    fn publish_advertisement(
        &self,
        room: AdvertisedRoom,
        info: mdns_sd::ServiceInfo,
    ) -> Result<(), DiscoveryError> {
        let fullname = info.get_fullname().to_owned();
        {
            let mut guard = self.lock_session();
            let Some(session) = guard.as_mut() else {
                return Err(DiscoveryError::Stopped);
            };
            if let Some(previous) = session.advertised_fullname.clone() {
                withdraw_fullname(&session.daemon, &previous)?;
                session.advertised_fullname = None;
                self.shared.update(|inner| {
                    inner.advertised = None;
                    inner.advertised_dns_name = None;
                });
            }
            call(|| session.daemon.register(info.clone()))?;
            session.advertised_fullname = Some(fullname.clone());
            self.shared.update(|inner| {
                inner.advertised = Some(room);
                inner.advertised_dns_name = Some(fullname);
            });
        }
        Ok(())
    }

    /// Withdraw the current advertisement and keep browsing.
    pub fn withdraw(&self) -> Result<(), DiscoveryError> {
        let mut guard = self.lock_session();
        let Some(session) = guard.as_mut() else {
            return Err(DiscoveryError::Stopped);
        };
        let Some(fullname) = session.advertised_fullname.clone() else {
            return Ok(());
        };
        withdraw_fullname(&session.daemon, &fullname)?;
        session.advertised_fullname = None;
        self.shared.update(|inner| {
            inner.advertised = None;
            inner.advertised_dns_name = None;
        });
        Ok(())
    }

    /// Latest candidate and status view.
    pub fn snapshot(&self) -> DiscoverySnapshot {
        self.shared.lock().snapshot()
    }

    /// Wait until [`DiscoverySnapshot::generation`] changes, discovery stops,
    /// or `timeout` elapses.
    pub fn wait_for_change(&self, after_generation: u64, timeout: Duration) -> WaitResult {
        let guard = self.shared.lock();
        let wait = self
            .shared
            .changed
            .wait_timeout_while(guard, timeout, |inner| {
                !inner.stopped && inner.generation == after_generation
            });
        let (guard, _) = match wait {
            Ok(ready) => ready,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.generation != after_generation {
            WaitResult::Changed(guard.snapshot())
        } else if guard.stopped {
            WaitResult::Stopped(guard.snapshot())
        } else {
            WaitResult::TimedOut
        }
    }

    /// Unregister any advertisement, stop browsing, and join the worker.
    pub fn shutdown(&self) -> Result<(), DiscoveryError> {
        let session = self.lock_session().take();
        let Some(session) = session else {
            return Ok(());
        };
        self.finish(session)
    }

    fn lock_session(&self) -> MutexGuard<'_, Option<Session>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn finish(&self, session: Session) -> Result<(), DiscoveryError> {
        self.stop.store(true, Ordering::Release);
        let withdraw_result = match &session.advertised_fullname {
            Some(fullname) => withdraw_fullname(&session.daemon, fullname),
            None => Ok(()),
        };
        let browse_result = call(|| session.daemon.stop_browse(SERVICE_TYPE));
        let daemon_result = shutdown_daemon(&session.daemon);
        let worker_ended = session.worker.join().is_err();
        if worker_ended {
            let message = "The discovery worker ended unexpectedly.".to_owned();
            self.shared.mark_stopped(Some(message.clone()));
            return Err(DiscoveryError::Service(message));
        }
        let result = withdraw_result.and(browse_result).and(daemon_result);
        self.shared
            .mark_stopped(result.as_ref().err().map(ToString::to_string));
        result
    }
}

impl Drop for RoomDiscovery {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn apply_interfaces(
    daemon: &ServiceDaemon,
    config: &DiscoveryConfig,
) -> Result<(), DiscoveryError> {
    for selector in &config.disable_interfaces {
        let selector = selector.clone();
        call(|| daemon.disable_interface(to_if_kind(&selector)))?;
    }
    // mdns-sd applies selectors in order; the last matching choice wins.
    for selector in &config.enable_interfaces {
        let selector = selector.clone();
        call(|| daemon.enable_interface(to_if_kind(&selector)))?;
    }
    Ok(())
}

fn withdraw_fullname(daemon: &ServiceDaemon, fullname: &str) -> Result<(), DiscoveryError> {
    let receiver = call(|| daemon.unregister(fullname))?;
    match receiver.recv_timeout(COMMAND_WAIT) {
        Ok(UnregisterStatus::OK | UnregisterStatus::NotFound) => Ok(()),
        Err(RecvTimeoutError::Timeout) => Err(DiscoveryError::Service(
            "Withdrawing the room advertisement timed out.".into(),
        )),
        Err(RecvTimeoutError::Disconnected) => Err(DiscoveryError::Stopped),
    }
}

fn shutdown_daemon(daemon: &ServiceDaemon) -> Result<(), DiscoveryError> {
    let receiver = call(|| daemon.shutdown())?;
    match receiver.recv_timeout(COMMAND_WAIT) {
        Ok(DaemonStatus::Shutdown) => Ok(()),
        Ok(DaemonStatus::Running) => Err(DiscoveryError::Service(
            "Room discovery was asked to stop and the worker was still running.".into(),
        )),
        Ok(_) => Err(DiscoveryError::Service(
            "Room discovery returned an unrecognized stop status.".into(),
        )),
        Err(RecvTimeoutError::Timeout) => Err(DiscoveryError::Service(
            "Room discovery stop timed out.".into(),
        )),
        Err(RecvTimeoutError::Disconnected) => Err(DiscoveryError::Stopped),
    }
}

fn call<T>(mut operation: impl FnMut() -> mdns_sd::Result<T>) -> Result<T, DiscoveryError> {
    let mut pause = Duration::from_millis(20);
    let mut attempt = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(mdns_sd::Error::DaemonShutdown) => return Err(DiscoveryError::Stopped),
            Err(mdns_sd::Error::Again) if attempt < 3 => {
                attempt += 1;
                thread::sleep(pause);
                pause = pause.saturating_mul(2);
            }
            Err(error) => return Err(DiscoveryError::Service(error.to_string())),
        }
    }
}

fn worker_loop(
    browse: Receiver<ServiceEvent>,
    monitor: Receiver<DaemonEvent>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) {
    let mut pool = CandidatePool::default();
    let mut monitor_open = true;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if monitor_open {
            monitor_open = drain_monitor(&monitor, &shared, &stop);
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        match browse.recv_timeout(EVENT_POLL) {
            Ok(event) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if handle_browse(event, &mut pool, &shared) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !stop.load(Ordering::Acquire) {
                    pool.clear();
                    shared.update(|inner| {
                        inner.browsing = false;
                        inner.candidates.clear();
                        inner.failure = Some("Room discovery browsing ended.".to_owned());
                    });
                }
                break;
            }
        }
    }
}

fn drain_monitor(monitor: &Receiver<DaemonEvent>, shared: &Shared, stop: &AtomicBool) -> bool {
    loop {
        match monitor.try_recv() {
            Ok(DaemonEvent::Error(error)) => {
                shared.update(|inner| inner.failure = Some(error.to_string()));
            }
            Ok(DaemonEvent::NameChange(change)) => {
                shared.update(|inner| {
                    inner.dns_label_change = Some(DnsLabelChange {
                        original: change.original,
                        current: change.new_name,
                    });
                });
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => {
                if !stop.load(Ordering::Acquire) {
                    shared.update(|inner| {
                        inner.failure = Some("Room discovery monitoring ended.".to_owned());
                    });
                }
                return false;
            }
        }
    }
}

/// Returns true when browsing has ended and the worker should exit.
fn handle_browse(event: ServiceEvent, pool: &mut CandidatePool, shared: &Shared) -> bool {
    match event {
        ServiceEvent::SearchStarted(service_type) if is_our_service_type(&service_type) => {
            shared.update(|inner| inner.browsing = true);
            false
        }
        ServiceEvent::ServiceResolved(resolved) => {
            if let Some(record) = record_from_resolved(&resolved) {
                let candidates = pool.apply(BrowseSignal::Resolved(record));
                shared.update(|inner| inner.candidates = candidates);
            }
            false
        }
        ServiceEvent::ServiceRemoved(service_type, fullname)
            if is_our_service_type(&service_type) =>
        {
            let candidates = pool.apply(BrowseSignal::Removed { fullname });
            shared.update(|inner| inner.candidates = candidates);
            false
        }
        ServiceEvent::SearchStopped(service_type) if is_our_service_type(&service_type) => {
            pool.clear();
            shared.update(|inner| {
                inner.browsing = false;
                inner.candidates.clear();
                inner.failure = Some("Room discovery browsing stopped.".to_owned());
            });
            true
        }
        _ => false,
    }
}
