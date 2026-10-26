//! One UDP owner hosts independently identified member pairs. Cold TCP joins
//! cannot block this thread. PCM has no handoff queue or device on this host.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mio::{Events, Interest, Poll, Token, Waker};
use rlb_contact::{MemberContact, ReturnMapping, ReturnState};
use rlb_model::{MemberId, RunId, SourceId};
use rlb_peer::{
    AdmissionProjection, CaptureTicket, CaptureWatermark, Catalogue, DatagramRoute, Intent,
    MAX_DATAGRAM_BYTES, MAX_SOURCES, MemberRun, Peer, PeerOperation, PreparedStorage,
    PublicationRef, PublicationState, SourceKind, SourcePublisher, datagram_route,
};
use socket2::{Domain, Protocol, Socket, Type};

const COMMAND: Token = Token(0);
const SOCKET_BASE: usize = 1;
const BUDGET: usize = 256;
const PROBE_INTERVAL_MS: u64 = 250;
const PROBE_PATHS_PER_TURN: usize = 4;
pub(super) const MAX_RETURN_PATHS: usize =
    rlb_contact::MAX_MEDIA_CANDIDATES * rlb_contact::MAX_MEDIA_CANDIDATES;
type SharedState = Arc<Mutex<HashMap<MemberId, (RunId, ReturnState)>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReturnPath {
    pub(super) remote: SocketAddr,
    pub(super) local: SocketAddr,
}

pub(super) struct BindingSnapshot {
    pub(super) addresses: Vec<SocketAddr>,
    pub(super) warnings: Vec<String>,
    pub(super) needs_retry: bool,
}

pub(super) type SharedBindings = Arc<Mutex<BindingSnapshot>>;

struct DesiredBindings {
    addresses: Vec<SocketAddr>,
    warnings: Vec<String>,
}

struct BoundSocket {
    socket: mio::net::UdpSocket,
    address: SocketAddr,
    registered: bool,
}

fn bind_one(address: SocketAddr) -> io::Result<BoundSocket> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    let socket: std::net::UdpSocket = socket.into();
    let address = socket.local_addr()?;
    Ok(BoundSocket {
        socket: mio::net::UdpSocket::from_std(socket),
        address,
        registered: false,
    })
}

/// The socket's concrete source address is the data-path identity. Wildcard
/// configuration expands only during cold catalogue refresh, never per packet.
fn desired_bindings(requested: SocketAddr) -> Result<DesiredBindings, String> {
    if !requested.ip().is_unspecified() {
        return Ok(DesiredBindings {
            addresses: vec![requested],
            warnings: Vec::new(),
        });
    }
    let entries = if_addrs::get_if_addrs().map_err(|e| e.to_string())?;
    let mut ordinary = Vec::new();
    let mut loopbacks = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        if !entry.is_oper_up() {
            continue;
        }
        let ip = entry.ip();
        if ip.is_unspecified()
            || ip.is_multicast()
            || matches!(ip,std::net::IpAddr::V4(v)if v.is_broadcast())
        {
            continue;
        }
        let address = match ip {
            std::net::IpAddr::V6(ip) if (ip.segments()[0] & 0xffc0) == 0xfe80 => {
                let Some(index) = entry.index.filter(|index| *index != 0) else {
                    warnings
                        .push("A link-local return address has no local interface index.".into());
                    continue;
                };
                SocketAddr::V6(std::net::SocketAddrV6::new(ip, requested.port(), 0, index))
            }
            _ => SocketAddr::new(ip, requested.port()),
        };
        let target = if ip.is_loopback() {
            &mut loopbacks
        } else {
            &mut ordinary
        };
        if !target.contains(&address) {
            target.push(address);
        }
    }
    let order = |a: &SocketAddr, b: &SocketAddr| {
        (a.is_ipv4() != requested.is_ipv4(), a).cmp(&(b.is_ipv4() != requested.is_ipv4(), b))
    };
    ordinary.sort_by(order);
    loopbacks.sort_by(order);
    let mut reserved = Vec::new();
    for ipv4 in [requested.is_ipv4(), !requested.is_ipv4()] {
        if let Some(address) = loopbacks.iter().find(|address| address.is_ipv4() == ipv4) {
            reserved.push(*address);
        }
    }
    ordinary.extend(reserved);
    if ordinary.is_empty() {
        return Err("No local return source address is currently available.".into());
    }
    Ok(DesiredBindings {
        addresses: ordinary,
        warnings,
    })
}

