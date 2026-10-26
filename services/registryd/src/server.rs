//! TCP 17099 + UDP 17099 assembly around `RegistryAuthority`.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rlb_control::Version2;
use rlb_model::{
    AdmissionState, Endpoint, ErrorCode, MemberId, RegistrySessionId, RequestId, RoomId,
};
use rlb_registry::{
    BIND_MEDIA_BYTES, BIND_TIMEOUT_MS, BindMedia, ClaimSecret, ClientMessage, DecimalU64,
    DecoderPhase, EngineHello, FrameDecoder, HEARTBEAT_MS, LEASE_MS, MemberToken, RLBREG2_PREFACE,
    RegistryAuthority, RegistryErrorScope, RoomAction, ServerMessage, SessionError, SourceSlot,
    TokenDigest, encode_json_frame, parse_client_message, reduce_room,
};

use crate::persist::{self, PersistError, commit_transition};
use crate::protocol::{error_message, now_ms, scope_for};

/// Listen address, state directory, and first-install policy.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    pub auto_approve: bool,
    pub room_name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 17_099)),
            state_dir: PathBuf::from("registry-state"),
            auto_approve: false,
            room_name: "家庭房间".to_owned(),
        }
    }
}

struct Inner {
    authority: RegistryAuthority,
    state_path: PathBuf,
    auto_approve: bool,
    outbound: HashMap<RegistrySessionId, VecDeque<ServerMessage>>,
}

/// Running single-room registry process.
pub struct RegistryService {
    local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    inner: Arc<Mutex<Inner>>,
    join: Option<JoinHandle<io::Result<()>>>,
}

