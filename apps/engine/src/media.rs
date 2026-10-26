//! Bound LAN UDP media path: history insert, fanout, receive, and assembly.
//!
//! MEDIA datagram sets enter [`ResendHistory`] before the first `send_to`.
//! Receive runs on a helper thread into a preallocated pool; the coordinator
//! parses, assembles, and pushes decoded units into jitter ingress. `loop_self`
//! sends each outbound datagram to the process's own bound endpoint so this PC
//! can prove the path without a second member.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rlb_audio::{MixGraphCommand, MixGraphPublisher, SourceRenderConfig};
use rlb_clock::{MonotonicClock, MonotonicInstant, StdMonotonic};
use rlb_codec::WireCodec;
use rlb_jitter::{GapKind, TargetMode};
use rlb_media::{
    AssemblyOutcome, FragmentAssembler, GapReason, MediaIdentity, NackCandidate, NackPlannerConfig,
    NackRecord, OutboundRecord, PacketError, ParseContext, ParsedDatagram, ResendHistory,
    encode_nack_into, parse_datagram,
};
use rlb_model::{
    Endpoint, MemberId, MemberMix, RoomId, RunId, SourceFormat, SourceId, SourceMix, StreamEpoch,
};
use rlb_net::{
    DatagramClass, DatagramLease, DatagramPool, IoMode, LanUdpSocket, LanUdpSocketConfig,
    ReceiveFromError,
};
use rlb_session::{MediaReceivePlan, MediaSourceProjection, SendPlan};

use crate::error::EngineError;
use crate::receive::{IngressCommand, JitterIngress, JitterReaderMetrics};

const INCOMING_CAPACITY: usize = 64;
const POOL_SLOTS: usize = 64;
const DECODE_CHANNELS_MAX: usize = 2;
const NACK_CANDIDATE_CAP: usize = 8;
const RETIRED_EPOCH_CAPACITY: usize = 8;
const RETIRED_EPOCH_SECONDS: u64 = 600;

/// Counters published by the media owner.
#[derive(Debug, Default)]
pub struct MediaMetrics {
    pub datagrams_sent: AtomicU64,
    pub datagrams_received: AtomicU64,
    pub datagrams_dropped_ingress: AtomicU64,
    pub units_assembled: AtomicU64,
    pub parse_rejected: AtomicU64,
    pub assembly_failed: AtomicU64,
    pub unauthorized_media: AtomicU64,
    pub room_mismatch: AtomicU64,
    pub endpoint_mismatch: AtomicU64,
    pub member_mismatch: AtomicU64,
    pub stale_run_packet: AtomicU64,
    pub source_unknown: AtomicU64,
    pub subscription_denied: AtomicU64,
    pub media_epoch_unannounced: AtomicU64,
    pub stale_epoch_packet: AtomicU64,
    pub send_failures: AtomicU64,
    pub nacks_sent: AtomicU64,
    pub nacks_received: AtomicU64,
    pub nacks_unauthorized: AtomicU64,
    pub fragments_retransmitted: AtomicU64,
    pub ingress_overflow: AtomicBool,
}

/// One bound media socket plus per-source send history and receive sessions.
pub struct MediaPath {
    socket: Arc<LanUdpSocket>,
    clock: StdMonotonic,
    room_id: Option<RoomId>,
    local_member_id: MemberId,
    local_run_id: RunId,
    loop_endpoint: Endpoint,
    loop_self: bool,
    send_plan: SendPlan,
    histories: BTreeMap<SourceId, ResendHistory>,
    sessions: BTreeMap<SourceId, ReceiveSession>,
    incoming: Receiver<(rlb_model::Endpoint, DatagramLease)>,
    stop: Arc<AtomicBool>,
    recv_join: Option<JoinHandle<()>>,
    decoded: Vec<f32>,
    pending_mix: Vec<SourceId>,
    send_stopped: Arc<AtomicBool>,
    emergency_stopped: bool,
    peer_transports: BTreeMap<MemberId, PeerTransport>,
    source_projections: BTreeMap<SourceId, MediaSourceProjection>,
    retired_epochs: BTreeMap<SourceId, VecDeque<RetiredEpoch>>,
    receive_mode: TargetMode,
    last_send_at: Option<MonotonicInstant>,
    rtt_ticks: u64,
    pub metrics: Arc<MediaMetrics>,
}

