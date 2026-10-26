//! Single-room contact service.
//!
//! One owner thread accepts control connections, reads framed JSON, and updates
//! the in-memory directory. The thread is independent of audio capture and
//! playback. Stopping it closes control sockets and emits service-stop
//! visibility. It does not emit member leave for that shutdown.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rlb_control::strict_json::StrictJsonError;
use rlb_model::{MemberId, RoomId};

use crate::directory::Directory;
use crate::error::ContactError;
use crate::framing::{
    ByteQueue, CONTACT_PREFACE, DecoderPhase, FrameDecoder, FrameError, MAX_SEND_QUEUE_BYTES,
};
use crate::media::resolve_media;
use crate::wire::{
    CONTACT_PROTOCOL_VERSION, ClientMessage, ContactCode, ContactLossCause, MemberContact,
    ReturnAction, ReturnState, RoomInfo, ServerMessage, decode_client, decode_server,
    encode_server, validate_display_name, validate_password, validate_room_name,
};
use crate::{DEFAULT_CONTACT_TIMEOUT, MAX_CONTROL_CONNECTIONS, MAX_MEMBERS};

const MAX_PENDING_RETURNS: usize = MAX_CONTROL_CONNECTIONS;

/// Return operation attributed by the directory owner to the current registration.
#[derive(Clone, Debug)]
pub struct ReturnRequest {
    /// Process-local request identity used only when responding.
    pub id: u64,
    /// Current member registered by the originating TCP connection.
    pub origin: MemberContact,
    /// Requested operation.
    pub action: ReturnAction,
}

/// Bounded asynchronous handoff to the optional return-participant owner.
pub struct ReturnRequests {
    requests: Receiver<ReturnRequest>,
    responses: SyncSender<(u64, ReturnState)>,
    wake: mpsc::Sender<()>,
}

impl ReturnRequests {
    /// Takes an available request without waiting on the contact directory.
    pub fn try_recv(&self) -> Option<ReturnRequest> {
        self.requests.try_recv().ok()
    }

    /// Queues a response without blocking. Retired connections discard late replies.
    pub fn respond(&self, request_id: u64, state: ReturnState) -> Result<(), ContactError> {
        // Check the same strict contract the receiving client will decode.
        let encoded = encode_server(&ServerMessage::Return {
            v: crate::ContactVersion,
            state: state.clone(),
        })?;
        decode_server(&encoded[4..])?;
        match self.responses.try_send((request_id, state)) {
            Ok(()) => {
                let _ = self.wake.send(());
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(ContactError::InvalidRequest(
                "The return response queue is full.",
            )),
            Err(TrySendError::Disconnected(_)) => Err(ContactError::Stopped),
        }
    }
}

struct PendingReturn {
    connection_id: u64,
    origin: MemberContact,
}

struct ReturnOwner {
    requests: SyncSender<ReturnRequest>,
    responses: Receiver<(u64, ReturnState)>,
    pending: BTreeMap<u64, PendingReturn>,
    next_id: u64,
}

impl ReturnOwner {
    fn submit(
        &mut self,
        connection_id: u64,
        origin: MemberContact,
        action: ReturnAction,
    ) -> Result<(), &'static str> {
        if self.pending.len() >= MAX_PENDING_RETURNS
            || self
                .pending
                .values()
                .any(|pending| pending.connection_id == connection_id)
        {
            return Err("A return operation is already pending; try again later.");
        }
        let id = self.next_id;
        let Some(next_id) = id.checked_add(1) else {
            return Err("The return request identity is exhausted.");
        };
        match self.requests.try_send(ReturnRequest {
            id,
            origin: origin.clone(),
            action,
        }) {
            Ok(()) => {
                self.next_id = next_id;
                self.pending.insert(
                    id,
                    PendingReturn {
                        connection_id,
                        origin,
                    },
                );
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err("The return request queue is full; try again later."),
            Err(TrySendError::Disconnected(_)) => {
                Err("The return participant owner is unavailable.")
            }
        }
    }