fn cap_bindings(
    mut desired: DesiredBindings,
    current: &[BoundSocket],
    pins: &[SocketAddr],
) -> DesiredBindings {
    let mut selected = Vec::new();
    for address in pins
        .iter()
        .copied()
        .filter(|address| desired.addresses.contains(address))
    {
        if !selected.contains(&address) && selected.len() < rlb_contact::MAX_MEDIA_CANDIDATES {
            selected.push(address);
        }
    }
    for loopback in [false, true] {
        for ipv4 in [true, false] {
            if let Some(address) = desired
                .addresses
                .iter()
                .find(|address| address.is_ipv4() == ipv4 && address.ip().is_loopback() == loopback)
            {
                if !selected.contains(address) && selected.len() < rlb_contact::MAX_MEDIA_CANDIDATES
                {
                    selected.push(*address);
                }
            }
        }
    }
    for address in current
        .iter()
        .map(|socket| socket.address)
        .chain(desired.addresses.iter().copied())
    {
        if desired.addresses.contains(&address)
            && !selected.contains(&address)
            && selected.len() < rlb_contact::MAX_MEDIA_CANDIDATES
        {
            selected.push(address);
        }
    }
    if selected.len() < desired.addresses.len() {
        desired.warnings.push("Return source addresses exceed the sixteen-socket bound. Healthy paths were retained; use a concrete bind address to select another network.".into());
    }
    selected.sort_by_key(|address| {
        desired
            .addresses
            .iter()
            .position(|candidate| candidate == address)
            .unwrap_or(usize::MAX)
    });
    desired.addresses = selected;
    desired
}

fn initial_sockets(
    requested: SocketAddr,
    desired: &DesiredBindings,
) -> Result<(Vec<BoundSocket>, Vec<String>), String> {
    for attempt in 0..8 {
        let mut sockets = Vec::new();
        let mut failures = Vec::new();
        let mut port = requested.port();
        let mut retry = false;
        for candidate in &desired.addresses {
            let mut address = *candidate;
            address.set_port(port);
            match bind_one(address) {
                Ok(socket) => {
                    port = socket.address.port();
                    sockets.push(socket);
                }
                Err(error) => {
                    if requested.port() == 0
                        && !sockets.is_empty()
                        && error.kind() == io::ErrorKind::AddrInUse
                        && attempt < 7
                    {
                        retry = true;
                        break;
                    }
                    failures.push(format!("Return listener {address} is unavailable: {error}"));
                }
            }
        }
        if retry {
            continue;
        }
        if sockets.is_empty() {
            return Err(failures.join("; "));
        }
        return Ok((sockets, failures));
    }
    Err("The return owner could not allocate a shared source port.".into())
}

fn update_bindings(
    poll: &Poll,
    sockets: &mut Vec<BoundSocket>,
    desired: DesiredBindings,
    shared: &SharedBindings,
    pins: &[SocketAddr],
) -> Result<(), String> {
    let desired = cap_bindings(desired, sockets, pins);
    let mut retained = Vec::new();
    let mut warnings = desired.warnings;
    let mut failed = false;
    for mut socket in std::mem::take(sockets) {
        if desired.addresses.contains(&socket.address) {
            retained.push(socket);
        } else {
            let _ = poll.registry().deregister(&mut socket.socket);
        }
    }
    for address in &desired.addresses {
        if retained.iter().any(|socket| socket.address == *address) {
            continue;
        }
        match bind_one(*address) {
            Ok(socket) => retained.push(socket),
            Err(error) => {
                failed = true;
                warnings.push(format!("Return listener {address} is unavailable: {error}"));
            }
        }
    }
    retained.sort_by_key(|socket| {
        desired
            .addresses
            .iter()
            .position(|address| *address == socket.address)
            .unwrap_or(usize::MAX)
    });
    let mut ready = Vec::new();
    for mut socket in retained {
        let token = Token(SOCKET_BASE + ready.len());
        let result = if socket.registered {
            poll.registry()
                .reregister(&mut socket.socket, token, Interest::READABLE)
        } else {
            poll.registry()
                .register(&mut socket.socket, token, Interest::READABLE)
        };
        match result {
            Ok(()) => {
                socket.registered = true;
                ready.push(socket);
            }
            Err(error) => {
                failed = true;
                warnings.push(format!(
                    "Return listener {} could not register: {error}",
                    socket.address
                ));
                if socket.registered {
                    let _ = poll.registry().deregister(&mut socket.socket);
                }
            }
        }
    }
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = BindingSnapshot {
        addresses: ready.iter().map(|socket| socket.address).collect(),
        warnings,
        needs_retry: failed,
    };
    *sockets = ready;
    if sockets.is_empty() {
        return Err("No return source socket is currently usable.".into());
    }
    Ok(())
}

