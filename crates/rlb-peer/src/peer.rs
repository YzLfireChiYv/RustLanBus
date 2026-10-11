use std::{collections::VecDeque, fmt};

use rlb_model::{BindNonce, DirectSessionId, HelloNonce, IdGenerationError, MemberId, RunId};

use crate::{
    OriginalAudio,
    media::{self, Header, Seen},
    wire::{self, Ack, Body, Control, Intent, Snapshot},
};

const RETRY_MS: u64 = 250;
const HEARTBEAT_MS: u64 = 500;
const TIMEOUT_MS: u64 = 3_000;
const MAX_CONTROL_QUEUE: usize = 8;
const MAX_ORIGINAL_FRAMES: usize = 4_096;

/// A rejected datagram or caller precondition; no accepted audio is sanitized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerError {
    SameMember,
    RandomSource,
    TimeWentBackwards,
    CounterExhausted,
    DatagramTooLarge,
    MalformedControl,
    WrongPeer,
    InvalidAudio,
    NonFiniteSample,
    NotSending,
    SourceFormatChanged,
    SourceTimelineWentBackwards,
}

impl fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SameMember => "local and remote member identities must differ",
            Self::RandomSource => "operating-system randomness is unavailable",
            Self::TimeWentBackwards => "supplied monotonic milliseconds went backwards",
            Self::CounterExhausted => "peer counter exhausted; start a new process run",
            Self::DatagramTooLarge => "datagram exceeds the 1200-byte contract",
            Self::MalformedControl => "invalid current-state control message",
            Self::WrongPeer => "control message belongs to a different peer",
            Self::InvalidAudio => "invalid mono/stereo F32 audio or frame count",
            Self::NonFiniteSample => "non-finite audio samples are rejected",
            Self::NotSending => "directed sending permission has not been confirmed",
            Self::SourceFormatChanged => "a source format change requires a fresh source epoch",
            Self::SourceTimelineWentBackwards => {
                "local source frame positions must progress forward"
            }
        })
    }
}

impl std::error::Error for PeerError {}

impl From<IdGenerationError> for PeerError {
    fn from(_: IdGenerationError) -> Self {
        Self::RandomSource
    }
}

/// Configured choices and current transport readiness have distinct fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerStatus {
    pub connected: bool,
    pub send_ready: bool,
    pub receive_ready: bool,
    pub local: Intent,
    pub remote: Option<Intent>,
    pub session: Option<DirectSessionId>,
    pub remote_run: Option<RunId>,
}

#[derive(Clone)]
struct Pending {
    session: DirectSessionId,
    remote_run: RunId,
    remote_contact: HelloNonce,
    remote: Snapshot,
    nonce: Option<BindNonce>,
    started_ms: u64,
    retried_ms: u64,
}

struct Session {
    id: DirectSessionId,
    remote_run: RunId,
    remote_contact: HelloNonce,
    remote: Snapshot,
    peer_ack: Ack,
    ready_nonce: Option<BindNonce>,
    last_seen_ms: u64,
    sent_ms: u64,
    next_sequence: u64,
    seen: Seen,
}

#[derive(Clone, Copy)]
struct RemoteSource {
    run: RunId,
    epoch: u64,
    format: (u32, u8),
}

/// One peer pair. All waits, sockets, saved JSON, devices and playback live in
/// the application. Call `tick` at the next scheduled deadline, and drain the
/// bounded outgoing control queue after each operation.
pub struct Peer {
    local_id: MemberId,
    remote_id: MemberId,
    run: RunId,
    contact: HelloNonce,
    local: Snapshot,
    active: Option<Session>,
    pending: Option<Pending>,
    outgoing: VecDeque<Vec<u8>>,
    now_ms: u64,
    hello_ms: u64,
    local_source_epoch: u64,
    local_format: Option<(u32, u8)>,
    local_end_frame: Option<u64>,
    remote_source: Option<RemoteSource>,
}