    fn collect(&mut self, connections: &mut [Conn], directory: &Directory) {
        self.pending.retain(|_, pending| {
            directory.owner(pending.origin.member_id) == Some(pending.connection_id)
                && directory.contact(pending.origin.member_id) == Some(&pending.origin)
                && connections.iter().any(|connection| {
                    connection.id == pending.connection_id
                        && connection.alive
                        && !connection.superseded
                        && !connection.close_after_flush
                })
        });
        while let Ok((id, state)) = self.responses.try_recv() {
            let Some(pending) = self.pending.remove(&id) else {
                continue;
            };
            if let Some(connection) = find_mut(connections, pending.connection_id) {
                enqueue(
                    connection,
                    &ServerMessage::Return {
                        v: crate::ContactVersion,
                        state,
                    },
                );
            }
        }
    }
}

fn return_failure(message: &'static str) -> ServerMessage {
    ServerMessage::Return {
        v: crate::ContactVersion,
        state: ReturnState {
            error: Some(message.to_owned()),
            ..ReturnState::default()
        },
    }
}

/// Local configuration for one room contact service.
///
/// Debug output redacts the password. A failed [`ContactService::start`] leaves
/// the caller's address and port unchanged.
pub struct ContactConfig {
    bind: SocketAddr,
    room_name: String,
    password: String,
    contact_timeout: Duration,
    max_members: usize,
    embedded_media: Option<MemberId>,
}

impl ContactConfig {
    /// Builds a configuration for an explicit bind address.
    ///
    /// Port 0 asks the operating system for an ephemeral port. The room name and
    /// password rules match the room profile limits: 1 to 80 name characters,
    /// and a password of at most 128 characters. Control characters are rejected.
    pub fn try_new(
        bind: SocketAddr,
        room_name: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, ContactError> {
        let room_name = room_name.into();
        let password = password.into();
        validate_room_name(&room_name).map_err(ContactError::InvalidConfig)?;
        validate_password(&password).map_err(ContactError::InvalidConfig)?;
        Ok(Self {
            bind,
            room_name,
            password,
            contact_timeout: DEFAULT_CONTACT_TIMEOUT,
            max_members: MAX_MEMBERS,
            embedded_media: None,
        })
    }

    /// Sets how long a silent connection remains registered.
    pub fn with_contact_timeout(mut self, timeout: Duration) -> Result<Self, ContactError> {
        if timeout.is_zero() {
            return Err(ContactError::InvalidConfig(
                "The contact timeout must be greater than zero.",
            ));
        }
        self.contact_timeout = timeout;
        Ok(self)
    }

    /// Sets the registration cap. The value is from 1 to [`MAX_MEMBERS`].
    pub fn with_max_members(mut self, max_members: usize) -> Result<Self, ContactError> {
        if max_members == 0 || max_members > MAX_MEMBERS {
            return Err(ContactError::InvalidConfig(
                "The member limit must be from 1 to 64.",
            ));
        }
        self.max_members = max_members;
        Ok(self)
    }

    /// A media participant co-located with this listener uses the listener's
    /// actual interface address as seen by each connected client. Use only
    /// when its UDP socket is wildcard-bound on this same host. It confers no
    /// room authority and changes no wire schema or remote member identity.
    pub fn with_embedded_media(mut self, member: MemberId) -> Self {
        self.embedded_media = Some(member);
        self
    }

    /// Returns the requested bind address.
    #[must_use]
    pub const fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// Returns the room display name.
    #[must_use]
    pub fn room_name(&self) -> &str {
        &self.room_name
    }

    /// Returns the configured room password for an embedded room participant.
    #[must_use]
    pub fn password(&self) -> &str {
        &self.password
    }
}

impl std::fmt::Debug for ContactConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContactConfig")
            .field("bind", &self.bind)
            .field("room_name", &self.room_name)
            .field("password", &"redacted")
            .field("contact_timeout", &self.contact_timeout)
            .field("max_members", &self.max_members)
            .finish()
    }
}