enum CommandBody {
    Add {
        origin: MemberContact,
        member: MemberContact,
        paths: Vec<ReturnPath>,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    RefreshBindings {
        desired: Result<DesiredBindings, String>,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    Remove(MemberId),
    Stop,
}

pub(super) struct MediaRuntime {
    address: SocketAddr,
    requested: SocketAddr,
    port: u16,
    bindings: SharedBindings,
    commands: mpsc::SyncSender<CommandBody>,
    wake: Arc<Waker>,
    states: SharedState,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl MediaRuntime {
    pub(super) fn start(bind: SocketAddr, local_scope: u32) -> Result<Self, String> {
        let mut poll = Poll::new().map_err(|e| e.to_string())?;
        let (desired, catalogue_failed) = match desired_bindings(bind) {
            Ok(desired) => (desired, false),
            Err(error) => {
                if !bind.ip().is_unspecified() {
                    return Err(error);
                }
                let ips = if bind.is_ipv4() {
                    ["127.0.0.1", "::1"]
                } else {
                    ["::1", "127.0.0.1"]
                };
                (
                    DesiredBindings {
                        addresses: ips
                            .into_iter()
                            .map(|ip| {
                                SocketAddr::new(ip.parse().expect("literal loopback"), bind.port())
                            })
                            .collect(),
                        warnings: vec![format!(
                            "Local return addresses are waiting for enumeration: {error}"
                        )],
                    },
                    true,
                )
            }
        };
        let desired = cap_bindings(desired, &[], &[]);
        let (mut sockets, failures) = initial_sockets(bind, &desired)?;
        let address = sockets[0].address;
        let mut warnings = desired.warnings;
        let needs_retry = catalogue_failed || !failures.is_empty();
        warnings.extend(failures);
        let bindings = Arc::new(Mutex::new(BindingSnapshot {
            addresses: sockets.iter().map(|socket| socket.address).collect(),
            warnings,
            needs_retry,
        }));
        for (index, socket) in sockets.iter_mut().enumerate() {
            poll.registry()
                .register(
                    &mut socket.socket,
                    Token(SOCKET_BASE + index),
                    Interest::READABLE,
                )
                .map_err(|e| e.to_string())?;
            socket.registered = true;
        }
        let wake = Arc::new(Waker::new(poll.registry(), COMMAND).map_err(|e| e.to_string())?);
        let (commands, rx) = mpsc::sync_channel(128);
        let states = Arc::new(Mutex::new(HashMap::new()));
        let shared = Arc::clone(&states);
        let shared_bindings = Arc::clone(&bindings);
        let thread = thread::Builder::new()
            .name("rlb-return-media".into())
            .spawn(move || run(&mut poll, sockets, rx, shared, shared_bindings, local_scope))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            address,
            requested: bind,
            port: address.port(),
            bindings,
            commands,
            wake,
            states,
            thread: Some(thread),
        })
    }
    pub(super) fn address(&self) -> SocketAddr {
        self.addresses().first().copied().unwrap_or(self.address)
    }
    pub(super) fn addresses(&self) -> Vec<SocketAddr> {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .addresses
            .clone()
    }
    pub(super) fn binding_handle(&self) -> SharedBindings {
        Arc::clone(&self.bindings)
    }
    pub(super) fn needs_network_retry(&self) -> bool {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .needs_retry
    }
    pub(super) fn refresh_network(&self) -> Result<(), String> {
        let mut requested = self.requested;
        requested.set_port(self.port);
        let desired = desired_bindings(requested);
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .try_send(CommandBody::RefreshBindings { desired, reply })
            .map_err(|e| e.to_string())?;
        self.wake.wake().map_err(|e| e.to_string())?;
        result
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| e.to_string())?
    }
    pub(super) fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
    pub(super) fn state(&self, owner: MemberId, run: RunId) -> ReturnState {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&owner)
            .filter(|(current, _)| *current == run)
            .map(|(_, state)| state.clone())
            .unwrap_or(ReturnState {
                member: None,
                mappings: Vec::new(),
                error: None,
            })
    }
    pub(super) fn add(
        &mut self,
        origin: MemberContact,
        member: MemberContact,
        paths: Vec<ReturnPath>,
    ) -> Result<(), String> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .try_send(CommandBody::Add {
                origin,
                member,
                paths,
                reply,
            })
            .map_err(|e| e.to_string())?;
        self.wake.wake().map_err(|e| e.to_string())?;
        result
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| e.to_string())?
    }
    pub(super) fn remove(&mut self, id: MemberId) {
        // Cold lifecycle commands may wait for capacity; PCM never does.
        let _ = self.commands.send(CommandBody::Remove(id));
        let _ = self.wake.wake();
    }
    pub(super) fn stop(&mut self) -> Result<(), String> {
        if let Some(thread) = self.thread.take() {
            let _ = self.commands.send(CommandBody::Stop);
            let _ = self.wake.wake();
            thread
                .join()
                .map_err(|_| "The return UDP owner panicked.".to_owned())?
        } else {
            Ok(())
        }
    }
}
impl Drop for MediaRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Slot {
    id: SourceId,
    kind: SourceKind,
    upstream: Option<SourceId>,
    storage: Option<PreparedStorage>,
}