impl Peer {
    pub fn new(
        local: MemberId,
        remote: MemberId,
        run: RunId,
        intent: Intent,
        now_ms: u64,
    ) -> Result<Self, PeerError> {
        if local == remote {
            return Err(PeerError::SameMember);
        }
        let mut peer = Self {
            local_id: local,
            remote_id: remote,
            run,
            contact: HelloNonce::generate()?,
            local: Snapshot::new(intent),
            active: None,
            pending: None,
            outgoing: VecDeque::with_capacity(MAX_CONTROL_QUEUE),
            now_ms,
            hello_ms: now_ms,
            local_source_epoch: 1,
            local_format: None,
            local_end_frame: None,
            remote_source: None,
        };
        peer.queue_hello()?;
        Ok(peer)
    }

    #[must_use]
    pub fn status(&self) -> PeerStatus {
        let session = self.active.as_ref();
        PeerStatus {
            connected: session.is_some(),
            send_ready: session.is_some_and(|s| {
                self.local.intent.send
                    && s.remote.intent.receive
                    && s.peer_ack.send_generation == self.local.send_generation
            }),
            receive_ready: session
                .is_some_and(|s| self.local.intent.receive && s.remote.intent.send),
            local: self.local.intent,
            remote: session.map(|s| s.remote.intent),
            session: session.map(|s| s.id),
            remote_run: session.map(|s| s.remote_run),
        }
    }

    /// Millisecond deadline for retry, heartbeat or disconnect detection.
    #[must_use]
    pub fn next_deadline_ms(&self) -> u64 {
        let mut next = if self.active.is_none() && self.pending.is_none() {
            self.hello_ms.saturating_add(RETRY_MS)
        } else {
            u64::MAX
        };
        if let Some(p) = &self.pending {
            next = next
                .min(p.retried_ms.saturating_add(RETRY_MS))
                .min(p.started_ms.saturating_add(TIMEOUT_MS));
        }
        if let Some(s) = &self.active {
            let interval = if s.ready_nonce.is_some() {
                RETRY_MS
            } else {
                HEARTBEAT_MS
            };
            next = next
                .min(s.sent_ms.saturating_add(interval))
                .min(s.last_seen_ms.saturating_add(TIMEOUT_MS));
        }
        next
    }

    pub fn tick(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if self
            .active
            .as_ref()
            .is_some_and(|s| now_ms.saturating_sub(s.last_seen_ms) >= TIMEOUT_MS)
        {
            self.reconnect(now_ms)?;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now_ms.saturating_sub(p.started_ms) >= TIMEOUT_MS)
        {
            self.pending = None;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now_ms.saturating_sub(p.retried_ms) >= RETRY_MS)
        {
            self.queue_pending()?;
            self.pending.as_mut().unwrap().retried_ms = now_ms;
        } else if self.active.is_none()
            && self.pending.is_none()
            && now_ms.saturating_sub(self.hello_ms) >= RETRY_MS
        {
            self.queue_hello()?;
        }
        if let Some(s) = &self.active {
            let interval = if s.ready_nonce.is_some() {
                RETRY_MS
            } else {
                HEARTBEAT_MS
            };
            if now_ms.saturating_sub(s.sent_ms) >= interval {
                if s.ready_nonce.is_some() {
                    self.queue_ready()?;
                } else {
                    self.queue_state()?;
                }
            }
        }
        Ok(())
    }

    /// Local refusal applies before any outgoing notification. Changing one
    /// direction preserves the unrelated direction's generation and session.
    pub fn set_intent(&mut self, intent: Intent, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if intent == self.local.intent {
            return Ok(());
        }
        let revision = self
            .local
            .revision
            .checked_add(1)
            .ok_or(PeerError::CounterExhausted)?;
        let send_generation = self
            .local
            .send_generation
            .checked_add(u64::from(intent.send != self.local.intent.send))
            .ok_or(PeerError::CounterExhausted)?;
        let receive_generation = self
            .local
            .receive_generation
            .checked_add(u64::from(intent.receive != self.local.intent.receive))
            .ok_or(PeerError::CounterExhausted)?;
        self.local = Snapshot {
            intent,
            revision,
            send_generation,
            receive_generation,
        };
        if self.active.is_some() {
            self.queue_state()?;
        }
        if self.pending.is_some() {
            self.queue_pending()?;
        }
        Ok(())
    }

    /// Fresh contact proof and media session; saved intent and the local source
    /// timeline remain. Queued old control messages are discarded.
    pub fn reconnect(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        let contact = HelloNonce::generate()?;
        self.contact = contact;
        self.active = None;
        self.pending = None;
        self.outgoing.clear();
        self.queue_hello()
    }