impl RegistryService {
    /// Binds TCP and UDP on the same port and starts the accept loop.
    pub fn start(config: ServerConfig) -> io::Result<Self> {
        let tcp = TcpListener::bind(config.listen)?;
        tcp.set_nonblocking(true)?;
        let local_addr = tcp.local_addr()?;
        let udp = UdpSocket::bind(local_addr)?;
        udp.set_nonblocking(true)?;
        let (authority, state_path) =
            persist::load_or_create(&config.state_dir, &config.room_name).map_err(persist_io)?;
        let inner = Arc::new(Mutex::new(Inner {
            authority,
            state_path,
            auto_approve: config.auto_approve,
            outbound: HashMap::new(),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_inner = Arc::clone(&inner);
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::spawn(move || run_loop(tcp, udp, thread_inner, thread_shutdown));
        Ok(Self {
            local_addr,
            shutdown,
            inner,
            join: Some(join),
        })
    }

    /// Bound TCP and UDP address, including an ephemeral port.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stable room identity from the live authority.
    pub fn room_id(&self) -> RoomId {
        self.inner
            .lock()
            .expect("registry lock")
            .authority
            .room()
            .room
            .room_id
    }

    /// Stops the accept loop and waits for the I/O thread.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            match join.join() {
                Ok(result) => result,
                Err(_) => Err(io::Error::other("registry accept thread panicked")),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for RegistryService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

fn persist_io(error: PersistError) -> io::Error {
    io::Error::other(error.to_string())
}

fn run_loop(
    tcp: TcpListener,
    udp: UdpSocket,
    inner: Arc<Mutex<Inner>>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut datagram = [0_u8; 2048];
    while !shutdown.load(Ordering::Relaxed) {
        match tcp.accept() {
            Ok((stream, _)) => {
                let session_inner = Arc::clone(&inner);
                thread::spawn(move || {
                    if let Err(error) = handle_tcp(stream, session_inner) {
                        eprintln!("registry tcp session: {error}");
                    }
                });
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }

        match udp.recv_from(&mut datagram) {
            Ok((length, from)) => {
                if let Err(error) = handle_udp(&udp, &datagram[..length], from, &inner) {
                    eprintln!("registry udp: {error}");
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }

        {
            let mut guard = inner.lock().expect("registry lock");
            let _ = guard.authority.expire_leases(now_ms());
            let _ = guard.authority.expire_member_commands(now_ms());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn handle_udp(
    socket: &UdpSocket,
    packet: &[u8],
    from: SocketAddr,
    inner: &Arc<Mutex<Inner>>,
) -> io::Result<()> {
    if packet.len() != BIND_MEDIA_BYTES {
        return Ok(());
    }
    let bind = match BindMedia::decode(packet) {
        Ok(bind) => bind,
        Err(_) => return Ok(()),
    };
    let std::net::IpAddr::V4(ip) = from.ip() else {
        return Ok(());
    };
    let observed = match Endpoint::new(ip, from.port()) {
        Ok(endpoint) => endpoint,
        Err(_) => return Ok(()),
    };
    let mut guard = inner.lock().expect("registry lock");
    let outcome = match guard.authority.bind_media(bind, observed, now_ms()) {
        Ok(outcome) => outcome,
        Err(_) => return Ok(()),
    };
    socket.send_to(&outcome.reply.encode(), from)?;
    let media_bound = ServerMessage::MediaBound {
        v: Version2,
        server_generation: guard.authority.server_generation(),
        registry_session_id: outcome.reply.registry_session_id,
        member_id: outcome.member_id,
        run_id: outcome.run_id,
        observed_media_ipv4: outcome.reply.observed_endpoint.ip(),
        observed_media_port: outcome.reply.observed_endpoint.port(),
        room_revision: guard.authority.room().room.room_revision,
        presence_revision: outcome.presence_revision,
    };
    if let Some(queue) = guard.outbound.get_mut(&outcome.reply.registry_session_id) {
        queue.push_back(media_bound);
    }
    Ok(())
}

fn handle_tcp(mut stream: TcpStream, inner: Arc<Mutex<Inner>>) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let length = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => length,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        match decoder.feed(&buffer[..length]) {
            Ok(frames) if decoder.phase() == DecoderPhase::Frames => {
                stream.write_all(RLBREG2_PREFACE)?;
                stream.flush()?;
                return after_preface(stream, decoder, frames, inner);
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("registry preface: {error}");
                return Ok(());
            }
        }
    }
}

fn after_preface(
    mut stream: TcpStream,
    mut decoder: FrameDecoder,
    first_frames: Vec<Vec<u8>>,
    inner: Arc<Mutex<Inner>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut pending = first_frames;
    if pending.is_empty() {
        pending = read_frames(&mut stream, &mut decoder)?;
    }
    if pending.is_empty() {
        return Ok(());
    }
    let first = pending.remove(0);
    let message = match parse_client_message(&first) {
        Ok(message) => message,
        Err(error) => {
            return write_message(
                &mut stream,
                &error_message(
                    None,
                    error.code(),
                    scope_for(error.code()),
                    error.to_string(),
                ),
            );
        }
    };
    match &message {
        ClientMessage::EnrollmentBegin { .. }
        | ClientMessage::EnrollmentStatus { .. }
        | ClientMessage::EnrollmentClaim { .. } => {
            let reply = {
                let mut guard = inner.lock().expect("registry lock");
                dispatch_provisioning(&mut guard, message)
            };
            write_message(&mut stream, &reply)
        }
        ClientMessage::EngineHello { .. } => {
            run_member_session(stream, decoder, pending, message, inner)
        }
        _ => write_message(
            &mut stream,
            &error_message(
                Some(client_request_id(&message)),
                ErrorCode::InvalidStateTransition,
                RegistryErrorScope::Session,
                "first frame after preface must be provisioning or engine_hello",
            ),
        ),
    }
}

fn run_member_session(
    mut stream: TcpStream,
    mut decoder: FrameDecoder,
    pending: Vec<Vec<u8>>,
    hello: ClientMessage,
    inner: Arc<Mutex<Inner>>,
) -> io::Result<()> {
    let replies = {
        let mut guard = inner.lock().expect("registry lock");
        dispatch_session(&mut guard, hello, None)
    };
    let mut session_id = None;
    for reply in replies {
        if let ServerMessage::SessionReady {
            registry_session_id,
            ..
        } = &reply
        {
            session_id = Some(*registry_session_id);
        }
        write_message(&mut stream, &reply)?;
    }
    let Some(session_id) = session_id else {
        return Ok(());
    };
    for payload in pending {
        let message = match parse_client_message(&payload) {
            Ok(message) => message,
            Err(error) => {
                write_message(
                    &mut stream,
                    &error_message(
                        None,
                        error.code(),
                        scope_for(error.code()),
                        error.to_string(),
                    ),
                )?;
                continue;
            }
        };
        let replies = {
            let mut guard = inner.lock().expect("registry lock");
            dispatch_session(&mut guard, message, Some(session_id))
        };
        for reply in replies {
            write_message(&mut stream, &reply)?;
        }
    }
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => match decoder.feed(&buffer[..length]) {
                Ok(frames) => {
                    for payload in frames {
                        let message = match parse_client_message(&payload) {
                            Ok(message) => message,
                            Err(error) => {
                                write_message(
                                    &mut stream,
                                    &error_message(
                                        None,
                                        error.code(),
                                        scope_for(error.code()),
                                        error.to_string(),
                                    ),
                                )?;
                                continue;
                            }
                        };
                        if matches!(message, ClientMessage::Leave { .. }) {
                            let replies = {
                                let mut guard = inner.lock().expect("registry lock");
                                dispatch_session(&mut guard, message, Some(session_id))
                            };
                            for reply in replies {
                                write_message(&mut stream, &reply)?;
                            }
                            break;
                        }
                        let replies = {
                            let mut guard = inner.lock().expect("registry lock");
                            dispatch_session(&mut guard, message, Some(session_id))
                        };
                        for reply in replies {
                            write_message(&mut stream, &reply)?;
                        }
                    }
                }
                Err(error) => {
                    write_message(
                        &mut stream,
                        &error_message(
                            None,
                            error.code(),
                            scope_for(error.code()),
                            error.to_string(),
                        ),
                    )?;
                    break;
                }
            },
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => {
                let _ = disconnect_session(&inner, session_id);
                return Err(error);
            }
        }
        let queued = {
            let mut guard = inner.lock().expect("registry lock");
            drain_session(&mut guard, session_id)
        };
        for reply in queued {
            write_message(&mut stream, &reply)?;
        }
    }
    disconnect_session(&inner, session_id)
}

fn drain_session(inner: &mut Inner, session_id: RegistrySessionId) -> Vec<ServerMessage> {
    inner
        .outbound
        .get_mut(&session_id)
        .map(|queue| queue.drain(..).collect())
        .unwrap_or_default()
}

fn disconnect_session(inner: &Arc<Mutex<Inner>>, session_id: RegistrySessionId) -> io::Result<()> {
    let mut guard = inner.lock().expect("registry lock");
    guard.outbound.remove(&session_id);
    let _ = guard.authority.disconnect(session_id);
    Ok(())
}

fn dispatch_provisioning(inner: &mut Inner, message: ClientMessage) -> ServerMessage {
    match message {
        ClientMessage::EnrollmentBegin {
            request_id,
            room_id,
            member_id,
            display_name,
            source_slots,
            claim_sha256,
            request_readmission,
            ..
        } => enrollment_begin(
            inner,
            request_id,
            room_id,
            member_id,
            display_name,
            source_slots,
            claim_sha256,
            request_readmission,
        ),
        ClientMessage::EnrollmentStatus {
            request_id,
            room_id,
            member_id,
            claim_secret,
            ..
        } => match inner
            .authority
            .room()
            .enrollment_status(room_id, member_id, &claim_secret)
        {
            Ok(view) => ServerMessage::EnrollmentStatusResult {
                v: Version2,
                reply_to: request_id,
                room_id: view.room_id,
                room_name: view.room_name,
                admission: view.admission,
                room_revision: view.room_revision,
            },
            Err(error) => error_message(
                Some(request_id),
                error.code(),
                scope_for(error.code()),
                error.to_string(),
            ),
        },
        ClientMessage::EnrollmentClaim {
            request_id,
            room_id,
            member_id,
            claim_secret,
            ..
        } => enrollment_claim(inner, request_id, room_id, member_id, claim_secret),
        other => error_message(
            Some(client_request_id(&other)),
            ErrorCode::InvalidStateTransition,
            RegistryErrorScope::Protocol,
            "provisioning connection accepts enrollment messages only",
        ),
    }
}

fn enrollment_begin(
    inner: &mut Inner,
    request_id: RequestId,
    room_id: RoomId,
    member_id: MemberId,
    display_name: String,
    source_slots: Vec<SourceSlot>,
    claim_sha256: TokenDigest,
    request_readmission: bool,
) -> ServerMessage {
    if room_id != inner.authority.room().room.room_id {
        return error_message(
            Some(request_id),
            ErrorCode::RoomScopeMismatch,
            RegistryErrorScope::Room,
            "enrollment room_id does not match this authority",
        );
    }
    let Ok(source_slots): Result<[SourceSlot; 2], _> = source_slots.try_into() else {
        return error_message(
            Some(request_id),
            ErrorCode::InvalidValue,
            RegistryErrorScope::Source,
            "enrollment requires exactly two source slots",
        );
    };
    if let Err(error) = apply_action(
        inner,
        RoomAction::Enroll {
            member_id,
            display_name: display_name.clone(),
            source_slots,
            claim_sha256,
            request_readmission,
        },
    ) {
        return error;
    }
    if inner.auto_approve
        && let Err(error) = apply_action(inner, RoomAction::Approve { member_id })
    {
        let code = match &error {
            ServerMessage::Error { code, .. } => *code,
            _ => ErrorCode::UnexpectedError,
        };
        if code != ErrorCode::InvalidStateTransition {
            return error;
        }
    }
    let record = inner.authority.room().member(member_id);
    let admission = record.map_or(AdmissionState::Pending, |record| record.admission);
    ServerMessage::EnrollmentPending {
        v: Version2,
        reply_to: request_id,
        room_id,
        room_name: inner.authority.room().room.name.clone(),
        admission,
        room_revision: inner.authority.room().room.room_revision,
    }
}

fn enrollment_claim(
    inner: &mut Inner,
    request_id: RequestId,
    room_id: RoomId,
    member_id: MemberId,
    claim_secret: ClaimSecret,
) -> ServerMessage {
    let token = match MemberToken::generate() {
        Ok(token) => token,
        Err(error) => {
            return error_message(
                Some(request_id),
                ErrorCode::UnexpectedError,
                RegistryErrorScope::Member,
                error.to_string(),
            );
        }
    };
    if let Err(error) = apply_action(
        inner,
        RoomAction::Claim {
            member_id,
            claim_secret,
            issued_token: token.clone(),
        },
    ) {
        return error;
    }
    let record = inner.authority.room().member(member_id);
    let Some(record) = record else {
        return error_message(
            Some(request_id),
            ErrorCode::MemberNotFound,
            RegistryErrorScope::Member,
            "claimed member record is missing",
        );
    };
    ServerMessage::EnrollmentClaimResult {
        v: Version2,
        reply_to: request_id,
        room_id,
        room_name: inner.authority.room().room.name.clone(),
        admission: record.admission,
        room_revision: inner.authority.room().room.room_revision,
        member_token: token,
        token_generation: record
            .token_generation
            .unwrap_or_else(|| DecimalU64::new(1)),
    }
}

fn apply_action(inner: &mut Inner, action: RoomAction) -> Result<(), ServerMessage> {
    let transition = reduce_room(inner.authority.room(), action).map_err(|error| {
        error_message(
            None,
            error.code(),
            scope_for(error.code()),
            error.to_string(),
        )
    })?;
    commit_transition(&mut inner.authority, &inner.state_path, transition).map_err(|error| {
        error_message(
            None,
            ErrorCode::RegistryStateCorrupt,
            RegistryErrorScope::Room,
            error.to_string(),
        )
    })
}

fn dispatch_session(
    inner: &mut Inner,
    message: ClientMessage,
    bound_session: Option<RegistrySessionId>,
) -> Vec<ServerMessage> {
    match message {
        hello_msg @ ClientMessage::EngineHello { .. } => {
            let request_id = client_request_id(&hello_msg);
            let member_id = match &hello_msg {
                ClientMessage::EngineHello { member_id, .. } => *member_id,
                _ => unreachable!("engine_hello arm"),
            };
            let hello = match EngineHello::try_from(hello_msg) {
                Ok(hello) => hello,
                Err(error) => return vec![session_error(Some(request_id), error)],
            };
            match inner.authority.accept_engine_hello(hello, now_ms()) {
                Ok(ready) => {
                    inner.outbound.entry(ready.registry_session_id).or_default();
                    let room = inner.authority.room();
                    let session_ready = ServerMessage::SessionReady {
                        v: Version2,
                        reply_to: ready.request_id,
                        server_generation: inner.authority.server_generation(),
                        registry_session_id: ready.registry_session_id,
                        room_id: room.room.room_id,
                        room_name: room.room.name.clone(),
                        admission: AdmissionState::Approved,
                        lease_ms: LEASE_MS as u32,
                        heartbeat_ms: HEARTBEAT_MS,
                        bind_timeout_ms: BIND_TIMEOUT_MS as u32,
                        bind_nonce: ready.bind_nonce,
                        room_revision: ready.room_revision,
                        presence_revision: ready.presence_revision,
                        quality_cap: room.room.quality_cap,
                        qos_policy: room.room.qos_policy,
                        room_send_muted: room.room.room_send_muted,
                    };
                    match inner.authority.snapshot_for(member_id) {
                        Ok(snapshot) => vec![session_ready, snapshot.into()],
                        Err(error) => vec![session_ready, session_error(Some(request_id), error)],
                    }
                }
                Err(error) => vec![session_error(Some(request_id), error)],
            }
        }
        ClientMessage::Heartbeat {
            request_id,
            room_id,
            server_generation,
            registry_session_id,
            applied_revisions,
            ..
        } => {
            if let Some(expected) = bound_session
                && expected != registry_session_id
            {
                return vec![error_message(
                    Some(request_id),
                    ErrorCode::UnknownSession,
                    RegistryErrorScope::Session,
                    "heartbeat session does not match this TCP transport",
                )];
            }
            if room_id != inner.authority.room().room.room_id {
                return vec![error_message(
                    Some(request_id),
                    ErrorCode::RoomScopeMismatch,
                    RegistryErrorScope::Room,
                    "heartbeat room_id does not match this authority",
                )];
            }
            if server_generation != inner.authority.server_generation() {
                return vec![error_message(
                    Some(request_id),
                    ErrorCode::StaleGeneration,
                    RegistryErrorScope::Session,
                    "heartbeat server_generation is stale",
                )];
            }
            let _ = applied_revisions;
            match inner.authority.heartbeat(registry_session_id, now_ms()) {
                Ok(lease_remaining_ms) => vec![ServerMessage::HeartbeatAck {
                    v: Version2,
                    reply_to: request_id,
                    server_generation: inner.authority.server_generation(),
                    registry_session_id,
                    room_revision: inner.authority.room().room.room_revision,
                    presence_revision: inner.authority.presence_revision(),
                    lease_remaining_ms,
                }],
                Err(error) => vec![session_error(Some(request_id), error)],
            }
        }
        ClientMessage::SnapshotRequest {
            request_id,
            registry_session_id,
            ..
        } => match member_id_for_session(inner, registry_session_id)
            .ok_or(SessionError::UnknownSession)
            .and_then(|member_id| inner.authority.snapshot_for(member_id))
        {
            Ok(snapshot) => {
                let _ = request_id;
                vec![snapshot.into()]
            }
            Err(error) => vec![session_error(Some(request_id), error)],
        },
        ClientMessage::MediaRebindRequest {
            request_id,
            registry_session_id,
            ..
        } => {
            let old_endpoint = member_id_for_session(inner, registry_session_id)
                .and_then(|member_id| inner.authority.active_session(member_id))
                .and_then(|session| session.endpoint);
            match inner
                .authority
                .request_rebind(registry_session_id, now_ms())
            {
                Ok(bind_nonce) => vec![ServerMessage::MediaRebindReady {
                    v: Version2,
                    reply_to: request_id,
                    server_generation: inner.authority.server_generation(),
                    registry_session_id,
                    bind_nonce,
                    bind_timeout_ms: BIND_TIMEOUT_MS as u32,
                    old_endpoint,
                }],
                Err(error) => vec![session_error(Some(request_id), error)],
            }
        }
        ClientMessage::Leave {
            request_id,
            registry_session_id,
            ..
        } => {
            let _ = inner.authority.disconnect(registry_session_id);
            inner.outbound.remove(&registry_session_id);
            let _ = request_id;
            Vec::new()
        }
        ClientMessage::CatalogReplace {
            request_id,
            registry_session_id,
            run_id,
            catalog_revision,
            sources,
            ..
        } => match inner.authority.replace_catalog(
            registry_session_id,
            run_id,
            catalog_revision,
            sources,
        ) {
            Ok(()) => Vec::new(),
            Err(error) => vec![session_error(Some(request_id), error)],
        },
        ClientMessage::MemberProjectionReplace {
            request_id,
            registry_session_id,
            member_config_revision,
            member_projection,
            ..
        } => match inner.authority.replace_member_projection(
            registry_session_id,
            member_config_revision,
            member_projection,
        ) {
            Ok(()) => Vec::new(),
            Err(error) => vec![session_error(Some(request_id), error)],
        },
        ClientMessage::CapabilityGrantsReplace {
            request_id,
            registry_session_id,
            grant_revision,
            capability_grants,
            ..
        } => match inner.authority.replace_grants(
            registry_session_id,
            grant_revision,
            capability_grants,
        ) {
            Ok(_) => Vec::new(),
            Err(error) => vec![session_error(Some(request_id), error)],
        },
        result @ ClientMessage::MemberCommandResult { request_id, .. } => {
            let request_id = request_id;
            match inner.authority.apply_member_command_result(result) {
                Ok(_) => Vec::new(),
                Err(error) => vec![session_error(Some(request_id), error)],
            }
        }
        other => vec![error_message(
            Some(client_request_id(&other)),
            ErrorCode::InvalidStateTransition,
            RegistryErrorScope::Session,
            "message is not valid on an established member session",
        )],
    }
}

fn member_id_for_session(inner: &Inner, session_id: RegistrySessionId) -> Option<MemberId> {
    inner.authority.room().members.iter().find_map(|record| {
        inner
            .authority
            .active_session(record.member_id)
            .filter(|session| session.registry_session_id == session_id)
            .map(|session| session.member_id)
    })
}

fn session_error(reply_to: Option<RequestId>, error: SessionError) -> ServerMessage {
    error_message(
        reply_to,
        error.code(),
        scope_for(error.code()),
        error.to_string(),
    )
}

fn client_request_id(message: &ClientMessage) -> RequestId {
    match message {
        ClientMessage::EnrollmentBegin { request_id, .. }
        | ClientMessage::EnrollmentStatus { request_id, .. }
        | ClientMessage::EnrollmentClaim { request_id, .. }
        | ClientMessage::EngineHello { request_id, .. }
        | ClientMessage::Heartbeat { request_id, .. }
        | ClientMessage::CatalogReplace { request_id, .. }
        | ClientMessage::MemberProjectionReplace { request_id, .. }
        | ClientMessage::CapabilityGrantsReplace { request_id, .. }
        | ClientMessage::SnapshotRequest { request_id, .. }
        | ClientMessage::MediaRebindRequest { request_id, .. }
        | ClientMessage::MemberCommandResult { request_id, .. }
        | ClientMessage::Leave { request_id, .. } => *request_id,
    }
}

fn write_message(stream: &mut TcpStream, message: &ServerMessage) -> io::Result<()> {
    let frame = encode_json_frame(message).map_err(|error| io::Error::other(error.to_string()))?;
    stream.write_all(&frame)?;
    stream.flush()
}

fn read_frames(stream: &mut TcpStream, decoder: &mut FrameDecoder) -> io::Result<Vec<Vec<u8>>> {
    let mut buffer = [0_u8; 4096];
    match stream.read(&mut buffer) {
        Ok(0) => Ok(Vec::new()),
        Ok(length) => decoder
            .feed(&buffer[..length])
            .map_err(|error| io::Error::other(error.to_string())),
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::TimedOut =>
        {
            Ok(Vec::new())
        }
        Err(error) => Err(error),
    }
}