struct ReceiveSession {
    identity: MediaIdentity,
    assembler: FragmentAssembler,
    ingress: JitterIngress,
    installed: bool,
    peer_endpoint: Option<Endpoint>,
    next_nack_id: u32,
    channels: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeerTransport {
    run_id: RunId,
    endpoint: Endpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetiredEpoch {
    run_id: RunId,
    epoch: StreamEpoch,
    retired_at: MonotonicInstant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IngressRejection {
    Room,
    Endpoint,
    Member,
    StaleRun,
    SourceUnknown,
    Subscription,
    EpochUnannounced,
    StaleEpoch,
}

impl MediaPath {
    /// Binds an exclusive IPv4 UDP socket and starts the receive thread.
    pub fn bind(
        room_id: Option<RoomId>,
        local_member_id: MemberId,
        local_run_id: RunId,
        loop_self: bool,
        clock: StdMonotonic,
    ) -> Result<Self, EngineError> {
        let bind_ip = if loop_self {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::UNSPECIFIED
        };
        let socket = LanUdpSocket::bind(LanUdpSocketConfig::new(
            SocketAddrV4::new(bind_ip, 0),
            IoMode::Timed {
                receive_timeout: Duration::from_millis(5),
                send_timeout: Duration::from_millis(5),
            },
        ))?;
        let local = socket.local_address();
        let loop_endpoint = Endpoint::new(Ipv4Addr::LOCALHOST, local.port())?;
        let pool = DatagramPool::new(NonZeroUsize::new(POOL_SLOTS).expect("pool"));
        let socket = Arc::new(socket);
        let stop = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(MediaMetrics::default());
        let (incoming_tx, incoming_rx) = sync_channel(INCOMING_CAPACITY);
        let recv_socket = Arc::clone(&socket);
        let recv_pool = pool.clone();
        let recv_stop = Arc::clone(&stop);
        let recv_metrics = Arc::clone(&metrics);
        let join = thread::Builder::new()
            .name("rlb-udp-recv".to_owned())
            .spawn(move || {
                while !recv_stop.load(Ordering::Relaxed) {
                    match recv_socket.recv_from(&recv_pool) {
                        Ok(datagram) => {
                            let source = datagram.source();
                            let lease = datagram.into_lease();
                            if incoming_tx.try_send((source, lease)).is_err() {
                                recv_metrics
                                    .datagrams_dropped_ingress
                                    .fetch_add(1, Ordering::Relaxed);
                                recv_metrics.ingress_overflow.store(true, Ordering::Relaxed);
                            }
                        }
                        Err(ReceiveFromError::WouldBlock | ReceiveFromError::TimedOut) => {}
                        Err(_) => thread::sleep(Duration::from_millis(1)),
                    }
                }
            })
            .map_err(|error| EngineError::ThreadSpawn(error.to_string()))?;
        Ok(Self {
            socket,
            clock,
            room_id,
            local_member_id,
            local_run_id,
            loop_endpoint,
            loop_self,
            send_plan: SendPlan {
                catalog_revision: rlb_model::CatalogRevision::new(1),
                sources: Vec::new(),
            },
            histories: BTreeMap::new(),
            sessions: BTreeMap::new(),
            incoming: incoming_rx,
            stop,
            recv_join: Some(join),
            decoded: vec![0.0_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES) * DECODE_CHANNELS_MAX],
            pending_mix: Vec::new(),
            send_stopped: Arc::new(AtomicBool::new(room_id.is_none())),
            emergency_stopped: false,
            peer_transports: BTreeMap::new(),
            source_projections: BTreeMap::new(),
            retired_epochs: BTreeMap::new(),
            receive_mode: TargetMode::Automatic,
            last_send_at: None,
            rtt_ticks: 0,
            metrics,
        })
    }

    /// Returns the bound UDP room identity used for media headers.
    #[must_use]
    pub const fn room_id(&self) -> Option<RoomId> {
        self.room_id
    }

    /// Returns the OS-assigned local UDP endpoint.
    #[must_use]
    pub fn local_address(&self) -> SocketAddrV4 {
        self.socket.local_address()
    }

    /// Replaces the current encode-once fanout plan.
    pub fn set_send_plan(&mut self, plan: SendPlan) {
        self.send_plan = plan;
    }

    /// Blocks or allows UDP `sendto` for the emergency send gate.
    pub fn set_send_gate(&mut self, stopped: bool) {
        self.emergency_stopped = stopped;
        if stopped {
            self.histories.clear();
        }
        self.send_stopped
            .store(stopped || self.room_id.is_none(), Ordering::Release);
    }

    /// Binds one Registry-approved direct peer transport.
    pub fn bind_peer(&mut self, member_id: MemberId, run_id: RunId, endpoint: Endpoint) {
        self.peer_transports
            .insert(member_id, PeerTransport { run_id, endpoint });
    }

    pub fn forget_peer(&mut self, member_id: MemberId) {
        self.peer_transports.remove(&member_id);
    }

    /// Atomically applies the coordinator's room and receive-subscription view.
    pub fn apply_receive_plan(
        &mut self,
        plan: MediaReceivePlan,
        publisher: Option<&MixGraphPublisher>,
    ) {
        if self.room_id != plan.room_id {
            self.transition_room(plan.room_id, publisher);
        }
        if self.loop_self {
            self.source_projections.clear();
            return;
        }

        let next: BTreeMap<_, _> = plan
            .sources
            .into_iter()
            .map(|projection| (projection.source_id, projection))
            .collect();
        let existing_ids: Vec<_> = self.source_projections.keys().copied().collect();
        for source_id in existing_ids {
            let previous = self
                .source_projections
                .get(&source_id)
                .copied()
                .expect("projection key exists");
            match next.get(&source_id).copied() {
                Some(current) => {
                    if previous.run_id != current.run_id {
                        self.retired_epochs.remove(&source_id);
                    } else if previous.active_epoch != current.active_epoch {
                        self.retire_epoch(
                            source_id,
                            previous.run_id,
                            previous.active_epoch,
                            self.clock.now(),
                        );
                    }
                    if projection_generation_changed(previous, current) {
                        self.remove_session(source_id, publisher);
                    }
                }
                None => self.remove_session(source_id, publisher),
            }
        }
        for projection in next.values() {
            if !projection.subscription_accepted {
                self.remove_session(projection.source_id, publisher);
            }
        }
        self.source_projections = next;
        self.trim_retired_epochs(self.clock.now());
    }

    fn transition_room(&mut self, room_id: Option<RoomId>, publisher: Option<&MixGraphPublisher>) {
        self.send_stopped.store(true, Ordering::Release);
        let session_ids: Vec<_> = self.sessions.keys().copied().collect();
        for source_id in session_ids {
            self.remove_session(source_id, publisher);
        }
        self.send_plan.sources.clear();
        self.histories.clear();
        self.peer_transports.clear();
        self.source_projections.clear();
        self.retired_epochs.clear();
        self.pending_mix.clear();
        self.last_send_at = None;
        self.rtt_ticks = 0;
        while self.incoming.try_recv().is_ok() {}
        self.room_id = room_id;
        self.send_stopped.store(
            self.emergency_stopped || self.room_id.is_none(),
            Ordering::Release,
        );
    }

    fn remove_session(&mut self, source_id: SourceId, publisher: Option<&MixGraphPublisher>) {
        let Some(session) = self.sessions.remove(&source_id) else {
            return;
        };
        session.ingress.retire();
        session.ingress.force_disable();
        if session.installed
            && let Some(publisher) = publisher
        {
            let _ = publisher.try_publish(MixGraphCommand::RemoveSource { source_id });
        }
        self.pending_mix.retain(|pending| *pending != source_id);
    }

    /// Follows `local.receive_buffer.set` on existing jitter readers.
    pub fn set_receive_buffer(&mut self, buffer: rlb_model::ReceiveBufferConfig) {
        self.receive_mode = match buffer {
            rlb_model::ReceiveBufferConfig::Automatic => TargetMode::Automatic,
            rlb_model::ReceiveBufferConfig::Manual { target_frames } => {
                TargetMode::Manual(target_frames.frames())
            }
        };
        for session in self.sessions.values() {
            let _ = session.ingress.try_push(IngressCommand::SetTarget {
                mode: self.receive_mode,
            });
        }
    }

    /// Sends one RLBD direct datagram to a peer endpoint.
    pub fn send_direct(&self, endpoint: Endpoint, datagram: rlb_media::DirectDatagram) {
        if self.room_id.is_none()
            || !self
                .peer_transports
                .values()
                .any(|transport| transport.endpoint == endpoint)
        {
            return;
        }
        let mut bytes = [0_u8; 2048];
        match rlb_media::encode_direct_into(datagram, &mut bytes) {
            Ok(len) => self.send_control_one(endpoint, &bytes[..len]),
            Err(_) => {
                self.metrics.send_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Inserts MEDIA into history, then submits every fragment.
    pub fn submit(&mut self, record: OutboundRecord) {
        self.submit_media(record, u32::MAX);
    }

    /// Inserts MEDIA into history and transmits fragments selected by `transmit_mask`.
    ///
    /// Production senders pass `u32::MAX`. Tests pass a subset to prove NACK recovery
    /// against the same history the live path uses.
    pub fn submit_media(&mut self, record: OutboundRecord, transmit_mask: u32) {
        let identity = outbound_identity(&record);
        if !self.outbound_identity_allowed(identity) {
            return;
        }
        match record {
            OutboundRecord::Media(set) => {
                let source_id = set.identity().source_id;
                let history = self
                    .histories
                    .entry(source_id)
                    .or_insert_with(ResendHistory::new_nanosecond_domain);
                let key = history.insert(set, self.clock.now());
                if let Some(set) = history.get(key) {
                    let datagrams: Vec<Vec<u8>> = set
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| transmit_mask & (1_u32 << index) != 0)
                        .map(|(_, bytes)| bytes.to_vec())
                        .collect();
                    self.last_send_at = Some(self.clock.now());
                    self.fanout_bytes(identity, &datagrams);
                }
            }
            OutboundRecord::Gap { datagram, record } => {
                self.fanout_bytes(record.identity, &[datagram.to_vec()]);
            }
            OutboundRecord::EpochEnd { datagram, record } => {
                self.fanout_bytes(record.identity, &[datagram.to_vec()]);
            }
        }
    }

    fn outbound_identity_allowed(&self, identity: MediaIdentity) -> bool {
        if self.send_stopped.load(Ordering::Acquire)
            || self.room_id != Some(identity.room_id)
            || identity.sender_member_id != self.local_member_id
            || identity.sender_run_id != self.local_run_id
        {
            return false;
        }
        if self.loop_self {
            return true;
        }
        self.send_plan.sources.iter().any(|source| {
            source.source_id == identity.source_id
                && source.stream_epoch == identity.stream_epoch
                && !source.recipients.is_empty()
        })
    }

    fn fanout_bytes(&self, identity: MediaIdentity, datagrams: &[Vec<u8>]) {
        if self.loop_self {
            for datagram in datagrams {
                if self.outbound_identity_allowed(identity) {
                    self.send_media_one(self.loop_endpoint, datagram);
                }
            }
        }
        let recipients = self
            .send_plan
            .sources
            .iter()
            .find(|source| {
                source.source_id == identity.source_id
                    && source.stream_epoch == identity.stream_epoch
            })
            .map(|source| source.recipients.clone())
            .unwrap_or_default();
        for recipient in recipients {
            for datagram in datagrams {
                if self.recipient_allowed(identity, recipient) {
                    self.send_media_one(recipient.endpoint, datagram);
                }
            }
        }
    }

    fn recipient_allowed(
        &self,
        identity: MediaIdentity,
        recipient: rlb_session::SendRecipient,
    ) -> bool {
        self.outbound_identity_allowed(identity)
            && self
                .peer_transports
                .get(&recipient.member_id)
                .is_some_and(|transport| {
                    transport.run_id == recipient.run_id && transport.endpoint == recipient.endpoint
                })
            && self.send_plan.sources.iter().any(|source| {
                source.source_id == identity.source_id
                    && source.stream_epoch == identity.stream_epoch
                    && source.recipients.contains(&recipient)
            })
    }

    fn send_media_one(&self, endpoint: Endpoint, datagram: &[u8]) {
        if self.send_stopped.load(Ordering::Acquire) || self.room_id.is_none() {
            return;
        }
        self.send_socket(endpoint, datagram);
    }

    fn send_control_one(&self, endpoint: Endpoint, datagram: &[u8]) {
        if self.room_id.is_none() {
            return;
        }
        self.send_socket(endpoint, datagram);
    }

    fn send_socket(&self, endpoint: Endpoint, datagram: &[u8]) {
        match self
            .socket
            .send_to(datagram, endpoint, DatagramClass::Media)
        {
            Ok(()) => {
                self.metrics.datagrams_sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.metrics.send_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn nack_rejection(&self, source: Endpoint, nack: NackRecord) -> Option<IngressRejection> {
        if self.room_id != Some(nack.identity.room_id) {
            return Some(IngressRejection::Room);
        }
        if self.send_stopped.load(Ordering::Acquire) {
            return Some(IngressRejection::Subscription);
        }
        if self.loop_self {
            if source != self.loop_endpoint {
                return Some(IngressRejection::Endpoint);
            }
            if nack.identity.sender_member_id != self.local_member_id {
                return Some(IngressRejection::Member);
            }
            if nack.identity.sender_run_id != self.local_run_id {
                return Some(IngressRejection::StaleRun);
            }
            return None;
        }
        let Some(plan) = self
            .send_plan
            .sources
            .iter()
            .find(|plan| plan.source_id == nack.identity.source_id)
        else {
            return Some(if self.histories.contains_key(&nack.identity.source_id) {
                IngressRejection::Subscription
            } else {
                IngressRejection::SourceUnknown
            });
        };
        if plan.stream_epoch != nack.identity.stream_epoch {
            return Some(IngressRejection::EpochUnannounced);
        }
        let Some(recipient) = plan
            .recipients
            .iter()
            .find(|recipient| recipient.member_id == nack.identity.sender_member_id)
        else {
            return Some(
                if self
                    .peer_transports
                    .contains_key(&nack.identity.sender_member_id)
                {
                    IngressRejection::Subscription
                } else {
                    IngressRejection::Member
                },
            );
        };
        if recipient.run_id != nack.identity.sender_run_id {
            return Some(IngressRejection::StaleRun);
        }
        if recipient.endpoint != source {
            return Some(IngressRejection::Endpoint);
        }
        if self
            .peer_transports
            .get(&recipient.member_id)
            .is_none_or(|transport| {
                transport.run_id != recipient.run_id || transport.endpoint != recipient.endpoint
            })
        {
            return Some(IngressRejection::Endpoint);
        }
        None
    }

    /// Parses queued datagrams, handles NACK/resend, and publishes completed units.
    pub fn poll_receive(&mut self, publisher: Option<&MixGraphPublisher>) {
        if self.metrics.ingress_overflow.swap(false, Ordering::Relaxed) {
            let unit_frames = u32::from(rlb_model::MEDIA_UNIT_FRAMES);
            let mut overflow: Vec<(SourceId, u64)> = Vec::new();
            for (source_id, session) in &self.sessions {
                let cursor = session.ingress.metrics.read_cursor.load(Ordering::Relaxed);
                let start = if cursor == u64::MAX { 0 } else { cursor };
                overflow.push((*source_id, start));
            }
            for (source_id, start_sample) in overflow {
                if let Some(session) = self.sessions.get_mut(&source_id) {
                    let _ = session.ingress.try_push(IngressCommand::Gap {
                        sequence: 0,
                        start_sample,
                        frame_count: unit_frames,
                        reason: GapKind::SendQueueOverflow,
                    });
                }
            }
        }
        while let Ok((source, lease)) = self.incoming.try_recv() {
            self.metrics
                .datagrams_received
                .fetch_add(1, Ordering::Relaxed);
            let Some(room_id) = self.room_id else {
                self.note_ingress_rejection(IngressRejection::Room);
                continue;
            };
            let context = ParseContext {
                expected_room_id: Some(room_id),
                expected_sender_member_id: None,
                expected_sender_run_id: None,
                expected_source_id: None,
                expected_stream_epoch: None,
                subscription_allowed: None,
                expected_channels: None,
                expected_codec: None,
            };
            match parse_datagram(lease.as_bytes(), context) {
                Ok(ParsedDatagram::Media(fragment)) => {
                    let identity = fragment.header.identity;
                    let projection = match self.ingress_projection(source, identity) {
                        Ok(projection) => projection,
                        Err(rejection) => {
                            self.note_ingress_rejection(rejection);
                            continue;
                        }
                    };
                    let channels = projection.map_or(fragment.header.channels, |projection| {
                        projection.format.channels()
                    });
                    if fragment.header.channels != channels
                        || projection.is_some_and(|projection| {
                            fragment.header.codec != WireCodec::from(projection.format.profile())
                        })
                    {
                        self.metrics.parse_rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if self.ensure_session(identity, channels, publisher).is_err() {
                        continue;
                    }
                    let Some(session) = self.sessions.get_mut(&identity.source_id) else {
                        continue;
                    };
                    if session.identity != identity {
                        continue;
                    }
                    session.peer_endpoint = Some(source);
                    if self.loop_self
                        && let Some(sent) = self.last_send_at
                    {
                        let now = self.clock.now();
                        self.rtt_ticks = now.ticks().saturating_sub(sent.ticks());
                    }
                    let needed = usize::from(rlb_model::MEDIA_UNIT_FRAMES) * usize::from(channels);
                    match session.assembler.push(
                        fragment,
                        self.clock.now(),
                        &mut self.decoded[..needed],
                    ) {
                        Ok(AssemblyOutcome::Complete(unit)) => {
                            self.metrics.units_assembled.fetch_add(1, Ordering::Relaxed);
                            let sample_count =
                                usize::from(unit.frames) * usize::from(unit.channels);
                            let mut samples = session.ingress.acquire_samples();
                            if samples.len() != sample_count {
                                samples = self.decoded[..sample_count].to_vec().into_boxed_slice();
                            } else {
                                samples.copy_from_slice(&self.decoded[..sample_count]);
                            }
                            if !session.ingress.try_push(IngressCommand::Media {
                                sequence: unit.unit_sequence,
                                start_sample: unit.start_sample,
                                interleaved: samples,
                                recovered_after_nack: unit.recovered_after_nack,
                            }) {
                                let _ = session.ingress.try_push(IngressCommand::Gap {
                                    sequence: unit.unit_sequence,
                                    start_sample: unit.start_sample,
                                    frame_count: u32::from(unit.frames),
                                    reason: GapKind::SendQueueOverflow,
                                });
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            self.metrics.assembly_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Ok(ParsedDatagram::Gap(gap)) => {
                    let projection = match self.ingress_projection(source, gap.identity) {
                        Ok(projection) => projection,
                        Err(rejection) => {
                            self.note_ingress_rejection(rejection);
                            continue;
                        }
                    };
                    let channels = projection.map_or_else(
                        || self.session_channels(gap.identity.source_id),
                        |projection| projection.format.channels(),
                    );
                    if self
                        .ensure_session(gap.identity, channels, publisher)
                        .is_err()
                    {
                        continue;
                    }
                    if let Some(session) = self.sessions.get_mut(&gap.identity.source_id) {
                        session.peer_endpoint = Some(source);
                        session.ingress.try_push(IngressCommand::Gap {
                            sequence: gap.record_sequence,
                            start_sample: gap.start_sample,
                            frame_count: gap.frame_count,
                            reason: gap_kind(gap.reason),
                        });
                    }
                }
                Ok(ParsedDatagram::EpochEnd(end)) => {
                    if let Err(rejection) = self.ingress_projection(source, end.identity) {
                        self.note_ingress_rejection(rejection);
                        continue;
                    }
                    if let Some(session) = self.sessions.get_mut(&end.identity.source_id) {
                        if session.identity != end.identity {
                            self.metrics
                                .unauthorized_media
                                .fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        session.ingress.try_push(IngressCommand::EpochEnd {
                            next_sequence: end.next_record_sequence,
                            final_sample: end.final_sample,
                        });
                    }
                }
                Ok(ParsedDatagram::Nack(nack)) => {
                    self.handle_nack(source, nack);
                }
                Err(PacketError::RoomMismatch) => {
                    self.note_ingress_rejection(IngressRejection::Room);
                }
                Err(PacketError::MemberMismatch) => {
                    self.note_ingress_rejection(IngressRejection::Member);
                }
                Err(PacketError::StaleRunPacket) => {
                    self.note_ingress_rejection(IngressRejection::StaleRun);
                }
                Err(PacketError::SourceUnknown) => {
                    self.note_ingress_rejection(IngressRejection::SourceUnknown);
                }
                Err(PacketError::SubscriptionDenied) => {
                    self.note_ingress_rejection(IngressRejection::Subscription);
                }
                Err(PacketError::MediaEpochUnannounced) => {
                    self.note_ingress_rejection(IngressRejection::EpochUnannounced);
                }
                Err(_) => {
                    self.metrics.parse_rejected.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.poll_assembler_nacks();
        for session in self.sessions.values_mut() {
            let cursor = session.ingress.metrics.read_cursor.load(Ordering::Relaxed);
            if cursor != u64::MAX {
                session.assembler.set_read_cursor(cursor);
            }
        }
    }

    fn ingress_projection(
        &self,
        source: Endpoint,
        identity: MediaIdentity,
    ) -> Result<Option<MediaSourceProjection>, IngressRejection> {
        if self.room_id != Some(identity.room_id) {
            return Err(IngressRejection::Room);
        }
        if self.loop_self {
            if source != self.loop_endpoint {
                return Err(IngressRejection::Endpoint);
            }
            if identity.sender_member_id != self.local_member_id {
                return Err(IngressRejection::Member);
            }
            if identity.sender_run_id != self.local_run_id {
                return Err(IngressRejection::StaleRun);
            }
            if self
                .sessions
                .get(&identity.source_id)
                .is_some_and(|session| session.identity != identity)
            {
                return Err(IngressRejection::EpochUnannounced);
            }
            return Ok(None);
        }

        let projection = self
            .source_projections
            .get(&identity.source_id)
            .copied()
            .ok_or(IngressRejection::SourceUnknown)?;
        if source != projection.media_endpoint {
            return Err(IngressRejection::Endpoint);
        }
        if identity.sender_member_id != projection.member_id {
            return Err(IngressRejection::Member);
        }
        if identity.sender_run_id != projection.run_id {
            return Err(IngressRejection::StaleRun);
        }
        if self
            .peer_transports
            .get(&projection.member_id)
            .is_none_or(|transport| {
                transport.run_id != projection.run_id
                    || transport.endpoint != projection.media_endpoint
            })
        {
            return Err(IngressRejection::Endpoint);
        }
        if identity.stream_epoch != projection.active_epoch {
            let retired = self
                .retired_epochs
                .get(&identity.source_id)
                .is_some_and(|epochs| {
                    epochs.iter().any(|epoch| {
                        epoch.run_id == identity.sender_run_id
                            && epoch.epoch == identity.stream_epoch
                    })
                });
            return Err(if retired {
                IngressRejection::StaleEpoch
            } else {
                IngressRejection::EpochUnannounced
            });
        }
        if !projection.subscription_accepted {
            return Err(IngressRejection::Subscription);
        }
        Ok(Some(projection))
    }

    fn note_ingress_rejection(&self, rejection: IngressRejection) {
        self.metrics
            .unauthorized_media
            .fetch_add(1, Ordering::Relaxed);
        let counter = match rejection {
            IngressRejection::Room => &self.metrics.room_mismatch,
            IngressRejection::Endpoint => &self.metrics.endpoint_mismatch,
            IngressRejection::Member => &self.metrics.member_mismatch,
            IngressRejection::StaleRun => &self.metrics.stale_run_packet,
            IngressRejection::SourceUnknown => &self.metrics.source_unknown,
            IngressRejection::Subscription => &self.metrics.subscription_denied,
            IngressRejection::EpochUnannounced => &self.metrics.media_epoch_unannounced,
            IngressRejection::StaleEpoch => &self.metrics.stale_epoch_packet,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn retire_epoch(
        &mut self,
        source_id: SourceId,
        run_id: RunId,
        epoch: StreamEpoch,
        now: MonotonicInstant,
    ) {
        let retired = self.retired_epochs.entry(source_id).or_default();
        if retired
            .iter()
            .any(|entry| entry.run_id == run_id && entry.epoch == epoch)
        {
            return;
        }
        while retired.len() >= RETIRED_EPOCH_CAPACITY {
            retired.pop_front();
        }
        retired.push_back(RetiredEpoch {
            run_id,
            epoch,
            retired_at: now,
        });
    }

    fn trim_retired_epochs(&mut self, now: MonotonicInstant) {
        let retention_ticks = self
            .clock
            .ticks_per_second()
            .get()
            .saturating_mul(RETIRED_EPOCH_SECONDS);
        for epochs in self.retired_epochs.values_mut() {
            while epochs.front().is_some_and(|entry| {
                now.ticks().saturating_sub(entry.retired_at.ticks()) > retention_ticks
            }) {
                epochs.pop_front();
            }
        }
        self.retired_epochs.retain(|_, epochs| !epochs.is_empty());
    }

    fn session_channels(&self, source_id: SourceId) -> u8 {
        self.sessions
            .get(&source_id)
            .map(|session| session.channels)
            .unwrap_or(2)
    }

    fn handle_nack(&mut self, source: Endpoint, nack: NackRecord) {
        self.metrics.nacks_received.fetch_add(1, Ordering::Relaxed);
        if let Some(rejection) = self.nack_rejection(source, nack) {
            self.note_ingress_rejection(rejection);
            self.metrics
                .nacks_unauthorized
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let now = self.clock.now();
        let Some(history) = self.histories.get_mut(&nack.identity.source_id) else {
            return;
        };
        let metrics = Arc::clone(&self.metrics);
        let socket = Arc::clone(&self.socket);
        let send_stopped = Arc::clone(&self.send_stopped);
        history.process_nack(nack.identity.sender_member_id, true, nack, now, |bytes| {
            if send_stopped.load(Ordering::Acquire) {
                return false;
            }
            match socket.send_to(bytes, source, DatagramClass::Media) {
                Ok(()) => {
                    metrics.datagrams_sent.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .fragments_retransmitted
                        .fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => {
                    metrics.send_failures.fetch_add(1, Ordering::Relaxed);
                    false
                }
            }
        });
    }

    fn poll_assembler_nacks(&mut self) {
        let now = self.clock.now();
        let ticks_per_second = self.clock.ticks_per_second().get();
        let loop_self = self.loop_self;
        let loop_endpoint = self.loop_endpoint;
        let local_member_id = self.local_member_id;
        let local_run_id = self.local_run_id;
        let mut encoded = [0_u8; rlb_media::FIXED_HEADER_BYTES];
        let mut pending: Vec<(Endpoint, MediaIdentity, Vec<u8>)> = Vec::new();
        for session in self.sessions.values_mut() {
            let cursor = session.ingress.metrics.read_cursor.load(Ordering::Relaxed);
            let target_frames = session
                .ingress
                .metrics
                .target_frames
                .load(Ordering::Relaxed);
            let mut candidates = [NackCandidate::default(); NACK_CANDIDATE_CAP];
            let count = session.assembler.poll_nacks(now, &mut candidates, |start| {
                nack_deadline(
                    now,
                    ticks_per_second,
                    cursor,
                    start,
                    self.rtt_ticks,
                    target_frames,
                )
            });
            if count == 0 {
                continue;
            }
            let Some(peer) = session.peer_endpoint.or(loop_self.then_some(loop_endpoint)) else {
                continue;
            };
            for candidate in candidates.iter().take(count) {
                let nack_id = session.next_nack_id;
                session.next_nack_id = session.next_nack_id.wrapping_add(1);
                let record = NackRecord {
                    identity: MediaIdentity {
                        room_id: session.identity.room_id,
                        sender_member_id: local_member_id,
                        sender_run_id: local_run_id,
                        source_id: session.identity.source_id,
                        stream_epoch: session.identity.stream_epoch,
                    },
                    target_unit_sequence: candidate.unit_sequence,
                    unit_start_sample: candidate.start_sample,
                    missing_fragment_mask: candidate.missing_fragment_mask,
                    nack_id,
                };
                match encode_nack_into(record, &mut encoded) {
                    Ok(written) => {
                        pending.push((peer, session.identity, encoded[..written].to_vec()));
                    }
                    Err(_) => {
                        self.metrics.parse_rejected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        for (endpoint, remote_identity, datagram) in pending {
            if self.send_stopped.load(Ordering::Acquire)
                || self.ingress_projection(endpoint, remote_identity).is_err()
            {
                continue;
            }
            self.send_media_one(endpoint, &datagram);
            self.metrics.nacks_sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Installs a jitter reader for a local loop source before the first packet.
    pub fn prepare_local_loop(
        &mut self,
        identity: MediaIdentity,
        format: SourceFormat,
        publisher: &MixGraphPublisher,
    ) -> Result<(), EngineError> {
        if !self.loop_self
            || self.room_id != Some(identity.room_id)
            || identity.sender_member_id != self.local_member_id
            || identity.sender_run_id != self.local_run_id
        {
            return Err(EngineError::Persist("local media identity is out of scope"));
        }
        self.ensure_session(identity, format.channels(), Some(publisher))?;
        Ok(())
    }

    fn ensure_session(
        &mut self,
        identity: MediaIdentity,
        channels: u8,
        publisher: Option<&MixGraphPublisher>,
    ) -> Result<(), EngineError> {
        if let Some(existing) = self.sessions.get(&identity.source_id)
            && existing.identity == identity
        {
            return Ok(());
        }
        self.remove_session(identity.source_id, publisher);
        let (ingress, reader) =
            JitterIngress::pair(channels, self.clock.clone(), self.receive_mode)?;
        let _ = ingress.try_push(IngressCommand::Enable);
        let mut installed = false;
        if let Some(publisher) = publisher {
            installed = publisher
                .try_publish(MixGraphCommand::AddSource {
                    config: SourceRenderConfig {
                        member_id: identity.sender_member_id,
                        source_id: identity.source_id,
                        channels,
                        routed: !self.loop_self,
                        member_solo_eligible: false,
                        source_mix: SourceMix::default(),
                        member_mix: MemberMix::default(),
                    },
                    reader,
                })
                .is_ok();
        } else {
            drop(reader);
        }
        self.sessions.insert(
            identity.source_id,
            ReceiveSession {
                identity,
                assembler: FragmentAssembler::new(
                    identity,
                    NackPlannerConfig::default().nanosecond_domain(),
                ),
                ingress,
                installed,
                peer_endpoint: None,
                next_nack_id: 0,
                channels,
            },
        );
        self.pending_mix.push(identity.source_id);
        Ok(())
    }

    /// Source identities that joined the mix graph since the last take.
    pub fn take_mix_refresh(&mut self) -> Vec<SourceId> {
        std::mem::take(&mut self.pending_mix)
    }

    /// Snapshot of one source's jitter ingress counters.
    #[must_use]
    pub fn reader_metrics(&self, source_id: SourceId) -> Option<Arc<JitterReaderMetrics>> {
        self.sessions
            .get(&source_id)
            .map(|session| Arc::clone(&session.ingress.metrics))
    }

    /// Stops the receive thread.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.recv_join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for MediaPath {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn nack_deadline(
    now: MonotonicInstant,
    ticks_per_second: u64,
    read_cursor: u64,
    start_sample: u64,
    rtt_ticks: u64,
    target_frames: u16,
) -> Option<MonotonicInstant> {
    if read_cursor != u64::MAX && start_sample < read_cursor {
        return None;
    }
    let playout_ticks = u64::from(target_frames)
        .saturating_mul(ticks_per_second)
        .checked_div(u64::from(rlb_model::SAMPLE_RATE_HZ))
        .unwrap_or(0);
    let target_ticks = playout_ticks.max(rtt_ticks.saturating_mul(3) / 2);
    let delay_ticks = if read_cursor == u64::MAX {
        target_ticks
    } else {
        start_sample
            .saturating_sub(read_cursor)
            .saturating_mul(ticks_per_second)
            .checked_div(u64::from(rlb_model::SAMPLE_RATE_HZ))
            .unwrap_or(target_ticks)
            .max(target_ticks)
    };
    Some(MonotonicInstant::from_ticks(
        now.ticks().saturating_add(delay_ticks),
    ))
}

fn projection_generation_changed(
    previous: MediaSourceProjection,
    current: MediaSourceProjection,
) -> bool {
    previous != current
}

fn outbound_identity(record: &OutboundRecord) -> MediaIdentity {
    match record {
        OutboundRecord::Media(set) => set.identity(),
        OutboundRecord::Gap { record, .. } => record.identity,
        OutboundRecord::EpochEnd { record, .. } => record.identity,
    }
}

fn gap_kind(reason: GapReason) -> GapKind {
    match reason {
        GapReason::CaptureOverrun => GapKind::CaptureOverrun,
        GapReason::SendQueueOverflow => GapKind::SendQueueOverflow,
        GapReason::SendDeadlineDrop => GapKind::SendDeadlineDrop,
        GapReason::KnownAdapterGap => GapKind::KnownAdapterGap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_codec::WireCodec;
    use rlb_media::{MediaIdentity, MediaPacketizer};
    use rlb_model::{
        CatalogRevision, Endpoint, MemberId, QualityProfile, RoomId, RunId, SourceFormat, SourceId,
        SourceKind, StreamEpoch, SubscriptionRevision,
    };
    use rlb_session::{MediaReceivePlan, MediaSourceProjection, SendRecipient, SourceSendPlan};

    fn identity() -> MediaIdentity {
        MediaIdentity {
            room_id: RoomId::from_bytes([1; 16]).expect("room"),
            sender_member_id: MemberId::from_bytes([2; 16]).expect("member"),
            sender_run_id: RunId::from_bytes([3; 16]).expect("run"),
            source_id: SourceId::from_bytes([4; 16]).expect("source"),
            stream_epoch: StreamEpoch::from_bytes([5; 16]).expect("epoch"),
        }
    }

    fn endpoint(port: u16) -> Endpoint {
        Endpoint::new(Ipv4Addr::LOCALHOST, port).expect("endpoint")
    }

    fn projection(
        identity: MediaIdentity,
        media_endpoint: Endpoint,
        catalog_revision: u64,
        subscription_revision: u64,
        subscription_accepted: bool,
    ) -> MediaSourceProjection {
        MediaSourceProjection {
            member_id: identity.sender_member_id,
            run_id: identity.sender_run_id,
            source_id: identity.source_id,
            active_epoch: identity.stream_epoch,
            catalog_revision: CatalogRevision::new(catalog_revision),
            media_endpoint,
            format: SourceFormat::for_source(SourceKind::Microphone, QualityProfile::PcmF32V1),
            subscription_revision: Some(SubscriptionRevision::new(subscription_revision)),
            subscription_accepted,
        }
    }

    fn packetizer(identity: MediaIdentity) -> MediaPacketizer {
        let format = SourceFormat::for_source(SourceKind::Microphone, QualityProfile::PcmF32V1);
        MediaPacketizer::new_nanosecond_domain(
            identity,
            WireCodec::from(format.profile()),
            format.channels(),
        )
        .expect("packetizer")
    }

    #[test]
    fn unjoined_path_neither_sends_nor_retains_history() {
        let identity = identity();
        let mut path = MediaPath::bind(
            None,
            identity.sender_member_id,
            identity.sender_run_id,
            true,
            StdMonotonic::new(),
        )
        .expect("bind");
        let mut packetizer = packetizer(identity);
        let samples = vec![0.25_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES)];
        let set = packetizer.packetize(0, &samples).expect("packetize");

        path.submit(OutboundRecord::Media(set));

        assert_eq!(path.room_id(), None);
        assert!(path.histories.is_empty());
        assert_eq!(path.metrics.datagrams_sent.load(Ordering::Relaxed), 0);
        path.shutdown();
    }

    #[test]
    fn joining_and_switching_room_clears_all_media_state() {
        let identity = identity();
        let next_room = RoomId::from_bytes([9; 16]).expect("next room");
        let mut path = MediaPath::bind(
            None,
            identity.sender_member_id,
            identity.sender_run_id,
            true,
            StdMonotonic::new(),
        )
        .expect("bind");
        path.apply_receive_plan(
            MediaReceivePlan {
                room_id: Some(identity.room_id),
                sources: Vec::new(),
            },
            None,
        );
        path.ensure_session(identity, 1, None).expect("session");
        let retired_metrics = path
            .reader_metrics(identity.source_id)
            .expect("session metrics");
        path.bind_peer(
            identity.sender_member_id,
            identity.sender_run_id,
            endpoint(31_001),
        );
        let mut packetizer = packetizer(identity);
        let samples = vec![0.25_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES)];
        path.submit(OutboundRecord::Media(
            packetizer.packetize(0, &samples).expect("packetize"),
        ));
        assert!(!path.sessions.is_empty());
        assert!(!path.histories.is_empty());
        assert!(!path.peer_transports.is_empty());

        path.apply_receive_plan(
            MediaReceivePlan {
                room_id: Some(next_room),
                sources: Vec::new(),
            },
            None,
        );

        assert_eq!(path.room_id(), Some(next_room));
        assert!(path.sessions.is_empty());
        assert!(path.histories.is_empty());
        assert!(path.peer_transports.is_empty());
        assert!(path.source_projections.is_empty());
        assert!(path.retired_epochs.is_empty());
        assert!(!path.send_stopped.load(Ordering::Acquire));
        assert!(retired_metrics.retired.load(Ordering::Acquire));
        assert!(!retired_metrics.enabled.load(Ordering::Acquire));
        path.shutdown();
    }

    #[test]
    fn endpoint_identity_subscription_and_epoch_are_all_required() {
        let remote = identity();
        let local_member = MemberId::from_bytes([7; 16]).expect("local member");
        let local_run = RunId::from_bytes([8; 16]).expect("local run");
        let expected_endpoint = endpoint(31_101);
        let forged_endpoint = endpoint(31_102);
        let mut path = MediaPath::bind(
            Some(remote.room_id),
            local_member,
            local_run,
            false,
            StdMonotonic::new(),
        )
        .expect("bind");
        path.bind_peer(
            remote.sender_member_id,
            remote.sender_run_id,
            expected_endpoint,
        );
        path.apply_receive_plan(
            MediaReceivePlan {
                room_id: Some(remote.room_id),
                sources: vec![projection(remote, expected_endpoint, 1, 1, false)],
            },
            None,
        );

        let subscription_rejection = path
            .ingress_projection(expected_endpoint, remote)
            .expect_err("unacknowledged subscription");
        assert_eq!(subscription_rejection, IngressRejection::Subscription);
        path.note_ingress_rejection(subscription_rejection);
        assert!(path.sessions.is_empty());
        let endpoint_rejection = path
            .ingress_projection(forged_endpoint, remote)
            .expect_err("forged endpoint");
        assert_eq!(endpoint_rejection, IngressRejection::Endpoint);
        path.note_ingress_rejection(endpoint_rejection);
        let member_rejection = path
            .ingress_projection(
                expected_endpoint,
                MediaIdentity {
                    sender_member_id: local_member,
                    ..remote
                },
            )
            .expect_err("forged member");
        assert_eq!(member_rejection, IngressRejection::Member);
        path.note_ingress_rejection(member_rejection);
        let run_rejection = path
            .ingress_projection(
                expected_endpoint,
                MediaIdentity {
                    sender_run_id: local_run,
                    ..remote
                },
            )
            .expect_err("forged run");
        assert_eq!(run_rejection, IngressRejection::StaleRun);
        path.note_ingress_rejection(run_rejection);
        let unannounced_epoch = StreamEpoch::from_bytes([6; 16]).expect("unannounced epoch");
        let epoch_rejection = path
            .ingress_projection(
                expected_endpoint,
                MediaIdentity {
                    stream_epoch: unannounced_epoch,
                    ..remote
                },
            )
            .expect_err("unannounced epoch");
        assert_eq!(epoch_rejection, IngressRejection::EpochUnannounced);
        path.note_ingress_rejection(epoch_rejection);
        assert_eq!(path.metrics.endpoint_mismatch.load(Ordering::Relaxed), 1);
        assert_eq!(path.metrics.member_mismatch.load(Ordering::Relaxed), 1);
        assert_eq!(path.metrics.stale_run_packet.load(Ordering::Relaxed), 1);
        assert_eq!(
            path.metrics.media_epoch_unannounced.load(Ordering::Relaxed),
            1
        );
        assert_eq!(path.metrics.subscription_denied.load(Ordering::Relaxed), 1);

        path.apply_receive_plan(
            MediaReceivePlan {
                room_id: Some(remote.room_id),
                sources: vec![projection(remote, expected_endpoint, 1, 1, true)],
            },
            None,
        );
        assert!(
            path.ingress_projection(expected_endpoint, remote)
                .expect("accepted projection")
                .is_some()
        );
        path.ensure_session(remote, 1, None)
            .expect("accepted receive session");

        let next_epoch = StreamEpoch::from_bytes([10; 16]).expect("next epoch");
        let next_identity = MediaIdentity {
            stream_epoch: next_epoch,
            ..remote
        };
        path.apply_receive_plan(
            MediaReceivePlan {
                room_id: Some(remote.room_id),
                sources: vec![projection(next_identity, expected_endpoint, 2, 2, true)],
            },
            None,
        );
        assert!(path.sessions.is_empty());
        path.ensure_session(next_identity, 1, None)
            .expect("new epoch session");
        let stale_epoch = path
            .ingress_projection(expected_endpoint, remote)
            .expect_err("retired epoch");
        assert_eq!(stale_epoch, IngressRejection::StaleEpoch);
        path.note_ingress_rejection(stale_epoch);
        assert_eq!(path.metrics.stale_epoch_packet.load(Ordering::Relaxed), 1);
        assert_eq!(
            path.ingress_projection(
                expected_endpoint,
                MediaIdentity {
                    stream_epoch: unannounced_epoch,
                    ..remote
                }
            ),
            Err(IngressRejection::EpochUnannounced)
        );
        assert!(
            path.ingress_projection(expected_endpoint, next_identity)
                .expect("new epoch")
                .is_some()
        );
        assert_eq!(
            path.sessions
                .get(&remote.source_id)
                .map(|session| session.identity),
            Some(next_identity)
        );
        path.shutdown();
    }

    #[test]
    fn nack_deadline_tracks_each_session_target() {
        let now = MonotonicInstant::from_ticks(100);
        let ticks_per_second = 1_000_000_000;
        for (target_frames, target_ns) in
            [(960, 20_000_000), (1_440, 30_000_000), (2_400, 50_000_000)]
        {
            assert_eq!(
                nack_deadline(now, ticks_per_second, u64::MAX, 0, 0, target_frames,),
                Some(MonotonicInstant::from_ticks(
                    now.ticks().saturating_add(target_ns)
                ))
            );
        }
        assert_eq!(nack_deadline(now, ticks_per_second, 480, 479, 0, 960), None);
    }

    #[test]
    fn emergency_nack_and_fanout_recheck_current_recipient_gate() {
        let local = identity();
        let recipient_member = MemberId::from_bytes([11; 16]).expect("recipient member");
        let recipient_run = RunId::from_bytes([12; 16]).expect("recipient run");
        let recipient_endpoint = endpoint(31_201);
        let recipient = SendRecipient {
            member_id: recipient_member,
            run_id: recipient_run,
            endpoint: recipient_endpoint,
        };
        let send_plan = SendPlan {
            catalog_revision: CatalogRevision::new(1),
            sources: vec![SourceSendPlan {
                source_id: local.source_id,
                stream_epoch: local.stream_epoch,
                recipients: vec![recipient],
            }],
        };
        let mut path = MediaPath::bind(
            Some(local.room_id),
            local.sender_member_id,
            local.sender_run_id,
            false,
            StdMonotonic::new(),
        )
        .expect("bind");
        path.bind_peer(recipient_member, recipient_run, recipient_endpoint);
        path.set_send_plan(send_plan.clone());
        let mut packetizer = packetizer(local);
        let samples = vec![0.25_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES)];
        path.submit(OutboundRecord::Media(
            packetizer.packetize(0, &samples).expect("first unit"),
        ));
        let sent_with_gate = path.metrics.datagrams_sent.load(Ordering::Relaxed);
        assert!(sent_with_gate > 0);
        assert!(!path.histories.is_empty());

        let nack = NackRecord {
            identity: MediaIdentity {
                sender_member_id: recipient_member,
                sender_run_id: recipient_run,
                ..local
            },
            target_unit_sequence: 0,
            unit_start_sample: 0,
            missing_fragment_mask: 1,
            nack_id: 1,
        };
        path.set_send_gate(true);
        assert!(path.histories.is_empty());
        path.handle_nack(recipient_endpoint, nack);
        assert_eq!(
            path.metrics.fragments_retransmitted.load(Ordering::Relaxed),
            0
        );

        path.set_send_gate(false);
        path.submit(OutboundRecord::Media(
            packetizer.packetize(480, &samples).expect("second unit"),
        ));
        let current_nack = NackRecord {
            target_unit_sequence: 1,
            unit_start_sample: 480,
            nack_id: 2,
            ..nack
        };
        path.handle_nack(recipient_endpoint, current_nack);
        assert!(path.metrics.fragments_retransmitted.load(Ordering::Relaxed) > 0);
        let retransmitted = path.metrics.fragments_retransmitted.load(Ordering::Relaxed);

        path.handle_nack(endpoint(31_202), current_nack);
        assert_eq!(
            path.metrics.fragments_retransmitted.load(Ordering::Relaxed),
            retransmitted
        );
        assert_eq!(path.metrics.endpoint_mismatch.load(Ordering::Relaxed), 1);

        path.set_send_plan(SendPlan {
            catalog_revision: CatalogRevision::new(1),
            sources: Vec::new(),
        });
        path.handle_nack(recipient_endpoint, current_nack);
        assert_eq!(
            path.metrics.fragments_retransmitted.load(Ordering::Relaxed),
            retransmitted
        );
        let sent_before_revoked_fanout = path.metrics.datagrams_sent.load(Ordering::Relaxed);
        path.submit(OutboundRecord::Media(
            packetizer.packetize(960, &samples).expect("revoked unit"),
        ));
        assert_eq!(
            path.metrics.datagrams_sent.load(Ordering::Relaxed),
            sent_before_revoked_fanout
        );

        path.set_send_plan(send_plan);
        path.set_send_gate(true);
        path.submit(OutboundRecord::Media(
            packetizer
                .packetize(1_440, &samples)
                .expect("emergency unit"),
        ));
        assert_eq!(
            path.metrics.datagrams_sent.load(Ordering::Relaxed),
            sent_before_revoked_fanout
        );
        path.shutdown();
    }

    #[test]
    fn loopback_socket_assembles_one_encoded_unit() {
        let identity = identity();
        let mut path = MediaPath::bind(
            Some(identity.room_id),
            identity.sender_member_id,
            identity.sender_run_id,
            true,
            StdMonotonic::new(),
        )
        .expect("bind");
        let format = SourceFormat::for_source(SourceKind::Microphone, QualityProfile::PcmF32V1);
        let mut packetizer = MediaPacketizer::new_nanosecond_domain(
            identity,
            WireCodec::from(format.profile()),
            format.channels(),
        )
        .expect("packetizer");
        let samples = vec![0.1_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES)];
        let set = packetizer.packetize(0, &samples).expect("packetize");
        path.submit(OutboundRecord::Media(set));
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline
            && path.metrics.units_assembled.load(Ordering::Relaxed) == 0
        {
            path.poll_receive(None);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            path.metrics.datagrams_sent.load(Ordering::Relaxed) >= 1,
            "expected at least one datagram send"
        );
        assert_eq!(path.metrics.units_assembled.load(Ordering::Relaxed), 1);
        path.shutdown();
    }

    #[test]
    fn missing_fragment_recovers_through_nack_and_history() {
        let identity = identity();
        let mut path = MediaPath::bind(
            Some(identity.room_id),
            identity.sender_member_id,
            identity.sender_run_id,
            true,
            StdMonotonic::new(),
        )
        .expect("bind");
        let format = SourceFormat::for_source(SourceKind::Microphone, QualityProfile::PcmF32V1);
        let mut packetizer = MediaPacketizer::new_nanosecond_domain(
            identity,
            WireCodec::from(format.profile()),
            format.channels(),
        )
        .expect("packetizer");
        let samples = vec![0.1_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES)];
        let set = packetizer.packetize(0, &samples).expect("packetize");
        assert!(
            set.fragment_count() >= 2,
            "this recovery test needs a multi-fragment unit"
        );
        path.submit_media(OutboundRecord::Media(set), 0b0001);
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline
            && path.metrics.units_assembled.load(Ordering::Relaxed) == 0
        {
            path.poll_receive(None);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            path.metrics.units_assembled.load(Ordering::Relaxed),
            1,
            "nacks_sent={} nacks_received={} retransmitted={}",
            path.metrics.nacks_sent.load(Ordering::Relaxed),
            path.metrics.nacks_received.load(Ordering::Relaxed),
            path.metrics.fragments_retransmitted.load(Ordering::Relaxed)
        );
        assert!(path.metrics.nacks_sent.load(Ordering::Relaxed) >= 1);
        assert!(path.metrics.nacks_received.load(Ordering::Relaxed) >= 1);
        assert!(path.metrics.fragments_retransmitted.load(Ordering::Relaxed) >= 1);
        path.shutdown();
    }

    #[test]
    fn incoming_queue_overflow_emits_timeline_gap() {
        let identity = identity();
        let mut path = MediaPath::bind(
            Some(identity.room_id),
            identity.sender_member_id,
            identity.sender_run_id,
            true,
            StdMonotonic::new(),
        )
        .expect("bind");
        let format = SourceFormat::for_source(SourceKind::Microphone, QualityProfile::PcmF32V1);
        let mut packetizer = MediaPacketizer::new_nanosecond_domain(
            identity,
            WireCodec::from(format.profile()),
            format.channels(),
        )
        .expect("packetizer");
        let samples = vec![0.1_f32; usize::from(rlb_model::MEDIA_UNIT_FRAMES)];
        let set = packetizer.packetize(0, &samples).expect("packetize");
        path.submit(OutboundRecord::Media(set));
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline
            && path.metrics.units_assembled.load(Ordering::Relaxed) == 0
        {
            path.poll_receive(None);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(path.metrics.units_assembled.load(Ordering::Relaxed), 1);
        path.metrics.ingress_overflow.store(true, Ordering::Relaxed);
        path.poll_receive(None);
        let dropped = path
            .sessions
            .values()
            .map(|session| {
                session
                    .ingress
                    .metrics
                    .ingress_dropped
                    .load(Ordering::Relaxed)
            })
            .sum::<u64>();
        let applied = path
            .sessions
            .values()
            .map(|session| {
                session
                    .ingress
                    .metrics
                    .ingress_applied
                    .load(Ordering::Relaxed)
            })
            .sum::<u64>();
        assert!(
            applied >= 1 || dropped >= 1,
            "overflow GAP must be stored or applied"
        );
        path.shutdown();
    }
}