struct VirtualPeer {
    origin: MemberContact,
    member: MemberContact,
    path: ReturnPath,
    probe: ReturnPath,
    pending_path: Option<(rlb_model::DirectSessionId, ReturnPath)>,
    candidates: Vec<ReturnPath>,
    probe_cursor: usize,
    next_probe_ms: u64,
    peer: Peer,
    publisher: SourcePublisher,
    slots: Vec<Slot>,
    admission_revision: u64,
    tickets: u64,
    state: ReturnState,
}

impl VirtualPeer {
    fn new(
        origin: MemberContact,
        member: MemberContact,
        candidates: Vec<ReturnPath>,
        now: u64,
    ) -> Result<Self, String> {
        let path = candidates
            .first()
            .copied()
            .ok_or("A virtual member needs a usable origin media candidate.")?;
        let publisher = SourcePublisher::new(member.run_id);
        let peer = Peer::new(
            member.member_id,
            origin.member_id,
            member.run_id,
            Intent {
                send: true,
                receive: true,
            },
            Catalogue::default(),
            now,
        )
        .map_err(|e| e.to_string())?;
        let state = ReturnState {
            member: Some(member.clone()),
            mappings: Vec::new(),
            error: None,
        };
        Ok(Self {
            origin,
            member,
            path,
            probe: path,
            pending_path: None,
            candidates,
            probe_cursor: 0,
            next_probe_ms: now.saturating_add(PROBE_INTERVAL_MS),
            peer,
            publisher,
            slots: Vec::new(),
            admission_revision: 0,
            tickets: 0,
            state,
        })
    }
    fn tick(&mut self, now: u64) -> Result<bool, String> {
        if !self.peer.is_paused() && !self.peer.status().connected && now >= self.next_probe_ms {
            self.peer
                .probe_contact(now)
                .map_err(|error| error.to_string())?;
            self.next_probe_ms = now.saturating_add(PROBE_INTERVAL_MS);
        }
        let connected = self.peer.status().connected;
        self.peer.tick(now).map_err(|error| error.to_string())?;
        if connected && !self.peer.status().connected {
            if !self.candidates.contains(&self.path) {
                if self.candidates.len() == MAX_RETURN_PATHS {
                    self.candidates.pop();
                }
                self.candidates.insert(0, self.path);
            }
            self.probe_cursor = self
                .candidates
                .iter()
                .position(|path| *path == self.path)
                .unwrap_or(0);
        }
        self.sync_sources(now)
    }
    fn sync_sources(&mut self, now: u64) -> Result<bool, String> {
        let mut view = AdmissionProjection::default();
        self.peer.project_admission(&mut view);
        if view.revision == self.admission_revision {
            return Ok(false);
        }
        self.admission_revision = view.revision;
        let current: Vec<_> = view
            .sources
            .iter()
            .flatten()
            .copied()
            .filter(|source| {
                view.remote_run == Some(self.origin.run_id)
                    && view.connected
                    && source.receive_ready
                    && source.entry.state == PublicationState::Active
            })
            .collect();
        for slot in &mut self.slots {
            if !current
                .iter()
                .any(|source| Some(source.entry.id) == slot.upstream)
            {
                if self.publisher.received_binding(slot.id).is_some() {
                    self.publisher
                        .mark_unavailable(slot.id)
                        .map_err(|e| e.to_string())?;
                }
                slot.upstream = None;
            }
        }
        for source in current {
            let entry = source.entry;
            let Some(format) = entry.format else {
                continue;
            };
            let position = self
                .slots
                .iter()
                .position(|slot| slot.upstream == Some(entry.id))
                .or_else(|| {
                    self.slots
                        .iter()
                        .position(|slot| slot.upstream.is_none() && slot.kind == entry.kind)
                });
            let position = match position {
                Some(position) => position,
                None => {
                    if self.slots.len() == MAX_SOURCES {
                        return Err("Virtual member source slots are full; restart this return participant.".into());
                    }
                    let id = SourceId::generate().map_err(|e| e.to_string())?;
                    self.publisher
                        .register(id, entry.kind, true)
                        .map_err(|e| e.to_string())?;
                    self.slots.push(Slot {
                        id,
                        kind: entry.kind,
                        upstream: None,
                        storage: None,
                    });
                    self.slots.len() - 1
                }
            };
            let slot = &mut self.slots[position];
            let input = PublicationRef {
                owner: MemberRun {
                    member: self.origin.member_id,
                    run: self.origin.run_id,
                },
                source: entry.id,
                epoch: entry.epoch,
            };
            let changed = self
                .publisher
                .received_binding(slot.id)
                .is_none_or(|binding| binding.input() != input || binding.format() != format);
            self.publisher
                .bind_received(slot.id, input, format)
                .map_err(|e| e.to_string())?;
            slot.upstream = Some(entry.id);
            if changed || slot.storage.is_none() {
                slot.storage =
                    Some(PreparedStorage::for_format(format).map_err(|e| e.to_string())?);
            }
        }
        self.peer
            .install_catalogue(&self.publisher.catalogue(), now)
            .map_err(|e| e.to_string())?;
        self.state.mappings = self
            .slots
            .iter()
            .filter_map(|slot| {
                let binding = self.publisher.received_binding(slot.id)?;
                let upstream = binding.input();
                Some(ReturnMapping {
                    source_id: slot.id,
                    epoch: binding.epoch().get(),
                    upstream_member_id: upstream.owner.member,
                    upstream_run_id: upstream.owner.run,
                    upstream_source_id: upstream.source,
                    upstream_epoch: upstream.epoch.get(),
                    input_first_frame: 0,
                    output_first_frame: 0,
                })
            })
            .collect();
        Ok(true)
    }
    fn ingest(
        &mut self,
        bytes: &[u8],
        now: u64,
        sockets: &[BoundSocket],
        scratch: &mut [f64],
        wire: &mut [u8],
    ) -> Result<bool, String> {
        let operation = PeerOperation {
            now_ms: now,
            watermark: CaptureWatermark {
                allocated_through: self.tickets,
            },
        };
        let audio = match self.peer.ingest_into(bytes, now, operation, scratch) {
            Ok(audio) => audio,
            Err(_) => return Ok(false), // Ordinary malformed/stale datagrams are dropped.
        };
        if let Some(audio) = audio {
            if audio.metadata.remote_run == self.origin.run_id {
                if let Some(slot) = self
                    .slots
                    .iter_mut()
                    .find(|slot| slot.upstream == Some(audio.metadata.entry.id))
                {
                    if let (Some(binding), Some(storage)) =
                        (self.publisher.received_binding(slot.id), &mut slot.storage)
                    {
                        self.tickets = self
                            .tickets
                            .checked_add(1)
                            .ok_or("Return handoff tickets exhausted.")?;
                        let ticket =
                            CaptureTicket::new(self.tickets).ok_or("Invalid return ticket.")?;
                        // This is the same synchronous owner turn as admission;
                        // upstream consent cannot change between decode and send.
                        let prepared = self
                            .publisher
                            .prepare_received_into(binding, audio, ticket, storage)
                            .map_err(|e| e.to_string())?;
                        for index in 0..prepared.len() {
                            let fragment = prepared.get(index).expect("prepared fragment exists");
                            if let Some(packet) = self
                                .peer
                                .write_original_into(&fragment, wire)
                                .map_err(|e| e.to_string())?
                            {
                                send(sockets, packet.bytes, self.path)?;
                            }
                        }
                    }
                }
            }
        }
        self.sync_sources(now)
    }
}

