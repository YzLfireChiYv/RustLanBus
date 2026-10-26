//! The only owner of participant UDP, publication and pair protocol state.
//!
//! Configuration is an ordered transport projection, never a file to save here.
//! The reactor wakes for UDP, queued commands and the nearest Peer deadline.
use super::PlaybackOwner;
use super::capture::{CaptureBlock, CaptureHandle, CaptureRuntime, CaptureState, TicketClock};
use super::output::{OutputHandle, OutputStatus};
use super::return_timing::ReturnTiming;
use crate::participant::config::{self, ParticipantConfig, SavedContact, TRACKED_PEER_BUDGET};
use crate::participant::network::{self, NetworkBinding, NetworkInterface, NetworkState};
use mio::{Events, Interest, Poll, Token, Waker};
use rlb_contact::ReturnState;
use rlb_model::{MemberId, RunId, SourceId};
use rlb_peer::{
    Catalogue, DatagramRoute, Intent, MAX_DATAGRAM_BYTES, PcmFormat, Peer, PeerError, PeerInstance,
    PeerOperation, PeerStatus, PreparedStorage, SourcePublisher, datagram_route,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DATAGRAMS_PER_TURN: usize = 32;
const CONTROLS_PER_PEER: usize = 8;
const COMMAND_CAPACITY: usize = 64;
const SOCKET: Token = Token(0);
const COMMAND: Token = Token(1);
const NETWORK_RETRY: Duration = Duration::from_millis(500);
// Cold connection recovery only; this never holds or delays an audio block.
const PATH_PROBE: Duration = Duration::from_millis(250);
// Each side publishes at most16 endpoints; cache the bounded cold combinations.
const MAX_PATHS: usize = 16 * 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeerPath {
    remote: SocketAddr,
    local: SocketAddr,
}

pub(crate) struct PeerSlot {
    pub(crate) peer: Peer,
    path: PeerPath,
    paths: Vec<PeerPath>,
    pending_path: Option<(rlb_model::DirectSessionId, PeerPath)>,
    next_probe_at: Instant,
    probe_cursor: usize,
    probe_budget: usize,
    pub(crate) allow_ticks: bool,
    pub(crate) last_error: Option<String>,
}
#[derive(Clone, Copy, Default)]
pub(crate) struct Counters {
    pub(crate) rejected_unconfigured: u64,
    pub(crate) rejected_malformed: u64,
    pub(crate) media_unavailable: u64,
    pub(crate) socket_errors: u64,
    pub(crate) controls_sent: u64,
}
#[derive(Clone)]
pub(crate) struct PeerView {
    pub(crate) address: SocketAddr,
    pub(crate) allow_ticks: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) status: PeerStatus,
    pub(crate) catalogue: Option<Catalogue>,
    pub(crate) local_revision: u64,
    pub(crate) paused: bool,
    pub(crate) admission: rlb_peer::AdmissionProjection,
    pub(crate) sending_sources: [Option<SourceId>; rlb_peer::MAX_SOURCES],
    pub(crate) source_peaks: [Option<(SourceId, f64, Instant)>; rlb_peer::MAX_SOURCES],
}
#[derive(Clone)]
pub(crate) struct MediaSnapshot {
    pub(crate) running: bool,
    pub(crate) local_address: SocketAddr,
    pub(crate) local_addresses: Vec<SocketAddr>,
    pub(crate) run_id: RunId,
    pub(crate) network_revision: u64,
    pub(crate) network: NetworkState,
    pub(crate) peers: HashMap<MemberId, PeerView>,
    pub(crate) counters: Counters,
    pub(crate) socket_error: Option<String>,
    pub(crate) fatal_socket_error: bool,
    pub(crate) admission_revision: u64,
    pub(crate) output_submitted_frames: u64,
    pub(crate) output: OutputStatus,
    pub(crate) captures: Vec<CaptureState>,
    pub(crate) local_catalogue: Catalogue,
    pub(crate) capture_error: Option<String>,
    pub(crate) endpoints: Vec<crate::participant::snapshot::AudioEndpoint>,
    pub(crate) return_loop: Option<crate::participant::snapshot::ReturnLoopState>,
}
#[derive(Clone, Copy)]
pub(crate) enum Resume {
    Automatic,
    Member(MemberId, bool),
    All(bool),
}
#[derive(Clone, Copy)]
pub(crate) enum RoomTransition {
    Keep,
    Enter,
    Leave,
}
#[derive(Clone, Copy)]
pub(crate) struct RoomFence {
    pub(crate) epoch: u64,
    pub(crate) closed: bool,
}
enum Request {
    NetworkCatalogue(Result<Vec<NetworkInterface>, String>),
    RefreshOutput,
    ConfigureReturn(Option<ReturnState>, u64, SyncSender<Result<(), String>>),
    Configure(
        Box<ParticipantConfig>,
        Resume,
        u64,
        SyncSender<Result<(), String>>,
    ),
    #[cfg(test)]
    Inspect(Box<dyn FnOnce(&mut MediaCore) + Send>),
}
#[derive(Clone)]
pub(crate) struct MediaHandle {
    captures: Vec<CaptureHandle>,
    output: OutputHandle,
    commands: SyncSender<Request>,
    wake: Arc<Waker>,
    stopped: Arc<AtomicBool>,
    pause_requested: Arc<AtomicBool>,
    state: Arc<Mutex<MediaSnapshot>>,
    room: Arc<Mutex<RoomFence>>,
}
pub(crate) struct MediaRuntime {
    pub(crate) handle: MediaHandle,
    thread: Option<JoinHandle<()>>,
}
impl MediaRuntime {
    pub(crate) fn start(config: ParticipantConfig) -> Result<Self, String> {
        let room = Arc::new(Mutex::new(RoomFence {
            epoch: 1,
            closed: !config.group.joined,
        }));
        let (tx, rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (boot_tx, boot_rx) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let pause_requested = Arc::new(AtomicBool::new(false));
        let owner_stop = Arc::clone(&stopped);
        let owner_pause = Arc::clone(&pause_requested);
        let owner = thread::Builder::new()
            .name("rlb-media".into())
            .spawn(move || {
                let setup = (|| -> Result<_, String> {
                    let mut core = MediaCore::boot(config)?;
                    let poll = Poll::new().map_err(|e| e.to_string())?;
                    core.synchronize_network(poll.registry());
                    let wake =
                        Arc::new(Waker::new(poll.registry(), COMMAND).map_err(|e| e.to_string())?);
                    #[cfg(not(test))]
                    {
                        core.capture = Some(CaptureRuntime::start(
                            &core.config,
                            core.run_id,
                            core.origin,
                            core.tickets.clone(),
                            core.playback.handle(),
                            wake.clone(),
                        )?);
                    }
                    let state = Arc::new(Mutex::new(core.initial_snapshot()));
                    core.publish(&state);
                    let handle = MediaHandle {
                        captures: core
                            .capture
                            .as_ref()
                            .map(|capture| capture.handles.clone())
                            .unwrap_or_default(),
                        output: core.playback.handle(),
                        commands: tx,
                        wake,
                        stopped: Arc::clone(&owner_stop),
                        pause_requested: Arc::clone(&owner_pause),
                        state: Arc::clone(&state),
                        room: Arc::clone(&room),
                    };
                    Ok((core, poll, state, handle))
                })();
                match setup {
                    Ok((core, poll, state, handle)) => {
                        if boot_tx.send(Ok(handle)).is_ok() {
                            run(core, poll, rx, state, owner_stop, owner_pause, room);
                        }
                    }
                    Err(e) => {
                        let _ = boot_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("The media owner could not start: {e}"))?;
        match boot_rx.recv() {
            Ok(Ok(handle)) => Ok(Self {
                handle,
                thread: Some(owner),
            }),
            Ok(Err(e)) => {
                let _ = owner.join();
                Err(e)
            }
            Err(_) => {
                let _ = owner.join();
                Err("The media owner stopped during startup.".into())
            }
        }
    }
    pub(crate) fn stop(&mut self) {
        self.handle.shutdown();
        if let Some(owner) = self.thread.take() {
            let _ = owner.join();
        }
    }
}
impl Drop for MediaRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}
impl MediaHandle {
    /// Cold enumeration is performed by the supervisor. A full control queue
    /// merely defers its next observation; no UDP producer waits on it.
    pub(crate) fn update_network_catalogue(
        &self,
        interfaces: Result<Vec<NetworkInterface>, String>,
    ) -> Result<(), String> {
        self.commands
            .try_send(Request::NetworkCatalogue(interfaces))
            .map_err(|error| format!("The network observation was not accepted: {error}"))?;
        self.wake.wake().map_err(|error| error.to_string())
    }
    pub(crate) fn configure_return_in(
        &self,
        state: Option<ReturnState>,
        epoch: u64,
    ) -> Result<(), String> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .try_send(Request::ConfigureReturn(state, epoch, reply))
            .map_err(|error| format!("The return observation command was not accepted: {error}"))?;
        self.wake.wake().map_err(|error| error.to_string())?;
        result.recv().map_err(|_| {
            "The media owner stopped before configuring return observation.".to_owned()
        })?
    }
    #[cfg(test)]
    pub(crate) fn attach_capture_fixture(&self) -> Vec<super::capture::FixtureSource> {
        let wake = self.wake.clone();
        self.with_core(move |core| {
            let (runtime, sources) = super::capture::fixture(
                &core.config,
                core.run_id,
                core.origin,
                core.tickets.clone(),
                core.playback.handle(),
                wake,
            )
            .unwrap();
            core.capture = Some(runtime);
            core.sync_capture().unwrap();
            sources
        })
    }
    pub(crate) fn refresh_output(&self) -> Result<(), String> {
        self.commands
            .try_send(Request::RefreshOutput)
            .map_err(|error| error.to_string())?;
        self.wake.wake().map_err(|error| error.to_string())
    }
    pub(crate) fn configure_in(
        &self,
        config: &ParticipantConfig,
        resume: Resume,
        epoch: u64,
    ) -> Result<(), String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("The media owner is stopped.".into());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .try_send(Request::Configure(
                Box::new(config.clone()),
                resume,
                epoch,
                tx,
            ))
            .map_err(|e| format!("The media command was not accepted: {e}"))?;
        self.wake.wake().map_err(|e| e.to_string())?;
        rx.recv()
            .map_err(|_| "The media owner stopped before applying the command.".to_owned())?
    }
    pub(crate) fn snapshot(&self) -> MediaSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        state.output = self.output.status();
        state.output_submitted_frames = state.output.submitted_frames;
        state.admission_revision = state.output.admission_revision;
        state.endpoints = self.output.catalogue();
        if !self.captures.is_empty() {
            state.captures = self.captures.iter().map(CaptureHandle::snapshot).collect();
        }
        state
    }
    pub(crate) fn room_fence(&self) -> RoomFence {
        *self.room.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Enqueue is nonblocking and runs under the short submission lock. No
    /// socket, disk, reply wait, or user callback may be performed by it.
    pub(crate) fn submit_scope<T>(
        &self,
        transition: RoomTransition,
        enqueue: impl FnOnce(u64) -> Result<T, String>,
    ) -> Result<T, String> {
        self.submit_scope_at(None, transition, enqueue)
    }

    /// Automatic recovery may enter only while the user-visible room fence
    /// still matches its observation. A newer explicit leave/entry wins.
    pub(crate) fn submit_scope_if_current<T>(
        &self,
        expected_epoch: u64,
        transition: RoomTransition,
        enqueue: impl FnOnce(u64) -> Result<T, String>,
    ) -> Result<T, String> {
        self.submit_scope_at(Some(expected_epoch), transition, enqueue)
    }

    fn submit_scope_at<T>(
        &self,
        expected_epoch: Option<u64>,
        transition: RoomTransition,
        enqueue: impl FnOnce(u64) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut current = self.room.lock().unwrap_or_else(|e| e.into_inner());
        if expected_epoch.is_some_and(|expected| current.epoch != expected) {
            return Err("Automatic room recovery was superseded by a newer room action.".into());
        }
        let next = match transition {
            RoomTransition::Keep => *current,
            RoomTransition::Enter => RoomFence {
                epoch: current
                    .epoch
                    .checked_add(1)
                    .ok_or("Room epoch exhausted.")?,
                closed: false,
            },
            RoomTransition::Leave => RoomFence {
                epoch: current
                    .epoch
                    .checked_add(1)
                    .ok_or("Room epoch exhausted.")?,
                closed: true,
            },
        };
        let result = enqueue(next.epoch);
        // Explicit closure wins even if the ordinary supervisor queue is full.
        // The supervisor observes this fence and later persists the leave.
        if result.is_ok() || matches!(transition, RoomTransition::Leave) {
            *current = next;
            let _ = self.wake.wake();
        }
        result
    }
    pub(crate) fn pause(&self) {
        self.pause_requested.store(true, Ordering::Release);
        let _ = self.wake.wake();
    }
    pub(crate) fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.wake();
    }
    #[cfg(test)]
    pub(crate) fn with_core<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut MediaCore) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .send(Request::Inspect(Box::new(move |core| {
                let _ = tx.send(f(core));
            })))
            .unwrap();
        self.wake.wake().unwrap();
        rx.recv().unwrap()
    }
}
fn run(
    mut core: MediaCore,
    mut poll: Poll,
    commands: Receiver<Request>,
    state: Arc<Mutex<MediaSnapshot>>,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    room: Arc<Mutex<RoomFence>>,
) {
    let mut events = Events::with_capacity(16);
    let mut applied_epoch = 1;
    while !stop.load(Ordering::Acquire) {
        let fence = *room.lock().unwrap_or_else(|e| e.into_inner());
        if fence.epoch != applied_epoch {
            core.config.group.joined = false;
            core.pause_all();
            applied_epoch = fence.epoch;
        }
        if pause.swap(false, Ordering::AcqRel) {
            core.config.group.joined = false;
            core.pause_all();
        }
        let mut count = 0;
        while count < COMMAND_CAPACITY {
            match commands.try_recv() {
                Ok(Request::NetworkCatalogue(interfaces)) => {
                    core.update_network_catalogue(interfaces);
                    core.synchronize_network(poll.registry());
                    core.publish(&state);
                    count += 1;
                }
                Ok(Request::ConfigureReturn(return_state, epoch, reply)) => {
                    let fence = *room.lock().unwrap_or_else(|error| error.into_inner());
                    let result = if epoch != fence.epoch {
                        Err("Return observation belongs to a superseded room action.".into())
                    } else {
                        core.return_timing.configure(if fence.closed {
                            None
                        } else {
                            return_state
                        });
                        Ok(())
                    };
                    core.publish(&state);
                    let _ = reply.send(result);
                    count += 1;
                }
                Ok(Request::RefreshOutput) => {
                    let _ = core.playback.refresh();
                    if let Some(capture) = &core.capture {
                        let _ = capture.configure(&core.config, true);
                    }
                    count += 1;
                }
                Ok(Request::Configure(mut config, resume, epoch, reply)) => {
                    if pause.swap(false, Ordering::AcqRel) {
                        core.config.group.joined = false;
                        core.pause_all();
                    }
                    let fence = *room.lock().unwrap_or_else(|e| e.into_inner());
                    let result = if epoch != fence.epoch {
                        Err("The media configuration belongs to a superseded room action.".into())
                    } else {
                        if epoch != applied_epoch {
                            core.config.group.joined = false;
                            core.pause_all();
                            applied_epoch = epoch;
                        }
                        if fence.closed {
                            config.group.joined = false;
                        }
                        core.configure(*config, resume)
                    };
                    core.synchronize_network(poll.registry());
                    let _ = core.synchronize_playback();
                    core.publish(&state);
                    let _ = reply.send(result);
                    count += 1;
                }
                #[cfg(test)]
                Ok(Request::Inspect(f)) => {
                    f(&mut core);
                    core.publish(&state);
                    count += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stop.store(true, Ordering::Release);
                    break;
                }
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        core.synchronize_network(poll.registry());
        if core.pump().is_err() {
            break;
        }
        core.synchronize_network(poll.registry());
        core.publish(&state);
        if count == COMMAND_CAPACITY
            || core.receive_budget_exhausted
            || core.capture_budget_exhausted
        {
            continue;
        }
        if stop.load(Ordering::Acquire) || pause.load(Ordering::Acquire) {
            continue;
        }
        match poll.poll(&mut events, core.next_wait()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                let message =
                    format!("The network reactor failed; local audio remains independent: {e}");
                core.socket_error = Some(message.clone());
                core.network.error = Some(message);
                for bound in &core.sockets {
                    bound.faulted.set(true);
                }
                core.synchronize_network(poll.registry());
                core.publish(&state);
                // A broken network wait must neither spin nor tear down the
                // independent native owners. Commands are retried next turn.
                thread::park_timeout(NETWORK_RETRY);
            }
        }
    }
    stop.store(true, Ordering::Release);
    core.close_runtime();
    core.publish(&state);
}

struct BoundDatagram {
    socket: mio::net::UdpSocket,
    plan: NetworkBinding,
    actual: SocketAddr,
    faulted: Cell<bool>,
}

pub(crate) struct MediaCore {
    pub(crate) config: ParticipantConfig,
    sockets: Vec<BoundDatagram>,
    receive_cursor: usize,
    local_addresses: Vec<SocketAddr>,
    candidate_snapshot: Vec<rlb_contact::MediaSocket>,
    pub(crate) local_address: SocketAddr,
    pub(crate) run_id: RunId,
    network_revision: u64,
    network: NetworkState,
    network_retry_pending: bool,
    network_dirty: bool,
    next_network_attempt: Instant,
    resume_on_bind: HashSet<MemberId>,
    next_instance: u64,
    pub(crate) publisher: SourcePublisher,
    pub(crate) peers: HashMap<MemberId, PeerSlot>,
    pub(crate) running: bool,
    pub(crate) counters: Counters,
    pub(crate) socket_error: Option<String>,
    fatal_socket_error: bool,
    origin: Instant,
    pub(crate) playback: PlaybackOwner,
    pcm_scratch: Vec<f64>,
    pub(crate) tickets: Arc<TicketClock>,
    capture: Option<CaptureRuntime>,
    capture_seen: [u64; 2],
    capture_states: [Option<CaptureState>; 2],
    capture_storage: Vec<(SourceId, PcmFormat, PreparedStorage)>,
    capture_error: Option<String>,
    capture_budget_exhausted: bool,
    receive_budget_exhausted: bool,
    return_timing: ReturnTiming,
}
impl MediaCore {
    #[cfg(test)]
    pub(crate) fn send_prepared_to_peer(
        &self,
        member_id: MemberId,
        bytes: &[u8],
    ) -> Result<usize, String> {
        let slot = self
            .peers
            .get(&member_id)
            .ok_or_else(|| format!("Member {member_id} has no peer."))?;
        send_on(&self.sockets, bytes, slot.path)
            .map_err(|error| format!("The original could not be sent: {error}"))
    }

    fn boot(config: ParticipantConfig) -> Result<Self, String> {
        config.validate()?;
        let mut local_address = config.bind;
        local_address.set_port(0);
        let mut network = NetworkState {
            policy: config.network.clone(),
            ..NetworkState::default()
        };
        match network::catalogue() {
            Ok(interfaces) => network.interfaces = interfaces,
            Err(error) => network.error = Some(error),
        }
        let run_id = RunId::generate().map_err(|e| e.to_string())?;
        let mut publisher = SourcePublisher::new(run_id);
        for source in &config.sources {
            publisher
                .register(source.id, publication_kind(source.kind), source.publish)
                .map_err(|e| e.to_string())?;
        }
        let mut core = Self {
            playback: PlaybackOwner::new(config.clone())?,
            config,
            sockets: Vec::with_capacity(16),
            receive_cursor: 0,
            local_addresses: Vec::with_capacity(16),
            candidate_snapshot: Vec::with_capacity(16),
            local_address,
            run_id,
            network_revision: 0,
            network,
            network_retry_pending: true,
            network_dirty: true,
            next_network_attempt: Instant::now(),
            resume_on_bind: HashSet::new(),
            next_instance: 1,
            publisher,
            peers: HashMap::new(),
            running: true,
            counters: Counters::default(),
            socket_error: None,
            fatal_socket_error: false,
            origin: Instant::now(),
            pcm_scratch: vec![0.; 280],
            tickets: Arc::new(TicketClock::default()),
            capture: None,
            capture_seen: [0; 2],
            capture_states: [None, None],
            capture_storage: Vec::with_capacity(2),
            capture_error: None,
            capture_budget_exhausted: false,
            receive_budget_exhausted: false,
            return_timing: ReturnTiming::default(),
        };
        core.synchronize_playback()?;
        Ok(core)
    }

    fn network_ready(&self) -> bool {
        !self.network_dirty && self.sockets.iter().any(|bound| !bound.faulted.get())
    }

    fn update_network_catalogue(&mut self, observed: Result<Vec<NetworkInterface>, String>) {
        match observed {
            Ok(interfaces) => {
                if self.network_ready() && !self.network_retry_pending {
                    self.network.error = None;
                }
                if self.network.interfaces != interfaces {
                    self.network.interfaces = interfaces;
                    self.network_revision = self.network_revision.saturating_add(1);
                    self.network_dirty = true;
                    self.next_network_attempt = Instant::now();
                }
            }
            Err(error) => self.network.error = Some(error),
        }
    }

    fn publish_network_bindings(&mut self) {
        self.local_addresses = self.sockets.iter().map(|bound| bound.actual).collect();
        self.local_address = self.local_addresses.first().copied().unwrap_or_else(|| {
            let mut address = self.config.bind;
            address.set_port(0);
            address
        });
        self.network.selections = self
            .sockets
            .iter()
            .map(|bound| NetworkBinding {
                bind: bound.actual,
                ..bound.plan.clone()
            })
            .collect();
        self.network.selection = self.network.selections.first().cloned();
        self.network.policy = self.config.network.clone();
    }

    fn invalidate_sockets(&mut self, removed: &[SocketAddr]) {
        for (id, slot) in &mut self.peers {
            if !removed.contains(&slot.path.local) {
                continue;
            }
            slot.peer.pause();
            slot.allow_ticks = false;
            slot.pending_path = None;
            if self.config.auto_reconnect {
                self.resume_on_bind.insert(*id);
            }
        }
        let _ = self.synchronize_playback();
    }

    fn reconcile_peer_paths(&mut self, changed: Option<&HashSet<MemberId>>) {
        let now = self.now_ms();
        for contact in self.config.contacts.clone() {
            if !self.peers.contains_key(&contact.member_id)
                || changed.is_some_and(|ids| !ids.contains(&contact.member_id))
            {
                continue;
            }
            let mut paths = self.contact_paths(&contact);
            if let Some(slot) = self.peers.get_mut(&contact.member_id) {
                let usable = self
                    .sockets
                    .iter()
                    .any(|bound| bound.actual == slot.path.local && !bound.faulted.get());
                let withdrawn = changed.is_some_and(|ids| ids.contains(&contact.member_id));
                if slot.peer.status().connected
                    && usable
                    && !withdrawn
                    && !paths.contains(&slot.path)
                {
                    if paths.len() == MAX_PATHS {
                        paths.pop();
                    }
                    paths.push(slot.path);
                }
                let retained = paths.contains(&slot.path);
                slot.paths = paths;
                if !retained {
                    slot.pending_path = None;
                    if let Some(path) = slot.paths.first().copied() {
                        let connected = slot.peer.status().connected;
                        slot.path = path;
                        slot.next_probe_at = Instant::now() + PATH_PROBE;
                        slot.probe_budget = 4;
                        slot.probe_cursor = 0;
                        let operation = PeerOperation {
                            now_ms: now,
                            watermark: self.tickets.watermark(),
                        };
                        let _ = slot.peer.set_source_policy_with(closed_policy(), operation);
                        if connected && !self.config.auto_reconnect {
                            slot.peer.pause();
                            slot.allow_ticks = false;
                        } else if slot.allow_ticks {
                            let _ = slot.peer.reconnect(now);
                        }
                    } else {
                        slot.peer.pause();
                        slot.allow_ticks = false;
                        if self.config.auto_reconnect {
                            self.resume_on_bind.insert(contact.member_id);
                        }
                    }
                }
            }
        }
    }

    fn synchronize_network(&mut self, registry: &mio::Registry) {
        let faults = self.sockets.iter().any(|bound| bound.faulted.get());
        if !self.network_dirty && !self.network_retry_pending && !faults {
            return;
        }
        if !self.network_dirty && !faults && Instant::now() < self.next_network_attempt {
            return;
        }
        let mut issues = Vec::new();
        let plans = match self
            .config
            .network
            .resolve_all_from(self.config.bind, &self.network.interfaces)
        {
            Ok(plans) => plans,
            Err(error) => {
                issues.push(error);
                Vec::new()
            }
        };
        let mut removed = Vec::new();
        self.sockets.retain_mut(|bound| {
            let keep = !bound.faulted.get() && plans.contains(&bound.plan);
            if !keep {
                let _ = registry.deregister(&mut bound.socket);
                removed.push(bound.actual);
            }
            keep
        });
        if !removed.is_empty() {
            self.invalidate_sockets(&removed);
        }
        let initial = self.sockets.is_empty();
        let attempts = if initial && self.config.bind.port() == 0 {
            4
        } else {
            1
        };
        for attempt in 0..attempts {
            let mut port = self
                .sockets
                .first()
                .map(|bound| bound.actual.port())
                .unwrap_or(self.config.bind.port());
            for original in &plans {
                if self.sockets.iter().any(|bound| bound.plan == *original) {
                    continue;
                }
                let mut binding = original.clone();
                binding.bind.set_port(port);
                let result = network::bind_udp(&binding).and_then(|socket| {
                    let actual = socket.local_addr().map_err(|e| e.to_string())?;
                    let mut socket = mio::net::UdpSocket::from_std(socket);
                    registry
                        .register(&mut socket, SOCKET, Interest::READABLE)
                        .map_err(|e| e.to_string())?;
                    Ok(BoundDatagram {
                        socket,
                        plan: original.clone(),
                        actual,
                        faulted: Cell::new(false),
                    })
                });
                match result {
                    Ok(bound) => {
                        if port == 0 {
                            port = bound.actual.port();
                        }
                        self.sockets.push(bound);
                    }
                    Err(error) => issues.push(error),
                }
            }
            if self.sockets.len() == plans.len() || attempt + 1 == attempts {
                break;
            }
            if initial && self.config.bind.port() == 0 {
                for bound in &mut self.sockets {
                    let _ = registry.deregister(&mut bound.socket);
                }
                self.sockets.clear();
                issues.clear();
            }
        }
        let old = self.local_addresses.clone();
        self.sockets.sort_by_key(|bound| {
            (
                bound.actual.ip().is_loopback(),
                bound.actual.is_ipv6(),
                bound.actual,
            )
        });
        self.publish_network_bindings();
        if old != self.local_addresses || !removed.is_empty() {
            self.network_revision = self.network_revision.saturating_add(1);
        }
        self.network_dirty = false;
        self.network_retry_pending = self.sockets.len() != plans.len() || plans.is_empty();
        self.next_network_attempt = Instant::now() + NETWORK_RETRY;
        self.fatal_socket_error = false;
        self.socket_error = if self.sockets.is_empty() {
            Some(
                issues
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Waiting for a usable network socket.".into()),
            )
        } else {
            None
        };
        self.network.error = issues.first().cloned();
        self.reconcile_peer_paths(None);
        let candidates = network::media_candidates(
            &self.config.network,
            &self.local_addresses,
            &self.network.interfaces,
            None,
        );
        if candidates != self.candidate_snapshot {
            self.candidate_snapshot = candidates;
            let now = self.now_ms();
            for slot in self.peers.values_mut() {
                if slot.allow_ticks {
                    let _ = slot.peer.announce_addresses_changed(now);
                }
            }
        }
        if self.config.group.joined && self.network_ready() {
            for contact in self.config.contacts.clone() {
                let requested = self.resume_on_bind.remove(&contact.member_id);
                if self.config.auto_reconnect || requested {
                    if let Err(error) = self.ensure_peer(contact.member_id, false) {
                        self.network.error = Some(error);
                    }
                }
            }
        }
    }

    fn contact_paths(&self, contact: &SavedContact) -> Vec<PeerPath> {
        let mut groups = Vec::new();
        for remote in std::iter::once(contact.address).chain(contact.alternates.iter().copied()) {
            let remote = match remote {
                SocketAddr::V6(ip) if ip.ip().to_ipv4_mapped().is_some() => SocketAddr::new(
                    IpAddr::V4(ip.ip().to_ipv4_mapped().expect("mapped IPv4")),
                    ip.port(),
                ),
                _ => remote,
            };
            if config::validate_contact_address(remote).is_err() {
                continue;
            }
            let mut paths = Vec::new();
            for bound in &self.sockets {
                if bound.faulted.get()
                    || bound.actual.is_ipv4() != remote.is_ipv4()
                    || bound.actual.ip().is_loopback() != remote.ip().is_loopback()
                {
                    continue;
                }
                let mut target = remote;
                if let SocketAddr::V6(address) = &mut target {
                    if address.ip().is_unicast_link_local() {
                        let Some(index) = bound_scope(bound) else {
                            continue;
                        };
                        address.set_scope_id(index);
                    }
                }
                paths.push(PeerPath {
                    remote: target,
                    local: bound.actual,
                });
            }
            // This affinity only orders real handshake attempts. It does not
            // declare a route reachable or replace the peer's contact proof.
            paths.sort_by_key(|path| {
                std::cmp::Reverse(prefix_affinity(path.local.ip(), path.remote.ip()))
            });
            groups.push(paths);
        }
        let mut result = Vec::with_capacity(MAX_PATHS);
        for at in 0..MAX_PATHS {
            for choices in &groups {
                if let Some(path) = choices.get(at) {
                    if !result.contains(path) && result.len() < MAX_PATHS {
                        result.push(*path);
                    }
                }
            }
        }
        result
    }

    fn configure(&mut self, config: ParticipantConfig, resume: Resume) -> Result<(), String> {
        config.validate()?;
        if config.member_id != self.config.member_id
            || config
                .sources
                .iter()
                .map(|s| (s.id, s.kind))
                .collect::<Vec<_>>()
                != self
                    .config
                    .sources
                    .iter()
                    .map(|s| (s.id, s.kind))
                    .collect::<Vec<_>>()
        {
            return Err("Media member and source identities stay fixed for this run.".into());
        }
        if config.bind != self.config.bind || config.network != self.config.network {
            self.network_dirty = true;
            self.next_network_attempt = Instant::now();
        }
        let changed_contacts: HashSet<MemberId> = config
            .contacts
            .iter()
            .filter(|contact| {
                self.config
                    .contacts
                    .iter()
                    .find(|old| old.member_id == contact.member_id)
                    .is_none_or(|old| !same_contact_candidates(old, contact))
            })
            .map(|contact| contact.member_id)
            .collect();
        let disable_auto = self.config.auto_reconnect && !config.auto_reconnect;
        if self.config.group.name != config.group.name || !config.group.joined {
            self.pause_all();
        }
        let mut changed = false;
        for source in &config.sources {
            if self
                .config
                .source(source.id)
                .is_some_and(|old| old.publish != source.publish)
            {
                self.publisher
                    .set_publication(source.id, source.publish)
                    .map_err(|e| e.to_string())?;
                changed = true;
            }
        }
        self.config = config;
        // A directory outage retains contacts. Explicit membership/room
        // removal does not need lifetime-long paused Peer instances.
        self.peers.retain(|id, _| {
            self.config
                .contacts
                .iter()
                .any(|contact| contact.member_id == *id)
        });
        self.resume_on_bind.retain(|id| {
            self.config
                .contacts
                .iter()
                .any(|contact| contact.member_id == *id)
        });
        if !self.config.group.joined {
            self.resume_on_bind.clear();
        }
        self.reconcile_peer_paths(Some(&changed_contacts));
        if let Err(error) = self.playback.configure(&self.config) {
            self.capture_error = Some(error);
        }
        if let Some(capture) = &self.capture {
            if let Err(error) = capture.configure(&self.config, false) {
                self.capture_error = Some(error);
            }
        }
        if changed {
            let catalogue = self.publisher.catalogue();
            let now = self.now_ms();
            for slot in self.peers.values_mut() {
                slot.peer
                    .install_catalogue(&catalogue, now)
                    .map_err(peer_sentence)?;
            }
        }
        let op = self.peer_operation();
        for (id, slot) in &mut self.peers {
            slot.peer
                .set_source_policy_with(
                    policy_for_path(&self.config, *id, slot.path.remote, slot.peer.status()),
                    op,
                )
                .map_err(peer_sentence)?;
        }
        if disable_auto {
            self.park_disconnected();
        }
        if self.config.group.joined {
            for contact in self.config.contacts.clone() {
                let force = match resume {
                    Resume::Member(id, reset) if id == contact.member_id => Some(reset),
                    Resume::All(reset) => Some(reset),
                    _ => None,
                };
                if let Some(reset) = force {
                    if self.network_ready() {
                        self.ensure_peer(contact.member_id, reset)?;
                    } else {
                        self.resume_on_bind.insert(contact.member_id);
                    }
                } else if self.config.auto_reconnect {
                    if self.network_ready() {
                        self.ensure_peer(contact.member_id, false)?;
                    }
                }
            }
        }
        if let Err(error) = self.sync_capture() {
            self.capture_error = Some(error);
        }
        self.synchronize_playback()
    }
    fn next_wait(&self) -> Option<Duration> {
        let now = Instant::now();
        let peer = self
            .peers
            .iter()
            .filter(|(_, slot)| slot.allow_ticks && self.config.group.joined)
            .map(|(_, slot)| {
                let protocol = Duration::from_millis(
                    slot.peer.next_deadline_ms().saturating_sub(self.now_ms()),
                );
                if !slot.peer.status().connected && !slot.paths.is_empty() {
                    protocol.min(slot.next_probe_at.saturating_duration_since(now))
                } else {
                    protocol
                }
            })
            .min();
        if self.network_retry_pending
            || self.network_dirty
            || self.sockets.iter().any(|bound| bound.faulted.get())
        {
            let retry = self.next_network_attempt.saturating_duration_since(now);
            Some(peer.map_or(retry, |deadline| deadline.min(retry)))
        } else {
            peer
        }
    }
    fn initial_snapshot(&self) -> MediaSnapshot {
        MediaSnapshot {
            running: self.running,
            local_address: self.local_address,
            local_addresses: self.local_addresses.clone(),
            run_id: self.run_id,
            network_revision: self.network_revision,
            network: self.network.clone(),
            peers: HashMap::new(),
            counters: self.counters,
            socket_error: self.socket_error.clone(),
            fatal_socket_error: self.fatal_socket_error,
            admission_revision: self.playback.admission_revision(),
            output_submitted_frames: self.playback.submitted_frames(),
            output: self.playback.status(),
            captures: self
                .capture
                .as_ref()
                .map(CaptureRuntime::snapshots)
                .unwrap_or_default(),
            local_catalogue: self.publisher.catalogue(),
            capture_error: self.capture_error.clone(),
            endpoints: Vec::new(),
            return_loop: self.return_timing.snapshot(),
        }
    }
    fn publish(&self, shared: &Mutex<MediaSnapshot>) {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        state.running = self.running;
        state.local_address = self.local_address;
        if state.local_addresses != self.local_addresses {
            state.local_addresses.clone_from(&self.local_addresses);
        }
        state.network_revision = self.network_revision;
        if state.network != self.network {
            state.network.clone_from(&self.network);
        }
        state.return_loop = self.return_timing.snapshot();
        state.counters = self.counters;
        state.fatal_socket_error = self.fatal_socket_error;
        if self.config.sources.iter().any(|source| {
            self.publisher.source(source.id) != state.local_catalogue.source(source.id)
        }) {
            state.local_catalogue = self.publisher.catalogue();
        }
        if state.capture_error != self.capture_error {
            state.capture_error.clone_from(&self.capture_error);
        }
        if state.socket_error != self.socket_error {
            state.socket_error.clone_from(&self.socket_error);
        }
        state.peers.retain(|id, _| self.peers.contains_key(id));
        for (id, slot) in &self.peers {
            let view = state.peers.entry(*id).or_insert_with(|| PeerView {
                address: slot.path.remote,
                allow_ticks: slot.allow_ticks,
                last_error: slot.last_error.clone(),
                status: slot.peer.status(),
                catalogue: slot.peer.remote_catalogue().cloned(),
                local_revision: slot.peer.local_revision(),
                paused: slot.peer.is_paused(),
                admission: rlb_peer::AdmissionProjection::default(),
                sending_sources: [None; rlb_peer::MAX_SOURCES],
                source_peaks: [None; rlb_peer::MAX_SOURCES],
            });
            view.address = slot.path.remote;
            view.allow_ticks = slot.allow_ticks;
            view.status = slot.peer.status();
            view.local_revision = slot.peer.local_revision();
            view.paused = slot.peer.is_paused();
            view.source_peaks = self.playback.source_peaks(*id);
            slot.peer.project_admission(&mut view.admission);
            view.sending_sources.fill(None);
            for (index, source) in self
                .config
                .sources
                .iter()
                .take(rlb_peer::MAX_SOURCES)
                .enumerate()
            {
                if slot.peer.source_send_ready(source.id) {
                    view.sending_sources[index] = Some(source.id);
                }
            }
            if view.last_error != slot.last_error {
                view.last_error.clone_from(&slot.last_error);
            }
            if view.catalogue.as_ref() != slot.peer.remote_catalogue() {
                view.catalogue = slot.peer.remote_catalogue().cloned();
            }
        }
    }
    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn ensure_peer(&mut self, member_id: MemberId, reset: bool) -> Result<(), String> {
        self.config
            .member(member_id)
            .ok_or_else(|| format!("Member {member_id} has no saved choices."))?;
        let operation = self.peer_operation();
        let now = operation.now_ms;
        if let Some(slot) = self.peers.get_mut(&member_id) {
            if slot.paths.is_empty() {
                self.resume_on_bind.insert(member_id);
                slot.peer.pause();
                slot.allow_ticks = false;
                slot.last_error =
                    Some("Waiting for a compatible local/remote address pair.".into());
                return Ok(());
            }
            let resumed = !slot.allow_ticks || slot.peer.is_paused();
            let policy = if reset || resumed {
                closed_policy()
            } else {
                policy_for_path(
                    &self.config,
                    member_id,
                    slot.path.remote,
                    slot.peer.status(),
                )
            };
            slot.peer
                .set_source_policy_with(policy, operation)
                .map_err(peer_sentence)?;
            slot.allow_ticks = true;
            slot.peer.resume(now).map_err(peer_sentence)?;
            if reset {
                slot.peer.reconnect(now).map_err(peer_sentence)?;
                slot.pending_path = None;
            }
            if reset || resumed {
                slot.next_probe_at = Instant::now() + PATH_PROBE;
                slot.probe_budget = 4;
                slot.probe_cursor = 0;
            }
            return self.drain_peer(member_id);
        }
        let contact = self
            .config
            .contacts
            .iter()
            .find(|contact| contact.member_id == member_id)
            .ok_or_else(|| format!("Member {member_id} has no saved contact address."))?;
        let paths = self.contact_paths(contact);
        let Some(preferred) = paths.first().copied() else {
            self.resume_on_bind.insert(member_id);
            self.network.error = Some(format!(
                "Member {member_id} has no compatible permitted local/remote address pair."
            ));
            return Ok(());
        };
        self.admit_new_member(member_id)?;
        let mut peer = Peer::new(
            self.config.member_id,
            member_id,
            self.run_id,
            Intent {
                send: false,
                receive: false,
            },
            self.publisher.catalogue(),
            now,
        )
        .map_err(peer_sentence)?;
        peer.attach_instance(PeerInstance::new(self.next_instance).expect("nonzero instance"))
            .map_err(peer_sentence)?;
        self.next_instance = self
            .next_instance
            .checked_add(1)
            .ok_or("Peer instance counter exhausted.")?;
        self.peers.insert(
            member_id,
            PeerSlot {
                peer,
                path: preferred,
                paths,
                pending_path: None,
                next_probe_at: Instant::now() + PATH_PROBE,
                probe_cursor: 0,
                probe_budget: 4,
                allow_ticks: true,
                last_error: None,
            },
        );
        self.drain_peer(member_id)
    }

    fn close_runtime(&mut self) {
        if let Some(capture) = &mut self.capture {
            capture.stop();
        }
        self.running = false;
        self.pause_all();
        self.peers.clear();
        let _ = self.synchronize_playback();
        self.playback.stop();
    }

    pub(crate) fn pause_all(&mut self) {
        self.return_timing.configure(None);
        for slot in self.peers.values_mut() {
            slot.peer.pause();
            slot.allow_ticks = false;
        }
        let _ = self.synchronize_playback();
    }

    fn admit_new_member(&self, member_id: MemberId) -> Result<(), String> {
        if self.peers.contains_key(&member_id)
            || self
                .config
                .contacts
                .iter()
                .any(|contact| contact.member_id == member_id)
        {
            return Ok(());
        }
        let mut tracked = self.peers.keys().copied().collect::<HashSet<_>>();
        for contact in &self.config.contacts {
            tracked.insert(contact.member_id);
        }
        if tracked.len() >= TRACKED_PEER_BUDGET {
            return Err(
                "The participant is already tracking 64 member pairs for this run, including paused pairs. The new contact was not saved."
                    .into(),
            );
        }
        Ok(())
    }

    fn park_disconnected(&mut self) {
        let ids: Vec<MemberId> = self.peers.keys().copied().collect();
        for id in ids {
            let Some(slot) = self.peers.get_mut(&id) else {
                continue;
            };
            if slot.peer.status().connected {
                continue;
            }
            slot.peer.pause();
            slot.allow_ticks = false;
        }
        let _ = self.synchronize_playback();
    }

    fn tick_peers(&mut self) -> Result<(), String> {
        if !self.network_ready() {
            return Ok(());
        }
        let ids: Vec<MemberId> = self.peers.keys().copied().collect();
        for id in ids {
            let now = self.now_ms();
            let send = {
                let Some(slot) = self.peers.get_mut(&id) else {
                    continue;
                };
                if !slot.allow_ticks || !self.config.group.joined {
                    continue;
                }
                if !slot.peer.status().connected && Instant::now() >= slot.next_probe_at {
                    slot.probe_budget = 4;
                    slot.next_probe_at = Instant::now() + PATH_PROBE;
                    if let Err(error) = slot.peer.probe_contact(now) {
                        slot.last_error = Some(peer_sentence(error));
                        continue;
                    }
                }
                let before = slot.peer.status();
                if let Err(error) = slot.peer.tick(now) {
                    slot.last_error = Some(peer_sentence(error));
                    continue;
                }
                if before.connected && !slot.peer.status().connected {
                    let op = PeerOperation {
                        now_ms: now,
                        watermark: self.tickets.watermark(),
                    };
                    let _ = slot.peer.set_source_policy_with(closed_policy(), op);
                    slot.pending_path = None;
                    slot.probe_budget = 4;
                    slot.next_probe_at = Instant::now() + PATH_PROBE;
                    if !self.config.auto_reconnect {
                        slot.peer.pause();
                        slot.allow_ticks = false;
                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            };
            if send {
                self.drain_peer(id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn drain_peer(&mut self, member_id: MemberId) -> Result<(), String> {
        self.drain_peer_to(member_id, None)
    }

    fn drain_peer_to(
        &mut self,
        member_id: MemberId,
        reply: Option<PeerPath>,
    ) -> Result<(), String> {
        if !self.network_ready() {
            return Ok(());
        }
        for _ in 0..CONTROLS_PER_PEER {
            let outgoing = {
                let Some(slot) = self.peers.get_mut(&member_id) else {
                    return Ok(());
                };
                if !slot.allow_ticks {
                    while slot.peer.take_control_routed().is_some() {}
                    return Ok(());
                }
                let Some(control) = slot.peer.take_control_routed() else {
                    return Ok(());
                };
                let active = slot.peer.status().session;
                let mut targets = [None; 4];
                if control.session.is_some() && control.session == active {
                    targets[0] = Some(slot.path);
                } else if let Some(session) = control.session {
                    if slot.peer.pending_session() != Some(session) {
                        continue;
                    }
                    targets[0] = Some(
                        slot.pending_path
                            .filter(|(pending, _)| *pending == session)
                            .map(|(_, path)| path)
                            .or(reply)
                            .unwrap_or(slot.path),
                    );
                } else if let Some(reply) = reply {
                    targets[0] = Some(reply);
                } else if !slot.peer.status().connected {
                    let count = slot.probe_budget.min(4).min(slot.paths.len());
                    if count == 0 {
                        continue;
                    }
                    for target in targets.iter_mut().take(count) {
                        *target = Some(slot.paths[slot.probe_cursor % slot.paths.len()]);
                        slot.probe_cursor = (slot.probe_cursor + 1) % slot.paths.len();
                    }
                    slot.probe_budget -= count;
                } else {
                    targets[0] = Some(slot.path);
                }
                (control.bytes, targets)
            };
            for path in outgoing.1.into_iter().flatten() {
                match send_on(&self.sockets, &outgoing.0, path) {
                    Ok(_) => {
                        self.counters.controls_sent = self.counters.controls_sent.saturating_add(1)
                    }
                    Err(error) => {
                        self.counters.socket_errors = self.counters.socket_errors.saturating_add(1);
                        if let Some(slot) = self.peers.get_mut(&member_id) {
                            slot.last_error = Some(format!(
                                "A control packet to {} could not be sent: {error}",
                                path.remote
                            ));
                        }
                        if !recoverable_udp_error(&error) {
                            self.network.error =
                                Some(format!("A media socket cannot send control: {error}"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn pump(&mut self) -> Result<(), String> {
        if let Err(error) = self.sync_capture() {
            self.capture_error = Some(error);
        }
        let received = self.receive_datagrams();
        let _ = self.synchronize_playback();
        received?;
        let ticked = self.tick_peers();
        self.capture_interest();
        if let Err(error) = self.send_captured() {
            if self.fatal_socket_error {
                return Err(error);
            }
            self.capture_error = Some(error);
        }
        let admission = self.synchronize_playback();
        ticked?;
        if admission.is_err() {
            return Ok(());
        }
        self.playback.service_software()
    }

    fn peer_operation(&self) -> PeerOperation {
        PeerOperation {
            now_ms: self.now_ms(),
            watermark: self.tickets.watermark(),
        }
    }

    fn sync_capture(&mut self) -> Result<(), String> {
        let Some(capture) = &self.capture else {
            return Ok(());
        };
        let mut catalogue_changed = false;
        for index in 0..capture.handles.len() {
            let handle = &capture.handles[index];
            if let Some(state) = handle.state_after(self.capture_seen[index]) {
                self.capture_seen[index] = state.revision;
                self.capture_states[index] = Some(state);
            }
            let Some(state) = &self.capture_states[index] else {
                continue;
            };
            let Some(source) = self.config.source(state.source) else {
                continue;
            };
            if !source.publish {
                handle.set_interest(false);
                handle.set_demand(false);
                handle.request_binding(None)?;
                continue;
            }
            if state.phase == "idle" {
                // A stopped native stream may keep its last valid public
                // descriptor. Subscriptions are still exchanged and are the
                // authority that wakes capture; no PCM can pass an old binding.
                handle.request_binding(None)?;
                continue;
            }
            let acquisition = state
                .acquisition
                .filter(|_| state.phase == "ready" && state.endpoint == source.endpoint_id);
            let Some(acquisition) = acquisition else {
                if self.publisher.binding(source.id).is_some()
                    || self.publisher.pending_binding(source.id).is_some()
                {
                    self.publisher
                        .mark_unavailable(source.id)
                        .map_err(|error| error.to_string())?;
                    catalogue_changed = true;
                }
                handle.set_interest(false);
                handle.request_binding(None)?;
                continue;
            };
            let bound = self.publisher.binding(source.id).is_some_and(|binding| {
                binding.acquisition() == acquisition.epoch && binding.format() == acquisition.format
            });
            if !bound {
                let mut pending = self.publisher.pending_binding(source.id);
                if !pending.is_some_and(|request| {
                    request.expected_acquisition() == acquisition.epoch
                        && request.format() == acquisition.format
                }) {
                    pending = self
                        .publisher
                        .begin_binding(source.id, acquisition.epoch, acquisition.format)
                        .map_err(|error| error.to_string())?
                        .pending;
                    catalogue_changed = true;
                }
                handle.request_binding(pending)?;
                if let Some(ack) = state.ack.filter(|ack| {
                    Some(ack.request) == pending
                        && ack.instance == acquisition.instance
                        && ack.acquisition == acquisition.epoch
                }) {
                    self.publisher
                        .commit_binding(ack.request, ack.acquisition, ack.first_frame)
                        .map_err(|error| error.to_string())?;
                    catalogue_changed = true;
                }
            }
            if let Some(stored) = self
                .capture_storage
                .iter_mut()
                .find(|(id, _, _)| *id == source.id)
            {
                if stored.1 != acquisition.format {
                    stored.1 = acquisition.format;
                    stored.2 = PreparedStorage::for_format(acquisition.format)
                        .map_err(|error| error.to_string())?;
                }
            } else {
                self.capture_storage.push((
                    source.id,
                    acquisition.format,
                    PreparedStorage::for_format(acquisition.format)
                        .map_err(|error| error.to_string())?,
                ));
            }
        }
        if catalogue_changed {
            let catalogue = self.publisher.catalogue();
            let now = self.now_ms();
            for slot in self.peers.values_mut() {
                slot.peer
                    .install_catalogue(&catalogue, now)
                    .map_err(peer_sentence)?;
            }
        }
        self.capture_interest();
        Ok(())
    }

    fn capture_interest(&self) {
        if let Some(capture) = &self.capture {
            for (index, handle) in capture.handles.iter().enumerate() {
                let Some(state) = &self.capture_states[index] else {
                    continue;
                };
                let demanded = self.config.group.joined
                    && self.peers.values().any(|slot| {
                        slot.allow_ticks && slot.peer.source_capture_requested(state.source)
                    });
                let interested = self.config.group.joined
                    && self
                        .peers
                        .values()
                        .any(|slot| slot.allow_ticks && slot.peer.source_send_ready(state.source));
                handle.set_interest(interested);
                handle.set_demand(demanded);
            }
        }
    }

    fn send_captured(&mut self) -> Result<(), String> {
        self.capture_budget_exhausted = false;
        for _ in 0..32 {
            let Some(block) = self.capture.as_ref().and_then(CaptureRuntime::next_block) else {
                return Ok(());
            };
            self.sync_capture()?;
            self.send_capture_block(block)?;
        }
        self.capture_budget_exhausted = true;
        Ok(())
    }

    fn send_capture_block(&mut self, block: CaptureBlock) -> Result<(), String> {
        if !self.config.group.joined || !self.network_ready() || Instant::now() > block.deadline {
            return Ok(());
        }
        let Some(state) = self
            .capture_states
            .iter()
            .flatten()
            .find(|state| state.source == block.packet.source_id)
        else {
            return Ok(());
        };
        let Some(acquisition) = state.acquisition.filter(|acquisition| {
            acquisition.instance == block.instance
                && acquisition.epoch == block.packet.stream_epoch
                && acquisition.native == block.packet.format
        }) else {
            return Ok(());
        };
        let Some(binding) = self.publisher.binding(state.source).filter(|binding| {
            binding.epoch() == block.request.epoch()
                && binding.acquisition() == acquisition.epoch
                && binding.format() == block.request.format()
        }) else {
            return Ok(());
        };
        if !self
            .peers
            .values()
            .any(|slot| slot.allow_ticks && slot.peer.source_send_ready(state.source))
        {
            return Ok(());
        }
        let publication_block_first = block
            .start_frame
            .checked_sub(binding.first_eligible_frame());
        let Some((_, format, storage)) = self
            .capture_storage
            .iter_mut()
            .find(|(id, _, _)| *id == state.source)
        else {
            return Ok(());
        };
        let samples = &block.samples[..block.frames * usize::from(format.channels())];
        let original = self
            .publisher
            .local_original(
                state.source,
                acquisition.epoch,
                block.start_frame,
                samples,
                block.enqueued_at_ms,
            )
            .map_err(|error| error.to_string())?;
        let prepared = self
            .publisher
            .prepare_pcm_into(original.with_ticket(block.ticket), storage)
            .map_err(|error| error.to_string())?;
        let mut datagram = [0u8; MAX_DATAGRAM_BYTES];
        for index in 0..prepared.len() {
            let Some(fragment) = prepared.get(index) else {
                continue;
            };
            for (member_id, slot) in &mut self.peers {
                if Instant::now() > block.deadline {
                    return Ok(());
                }
                if !slot.allow_ticks {
                    continue;
                }
                let written = match slot.peer.write_original_into(&fragment, &mut datagram) {
                    Ok(Some(written)) => written,
                    Ok(None) => continue,
                    Err(error) => {
                        slot.last_error = Some(peer_sentence(error));
                        continue;
                    }
                };
                if let Err(error) = send_on(&self.sockets, written.bytes, slot.path) {
                    self.counters.socket_errors = self.counters.socket_errors.saturating_add(1);
                    slot.last_error = Some(format!(
                        "An original audio packet could not be sent: {error}"
                    ));
                    if !recoverable_udp_error(&error) {
                        let message =
                            format!("The media socket cannot send original audio: {error}");
                        self.socket_error = Some(message.clone());
                        self.network.error = Some(message);
                        return Ok(());
                    }
                } else if self.return_timing.tracks(*member_id) {
                    // Both offsets are relative: native block within its packet,
                    // then publication fragment within that same native block.
                    let packet_frame_offset = block
                        .start_frame
                        .checked_sub(block.packet.start_sample)
                        .and_then(|block_offset| {
                            fragment
                                .start_frame()
                                .checked_sub(publication_block_first?)
                                .and_then(|fragment_offset| {
                                    block_offset.checked_add(fragment_offset)
                                })
                        });
                    self.return_timing.sent(
                        *member_id,
                        &fragment,
                        block.packet,
                        packet_frame_offset,
                        Instant::now(),
                    );
                }
            }
        }
        self.capture_error = None;
        Ok(())
    }

    fn synchronize_playback(&mut self) -> Result<(), String> {
        synchronize_playback_fields(
            &mut self.playback,
            self.config.group.joined,
            &self.peers,
            &self.config.contacts,
        )
    }

    fn receive_one(&mut self, buffer: &mut [u8]) -> Option<(usize, PeerPath)> {
        let count = self.sockets.len();
        for offset in 0..count {
            let at = (self.receive_cursor + offset) % count;
            let bound = &self.sockets[at];
            if bound.faulted.get() {
                continue;
            }
            match bound.socket.recv_from(buffer) {
                Ok((length, mut remote)) => {
                    if let SocketAddr::V6(address) = &mut remote {
                        if address.ip().is_unicast_link_local() && address.scope_id() == 0 {
                            if let Some(index) = bound_scope(bound) {
                                address.set_scope_id(index);
                            }
                        }
                    }
                    self.receive_cursor = (at + 1) % count;
                    return Some((
                        length,
                        PeerPath {
                            remote,
                            local: bound.actual,
                        },
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.raw_os_error() == Some(10040) => {
                    self.counters.rejected_malformed =
                        self.counters.rejected_malformed.saturating_add(1)
                }
                Err(error) if recoverable_udp_error(&error) => {
                    self.counters.socket_errors = self.counters.socket_errors.saturating_add(1)
                }
                Err(error) => {
                    self.counters.socket_errors = self.counters.socket_errors.saturating_add(1);
                    self.network.error =
                        Some(format!("A media socket cannot receive packets: {error}"));
                    bound.faulted.set(true);
                }
            }
        }
        None
    }

    fn receive_datagrams(&mut self) -> Result<(), String> {
        if !self.network_ready() {
            self.receive_budget_exhausted = false;
            return Ok(());
        }
        self.receive_budget_exhausted = true;
        let mut buffer = [0u8; MAX_DATAGRAM_BYTES + 1];
        for _ in 0..DATAGRAMS_PER_TURN {
            let Some((length, received_path)) = self.receive_one(&mut buffer) else {
                self.receive_budget_exhausted = false;
                break;
            };
            let source = received_path.remote;
            let received = Instant::now();
            if !self.config.group.joined || length > MAX_DATAGRAM_BYTES {
                self.counters.rejected_unconfigured =
                    self.counters.rejected_unconfigured.saturating_add(1);
                continue;
            }
            let route = match datagram_route(&buffer[..length]) {
                Ok(route) => route,
                Err(_) => {
                    self.counters.rejected_malformed =
                        self.counters.rejected_malformed.saturating_add(1);
                    continue;
                }
            };
            let Some(member_id) = select_datagram_member(
                self.config.member_id,
                source,
                route,
                &self.config.contacts,
                &self.peers,
            ) else {
                self.counters.rejected_unconfigured =
                    self.counters.rejected_unconfigured.saturating_add(1);
                continue;
            };
            if matches!(route, DatagramRoute::Media { .. })
                && self
                    .peers
                    .get(&member_id)
                    .is_none_or(|slot| slot.path != received_path)
            {
                self.counters.rejected_unconfigured =
                    self.counters.rejected_unconfigured.saturating_add(1);
                continue;
            }
            let operation = self.peer_operation();
            let before = self
                .peers
                .get(&member_id)
                .expect("selected peer exists")
                .peer
                .status();
            let received_qpc = if self.return_timing.tracks(member_id)
                && matches!(route, DatagramRoute::Media { .. })
            {
                rlb_audio_windows::wasapi_qpc_100ns_now()
            } else {
                None
            };
            let now = operation.now_ms;
            let ingested = {
                let slot = self.peers.get_mut(&member_id).expect("peer exists");
                slot.peer
                    .ingest_into(&buffer[..length], now, operation, &mut self.pcm_scratch)
            };
            let mut reply_path = None;
            if matches!(route, DatagramRoute::Control { .. }) {
                let slot = self
                    .peers
                    .get_mut(&member_id)
                    .expect("selected peer exists");
                if slot.peer.last_control_accepted() {
                    let after = slot.peer.status();
                    if after.connected && after.session != before.session {
                        slot.path = received_path;
                        slot.next_probe_at = Instant::now() + PATH_PROBE;
                        slot.probe_budget = 4;
                        slot.probe_cursor = 0;
                        if !slot.paths.contains(&received_path) {
                            if slot.paths.len() == MAX_PATHS {
                                slot.paths.pop();
                            }
                            slot.paths.push(received_path);
                        }
                    }
                    if let Some(pending) = slot.peer.pending_session() {
                        if slot.peer.last_control_session().is_none()
                            || slot.peer.last_control_session() == Some(pending)
                        {
                            slot.pending_path = Some((pending, received_path));
                        }
                    } else {
                        slot.pending_path = None;
                    }
                    if !before.connected && !after.connected && self.config.member_id > member_id {
                        slot.path = received_path;
                    }
                    let policy = policy_for_path(&self.config, member_id, slot.path.remote, after);
                    if let Err(error) = slot.peer.set_source_policy_with(policy, operation) {
                        slot.last_error = Some(peer_sentence(error));
                    }
                    reply_path = Some(received_path);
                }
            }
            // Split field borrows preserve the caller-owned PCM view while
            // synchronizing controls, including closes returned with an error.
            let admission_current = synchronize_playback_fields(
                &mut self.playback,
                self.config.group.joined,
                &self.peers,
                &self.config.contacts,
            )
            .is_ok();
            match ingested {
                Ok(Some(audio)) => {
                    self.return_timing.received(
                        self.config.member_id,
                        self.run_id,
                        audio.metadata,
                        received,
                        received_qpc,
                    );
                    if !admission_current
                        || self
                            .playback
                            .accept_remote(audio.metadata, audio.samples)
                            .is_err()
                    {
                        self.counters.media_unavailable =
                            self.counters.media_unavailable.saturating_add(1);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.counters.rejected_malformed =
                        self.counters.rejected_malformed.saturating_add(1);
                    if let Some(slot) = self.peers.get_mut(&member_id) {
                        slot.last_error = Some(peer_sentence(error));
                    }
                }
            }
            self.drain_peer_to(member_id, reply_path)?;
        }
        Ok(())
    }
}

fn same_contact_candidates(a: &SavedContact, b: &SavedContact) -> bool {
    std::iter::once(a.address)
        .chain(a.alternates.iter().copied())
        .all(|address| address == b.address || b.alternates.contains(&address))
        && std::iter::once(b.address)
            .chain(b.alternates.iter().copied())
            .all(|address| address == a.address || a.alternates.contains(&address))
}

fn bound_scope(bound: &BoundDatagram) -> Option<u32> {
    bound
        .plan
        .interface_index
        .filter(|index| *index != 0)
        .or_else(|| match bound.actual {
            SocketAddr::V6(address) => Some(address.scope_id()).filter(|index| *index != 0),
            _ => None,
        })
}

fn prefix_affinity(a: IpAddr, b: IpAddr) -> u32 {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => (u32::from(a) ^ u32::from(b)).leading_zeros(),
        (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a) ^ u128::from(b)).leading_zeros(),
        _ => 0,
    }
}

fn send_on(sockets: &[BoundDatagram], bytes: &[u8], path: PeerPath) -> io::Result<usize> {
    let bound = sockets
        .iter()
        .find(|bound| bound.actual == path.local && !bound.faulted.get())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "The selected local media socket is unavailable.",
            )
        })?;
    let result = bound.socket.send_to(bytes, path.remote);
    if result
        .as_ref()
        .err()
        .is_some_and(|error| !recoverable_udp_error(error))
    {
        bound.faulted.set(true);
    }
    result
}

fn closed_policy() -> rlb_peer::SourcePolicy {
    rlb_peer::SourcePolicy::all(Intent {
        send: false,
        receive: false,
    })
}

fn policy_for_path(
    config: &ParticipantConfig,
    member: MemberId,
    address: SocketAddr,
    status: PeerStatus,
) -> rlb_peer::SourcePolicy {
    let resolved = status
        .session
        .zip(status.remote_run)
        .map(|(session, remote_run)| config::ResolvedPath {
            address,
            session,
            remote_run,
        });
    if !status.connected
        || resolved.is_none()
        || config
            .contacts
            .iter()
            .find(|contact| contact.member_id == member)
            .and_then(|contact| contact.resolved_for)
            != resolved
    {
        return closed_policy();
    }
    config
        .member(member)
        .map(|member| {
            member.source_policy.clone().unwrap_or_else(|| {
                rlb_peer::SourcePolicy::all(Intent {
                    send: member.send,
                    receive: member.receive,
                })
            })
        })
        .unwrap_or_else(closed_policy)
}

/// Endpoint ownership and protocol identity must agree. Multiple logical
/// members may share an endpoint; an ambiguous session has no receiver.
fn select_datagram_member(
    local: MemberId,
    source: SocketAddr,
    route: DatagramRoute,
    contacts: &[SavedContact],
    peers: &HashMap<MemberId, PeerSlot>,
) -> Option<MemberId> {
    let mut selected = None;
    for contact in contacts {
        let Some(slot) = peers.get(&contact.member_id) else {
            continue;
        };
        if !slot.allow_ticks {
            continue;
        }
        let matches = match route {
            DatagramRoute::Control { from, to } => from == contact.member_id && to == local,
            DatagramRoute::Media { session } => {
                source == slot.path.remote && slot.peer.status().session == Some(session)
            }
        };
        if matches {
            if selected.is_some() {
                return None;
            }
            selected = Some(contact.member_id);
        }
    }
    selected
}

fn synchronize_playback_fields(
    playback: &mut PlaybackOwner,
    joined: bool,
    peers: &HashMap<MemberId, PeerSlot>,
    contacts: &[SavedContact],
) -> Result<(), String> {
    playback.synchronize(
        joined,
        peers
            .iter()
            .filter(|(member, _)| contacts.iter().any(|contact| contact.member_id == **member))
            .map(|(member, slot)| (*member, &slot.peer)),
    )
}

pub(crate) fn peer_sentence(error: PeerError) -> String {
    format!("The peer contact operation failed: {error}.")
}

fn publication_kind(kind: config::SourceKind) -> rlb_peer::SourceKind {
    match kind {
        config::SourceKind::System => rlb_peer::SourceKind::System,
        config::SourceKind::Microphone => rlb_peer::SourceKind::Microphone,
    }
}

pub(crate) fn recoverable_udp_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::TimedOut
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::PermissionDenied
    ) || matches!(
        error.raw_os_error(),
        // A path/family/permission/MTU/resource error does not destroy the
        // shared socket. Other peers may still use it successfully.
        Some(
            10013
                | 10022
                | 10040
                | 10047
                | 10049
                | 10050
                | 10051
                | 10052
                | 10053
                | 10054
                | 10055
                | 10060
                | 10064
                | 10065
        )
    )
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use rlb_model::DirectSessionId;

    fn member(byte: u8) -> MemberId {
        MemberId::from_bytes([byte; 16]).unwrap()
    }

    fn connected_peer(local: MemberId, remote: MemberId) -> Peer {
        let intent = Intent {
            send: true,
            receive: true,
        };
        let mut first = Peer::new(
            local,
            remote,
            RunId::from_bytes([7; 16]).unwrap(),
            intent,
            Catalogue::default(),
            0,
        )
        .unwrap();
        let mut second = Peer::new(
            remote,
            local,
            RunId::from_bytes([8; 16]).unwrap(),
            intent,
            Catalogue::default(),
            0,
        )
        .unwrap();
        for _ in 0..8 {
            while let Some(bytes) = first.take_control() {
                second.ingest(&bytes, 0, 0).unwrap();
            }
            while let Some(bytes) = second.take_control() {
                first.ingest(&bytes, 0, 0).unwrap();
            }
        }
        assert!(first.status().connected);
        first
    }

    #[test]
    fn shared_endpoint_dispatches_each_member_and_rejects_stale_sessions() {
        let local = member(1);
        let address: SocketAddr = "127.0.0.1:17110".parse().unwrap();
        let contacts: Vec<_> = [member(2), member(3)]
            .into_iter()
            .map(|member_id| SavedContact {
                member_id,
                address,
                alternates: Vec::new(),
                resolved_for: None,
            })
            .collect();
        let mut peers: HashMap<_, _> = contacts
            .iter()
            .map(|contact| {
                (
                    contact.member_id,
                    PeerSlot {
                        peer: connected_peer(local, contact.member_id),
                        path: PeerPath {
                            remote: address,
                            local: address,
                        },
                        paths: vec![PeerPath {
                            remote: address,
                            local: address,
                        }],
                        pending_path: None,
                        next_probe_at: Instant::now() + PATH_PROBE,
                        probe_cursor: 0,
                        probe_budget: 4,
                        allow_ticks: true,
                        last_error: None,
                    },
                )
            })
            .collect();

        for contact in &contacts {
            let id = contact.member_id;
            let control = DatagramRoute::Control {
                from: id,
                to: local,
            };
            assert_eq!(
                select_datagram_member(local, address, control, &contacts, &peers),
                Some(id)
            );
            let session = peers[&id].peer.status().session.unwrap();
            assert_eq!(
                select_datagram_member(
                    local,
                    address,
                    DatagramRoute::Media { session },
                    &contacts,
                    &peers,
                ),
                Some(id)
            );
            assert_eq!(
                select_datagram_member(
                    local,
                    "127.0.0.1:17111".parse().unwrap(),
                    control,
                    &contacts,
                    &peers,
                ),
                None
            );
        }

        for control in [
            DatagramRoute::Control {
                from: member(4),
                to: local,
            },
            DatagramRoute::Control {
                from: member(2),
                to: member(4),
            },
        ] {
            assert_eq!(
                select_datagram_member(local, address, control, &contacts, &peers),
                None
            );
        }
        let old_session = peers[&member(2)].peer.status().session.unwrap();
        peers
            .get_mut(&member(2))
            .unwrap()
            .peer
            .reconnect(1)
            .unwrap();
        for session in [old_session, DirectSessionId::from_bytes([9; 16]).unwrap()] {
            assert_eq!(
                select_datagram_member(
                    local,
                    address,
                    DatagramRoute::Media { session },
                    &contacts,
                    &peers,
                ),
                None
            );
        }
        peers.get_mut(&member(3)).unwrap().allow_ticks = false;
        assert_eq!(
            select_datagram_member(
                local,
                address,
                DatagramRoute::Control {
                    from: member(3),
                    to: local,
                },
                &contacts,
                &peers,
            ),
            None
        );
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::participant::config::SavedMember;

    #[test]
    fn native_handoff_binds_two_originals_crosses_udp_and_keeps_monitor_after_leave() {
        let mut ada = ParticipantConfig::create("Capture", "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut bea = ParticipantConfig::create("Listen", "127.0.0.1:0".parse().unwrap()).unwrap();
        for config in [&mut ada, &mut bea] {
            config.group.name = "Fixture room".into();
            config.group.joined = true;
        }
        for (index, source) in ada.sources.iter_mut().enumerate() {
            source.publish = true;
            source.monitor = true;
            source.endpoint_id = Some(format!("fixture-{index}"));
        }
        let mut a = MediaRuntime::start(ada.clone()).unwrap();
        let mut b = MediaRuntime::start(bea.clone()).unwrap();
        let aid = ada.member_id;
        let bid = bea.member_id;
        ada.members.push(SavedMember::new(bid, "Listen"));
        let mut receive = SavedMember::new(aid, "Capture");
        receive.receive = true;
        receive.send = false;
        bea.members.push(receive);
        ada.contacts.push(SavedContact {
            member_id: bid,
            address: b.handle.snapshot().local_address,
            alternates: Vec::new(),
            resolved_for: None,
        });
        bea.contacts.push(SavedContact {
            member_id: aid,
            address: a.handle.snapshot().local_address,
            alternates: Vec::new(),
            resolved_for: None,
        });
        a.handle.configure_in(&ada, Resume::Automatic, 1).unwrap();
        b.handle.configure_in(&bea, Resume::Automatic, 1).unwrap();
        a.handle
            .with_core(|core| core.playback.select_software_route());
        b.handle
            .with_core(|core| core.playback.select_software_route());
        let mut captures = a.handle.attach_capture_fixture();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && !a
                .handle
                .snapshot()
                .local_catalogue
                .as_slice()
                .iter()
                .all(|source| source.state == rlb_peer::PublicationState::Active)
        {
            for source in &mut captures {
                source.prepare().unwrap();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            a.handle
                .snapshot()
                .local_catalogue
                .as_slice()
                .iter()
                .all(|source| source.state == rlb_peer::PublicationState::Active),
            "silent live sources could not complete binding"
        );
        assert_eq!(
            a.handle
                .with_core(|core| core.tickets.watermark().allocated_through),
            0,
            "binding fabricated a capture block"
        );
        let mut received = false;
        while Instant::now() < deadline {
            captures[0].emit(&[0.25; 960]).unwrap();
            captures[1].emit(&[0.125; 441]).unwrap();
            std::thread::sleep(Duration::from_millis(2));
            received = b.handle.with_core(|core| {
                core.playback.admitted_timelines().len() == 2
                    && core
                        .playback
                        .admitted_closed()
                        .iter()
                        .all(|closed| *closed > 0)
            });
            if received {
                break;
            }
        }
        assert!(received, "capture originals did not reach the output owner");
        let state = a.handle.snapshot();
        assert!(ada.sources.iter().all(|source| {
            state.local_catalogue.source(source.id).unwrap().state
                == rlb_peer::PublicationState::Active
        }));
        let formats: Vec<_> = b.handle.snapshot().peers[&aid]
            .catalogue
            .as_ref()
            .unwrap()
            .as_slice()
            .iter()
            .filter_map(|source| {
                source
                    .format
                    .map(|format| (format.sample_rate_hz(), format.channels()))
            })
            .collect();
        assert!(formats.contains(&(48_000, 2)) && formats.contains(&(44_100, 1)));

        let local_source = ada.sources[0].id;
        let monitor = a.handle.with_core(move |core| {
            let state = core
                .capture_states
                .iter()
                .flatten()
                .find(|state| state.source == local_source)
                .unwrap();
            crate::participant::audio::render::TimelineId::Local {
                run: core.run_id,
                source: local_source,
                acquisition: state.acquisition.unwrap().epoch,
            }
        });
        let epoch = a.handle.submit_scope(RoomTransition::Leave, Ok).unwrap();
        ada.group.joined = false;
        a.handle
            .configure_in(&ada, Resume::Automatic, epoch)
            .unwrap();
        captures[0].emit(&[0.25; 960]).unwrap();
        assert!(a.handle.with_core(move |core| {
            core.playback
                .handle()
                .inspect(move |render| render.source_status(monitor).unwrap().active)
        }));
        assert!(!a.handle.snapshot().peers[&bid].status.send_ready);
        drop(captures);
        a.stop();
        b.stop();
    }
}