struct RoomContext {
    password: String,
    info: RoomInfo,
    max_members: usize,
    timeout: Duration,
    stopping: Arc<AtomicBool>,
    embedded_media: Option<MemberId>,
}

/// Running contact service for one room.
#[must_use = "dropping the service stops the room"]
pub struct ContactService {
    local_addr: SocketAddr,
    local_addrs: Vec<SocketAddr>,
    info: RoomInfo,
    shutdown: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    thread: Mutex<Option<JoinHandle<Result<(), ContactError>>>>,
}

impl ContactService {
    /// Binds the requested socket and starts the contact owner thread.
    ///
    /// The bound address is [`Self::local_addr`]. [`RoomInfo::port`] is that
    /// socket's port. On bind failure the error names `config.bind` and the
    /// function returns before publishing an instance.
    pub fn start(config: ContactConfig) -> Result<Self, ContactError> {
        Self::start_inner(config, None)
    }

    /// Use caller-constrained listeners with the same actual port. The room
    /// still has one directory owner and one instance identity.
    pub fn from_listeners(config: ContactConfig, listeners: Vec<TcpListener>) -> Result<Self, ContactError> {
        Self::start_listeners(config, listeners, None)
    }

    /// Starts the room with an asynchronous optional return-participant handoff.
    pub fn start_with_returns(
        config: ContactConfig,
    ) -> Result<(Self, ReturnRequests), ContactError> {
        let (requests_tx, requests_rx) = mpsc::sync_channel(MAX_PENDING_RETURNS);
        let (responses_tx, responses_rx) = mpsc::sync_channel(MAX_PENDING_RETURNS);
        let service = Self::start_inner(
            config,
            Some(ReturnOwner {
                requests: requests_tx,
                responses: responses_rx,
                pending: BTreeMap::new(),
                next_id: 1,
            }),
        )?;
        let requests = ReturnRequests {
            requests: requests_rx,
            responses: responses_tx,
            wake: service.wake.clone(),
        };
        Ok((service, requests))
    }