fn send(sockets: &[BoundSocket], bytes: &[u8], path: ReturnPath) -> Result<(), String> {
    let socket = sockets
        .iter()
        .find(|socket| socket.address == path.local)
        .ok_or("The selected return source socket has retired.")?;
    match socket.socket.send_to(bytes, path.remote) {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock
                    | io::ErrorKind::Interrupted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
                    | io::ErrorKind::AddrNotAvailable
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn publish_state(states: &SharedState, id: MemberId, peer: &VirtualPeer) {
    states
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, (peer.origin.run_id, peer.state.clone()));
}

fn source_matches(member: &MemberContact, address: SocketAddr) -> bool {
    member
        .media_candidates()
        .any(|candidate| candidate.ip() == address.ip() && candidate.port() == address.port())
}

fn local_matches(member: &MemberContact, address: SocketAddr) -> bool {
    member.media_candidates().any(|candidate| {
        candidate.ip() == address.ip()
            && candidate.port() == address.port()
            && match address {
                SocketAddr::V6(value) if (value.ip().segments()[0] & 0xffc0) == 0xfe80 => {
                    candidate.scope_id() == value.scope_id()
                }
                _ => true,
            }
    })
}

/// Active-session controls, including duplicate Ready, use only the proven
/// data pair. Hello and the one pending session use their accepted probe pair.
fn drain_controls(
    peer: &mut VirtualPeer,
    sockets: &[BoundSocket],
    reply: Option<ReturnPath>,
) -> Result<(), String> {
    let mut hello = None;
    let mut last_error = None;
    let mut succeeded = false;
    while let Some(packet) = peer.peer.take_control_routed() {
        if packet.session.is_none() {
            hello = Some(packet.bytes);
            continue;
        }
        let active = peer.peer.status().session;
        let pending = peer.peer.pending_session();
        let path = if packet.session.is_some() && packet.session == active {
            peer.path
        } else if packet.session.is_some() && packet.session == pending {
            reply
                .or_else(|| {
                    peer.pending_path
                        .filter(|(session, _)| Some(*session) == pending)
                        .map(|(_, path)| path)
                })
                .unwrap_or(peer.probe)
        } else {
            continue;
        };
        match send(sockets, &packet.bytes, path) {
            Ok(()) => succeeded = true,
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(bytes) = hello {
        if let Some(reply) = reply {
            match send(sockets, &bytes, reply) {
                Ok(()) => succeeded = true,
                Err(error) => last_error = Some(error),
            }
        } else if peer.peer.status().connected {
            match send(sockets, &bytes, peer.path) {
                Ok(()) => succeeded = true,
                Err(error) => last_error = Some(error),
            }
        } else if !peer.candidates.is_empty() {
            for _ in 0..peer.candidates.len().min(PROBE_PATHS_PER_TURN) {
                let path = peer.candidates[peer.probe_cursor % peer.candidates.len()];
                peer.probe_cursor = (peer.probe_cursor + 1) % peer.candidates.len();
                match send(sockets, &bytes, path) {
                    Ok(()) => succeeded = true,
                    Err(error) => last_error = Some(error),
                }
            }
        }
    }
    if !succeeded {
        if let Some(error) = last_error {
            return Err(error);
        }
    }
    Ok(())
}

fn run(
    poll: &mut Poll,
    mut sockets: Vec<BoundSocket>,
    commands: mpsc::Receiver<CommandBody>,
    states: SharedState,
    bindings: SharedBindings,
    local_scope: u32,
) -> Result<(), String> {
    let origin = Instant::now();
    let mut peers = HashMap::<MemberId, VirtualPeer>::new();
    let mut events = Events::with_capacity(32);
    let mut input = [0_u8; MAX_DATAGRAM_BYTES + 1];
    let mut output = [0_u8; MAX_DATAGRAM_BYTES];
    let mut pcm = [0_f64; MAX_DATAGRAM_BYTES / 4];
    let mut receive_more = vec![false; sockets.len()];
    let mut next_socket = 0;
    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                CommandBody::Stop => return Ok(()),
                CommandBody::Remove(id) => {
                    peers.remove(&id);
                    states.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                }
                CommandBody::RefreshBindings { desired, reply } => {
                    let mut pins = Vec::new();
                    for peer in peers.values() {
                        pins.push(if peer.peer.status().connected {
                            peer.path.local
                        } else {
                            peer.probe.local
                        });
                        if let Some((_, path)) = peer.pending_path {
                            pins.push(path.local);
                        }
                    }
                    let result = match desired {
                        Ok(desired) => {
                            update_bindings(poll, &mut sockets, desired, &bindings, &pins)
                        }
                        Err(error) => {
                            let mut state = bindings.lock().unwrap_or_else(|e| e.into_inner());
                            state.needs_retry = true;
                            state.warnings =
                                vec![format!("Return source enumeration is unavailable: {error}")];
                            Err(error)
                        }
                    };
                    if let Err(error) = &result {
                        // A registry failure may have retired the old handles.
                        // Publish actual ownership and retry, never stale IPs.
                        let mut state = bindings.lock().unwrap_or_else(|e| e.into_inner());
                        state.addresses = sockets.iter().map(|socket| socket.address).collect();
                        state.needs_retry = true;
                        let detail = format!("Return source refresh failed: {error}");
                        if !state.warnings.contains(&detail) {
                            state.warnings.push(detail);
                        }
                    }
                    receive_more = vec![true; sockets.len()];
                    next_socket = 0;
                    let now = origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    for (&id, peer) in &mut peers {
                        let had_paths = !peer.candidates.is_empty();
                        peer.candidates.retain(|path| {
                            sockets.iter().any(|socket| socket.address == path.local)
                        });
                        if !sockets
                            .iter()
                            .any(|socket| socket.address == peer.path.local)
                        {
                            if let Some(path) = peer.candidates.first().copied() {
                                peer.probe = path;
                                peer.path = path;
                            }
                            peer.pending_path = None;
                            if had_paths
                                || peer.peer.status().connected
                                || peer.peer.pending_session().is_some()
                            {
                                if let Err(error) = peer.peer.reconnect(now) {
                                    peer.peer.pause();
                                    peer.state.error =
                                        Some(super::bounded_error(error.to_string()));
                                }
                            }
                            publish_state(&states, id, peer);
                        }
                    }
                    let _ = reply.send(result);
                }
                CommandBody::Add {
                    origin: remote,
                    member,
                    paths,
                    reply,
                } => {
                    let now = origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    let key = remote.member_id;
                    let result = (|| -> Result<(), String> {
                        if paths.is_empty()
                            || paths.len() > MAX_RETURN_PATHS
                            || paths.iter().any(|path| {
                                path.remote.port() != remote.media.port()
                                    || !sockets.iter().any(|socket| socket.address == path.local)
                                    || !source_matches(&remote, path.remote)
                                    || !local_matches(&member, path.local)
                            })
                        {
                            return Err("The origin path set is incompatible with the return source sockets.".into());
                        }
                        if let Some(existing) = peers.get_mut(&key).filter(|peer| {
                            peer.origin.run_id == remote.run_id
                                && peer.member.member_id == member.member_id
                                && peer.member.run_id == member.run_id
                        }) {
                            let was_waiting = existing.candidates.is_empty();
                            let changed_local = existing.member.media != member.media
                                || existing.member.media_alternates != member.media_alternates;
                            let keeps = source_matches(&remote, existing.path.remote)
                                && local_matches(&member, existing.path.local)
                                && sockets
                                    .iter()
                                    .any(|socket| socket.address == existing.path.local);
                            existing.origin = remote;
                            existing.member = member;
                            existing.candidates = paths;
                            existing.state.member = Some(existing.member.clone());
                            if !keeps {
                                existing.probe = existing.candidates[0];
                                existing.path = existing.probe;
                                existing.pending_path = None;
                                if !was_waiting {
                                    existing.peer.reconnect(now).map_err(|e| e.to_string())?;
                                }
                            } else if changed_local {
                                existing
                                    .peer
                                    .announce_addresses_changed(now)
                                    .map_err(|e| e.to_string())?;
                            }
                            if !existing.candidates.contains(&existing.probe) {
                                existing.probe = existing.candidates[0];
                            }
                            existing.next_probe_ms = now.saturating_add(PROBE_INTERVAL_MS);
                            publish_state(&states, key, existing);
                            return Ok(());
                        }
                        let peer = VirtualPeer::new(remote, member, paths, now)?;
                        publish_state(&states, key, &peer);
                        peers.insert(key, peer);
                        Ok(())
                    })();
                    let _ = reply.send(result);
                }
            }
        }
        let now = origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        for (&id, peer) in &mut peers {
            if peer.candidates.is_empty() {
                continue;
            }
            match peer.tick(now) {
                Ok(true) => publish_state(&states, id, peer),
                Ok(false) => {}
                Err(error) => {
                    peer.peer.pause();
                    peer.state.error = Some(super::bounded_error(error));
                    publish_state(&states, id, peer);
                }
            }
            if let Err(error) = drain_controls(peer, &sockets, None) {
                peer.state.error = Some(super::bounded_error(error));
                publish_state(&states, id, peer);
            }
        }
        let timeout = if receive_more.iter().any(|value| *value) {
            Duration::ZERO
        } else {
            peers
                .values()
                .filter(|peer| !peer.candidates.is_empty())
                .map(|peer| {
                    let mut deadline = peer.peer.next_deadline_ms();
                    if !peer.peer.is_paused() && !peer.peer.status().connected {
                        deadline = deadline.min(peer.next_probe_ms);
                    }
                    Duration::from_millis(deadline.saturating_sub(now))
                })
                .min()
                .unwrap_or(Duration::from_secs(1))
        };
        match poll.poll(&mut events, Some(timeout)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
        for event in events.iter() {
            if let Some(index) = event
                .token()
                .0
                .checked_sub(SOCKET_BASE)
                .filter(|index| *index < sockets.len())
            {
                receive_more[index] = true;
            }
        }
        if sockets.is_empty() {
            continue;
        }
        for turn in 0..sockets.len() {
            let index = (next_socket + turn) % sockets.len();
            if !receive_more[index] {
                continue;
            }
            for _ in 0..(BUDGET / sockets.len()).max(1) {
                let (length, mut address) = match sockets[index].socket.recv_from(&mut input) {
                    Ok(packet) => packet,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        receive_more[index] = false;
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                        ) || error.raw_os_error() == Some(10040) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                };
                if let SocketAddr::V6(source) = &mut address {
                    if (source.ip().segments()[0] & 0xffc0) == 0xfe80 {
                        if source.scope_id() == 0 {
                            let fallback = if local_scope != 0 {
                                local_scope
                            } else {
                                match sockets[index].address {
                                    SocketAddr::V6(local) => local.scope_id(),
                                    _ => 0,
                                }
                            };
                            if fallback == 0 {
                                continue;
                            }
                            source.set_scope_id(fallback);
                        }
                        if local_scope != 0 && source.scope_id() != local_scope {
                            continue;
                        }
                    }
                }
                let received = ReturnPath {
                    remote: address,
                    local: sockets[index].address,
                };
                let Ok(route) = datagram_route(&input[..length]) else {
                    continue;
                };
                let mut candidates = peers
                    .iter()
                    .filter(|(_, peer)| {
                        if peer.peer.is_paused() || !local_matches(&peer.member, received.local) {
                            return false;
                        }
                        match route {
                            DatagramRoute::Control { from, to } => {
                                source_matches(&peer.origin, address)
                                    && from == peer.origin.member_id
                                    && to == peer.member.member_id
                            }
                            DatagramRoute::Media { session } => {
                                peer.path == received && peer.peer.status().session == Some(session)
                            }
                        }
                    })
                    .map(|(&id, _)| id);
                let Some(id) = candidates.next() else {
                    continue;
                };
                if candidates.next().is_some() {
                    continue;
                }
                let peer = peers.get_mut(&id).expect("selected virtual peer exists");
                let before_session = peer.peer.status().session;
                let now = origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                let mut reply = None;
                match peer.ingest(&input[..length], now, &sockets, &mut pcm, &mut output) {
                    Ok(changed) => {
                        let mut changed = changed;
                        if matches!(route, DatagramRoute::Control { .. })
                            && peer.peer.last_control_accepted()
                        {
                            let active = peer.peer.status().session;
                            let pending = peer.peer.pending_session();
                            if active.is_some() && active != before_session {
                                if !peer.candidates.contains(&received) {
                                    if peer.candidates.len() == MAX_RETURN_PATHS {
                                        peer.candidates.pop();
                                    }
                                    peer.candidates.insert(0, received);
                                }
                                peer.path = received;
                                peer.probe = received;
                                peer.next_probe_ms = now.saturating_add(PROBE_INTERVAL_MS);
                            }
                            if let Some(session) = pending {
                                if peer.peer.last_control_session().is_none()
                                    || peer.peer.last_control_session() == Some(session)
                                {
                                    peer.pending_path = Some((session, received));
                                    reply = Some(received);
                                }
                            } else {
                                peer.pending_path = None;
                            }
                            if peer.candidates.is_empty() {
                                peer.candidates.push(received);
                                peer.probe_cursor = 0;
                            }
                            if peer.peer.last_control_session().is_none() {
                                reply = Some(received);
                                if !peer.peer.status().connected {
                                    peer.probe = received;
                                }
                            }
                            if peer.state.error.take().is_some() {
                                changed = true;
                            }
                        }
                        if changed {
                            publish_state(&states, id, peer);
                        }
                    }
                    Err(error) => {
                        peer.peer.pause();
                        peer.state.error = Some(super::bounded_error(error));
                        publish_state(&states, id, peer);
                    }
                }
                if let Err(error) = drain_controls(peer, &sockets, reply) {
                    peer.state.error = Some(super::bounded_error(error));
                    publish_state(&states, id, peer);
                }
            }
        }
        next_socket = (next_socket + 1) % sockets.len();
    }
}