    pub fn take_control(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }

    /// Begin a fresh local capture timeline after a device/source restart or
    /// format change. Call before encoding the new source's first frame. The
    /// process-run epoch advances even while disconnected; session and both
    /// directed permissions remain usable. Receivers adopt it on the first
    /// accepted packet and reject late packets from preceding source epochs.
    pub fn reset_source(&mut self) -> Result<u64, PeerError> {
        let epoch = self
            .local_source_epoch
            .checked_add(1)
            .ok_or(PeerError::CounterExhausted)?;
        self.local_source_epoch = epoch;
        self.local_format = None;
        self.local_end_frame = None;
        Ok(epoch)
    }

    /// Only bytes from this peer's configured UDP endpoint should reach here.
    /// Valid stale sessions/control are ignored. Malformed datagrams return an
    /// error. Accepted audio has no internal queue or local publication path.
    pub fn ingest(
        &mut self,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<Option<OriginalAudio>, PeerError> {
        self.observe_time(now_ms)?;
        if bytes.starts_with(media::MEDIA_MAGIC) {
            return self.ingest_audio(bytes);
        }
        let message = wire::decode(bytes)?;
        if message.from != self.remote_id || message.to != self.local_id {
            return Err(PeerError::WrongPeer);
        }
        self.ingest_control(message)?;
        Ok(None)
    }

    /// Encode captured local original F32, split into independently useful
    /// datagrams. Callers own sample acquisition; passing decoded peer audio
    /// here would violate the source-only application contract. This diagnostic
    /// allocating API accepts up to 4096 frames per call. Realtime adapters will
    /// require a caller-owned output storage variant in a later slice.
    pub fn encode_original(
        &mut self,
        sample_rate: u32,
        channels: u8,
        start_frame: u64,
        samples: &[f32],
    ) -> Result<Vec<Vec<u8>>, PeerError> {
        if !self.status().send_ready {
            return Err(PeerError::NotSending);
        }
        if !matches!(channels, 1 | 2)
            || !(8_000..=384_000).contains(&sample_rate)
            || samples.is_empty()
            || samples.len() % usize::from(channels) != 0
        {
            return Err(PeerError::InvalidAudio);
        }
        let frames = samples.len() / usize::from(channels);
        if frames > MAX_ORIGINAL_FRAMES {
            return Err(PeerError::InvalidAudio);
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(PeerError::NonFiniteSample);
        }
        if self
            .local_format
            .is_some_and(|format| format != (sample_rate, channels))
        {
            return Err(PeerError::SourceFormatChanged);
        }
        if self.local_end_frame.is_some_and(|end| start_frame < end) {
            return Err(PeerError::SourceTimelineWentBackwards);
        }
        let end_frame = start_frame
            .checked_add(frames as u64)
            .ok_or(PeerError::InvalidAudio)?;
        let max_frames =
            (media::MAX_DATAGRAM_BYTES - media::HEADER_BYTES) / (4 * usize::from(channels));
        let count = frames.div_ceil(max_frames);
        let active = self.active.as_mut().unwrap();
        active
            .next_sequence
            .checked_add(count as u64)
            .ok_or(PeerError::CounterExhausted)?;
        let mut packets = Vec::with_capacity(count);
        for (index, chunk) in samples
            .chunks(max_frames * usize::from(channels))
            .enumerate()
        {
            let header = Header {
                session: active.id,
                send_generation: self.local.send_generation,
                receive_generation: active.remote.receive_generation,
                source_epoch: self.local_source_epoch,
                sequence: active.next_sequence,
                start_frame: start_frame + (index * max_frames) as u64,
                sample_rate,
                channels,
                frames: (chunk.len() / usize::from(channels)) as u16,
            };
            packets.push(media::encode(header, chunk)?);
            active.next_sequence += 1;
        }
        self.local_format = Some((sample_rate, channels));
        self.local_end_frame = Some(end_frame);
        Ok(packets)
    }

    fn observe_time(&mut self, now_ms: u64) -> Result<(), PeerError> {
        if now_ms < self.now_ms {
            return Err(PeerError::TimeWentBackwards);
        }
        self.now_ms = now_ms;
        Ok(())
    }

    fn leader(&self) -> bool {
        self.local_id < self.remote_id
    }

    fn queue(&mut self, body: Body) -> Result<(), PeerError> {
        let bytes = wire::encode(&Control {
            from: self.local_id,
            to: self.remote_id,
            run: self.run,
            body,
        })?;
        // Identical retransmissions do not grow the queue. Latest complete
        // snapshots supersede dropped control; retries recover packet loss.
        if self.outgoing.iter().any(|queued| queued == &bytes) {
            return Ok(());
        }
        if self.outgoing.len() == MAX_CONTROL_QUEUE {
            self.outgoing.pop_front();
        }
        self.outgoing.push_back(bytes);
        Ok(())
    }

    fn queue_hello(&mut self) -> Result<(), PeerError> {
        self.hello_ms = self.now_ms;
        self.queue(Body::Hello {
            contact: self.contact,
            state: self.local,
        })
    }

    fn queue_pending(&mut self) -> Result<(), PeerError> {
        let pending = self.pending.as_ref().unwrap();
        let body = if let Some(nonce) = pending.nonce {
            Body::Confirm {
                session: pending.session,
                target_run: pending.remote_run,
                contact: self.contact,
                target_contact: pending.remote_contact,
                nonce,
                state: self.local,
            }
        } else {
            Body::Offer {
                session: pending.session,
                target_run: pending.remote_run,
                contact: self.contact,
                target_contact: pending.remote_contact,
                state: self.local,
            }
        };
        self.queue(body)
    }

    fn queue_state(&mut self) -> Result<(), PeerError> {
        let active = self.active.as_mut().unwrap();
        active.sent_ms = self.now_ms;
        let body = Body::State {
            session: active.id,
            target_run: active.remote_run,
            state: self.local,
            ack: active.remote.ack(),
        };
        self.queue(body)
    }

    fn queue_ready(&mut self) -> Result<(), PeerError> {
        let active = self.active.as_mut().unwrap();
        active.sent_ms = self.now_ms;
        let body = Body::Ready {
            session: active.id,
            target_run: active.remote_run,
            contact: self.contact,
            target_contact: active.remote_contact,
            nonce: active.ready_nonce.unwrap(),
            state: self.local,
            ack: active.remote.ack(),
        };
        self.queue(body)
    }

    fn activate(
        &mut self,
        pending: Pending,
        remote: Snapshot,
        ack: Ack,
        ready_nonce: Option<BindNonce>,
    ) {
        // A recontact in the same process run preserves the source high-water
        // mark. Only an actually activated replacement run can restart epochs.
        if self
            .remote_source
            .is_some_and(|source| source.run != pending.remote_run)
        {
            self.remote_source = None;
        }
        self.active = Some(Session {
            id: pending.session,
            remote_run: pending.remote_run,
            remote_contact: pending.remote_contact,
            remote,
            peer_ack: ack,
            ready_nonce,
            last_seen_ms: self.now_ms,
            sent_ms: self.now_ms,
            next_sequence: 1,
            seen: Seen::default(),
        });
        self.pending = None;
    }

    fn ingest_control(&mut self, message: Control) -> Result<(), PeerError> {
        let run = message.run;
        match message.body {
            Body::Hello { contact, state } => {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|s| s.remote_run == run && s.remote_contact == contact)
                {
                    return self.queue_state();
                }
                if self.leader() {
                    if !self
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.remote_run == run && p.remote_contact == contact)
                    {
                        self.pending = Some(Pending {
                            session: DirectSessionId::generate()?,
                            remote_run: run,
                            remote_contact: contact,
                            remote: state,
                            nonce: None,
                            started_ms: self.now_ms,
                            retried_ms: self.now_ms,
                        });
                    }
                    self.queue_pending()?;
                } else if self.pending.is_none() {
                    self.queue_hello()?;
                }
            }
            Body::Offer {
                session,
                target_run,
                contact,
                target_contact,
                state,
            } => {
                if self.leader() || target_run != self.run || target_contact != self.contact {
                    return Ok(());
                }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|s| s.id == session && s.remote_run == run)
                {
                    return self.queue_state();
                }
                if !self.pending.as_ref().is_some_and(|p| {
                    p.session == session && p.remote_run == run && p.remote_contact == contact
                }) {
                    self.pending = Some(Pending {
                        session,
                        remote_run: run,
                        remote_contact: contact,
                        remote: state,
                        nonce: Some(BindNonce::generate()?),
                        started_ms: self.now_ms,
                        retried_ms: self.now_ms,
                    });
                }
                self.queue_pending()?;
            }
            Body::Confirm {
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
            } => {
                if !self.leader() || target_run != self.run || target_contact != self.contact {
                    return Ok(());
                }
                if self.active.as_ref().is_some_and(|s| {
                    s.id == session && s.remote_run == run && s.ready_nonce == Some(nonce)
                }) {
                    return self.queue_ready();
                }
                let Some(pending) = self.pending.as_ref() else {
                    return Ok(());
                };
                if pending.session != session
                    || pending.remote_run != run
                    || pending.remote_contact != contact
                {
                    return Ok(());
                }
                ensure_snapshot(state, pending.remote)?;
                let pending = self.pending.take().unwrap();
                self.activate(pending, state, Ack::default(), Some(nonce));
                self.queue_ready()?;
            }
            Body::Ready {
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
                ack,
            } => {
                if self.leader() || target_run != self.run || target_contact != self.contact {
                    return Ok(());
                }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|s| s.id == session && s.remote_run == run)
                {
                    return self.queue_state();
                }
                let Some(pending) = self.pending.as_ref() else {
                    return Ok(());
                };
                if pending.session != session
                    || pending.remote_run != run
                    || pending.remote_contact != contact
                    || pending.nonce != Some(nonce)
                {
                    return Ok(());
                }
                ensure_snapshot(state, pending.remote)?;
                self.validate_ack(ack)?;
                let pending = self.pending.take().unwrap();
                self.activate(pending, state, ack, None);
                self.queue_state()?;
            }
            Body::State {
                session,
                target_run,
                state,
                ack,
            } => {
                if target_run != self.run
                    || !self
                        .active
                        .as_ref()
                        .is_some_and(|s| s.id == session && s.remote_run == run)
                {
                    return Ok(());
                }
                self.validate_ack(ack)?;
                let active = self.active.as_mut().unwrap();
                let changed = state.revision > active.remote.revision;
                if state.revision >= active.remote.revision {
                    ensure_snapshot(state, active.remote)?;
                    active.remote = state;
                }
                active.peer_ack.send_generation =
                    active.peer_ack.send_generation.max(ack.send_generation);
                active.peer_ack.receive_generation = active
                    .peer_ack
                    .receive_generation
                    .max(ack.receive_generation);
                active.ready_nonce = None;
                active.last_seen_ms = self.now_ms;
                if changed {
                    self.queue_state()?;
                }
            }
        }
        Ok(())
    }

    fn validate_ack(&self, ack: Ack) -> Result<(), PeerError> {
        if ack.send_generation > self.local.send_generation
            || ack.receive_generation > self.local.receive_generation
        {
            return Err(PeerError::MalformedControl);
        }
        Ok(())
    }

    fn ingest_audio(&mut self, bytes: &[u8]) -> Result<Option<OriginalAudio>, PeerError> {
        let header = media::header(bytes)?;
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        if header.session != active.id
            || !self.local.intent.receive
            || !active.remote.intent.send
            || header.send_generation != active.remote.send_generation
            || header.receive_generation != self.local.receive_generation
            || self
                .remote_source
                .is_some_and(|source| header.source_epoch < source.epoch)
        {
            return Ok(None);
        }
        if self.remote_source.is_some_and(|source| {
            header.source_epoch == source.epoch
                && source.format != (header.sample_rate, header.channels)
        }) {
            return Err(PeerError::SourceFormatChanged);
        }
        let audio = media::decode(bytes, header)?;
        if !active.seen.insert(header.sequence) {
            return Ok(None);
        }
        self.remote_source = Some(RemoteSource {
            run: active.remote_run,
            epoch: header.source_epoch,
            format: (header.sample_rate, header.channels),
        });
        active.last_seen_ms = self.now_ms;
        Ok(Some(audio))
    }
}

fn ensure_snapshot(state: Snapshot, previous: Snapshot) -> Result<(), PeerError> {
    if state == previous || state.follows(previous) {
        Ok(())
    } else {
        Err(PeerError::MalformedControl)
    }
}