    fn start_inner(
        config: ContactConfig,
        returns: Option<ReturnOwner>,
    ) -> Result<Self, ContactError> {
        let requested = config.bind;
        let addresses = if requested.ip().is_unspecified() {
            vec!["0.0.0.0".parse().expect("IPv4 wildcard"), "::".parse().expect("IPv6 wildcard")]
        } else { vec![requested.ip()] };
        let mut listeners = Vec::new();
        let mut port = requested.port();
        let mut last = None;
        for ip in addresses {
            let address = SocketAddr::new(ip, port);
            let result = (|| -> std::io::Result<TcpListener> {
                let socket = socket2::Socket::new(socket2::Domain::for_address(address), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
                if address.is_ipv6() { socket.set_only_v6(true)?; }
                #[cfg(not(windows))]
                socket.set_reuse_address(true)?;
                socket.bind(&address.into())?;
                socket.listen(128)?;
                Ok(socket.into())
            })();
            match result {
                Ok(listener) => { port = listener.local_addr()?.port(); listeners.push(listener); }
                Err(error) => last = Some(error),
            }
        }
        if listeners.is_empty() {
            return Err(ContactError::Bind { address: requested, source: last.unwrap_or_else(|| std::io::Error::other("No listener address is available.")) });
        }
        Self::start_listeners(config, listeners, returns)
    }

    fn start_listeners(config: ContactConfig, listeners: Vec<TcpListener>, returns: Option<ReturnOwner>) -> Result<Self, ContactError> {
        let local_addr = listeners.first().ok_or(ContactError::InvalidConfig("At least one contact listener is required."))?.local_addr()?;
        let mut local_addrs = Vec::with_capacity(listeners.len());
        for listener in &listeners {
            let address = listener.local_addr()?;
            if address.port() != local_addr.port() { return Err(ContactError::InvalidConfig("Contact listeners must share one actual port.")); }
            listener.set_nonblocking(true)?;
            local_addrs.push(address);
        }
        let instance_id = RoomId::generate()?;
        let info = RoomInfo {
            protocol_version: CONTACT_PROTOCOL_VERSION,
            room_name: config.room_name,
            instance_id,
            port: local_addr.port(),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let (wake, wake_rx) = mpsc::channel();
        let room = RoomContext {
            password: config.password,
            info: info.clone(),
            max_members: config.max_members,
            timeout: config.contact_timeout,
            stopping: Arc::clone(&shutdown),
            embedded_media: config.embedded_media,
        };
        let thread = thread::Builder::new()
            .name("rlb-contact".to_owned())
            .spawn(move || owner_loop(listeners, wake_rx, room, returns))?;
        Ok(Self {
            local_addr,
            local_addrs,
            info,
            shutdown,
            wake,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Socket the operating system bound, including an ephemeral port.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn local_addrs(&self) -> &[SocketAddr] { &self.local_addrs }

    /// Public room facts for discovery and direct-address probes.
    #[must_use]
    pub fn room_info(&self) -> RoomInfo {
        self.info.clone()
    }

    /// Closes control sockets and waits for the owner thread.
    pub fn stop(self) -> Result<(), ContactError> {
        self.shutdown_join()
    }

    fn shutdown_join(&self) -> Result<(), ContactError> {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.wake.send(());
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            match handle.join() {
                Ok(result) => result,
                Err(_) => Err(ContactError::OwnerPanic),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for ContactService {
    fn drop(&mut self) {
        let _ = self.shutdown_join();
    }
}

fn owner_loop(
    listeners: Vec<TcpListener>,
    wake: mpsc::Receiver<()>,
    room: RoomContext,
    mut returns: Option<ReturnOwner>,
) -> Result<(), ContactError> {
    let mut connections = Vec::new();
    let mut directory = Directory::new();
    let mut next_id = 1_u64;
    let mut fatal = None;
    loop {
        if room.stopping.load(Ordering::SeqCst) {
            break;
        }
        for listener in &listeners {
            if let Err(error) = accept_available(listener, &mut connections, &mut next_id, room.embedded_media) {
                fatal = Some(error);
                break;
            }
        }
        if fatal.is_some() { break; }
        if room.stopping.load(Ordering::SeqCst) {
            break;
        }
        poll_all(&mut connections, &mut directory, &room, &mut returns);
        expire(
            &mut connections,
            &mut directory,
            room.timeout,
            Instant::now(),
        );
        settle(&mut connections, &mut directory);
        if let Some(returns) = &mut returns {
            returns.collect(&mut connections, &directory);
        }
        let failed = flush_all(&mut connections);
        for id in failed {
            publish_loss(
                &mut connections,
                &mut directory,
                id,
                ContactLossCause::ControlDisconnect,
            );
        }
        settle(&mut connections, &mut directory);
        connections.retain(|connection| connection.alive);
        if room.stopping.load(Ordering::SeqCst) {
            break;
        }
        match wake.recv_timeout(Duration::from_millis(5)) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(listeners);
    stop_clients(&mut connections);
    match fatal {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn accept_available(
    listener: &TcpListener,
    connections: &mut Vec<Conn>,
    next_id: &mut u64,
    embedded_media: Option<MemberId>,
) -> Result<(), ContactError> {
    loop {
        if connections.len() >= MAX_CONTROL_CONNECTIONS {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, remote)) => {
                if prepare_stream(&stream).is_err() {
                    continue;
                }
                let mut connection = Conn::new(*next_id, stream, remote);
                connection.embedded_media = embedded_media;
                connections.push(connection);
                *next_id = next_id.wrapping_add(1).max(1);
            }
            Err(error) if is_idle(&error) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                ) => {}
            Err(error) => return Err(ContactError::Io(error)),
        }
    }
}

fn prepare_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    Ok(())
}

fn poll_all(
    connections: &mut [Conn],
    directory: &mut Directory,
    room: &RoomContext,
    returns: &mut Option<ReturnOwner>,
) {
    for index in 0..connections.len() {
        if connections[index].alive {
            service_connection(connections, directory, room, index, returns);
        }
    }
}

fn service_connection(
    connections: &mut [Conn],
    directory: &mut Directory,
    room: &RoomContext,
    index: usize,
    returns: &mut Option<ReturnOwner>,
) {
    let read = connections[index].read_batch();
    let failure = match read {
        Ok(messages) => {
            for message in messages {
                let meta = connections[index].meta();
                let effects = decide(&meta, message, directory, room);
                apply(connections, effects, returns);
                settle(connections, directory);
            }
            None
        }
        Err(failure) => Some(failure),
    };
    if let Some(failure) = failure {
        fail_connection(connections, directory, index, failure);
        settle(connections, directory);
    }
}

fn fail_connection(
    connections: &mut [Conn],
    directory: &mut Directory,
    index: usize,
    failure: ReadFailure,
) {
    let id = connections[index].id;
    match failure {
        ReadFailure::Eof => {
            connections[index].alive = false;
            publish_loss(
                connections,
                directory,
                id,
                ContactLossCause::ControlDisconnect,
            );
        }
        ReadFailure::Protocol(error) => {
            if connections[index].preface_sent {
                enqueue(
                    &mut connections[index],
                    &ServerMessage::error(code_for_frame(&error)),
                );
                connections[index].close_after_flush = true;
            } else {
                connections[index].alive = false;
            }
            publish_loss(
                connections,
                directory,
                id,
                ContactLossCause::ControlDisconnect,
            );
        }
    }
}

fn code_for_frame(error: &FrameError) -> ContactCode {
    match error {
        FrameError::InvalidMediaCandidates(_) => ContactCode::InvalidMedia,
        FrameError::Json(StrictJsonError::ProtocolVersion) => ContactCode::UnsupportedVersion,
        _ => ContactCode::Malformed,
    }
}

fn expire(connections: &mut [Conn], directory: &mut Directory, timeout: Duration, now: Instant) {
    let expired: Vec<u64> = connections
        .iter()
        .filter(|connection| connection.alive && now.duration_since(connection.last_rx) >= timeout)
        .map(|connection| connection.id)
        .collect();
    for id in expired {
        if let Some(connection) = find_mut(connections, id) {
            connection.alive = false;
        }
        publish_loss(connections, directory, id, ContactLossCause::Timeout);
    }
}

fn publish_loss(
    connections: &mut [Conn],
    directory: &mut Directory,
    id: u64,
    cause: ContactLossCause,
) {
    if let Some(contact) = directory.remove_owner(id) {
        let message = ServerMessage::contact_only(&contact, cause);
        broadcast(connections, id, &message);
    }
}

fn settle(connections: &mut [Conn], directory: &mut Directory) {
    for _ in 0..MAX_CONTROL_CONNECTIONS {
        let overflowed: Vec<u64> = connections
            .iter()
            .filter(|connection| connection.overflow)
            .map(|connection| connection.id)
            .collect();
        if overflowed.is_empty() {
            return;
        }
        for id in overflowed {
            if let Some(connection) = find_mut(connections, id) {
                connection.overflow = false;
                connection.alive = false;
            }
            publish_loss(
                connections,
                directory,
                id,
                ContactLossCause::ControlDisconnect,
            );
        }
    }
}

fn flush_all(connections: &mut [Conn]) -> Vec<u64> {
    let mut failed = Vec::new();
    for connection in connections.iter_mut() {
        if connection
            .outbound
            .write_to(&mut connection.stream)
            .is_err()
        {
            connection.alive = false;
            failed.push(connection.id);
            continue;
        }
        if connection.close_after_flush && connection.outbound.is_empty() {
            connection.alive = false;
        }
    }
    failed
}

fn stop_clients(connections: &mut [Conn]) {
    let Ok(bytes) = encode_server(&ServerMessage::service_stop()) else {
        return;
    };
    for connection in connections {
        if connection.preface_sent {
            let _ = connection.outbound.push(&bytes);
            let _ = connection.stream.set_nonblocking(false);
            let _ = connection
                .stream
                .set_write_timeout(Some(Duration::from_millis(200)));
            let _ = connection.outbound.write_to(&mut connection.stream);
        }
        let _ = connection.stream.shutdown(Shutdown::Both);
    }
}

struct ConnMeta {
    id: u64,
    member_id: Option<MemberId>,
    superseded: bool,
    remote: SocketAddr,
}

struct JoinRequest {
    room_name: String,
    password: String,
    member_id: MemberId,
    run_id: rlb_model::RunId,
    display_name: String,
    media_port: u16,
    media_ip: Option<std::net::IpAddr>,
    media_scope_id: u32,
    media_alternates: Vec<crate::MediaSocket>,
}

enum Effect {
    Return(u64, MemberContact, ReturnAction),
    Send(u64, ServerMessage),
    Broadcast { except: u64, message: ServerMessage },
    SetJoined(u64, MemberId),
    ClearJoined(u64),
    Supersede(u64),
    CloseAfterFlush(u64),
}

fn decide(
    meta: &ConnMeta,
    message: ClientMessage,
    directory: &mut Directory,
    room: &RoomContext,
) -> Vec<Effect> {
    if room.stopping.load(Ordering::SeqCst) {
        return vec![send_error(meta.id, ContactCode::ShuttingDown)];
    }
    match message {
        ClientMessage::Return { action, .. } => {
            if meta.superseded {
                return vec![send_error(meta.id, ContactCode::Superseded)];
            }
            let origin = meta
                .member_id
                .filter(|member_id| directory.owner(*member_id) == Some(meta.id))
                .and_then(|member_id| directory.contact(member_id));
            match origin {
                Some(origin) => vec![Effect::Return(meta.id, origin.clone(), action)],
                None => vec![send_error(meta.id, ContactCode::NotJoined)],
            }
        }
        ClientMessage::Inspect { .. } => {
            vec![Effect::Send(meta.id, ServerMessage::room_info(&room.info))]
        }
        ClientMessage::Keepalive { .. } => vec![Effect::Send(meta.id, ServerMessage::ack())],
        ClientMessage::Snapshot { .. } => snapshot_effects(meta, directory, room),
        ClientMessage::Leave { .. } => leave_effects(meta, directory),
        ClientMessage::Join {
            room_name,
            password,
            member_id,
            run_id,
            display_name,
            media_port,
            media_ip,
            media_scope_id,
            media_alternates,
            ..
        } => join_effects(
            meta,
            directory,
            room,
            JoinRequest {
                room_name,
                password,
                member_id,
                run_id,
                display_name,
                media_port,
                media_ip,
                media_scope_id,
                media_alternates,
            },
        ),
    }
}

fn snapshot_effects(meta: &ConnMeta, directory: &Directory, room: &RoomContext) -> Vec<Effect> {
    if meta
        .member_id
        .is_none_or(|member_id| directory.owner(member_id) != Some(meta.id))
    {
        return vec![send_error(meta.id, ContactCode::NotJoined)];
    }
    vec![Effect::Send(
        meta.id,
        ServerMessage::snapshot(&room.info, directory.members()),
    )]
}

fn leave_effects(meta: &ConnMeta, directory: &mut Directory) -> Vec<Effect> {
    if meta.superseded {
        return vec![send_error(meta.id, ContactCode::Superseded)];
    }
    let Some(contact) = directory.remove_owner(meta.id) else {
        return vec![send_error(meta.id, ContactCode::NotJoined)];
    };
    vec![
        Effect::ClearJoined(meta.id),
        Effect::Broadcast {
            except: meta.id,
            message: ServerMessage::explicit_leave(&contact),
        },
        Effect::Send(meta.id, ServerMessage::goodbye()),
        Effect::CloseAfterFlush(meta.id),
    ]
}

fn join_effects(
    meta: &ConnMeta,
    directory: &mut Directory,
    room: &RoomContext,
    request: JoinRequest,
) -> Vec<Effect> {
    if meta.superseded {
        return vec![send_error(meta.id, ContactCode::Superseded)];
    }
    if validate_display_name(&request.display_name).is_err() {
        return vec![send_error(meta.id, ContactCode::InvalidName)];
    }
    let media = match resolve_media(
        meta.remote,
        request.media_port,
        request.media_ip,
        request.media_scope_id,
    ) {
        Ok(media) => media,
        Err(_) => return vec![send_error(meta.id, ContactCode::InvalidMedia)],
    };
    if request.room_name != room.info.room_name {
        return vec![send_error(meta.id, ContactCode::RoomNameMismatch)];
    }
    if request.password != room.password {
        return vec![send_error(meta.id, ContactCode::PasswordRejected)];
    }
    if meta
        .member_id
        .is_some_and(|current| current != request.member_id)
    {
        return vec![send_error(meta.id, ContactCode::AlreadyJoined)];
    }
    if !directory.contains(request.member_id) && directory.len() >= room.max_members {
        return vec![send_error(meta.id, ContactCode::RoomFull)];
    }
    let media_alternates = match crate::normalize_media_alternates(request.media_port, Some(media), &request.media_alternates) {
        Ok(alternates) => alternates,
        Err(_) => return vec![send_error(meta.id, ContactCode::InvalidMedia)],
    };
    let contact = MemberContact {
        member_id: request.member_id,
        run_id: request.run_id,
        display_name: request.display_name,
        media,
        media_alternates,
    };
    let previous = directory.insert(meta.id, contact.clone());
    let members = directory.members();
    let mut effects = Vec::new();
    if let Some(previous_owner) = previous {
        if previous_owner != meta.id {
            effects.push(Effect::Supersede(previous_owner));
        }
    }
    effects.push(Effect::SetJoined(meta.id, contact.member_id));
    effects.push(Effect::Broadcast {
        except: meta.id,
        message: ServerMessage::visible(contact.clone()),
    });
    effects.push(Effect::Send(
        meta.id,
        ServerMessage::joined(&room.info, contact, members),
    ));
    effects
}

fn send_error(id: u64, code: ContactCode) -> Effect {
    Effect::Send(id, ServerMessage::error(code))
}

fn apply(connections: &mut [Conn], effects: Vec<Effect>, returns: &mut Option<ReturnOwner>) {
    for effect in effects {
        match effect {
            Effect::Return(id, origin, action) => {
                let result = match returns {
                    Some(returns) => returns.submit(id, origin, action),
                    None => Err("Virtual return is not enabled on this contact service."),
                };
                if let Err(message) = result {
                    if let Some(connection) = find_mut(connections, id) {
                        enqueue(connection, &return_failure(message));
                    }
                }
            }
            Effect::Send(id, message) => {
                if let Some(connection) = find_mut(connections, id) {
                    enqueue(connection, &message);
                }
            }
            Effect::Broadcast { except, message } => broadcast(connections, except, &message),
            Effect::SetJoined(id, member_id) => {
                if let Some(connection) = find_mut(connections, id) {
                    connection.member_id = Some(member_id);
                }
            }
            Effect::ClearJoined(id) => {
                if let Some(connection) = find_mut(connections, id) {
                    connection.member_id = None;
                }
            }
            Effect::Supersede(id) => supersede(connections, id),
            Effect::CloseAfterFlush(id) => {
                if let Some(connection) = find_mut(connections, id) {
                    connection.close_after_flush = true;
                }
            }
        }
    }
}

fn supersede(connections: &mut [Conn], id: u64) {
    if let Some(connection) = find_mut(connections, id) {
        connection.superseded = true;
        connection.member_id = None;
        connection.close_after_flush = true;
        enqueue(connection, &ServerMessage::superseded());
    }
}

fn broadcast(connections: &mut [Conn], except: u64, message: &ServerMessage) {
    for connection in connections.iter_mut() {
        if connection.id == except
            || !connection.alive
            || connection.superseded
            || connection.member_id.is_none()
        {
            continue;
        }
        enqueue(connection, message);
    }
}

fn enqueue(connection: &mut Conn, message: &ServerMessage) {
    let mut projected = message.clone();
    if let (Some(member_id), Ok(local)) = (connection.embedded_media, connection.stream.local_addr()) {
        let project = |member: &mut MemberContact| {
            if member.member_id == member_id {
                let scope = match local { SocketAddr::V6(value) => value.scope_id(), _ => 0 };
                let offered_here = member.media.ip() == local.ip() || member.media_alternates.iter().any(|candidate|candidate.ip()==local.ip());
                if offered_here {
                if let Ok(media) = crate::MediaSocket::explicit(local.ip(), member.media.port(), scope) {
                    member.media = media;
                    member.media_alternates.retain(|candidate| *candidate != media);
                }
                }
            }
        };
        match &mut projected {
            ServerMessage::Joined { you, members, .. } => {
                project(you);
                for member in members { project(member); }
            }
            ServerMessage::Snapshot { members, .. } => {
                for member in members { project(member); }
            }
            ServerMessage::Presence { change: crate::PresenceChange::Visible { member }, .. } => project(member),
            _ => {}
        }
    }
    match encode_server(&projected) {
        Ok(bytes) => {
            if connection.outbound.push(&bytes).is_err() {
                connection.overflow = true;
            }
        }
        Err(_) => connection.overflow = true,
    }
}

fn find_mut(connections: &mut [Conn], id: u64) -> Option<&mut Conn> {
    connections
        .iter_mut()
        .find(|connection| connection.id == id)
}

struct Conn {
    id: u64,
    stream: TcpStream,
    remote: SocketAddr,
    decoder: FrameDecoder,
    outbound: ByteQueue,
    member_id: Option<MemberId>,
    superseded: bool,
    preface_sent: bool,
    last_rx: Instant,
    alive: bool,
    close_after_flush: bool,
    overflow: bool,
    embedded_media: Option<MemberId>,
}

impl Conn {
    fn new(id: u64, stream: TcpStream, remote: SocketAddr) -> Self {
        Self {
            id,
            stream,
            remote,
            decoder: FrameDecoder::new(),
            outbound: ByteQueue::with_limit(MAX_SEND_QUEUE_BYTES),
            member_id: None,
            superseded: false,
            preface_sent: false,
            last_rx: Instant::now(),
            alive: true,
            close_after_flush: false,
            overflow: false,
            embedded_media: None,
        }
    }

    fn meta(&self) -> ConnMeta {
        ConnMeta {
            id: self.id,
            member_id: self.member_id,
            superseded: self.superseded,
            remote: self.remote,
        }
    }

    fn read_batch(&mut self) -> Result<Vec<ClientMessage>, ReadFailure> {
        let mut buffer = [0_u8; 8192];
        for _ in 0..4 {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(ReadFailure::Eof),
                Ok(size) => {
                    self.last_rx = Instant::now();
                    let frames = self
                        .decoder
                        .feed(&buffer[..size])
                        .map_err(ReadFailure::Protocol)?;
                    self.note_preface();
                    let mut messages = Vec::with_capacity(frames.len());
                    for frame in frames {
                        messages.push(decode_client(&frame).map_err(ReadFailure::Protocol)?);
                    }
                    if !messages.is_empty() {
                        return Ok(messages);
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if is_idle(&error) => return Ok(Vec::new()),
                Err(_) => return Err(ReadFailure::Eof),
            }
        }
        Ok(Vec::new())
    }

    fn note_preface(&mut self) {
        if self.decoder.phase() == DecoderPhase::Frames && !self.preface_sent {
            if self.outbound.push(CONTACT_PREFACE).is_ok() {
                self.preface_sent = true;
            } else {
                self.overflow = true;
            }
        }
    }
}

enum ReadFailure {
    Eof,
    Protocol(FrameError),
}

fn is_idle(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn debug_and_invalid_config_omit_the_password() {
        let config = ContactConfig::try_new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            "Headphones",
            "contact-test-secret",
        )
        .unwrap();
        let text = format!("{config:?}");
        assert!(!text.contains("contact-test-secret"));
        assert!(text.contains("redacted"));
        let error = ContactConfig::try_new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            "",
            "contact-test-secret",
        )
        .unwrap_err();
        assert!(!error.to_string().contains("contact-test-secret"));
    }
}
