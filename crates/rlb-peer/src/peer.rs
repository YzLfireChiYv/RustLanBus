//! One member-pair contact. Source publication and packetization live in the
//! participant publisher; this object adds the pair's session, grant and
//! catalogue admission.

use core::num::NonZeroU64;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rlb_model::{
    BindNonce, DirectSessionId, HelloNonce, IdGenerationError, MemberId, RunId, SourceId,
};

use crate::SourcePolicy;
use crate::media::{self, Header, Seen};
use crate::publisher::{CaptureWatermark, PreparedFragment, PreparedOriginalPacket};
use crate::source::{
    Catalogue, MAX_SOURCES, MEDIA_HEADER_BYTES, PcmEncoding, PcmFormat, PublicationEpoch,
    PublicationState, SourceEntry,
};
use crate::wire::{self, Ack, Body, Control, F32_ENCODING_BIT, Intent, ReceiveGrant, Snapshot};

const RETRY_MS: u64 = 250;
const HEARTBEAT_MS: u64 = 500;
const TIMEOUT_MS: u64 = 3_000;
const MAX_CONTROL_QUEUE: usize = 8;

// Opaque local counters may outlive a retired pair in the other participant.
// Reserve fresh initial floors without retaining every old Peer forever.
// Active pairs retain their own counters; these atomics track maxima, not PCM.
static LOCAL_REVISION_FLOOR: AtomicU64 = AtomicU64::new(0);
static LOCAL_SEND_TOKEN_FLOOR: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedControl {
    pub bytes: Vec<u8>,
    pub session: Option<DirectSessionId>,
}

/// A rejected datagram or caller precondition. Dropped audio is `Ok(None)`.
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
    StalePublication,
    Quiesced,
    /// Production write received a fragment that was prepared without a capture ticket.
    MissingCaptureTicket,
    /// The supplied watermark is older than one already applied on this pair.
    RegressingWatermark,
    /// Caller PCM or datagram storage cannot hold this fragment.
    StorageTooSmall,
    /// This pair is already bound to a different outer instance.
    InstanceConflict,
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
            Self::InvalidAudio => "invalid original audio framing or payload length",
            Self::NonFiniteSample => "non-finite audio samples are rejected",
            Self::StalePublication => "prepared original does not match the current publication",
            Self::Quiesced => "peer counters are exhausted; this pair is closed until a fresh run",
            Self::MissingCaptureTicket => {
                "production write requires the capture ticket attached before distribution"
            }
            Self::RegressingWatermark => {
                "capture watermark moved backwards for this pair's ticket domain"
            }
            Self::StorageTooSmall => "caller audio storage cannot hold this datagram",
            Self::InstanceConflict => "peer instance is already bound to a different value",
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

/// Latest installed admission, independent of any PCM queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionView {
    pub connected: bool,
    pub receive_ready: bool,
    pub send_ready: bool,
    pub quiesced: bool,
    pub session: Option<DirectSessionId>,
    pub remote_run: Option<RunId>,
    pub direction_fence: u64,
    pub sources: Vec<SourceAdmission>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceAdmission {
    pub entry: SourceEntry,
    pub fence: u64,
    pub receive_ready: bool,
}

impl SourceAdmission {
    #[must_use]
    pub const fn source_id(self) -> SourceId {
        self.entry.id
    }

    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.entry.epoch.get()
    }

    #[must_use]
    pub const fn state(self) -> PublicationState {
        self.entry.state
    }

    #[must_use]
    pub const fn kind(self) -> crate::source::SourceKind {
        self.entry.kind
    }

    #[must_use]
    pub const fn format(self) -> Option<PcmFormat> {
        self.entry.format
    }
}

/// Owner-held lifetime of one `Peer` object.
///
/// Keep one value across reconnect, pause, resume and address changes. Allocate
/// a different value only when the owner replaces the `Peer` itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PeerInstance(NonZeroU64);

impl PeerInstance {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Current capture-allocator watermark supplied at an operation that may
/// replace an outgoing permission interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerOperation {
    pub now_ms: u64,
    pub watermark: CaptureWatermark,
}

/// Explicit outgoing permission interval for one pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutgoingInterval {
    pub revision: u64,
    pub ticket_floor: u64,
}

/// Coherent admission view. `revision` increases when this exported content changes
/// and stays on this object across reconnect. It does not wrap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionSnapshot {
    pub revision: u64,
    pub peer_instance: Option<PeerInstance>,
    pub connected: bool,
    pub receive_ready: bool,
    pub send_ready: bool,
    pub quiesced: bool,
    pub remote_run: Option<RunId>,
    pub session: Option<DirectSessionId>,
    pub sources: Vec<SourceAdmission>,
}

/// Fixed-capacity projection of [`AdmissionSnapshot`] for a preallocated mailbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionProjection {
    pub revision: u64,
    pub peer_instance: Option<PeerInstance>,
    pub connected: bool,
    pub receive_ready: bool,
    pub send_ready: bool,
    pub quiesced: bool,
    pub remote_run: Option<RunId>,
    pub session: Option<DirectSessionId>,
    pub source_count: u8,
    pub sources: [Option<SourceAdmission>; MAX_SOURCES],
}

impl Default for AdmissionProjection {
    fn default() -> Self {
        Self {
            revision: 0,
            peer_instance: None,
            connected: false,
            receive_ready: false,
            send_ready: false,
            quiesced: false,
            remote_run: None,
            session: None,
            source_count: 0,
            sources: [None; MAX_SOURCES],
        }
    }
}

/// Admitted fragment metadata without PCM. Render eligibility uses these fields
/// together; the source fence number alone does not identify the source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmittedMetadata {
    pub peer_instance: Option<PeerInstance>,
    pub admission_revision: u64,
    pub member_id: MemberId,
    pub remote_run: RunId,
    pub entry: SourceEntry,
    pub sequence: u64,
    pub start_frame: u64,
    pub frames: u16,
    pub session: DirectSessionId,
    pub send_generation: u64,
    pub receive_generation: u64,
    pub source_fence: u64,
    pub arrived_at_ms: u64,
}

/// Borrowed PCM from caller storage. Drop it before reusing that storage.
#[derive(Clone, Copy, Debug)]
pub struct AdmittedPcm<'a> {
    pub metadata: AdmittedMetadata,
    pub samples: &'a [f64],
}

/// Bytes written into caller datagram storage. The slice ends at the valid datagram.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrittenDatagram<'a> {
    pub bytes: &'a [u8],
}

/// One original fragment admitted for a later render owner.
/// `arrived_at_ms` is the caller's receive observation and is not refreshed.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedOriginal {
    pub member_id: MemberId,
    pub remote_run: RunId,
    pub source_id: SourceId,
    pub epoch: PublicationEpoch,
    pub format: PcmFormat,
    pub sequence: u64,
    pub start_frame: u64,
    pub frames: u16,
    pub session: DirectSessionId,
    pub send_generation: u64,
    pub receive_generation: u64,
    pub admission_token: u64,
    pub arrived_at_ms: u64,
    pub peer_instance: Option<PeerInstance>,
    pub admission_revision: u64,
    pub samples: Vec<f64>,
}

impl AdmittedOriginal {
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.format.channels()
    }

    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.format.sample_rate_hz()
    }
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

#[derive(Clone)]
struct Session {
    id: DirectSessionId,
    remote_run: RunId,
    remote_contact: HelloNonce,
    peer_ack: Ack,
    ready_nonce: Option<BindNonce>,
    last_seen_ms: u64,
    sent_ms: u64,
}

#[derive(Clone)]
struct Window {
    id: SourceId,
    epoch: u64,
    seen: Seen,
}

#[derive(Clone)]
struct Remembered {
    run: RunId,
    snapshot: Snapshot,
    windows: Vec<Window>,
}

struct AudioCandidate {
    header: Header,
    remote_run: RunId,
    entry: SourceEntry,
    format: PcmFormat,
}

enum Installed {
    Replaced,
    Updated,
    Identical,
    Stale,
}

#[derive(Default)]
struct Subscriptions {
    revision: u64,
    grants: Vec<ReceiveGrant>,
}

/// One directed pair. Sockets, saved files and playback stay in the application.
pub struct Peer {
    local_id: MemberId,
    remote_id: MemberId,
    run: RunId,
    contact: HelloNonce,
    local: Snapshot,
    announcements: Vec<(SourceId, u64)>,
    remembered: Option<Remembered>,
    active: Option<Session>,
    pending: Option<Pending>,
    outgoing: VecDeque<QueuedControl>,
    now_ms: u64,
    hello_ms: u64,
    paused: bool,
    quiesced: bool,
    fence_cursor: u64,
    direction_fence: u64,
    source_fences: Vec<(SourceId, u64)>,
    send_open: bool,
    send_not_before_ms: u64,
    admission_revision: u64,
    outgoing_revision: u64,
    ticket_floor: u64,
    applied_watermark: u64,
    peer_instance: Option<PeerInstance>,
    cached_send_ready: bool,
    call_watermark: Option<CaptureWatermark>,
    interval_replaced: bool,
    policy: SourcePolicy,
    subscriptions: Subscriptions,
    remote_subscriptions: Subscriptions,
    subscriptions_ack: u64,
    last_control_accepted: bool,
    last_control_session: Option<DirectSessionId>,
    last_control_revision: Option<u64>,
    source_send_intervals: Vec<(SourceId, u64, u64)>,
}

impl Peer {
    pub fn new(
        local: MemberId,
        remote: MemberId,
        run: RunId,
        intent: Intent,
        catalogue: Catalogue,
        now_ms: u64,
    ) -> Result<Self, PeerError> {
        if local == remote {
            return Err(PeerError::SameMember);
        }
        let local_state = Snapshot::new(intent, catalogue);
        let revision = local_state.revision;
        let announcements = local_state
            .sources
            .as_slice()
            .iter()
            .map(|entry| (entry.id, revision))
            .collect();
        let mut peer = Self {
            local_id: local,
            remote_id: remote,
            run,
            contact: HelloNonce::generate()?,
            local: local_state,
            announcements,
            remembered: None,
            active: None,
            pending: None,
            outgoing: VecDeque::with_capacity(MAX_CONTROL_QUEUE),
            now_ms,
            hello_ms: now_ms,
            paused: false,
            quiesced: false,
            fence_cursor: 1,
            direction_fence: 1,
            source_fences: Vec::new(),
            send_open: false,
            send_not_before_ms: 0,
            admission_revision: 1,
            outgoing_revision: 1,
            ticket_floor: 0,
            applied_watermark: 0,
            peer_instance: None,
            cached_send_ready: false,
            call_watermark: None,
            interval_replaced: false,
            policy: SourcePolicy::all(intent),
            subscriptions: Subscriptions {
                revision: 1,
                grants: Vec::new(),
            },
            remote_subscriptions: Subscriptions::default(),
            subscriptions_ack: 0,
            last_control_accepted: false,
            last_control_session: None,
            last_control_revision: None,
            source_send_intervals: Vec::with_capacity(MAX_SOURCES),
        };
        peer.queue_hello()?;
        Ok(peer)
    }

    #[must_use]
    pub fn status(&self) -> PeerStatus {
        let remote = self.remote_snapshot();
        PeerStatus {
            connected: self.active.is_some(),
            send_ready: self.member_send_ready(),
            receive_ready: self.receive_ready(),
            local: self.local.intent,
            remote: remote.map(|snapshot| snapshot.intent),
            session: self.active.as_ref().map(|session| session.id),
            remote_run: self.active.as_ref().map(|session| session.remote_run),
        }
    }

    #[must_use]
    pub fn admission(&self) -> AdmissionView {
        let sources = self
            .remembered
            .as_ref()
            .map(|remembered| {
                remembered
                    .snapshot
                    .sources
                    .as_slice()
                    .iter()
                    .map(|entry| SourceAdmission {
                        entry: *entry,
                        fence: self.source_fence(entry.id),
                        receive_ready: self.source_receive_ready(entry.id),
                    })
                    .collect()
            })
            .unwrap_or_default();
        AdmissionView {
            connected: self.active.is_some(),
            receive_ready: self.receive_ready(),
            send_ready: self.member_send_ready(),
            quiesced: self.quiesced,
            session: self.active.as_ref().map(|session| session.id),
            remote_run: self.remembered.as_ref().map(|remembered| remembered.run),
            direction_fence: self.direction_fence,
            sources,
        }
    }

    /// Binds the outer owner identity. A second equal call is unchanged.
    /// A different value is rejected; replace the `Peer` object instead.
    pub fn attach_instance(&mut self, instance: PeerInstance) -> Result<(), PeerError> {
        match self.peer_instance {
            Some(current) if current == instance => Ok(()),
            Some(_) => Err(PeerError::InstanceConflict),
            None if self.quiesced => Err(PeerError::Quiesced),
            None => {
                if self.active.is_none() && self.pending.is_none() && self.remembered.is_none() {
                    self.reserve_initial_floors()?;
                    self.outgoing.clear();
                    self.queue_hello()?;
                }
                self.peer_instance = Some(instance);
                self.record_local_floors();
                self.bump_admission_revision();
                Ok(())
            }
        }
    }

    #[must_use]
    pub const fn outgoing_interval(&self) -> OutgoingInterval {
        OutgoingInterval {
            revision: self.outgoing_revision,
            ticket_floor: self.ticket_floor,
        }
    }

    #[must_use]
    pub fn admission_snapshot(&self) -> AdmissionSnapshot {
        let mut projection = AdmissionProjection::default();
        self.project_admission(&mut projection);
        AdmissionSnapshot {
            revision: projection.revision,
            peer_instance: projection.peer_instance,
            connected: projection.connected,
            receive_ready: projection.receive_ready,
            send_ready: projection.send_ready,
            quiesced: projection.quiesced,
            remote_run: projection.remote_run,
            session: projection.session,
            sources: projection.sources.into_iter().flatten().collect(),
        }
    }

    /// Writes the current admission into caller storage. Safe to call after
    /// success and after errors. PCM capacity does not affect this projection.
    pub fn project_admission(&self, into: &mut AdmissionProjection) {
        *into = AdmissionProjection {
            revision: self.admission_revision,
            peer_instance: self.peer_instance,
            connected: self.active.is_some(),
            receive_ready: self.receive_ready(),
            send_ready: self.member_send_ready(),
            quiesced: self.quiesced,
            remote_run: self.remembered.as_ref().map(|remembered| remembered.run),
            session: self.active.as_ref().map(|session| session.id),
            source_count: 0,
            sources: [None; MAX_SOURCES],
        };
        let Some(remembered) = &self.remembered else {
            return;
        };
        let entries = remembered.snapshot.sources.as_slice();
        let count = entries.len().min(MAX_SOURCES);
        into.source_count = u8::try_from(count).unwrap_or(u8::MAX);
        for (index, entry) in entries.iter().take(count).enumerate() {
            into.sources[index] = Some(SourceAdmission {
                entry: *entry,
                fence: self.source_fence(entry.id),
                receive_ready: self.source_receive_ready(entry.id),
            });
        }
    }

    #[must_use]
    pub fn local_revision(&self) -> u64 {
        self.local.revision
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    #[must_use]
    pub fn is_quiesced(&self) -> bool {
        self.quiesced
    }

    #[must_use]
    pub fn remote_source_ids(&self) -> Vec<SourceId> {
        self.remembered
            .as_ref()
            .map(|remembered| {
                remembered
                    .snapshot
                    .sources
                    .as_slice()
                    .iter()
                    .map(|entry| entry.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Last installed catalogue; contact/readiness must be checked separately.
    #[must_use]
    pub fn remote_catalogue(&self) -> Option<&Catalogue> {
        self.remembered
            .as_ref()
            .map(|remembered| &remembered.snapshot.sources)
    }

    #[must_use]
    pub fn next_deadline_ms(&self) -> u64 {
        if self.paused || self.quiesced {
            return self.now_ms.saturating_add(TIMEOUT_MS);
        }
        let mut next = if self.active.is_none() && self.pending.is_none() {
            self.hello_ms.saturating_add(RETRY_MS)
        } else {
            u64::MAX
        };
        if let Some(pending) = &self.pending {
            next = next
                .min(pending.retried_ms.saturating_add(RETRY_MS))
                .min(pending.started_ms.saturating_add(TIMEOUT_MS));
        }
        if let Some(session) = &self.active {
            let interval = if session.ready_nonce.is_some()
                || self.subscriptions_ack < self.subscriptions.revision
            {
                RETRY_MS
            } else {
                HEARTBEAT_MS
            };
            next = next
                .min(session.sent_ms.saturating_add(interval))
                .min(session.last_seen_ms.saturating_add(TIMEOUT_MS));
        }
        next
    }

    pub fn tick(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if self.paused || self.quiesced {
            return Ok(());
        }
        if self
            .active
            .as_ref()
            .is_some_and(|session| now_ms.saturating_sub(session.last_seen_ms) >= TIMEOUT_MS)
        {
            self.reconnect(now_ms)?;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now_ms.saturating_sub(pending.started_ms) >= TIMEOUT_MS)
        {
            self.pending = None;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now_ms.saturating_sub(pending.retried_ms) >= RETRY_MS)
        {
            self.queue_pending()?;
            self.pending.as_mut().unwrap().retried_ms = now_ms;
        } else if self.active.is_none()
            && self.pending.is_none()
            && now_ms.saturating_sub(self.hello_ms) >= RETRY_MS
        {
            self.queue_hello()?;
        }
        if let Some(session) = &self.active {
            let interval = if session.ready_nonce.is_some()
                || self.subscriptions_ack < self.subscriptions.revision
            {
                RETRY_MS
            } else {
                HEARTBEAT_MS
            };
            if now_ms.saturating_sub(session.sent_ms) >= interval {
                if session.ready_nonce.is_some() {
                    self.queue_ready()?;
                } else {
                    self.queue_state()?;
                }
            }
        }
        Ok(())
    }

    /// Close contact and drop queued controls. Pair counters, catalogues and
    /// remembered source windows stay. A second pause changes nothing.
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;
        self.active = None;
        self.pending = None;
        self.outgoing.clear();
        self.bump_direction();
        self.note_send_gate();
    }

    /// Open a fresh contact with the current complete local state.
    /// An already running pair is left unchanged.
    pub fn resume(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if !self.paused {
            return Ok(());
        }
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        self.paused = false;
        self.renew_contact()?;
        self.queue_hello()
    }

    pub fn set_intent(&mut self, intent: Intent, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        self.apply_policy(SourcePolicy::all(intent))
    }

    /// Applies one ordered intent transition and, when local send reopens,
    /// samples `operation.watermark` as the new ticket floor.
    ///
    /// An identical intent does not create an interval. A missing production
    /// caller must use this method rather than [`Self::set_intent`]: the older
    /// method only updates the legacy millisecond cutoff.
    pub fn set_intent_with(
        &mut self,
        intent: Intent,
        operation: PeerOperation,
    ) -> Result<(), PeerError> {
        self.observe_time(operation.now_ms)?;
        self.observe_watermark(operation.watermark)?;
        self.with_watermark(operation.watermark, |peer| {
            if peer.policy == SourcePolicy::all(intent) {
                return Ok(());
            }
            let reopen = intent.send && !peer.local.intent.send;
            peer.apply_policy(SourcePolicy::all(intent))?;
            if reopen {
                peer.replace_interval(operation.watermark)?;
            }
            Ok(())
        })
    }

    pub fn set_source_policy_with(
        &mut self,
        policy: SourcePolicy,
        operation: PeerOperation,
    ) -> Result<(), PeerError> {
        policy.validate()?;
        self.observe_time(operation.now_ms)?;
        self.observe_watermark(operation.watermark)?;
        self.with_watermark(operation.watermark, |peer| {
            let reopen = policy.envelope().send && !peer.local.intent.send;
            peer.apply_policy(policy)?;
            if reopen {
                peer.replace_interval(operation.watermark)?;
            }
            Ok(())
        })
    }

    fn apply_policy(&mut self, policy: SourcePolicy) -> Result<(), PeerError> {
        if policy == self.policy {
            return Ok(());
        }
        let intent = policy.envelope();
        let send_delta = u64::from(intent.send != self.local.intent.send);
        let receive_delta = u64::from(intent.receive != self.local.intent.receive);
        let (revision, send, receive) = self.reserve_revision(send_delta, receive_delta)?;
        let mut tokens = self.local.send_tokens.clone();
        let mut changed = Vec::new();
        for (index, entry) in self.local.sources.as_slice().iter().enumerate() {
            let enabled = policy.for_kind(entry.kind).send;
            if (tokens[index] & 1 != 0) != enabled {
                let Some(next) = tokens[index].checked_add(1) else {
                    self.quiesce();
                    return Err(PeerError::CounterExhausted);
                };
                tokens[index] = next;
                changed.push((entry.id, enabled));
            }
        }
        let receive_changed: Vec<_> = self
            .remote_catalogue()
            .into_iter()
            .flat_map(|catalogue| catalogue.as_slice())
            .filter(|entry| {
                self.policy.for_kind(entry.kind).receive != policy.for_kind(entry.kind).receive
            })
            .map(|entry| entry.id)
            .collect();
        self.policy = policy;
        self.local.intent = intent;
        self.local.revision = revision;
        self.local.send_generation = send;
        self.local.receive_generation = receive;
        self.local.send_tokens = tokens;
        for (id, enabled) in changed {
            if let Some((_, announced)) = self
                .announcements
                .iter_mut()
                .find(|(source, _)| *source == id)
            {
                *announced = revision;
            }
            if enabled {
                self.replace_source_interval(id);
            }
        }
        if receive_delta == 1 {
            self.bump_direction();
        } else {
            for id in receive_changed {
                self.bump_source(id);
            }
        }
        self.note_send_gate();
        self.publish_local()
    }

    /// Replace this pair's advertised catalogue. The caller supplies the
    /// publisher's already committed records; this method does not allocate epochs.
    pub fn install_catalogue(
        &mut self,
        catalogue: &Catalogue,
        now_ms: u64,
    ) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if catalogue == &self.local.sources {
            return Ok(());
        }
        self.prepare_catalogue(catalogue)?;
        let (revision, send, receive) = self.reserve_revision(0, 0)?;
        let announcements = catalogue
            .as_slice()
            .iter()
            .map(|entry| {
                let revision = if self.local.sources.source(entry.id) == Some(entry) {
                    self.announcement(entry.id)
                } else {
                    revision
                };
                (entry.id, revision)
            })
            .collect();
        let send_tokens = catalogue
            .as_slice()
            .iter()
            .map(|entry| {
                if self.local.sources.source(entry.id).is_some() {
                    self.local.send_token(entry.id)
                } else {
                    u32::from(self.policy.for_kind(entry.kind).send)
                }
            })
            .collect();
        self.local.revision = revision;
        self.local.send_generation = send;
        self.local.receive_generation = receive;
        self.local.sources = catalogue.clone();
        self.local.send_tokens = send_tokens;
        self.announcements = announcements;
        self.note_send_gate();
        self.publish_local()
    }

    pub fn reconnect(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if self.paused || self.quiesced {
            return Ok(());
        }
        self.renew_contact()?;
        self.active = None;
        self.pending = None;
        self.outgoing.clear();
        self.bump_direction();
        self.note_send_gate();
        self.queue_hello()
    }

    fn renew_contact(&mut self) -> Result<(), PeerError> {
        let Some(revision) = self.local.revision.checked_add(1) else {
            self.quiesce();
            return Err(PeerError::CounterExhausted);
        };
        let contact = HelloNonce::generate()?;
        self.local.revision = revision;
        self.contact = contact;
        self.record_local_floors();
        Ok(())
    }

    /// Whether the last ingested control was accepted by the current protocol
    /// context. Socket owners can learn a reply path without parsing private
    /// handshake bodies or resetting contact nonces on ignored packets.
    #[must_use]
    pub fn last_control_accepted(&self) -> bool { self.last_control_accepted }

    #[must_use]
    pub fn last_control_session(&self) -> Option<DirectSessionId> { self.last_control_session }

    #[must_use]
    pub fn last_control_revision(&self) -> Option<u64> { self.last_control_revision }

    #[must_use]
    pub fn remote_state_revision(&self) -> Option<u64> {
        self.remembered.as_ref().map(|remote| remote.snapshot.revision)
    }

    /// Announce a changed local address set using the existing complete-state
    /// revision. Healthy sessions and PCM permissions are retained. A receiver
    /// can migrate a proven session to a new source address while rejecting
    /// older in-flight control packets from the previous address.
    pub fn announce_addresses_changed(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        let Some(revision) = self.local.revision.checked_add(1) else {
            self.quiesce();
            return Err(PeerError::CounterExhausted);
        };
        self.local.revision = revision;
        self.record_local_floors();
        if self.paused || self.quiesced { return Ok(()); }
        if self.active.is_none() && self.pending.is_none() { self.queue_hello() } else { self.publish_local() }
    }

    pub fn take_control(&mut self) -> Option<Vec<u8>> {
        self.take_control_routed().map(|control| control.bytes)
    }

    pub fn take_control_routed(&mut self) -> Option<QueuedControl> { self.outgoing.pop_front() }

    #[must_use]
    pub fn pending_session(&self) -> Option<DirectSessionId> { self.pending.as_ref().map(|pending| pending.session) }

    /// Ask the existing handshake to greet another transport candidate without
    /// renewing nonces or discarding a useful in-flight offer. Only the leader
    /// creates the session; its first completed proof fixes the socket pair.
    pub fn probe_contact(&mut self, now_ms: u64) -> Result<(), PeerError> {
        self.observe_time(now_ms)?;
        if self.active.is_none() && !self.paused && !self.quiesced { self.queue_hello()?; }
        Ok(())
    }

    fn record_local_floors(&self) {
        if self.peer_instance.is_none() { return; }
        LOCAL_REVISION_FLOOR.fetch_max(self.local.revision, Ordering::Relaxed);
        if let Some(token) = self.local.send_tokens.iter().copied().max() {
            LOCAL_SEND_TOKEN_FLOOR.fetch_max(token, Ordering::Relaxed);
        }
    }

    fn reserve_initial_floors(&mut self) -> Result<(), PeerError> {
        let token_floor = LOCAL_SEND_TOKEN_FLOOR.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
            |value| value.checked_add(2)).map_err(|_| PeerError::CounterExhausted)?
            .checked_add(2).ok_or(PeerError::CounterExhausted)? & !1;
        for token in &mut self.local.send_tokens { *token = token_floor | (*token & 1); }
        let prior = LOCAL_REVISION_FLOOR.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
            |value| value.max(u64::from(token_floor >> 1)).checked_add(1)).map_err(|_| PeerError::CounterExhausted)?;
        let revision = prior.max(u64::from(token_floor >> 1)) + 1;
        self.local.revision = revision;
        self.local.send_generation = revision;
        self.local.receive_generation = revision;
        for (_, announcement) in &mut self.announcements { *announcement = revision; }
        LOCAL_SEND_TOKEN_FLOOR.fetch_max(self.local.send_tokens.iter().copied().max().unwrap_or(token_floor), Ordering::Relaxed);
        Ok(())
    }

    /// `arrived_at_ms` is the receiver's original observation of this datagram.
    ///
    /// This entry does not sample a capture watermark. Interval replacement
    /// that happens inside it keeps the legacy millisecond cutoff only.
    pub fn ingest(
        &mut self,
        bytes: &[u8],
        now_ms: u64,
        arrived_at_ms: u64,
    ) -> Result<Option<AdmittedOriginal>, PeerError> {
        self.observe_time(now_ms)?;
        self.ingest_message(bytes, arrived_at_ms)
    }

    /// `ingest` with the allocator watermark used when this datagram installs
    /// a session or changes remote receive generation.
    pub fn ingest_with(
        &mut self,
        bytes: &[u8],
        arrived_at_ms: u64,
        operation: PeerOperation,
    ) -> Result<Option<AdmittedOriginal>, PeerError> {
        self.observe_time(operation.now_ms)?;
        self.observe_watermark(operation.watermark)?;
        self.with_watermark(operation.watermark, |peer| {
            peer.ingest_message(bytes, arrived_at_ms)
        })
    }

    /// Decodes one datagram into `scratch` when it is admitted PCM.
    ///
    /// A short `scratch` returns [`PeerError::StorageTooSmall`] and does not
    /// consume the source sequence. Control datagrams ignore `scratch` and
    /// return `Ok(None)` after applying the watermarked transition. The
    /// admission projection remains readable after either result.
    pub fn ingest_into<'a>(
        &mut self,
        bytes: &[u8],
        arrived_at_ms: u64,
        operation: PeerOperation,
        scratch: &'a mut [f64],
    ) -> Result<Option<AdmittedPcm<'a>>, PeerError> {
        self.observe_time(operation.now_ms)?;
        self.observe_watermark(operation.watermark)?;
        self.with_watermark(operation.watermark, |peer| {
            if bytes.starts_with(media::MEDIA_MAGIC) {
                if peer.paused || peer.quiesced {
                    media::header(bytes)?;
                    return Ok(None);
                }
                let metadata = peer.ingest_audio_into(bytes, arrived_at_ms, scratch)?;
                let samples = scratch;
                return Ok(metadata.map(|metadata| {
                    let count = usize::from(metadata.frames)
                        * usize::from(
                            metadata
                                .entry
                                .format
                                .map(|format| format.channels())
                                .unwrap_or(0),
                        );
                    AdmittedPcm {
                        metadata,
                        samples: &samples[..count],
                    }
                }));
            }
            peer.ingest_message(bytes, arrived_at_ms)?;
            Ok(None)
        })
    }

    fn with_watermark<T>(
        &mut self,
        watermark: CaptureWatermark,
        body: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.call_watermark = Some(watermark);
        self.interval_replaced = false;
        let result = body(self);
        self.call_watermark = None;
        self.interval_replaced = false;
        result
    }

    fn ingest_message(
        &mut self,
        bytes: &[u8],
        arrived_at_ms: u64,
    ) -> Result<Option<AdmittedOriginal>, PeerError> {
        self.last_control_accepted = false;
        self.last_control_session = None;
        self.last_control_revision = None;
        if bytes.starts_with(media::MEDIA_MAGIC) {
            if self.paused || self.quiesced {
                media::header(bytes)?;
                return Ok(None);
            }
            return self.ingest_audio(bytes, arrived_at_ms);
        }
        let message = wire::decode(bytes)?;
        if self.paused || self.quiesced {
            return Ok(None);
        }
        if message.from != self.remote_id || message.to != self.local_id {
            return Err(PeerError::WrongPeer);
        }
        self.ingest_control(message)?;
        Ok(None)
    }

    /// Legacy allocating writer.
    ///
    /// It gates on `enqueued_at_ms` and the old millisecond cutoff. It does not
    /// read a capture ticket and is not a production send path.
    pub fn write_original(
        &self,
        packet: &PreparedOriginalPacket,
    ) -> Result<Option<Vec<u8>>, PeerError> {
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        let Some(entry) = self.local.sources.source(packet.source()) else {
            return Err(PeerError::StalePublication);
        };
        if packet.run() != self.run
            || entry.state != PublicationState::Active
            || entry.epoch != packet.epoch()
            || entry.format != Some(packet.format())
            || packet.sequence() == 0
            || packet.frames() == 0
            || packet.payload().len() != media::expected_payload(packet.format(), packet.frames())?
        {
            return Err(PeerError::StalePublication);
        }
        if !self.source_send_ready(packet.source())
            || packet.enqueued_at_ms() < self.source_not_before_ms(packet.source())
        {
            return Ok(None);
        }
        let session = self.active.as_ref().expect("ready source has a session");
        let payload_bytes =
            u16::try_from(packet.payload().len()).map_err(|_| PeerError::InvalidAudio)?;
        let header = Header {
            session: session.id,
            send_generation: u64::from(self.local.send_token(packet.source())),
            receive_generation: self
                .remote_grant(packet.source())
                .expect("ready source has a grant")
                .token,
            source: packet.source(),
            epoch: packet.epoch().get(),
            sequence: packet.sequence(),
            start_frame: packet.start_frame(),
            frames: packet.frames(),
            payload_bytes,
        };
        if MEDIA_HEADER_BYTES + packet.payload().len() > media::MAX_DATAGRAM_BYTES {
            return Err(PeerError::DatagramTooLarge);
        }
        Ok(Some(media::encode(header, packet.payload())?))
    }

    /// Writes one fragment when its capture ticket is strictly above this pair's floor.
    ///
    /// Checks run, source, publication, acknowledgement and current eligibility
    /// before any header bytes are stored. `Ok(None)` is not sendable. A short
    /// scratch buffer leaves the destination unchanged. Age remains
    /// `fragment.enqueued_at_ms()` for the caller to recheck; this method does
    /// not apply the legacy millisecond cutoff.
    pub fn write_original_into<'a>(
        &self,
        fragment: &PreparedFragment<'_>,
        scratch: &'a mut [u8],
    ) -> Result<Option<WrittenDatagram<'a>>, PeerError> {
        let ticket = fragment.ticket().ok_or(PeerError::MissingCaptureTicket)?;
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        if !self.fragment_current(fragment)? {
            return Err(PeerError::StalePublication);
        }
        if !self.source_send_ready(fragment.source())
            || ticket.get() <= self.source_ticket_floor(fragment.source())
        {
            return Ok(None);
        }
        let session = self.active.as_ref().expect("ready source has a session");
        let payload_bytes =
            u16::try_from(fragment.payload().len()).map_err(|_| PeerError::InvalidAudio)?;
        let header = Header {
            session: session.id,
            send_generation: u64::from(self.local.send_token(fragment.source())),
            receive_generation: self
                .remote_grant(fragment.source())
                .expect("ready source has a grant")
                .token,
            source: fragment.source(),
            epoch: fragment.epoch().get(),
            sequence: fragment.sequence(),
            start_frame: fragment.start_frame(),
            frames: fragment.frames(),
            payload_bytes,
        };
        let length = media::encode_into(header, fragment.payload(), scratch)?;
        Ok(Some(WrittenDatagram {
            bytes: &scratch[..length],
        }))
    }

    fn fragment_current(&self, fragment: &PreparedFragment<'_>) -> Result<bool, PeerError> {
        let Some(entry) = self.local.sources.source(fragment.source()) else {
            return Ok(false);
        };
        if fragment.run() != self.run
            || entry.state != PublicationState::Active
            || entry.epoch != fragment.epoch()
            || entry.format != Some(fragment.format())
            || fragment.sequence() == 0
            || fragment.frames() == 0
            || fragment.payload().len()
                != media::expected_payload(fragment.format(), fragment.frames())?
        {
            return Ok(false);
        }
        if MEDIA_HEADER_BYTES + fragment.payload().len() > media::MAX_DATAGRAM_BYTES {
            return Err(PeerError::DatagramTooLarge);
        }
        Ok(true)
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

    fn remote_snapshot(&self) -> Option<&Snapshot> {
        self.active
            .as_ref()
            .and_then(|session| {
                self.remembered
                    .as_ref()
                    .filter(|remembered| remembered.run == session.remote_run)
            })
            .map(|remembered| &remembered.snapshot)
    }

    fn receive_ready(&self) -> bool {
        self.receive_envelope_ready()
    }

    fn receive_envelope_ready(&self) -> bool {
        self.local.intent.receive
            && self
                .remote_snapshot()
                .is_some_and(|snapshot| snapshot.intent.send)
    }

    fn member_send_ready(&self) -> bool {
        self.send_envelope_ready()
    }

    fn send_envelope_ready(&self) -> bool {
        let Some(session) = &self.active else {
            return false;
        };
        let Some(remote) = self.remote_snapshot() else {
            return false;
        };
        self.local.intent.send
            && remote.intent.receive
            && session.peer_ack.send_generation == self.local.send_generation
            && session.peer_ack.send_generation != 0
    }

    /// Whether this original has a current acknowledged send direction.
    /// Owners may use this as a capture-queue interest hint. The final writer
    /// still validates the capture ticket and complete publication context.
    #[must_use]
    pub fn source_send_ready(&self, id: SourceId) -> bool {
        let Some(entry) = self.local.sources.source(id) else {
            return false;
        };
        let Some(session) = &self.active else {
            return false;
        };
        let Some(remote) = self.remote_snapshot() else {
            return false;
        };
        self.send_envelope_ready()
            && entry.state == PublicationState::Active
            && self.local.send_token(id) & 1 != 0
            && self.remote_grant(id).is_some_and(|grant| {
                grant.epoch == entry.epoch && grant.sender_token == self.local.send_token(id)
            })
            && session.peer_ack.state_revision >= self.announcement(id)
            && supports_format(remote.receive_encodings, entry.format)
    }

    /// A receiver's source request can wake an idle native acquisition. This
    /// is control demand only: exact epoch/grant/format checks remain in
    /// source_send_ready and the final PCM writer. A rebinding publication
    /// must not tear down the device while its replacement grant is in flight.
    #[must_use]
    pub fn source_capture_requested(&self, id: SourceId) -> bool {
        let Some(entry) = self.local.sources.source(id) else {
            return false;
        };
        !self.paused
            && !self.quiesced
            && self.send_envelope_ready()
            && entry.state != PublicationState::Disabled
            && self.local.send_token(id) & 1 != 0
            && self.policy.for_kind(entry.kind).send
            && self.remote_grant(id).is_some()
    }

    /// Current exact-publication admission, independent of output destinations.
    #[must_use]
    pub fn source_receive_ready(&self, id: SourceId) -> bool {
        if self.active.is_none() || self.quiesced || !self.receive_envelope_ready() {
            return false;
        }
        let Some(remote) = self.remote_snapshot() else {
            return false;
        };
        let Some(entry) = remote.sources.source(id) else {
            return false;
        };
        self.policy.for_kind(entry.kind).receive
            && remote.send_token(id) & 1 != 0
            && entry.state == PublicationState::Active
            && supports_format(self.local.receive_encodings, entry.format)
            && self.subscriptions.grants.iter().any(|grant| {
                grant.source == id
                    && grant.epoch == entry.epoch
                    && grant.sender_token == remote.send_token(id)
                    && grant.token == self.source_fence(id)
            })
    }

    fn remote_grant(&self, id: SourceId) -> Option<&ReceiveGrant> {
        self.remote_subscriptions
            .grants
            .iter()
            .find(|grant| grant.source == id)
    }

    fn source_ticket_floor(&self, id: SourceId) -> u64 {
        self.source_send_intervals
            .iter()
            .find(|interval| interval.0 == id)
            .map(|interval| interval.1)
            .unwrap_or(0)
            .max(self.ticket_floor)
    }

    fn source_not_before_ms(&self, id: SourceId) -> u64 {
        self.source_send_intervals
            .iter()
            .find(|interval| interval.0 == id)
            .map(|interval| interval.2)
            .unwrap_or(0)
            .max(self.send_not_before_ms)
    }

    fn replace_source_interval(&mut self, id: SourceId) {
        let floor = self
            .call_watermark
            .map(|value| value.allocated_through)
            .unwrap_or(self.applied_watermark);
        if let Some(interval) = self
            .source_send_intervals
            .iter_mut()
            .find(|interval| interval.0 == id)
        {
            interval.1 = floor;
            interval.2 = self.now_ms;
        } else {
            self.source_send_intervals.push((id, floor, self.now_ms));
        }
    }

    fn refresh_subscriptions(&mut self) -> Result<(), PeerError> {
        let grants: Vec<_> =
            if self.active.is_some() && self.receive_envelope_ready() && !self.quiesced {
                self.remote_snapshot()
                    .into_iter()
                    .flat_map(|remote| {
                        remote
                            .sources
                            .as_slice()
                            .iter()
                            .filter(|entry| remote.send_token(entry.id) & 1 != 0)
                    })
                    .filter(|entry| {
                        entry.state != PublicationState::Disabled
                            && self.policy.for_kind(entry.kind).receive
                            && (entry.state == PublicationState::Unavailable
                                || supports_format(self.local.receive_encodings, entry.format))
                    })
                    .map(|entry| ReceiveGrant {
                        source: entry.id,
                        epoch: entry.epoch,
                        sender_token: self
                            .remote_snapshot()
                            .expect("current remote")
                            .send_token(entry.id),
                        token: self.source_fence(entry.id),
                    })
                    .collect()
            } else {
                Vec::new()
            };
        if grants != self.subscriptions.grants {
            let Some(revision) = self.subscriptions.revision.checked_add(1) else {
                self.quiesce();
                return Err(PeerError::CounterExhausted);
            };
            self.subscriptions = Subscriptions { revision, grants };
            self.bump_admission_revision();
        }
        Ok(())
    }

    fn queue_subscriptions(&mut self) -> Result<(), PeerError> {
        self.refresh_subscriptions()?;
        let Some(active) = &self.active else {
            return Ok(());
        };
        self.queue(Body::Subscriptions {
            session: active.id,
            target_run: active.remote_run,
            revision: self.subscriptions.revision,
            ack: self.remote_subscriptions.revision,
            grants: self.subscriptions.grants.clone(),
        })
    }

    fn announcement(&self, id: SourceId) -> u64 {
        self.announcements
            .iter()
            .find(|(source, _)| *source == id)
            .map(|(_, revision)| *revision)
            .unwrap_or(u64::MAX)
    }

    fn publish_local(&mut self) -> Result<(), PeerError> {
        self.record_local_floors();
        if self.paused || self.quiesced {
            return Ok(());
        }
        if self.active.is_some() {
            self.queue_state()?;
        }
        if self.pending.is_some() {
            self.queue_pending()?;
        }
        Ok(())
    }

    fn prepare_catalogue(&mut self, catalogue: &Catalogue) -> Result<(), PeerError> {
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        for prior in self.local.sources.as_slice() {
            let Some(next) = catalogue.source(prior.id) else {
                return Err(PeerError::StalePublication);
            };
            if next.kind != prior.kind
                || next.epoch < prior.epoch
                || (next.epoch == prior.epoch && next != prior)
            {
                return Err(PeerError::StalePublication);
            }
        }
        if self.local.revision.checked_add(1).is_none() {
            self.quiesce();
            return Err(PeerError::CounterExhausted);
        }
        Ok(())
    }

    fn reserve_revision(
        &mut self,
        send_delta: u64,
        receive_delta: u64,
    ) -> Result<(u64, u64, u64), PeerError> {
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        let revision = self.local.revision.checked_add(1);
        let send = self.local.send_generation.checked_add(send_delta);
        let receive = self.local.receive_generation.checked_add(receive_delta);
        if revision.is_none() || send.is_none() || receive.is_none() {
            self.quiesce();
            return Err(PeerError::CounterExhausted);
        }
        Ok((revision.unwrap(), send.unwrap(), receive.unwrap()))
    }

    fn quiesce(&mut self) {
        if self.quiesced {
            return;
        }
        self.quiesced = true;
        self.paused = true;
        self.active = None;
        self.pending = None;
        self.outgoing.clear();
        self.bump_direction();
        self.note_send_gate();
    }

    fn note_send_gate(&mut self) {
        let ready = self.member_send_ready();
        if ready && !self.send_open {
            self.send_not_before_ms = self.now_ms;
            self.send_open = true;
        } else if !ready {
            self.send_open = false;
        }
        if ready != self.cached_send_ready {
            self.cached_send_ready = ready;
            self.bump_admission_revision();
        }
    }

    fn observe_watermark(&mut self, watermark: CaptureWatermark) -> Result<(), PeerError> {
        if watermark.allocated_through < self.applied_watermark {
            return Err(PeerError::RegressingWatermark);
        }
        self.applied_watermark = watermark.allocated_through;
        Ok(())
    }

    fn replace_interval(&mut self, watermark: CaptureWatermark) -> Result<(), PeerError> {
        if self.interval_replaced {
            return Ok(());
        }
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        if watermark.allocated_through < self.applied_watermark {
            return Err(PeerError::RegressingWatermark);
        }
        let Some(next) = self.outgoing_revision.checked_add(1) else {
            self.quiesce();
            return Err(PeerError::CounterExhausted);
        };
        self.applied_watermark = watermark.allocated_through;
        self.outgoing_revision = next;
        self.ticket_floor = watermark.allocated_through;
        self.interval_replaced = true;
        Ok(())
    }

    fn bump_admission_revision(&mut self) {
        match self.admission_revision.checked_add(1) {
            Some(next) if next < u64::MAX => self.admission_revision = next,
            _ => {
                // Reserve the last revision for a closed snapshot. Do not call
                // quiesce here: it bumps direction and returns to this method.
                self.admission_revision = u64::MAX;
                self.quiesced = true;
                self.paused = true;
                self.active = None;
                self.pending = None;
                self.outgoing.clear();
                self.send_open = false;
                self.cached_send_ready = false;
            }
        }
    }

    fn bump_direction(&mut self) {
        let token = self.alloc_fence();
        self.direction_fence = token;
        for fence in &mut self.source_fences {
            fence.1 = token;
        }
        self.bump_admission_revision();
    }

    fn bump_source(&mut self, id: SourceId) {
        let token = self.alloc_fence();
        if let Some(fence) = self.source_fences.iter_mut().find(|fence| fence.0 == id) {
            fence.1 = token;
        } else {
            self.source_fences.push((id, token));
        }
        self.bump_admission_revision();
    }

    fn alloc_fence(&mut self) -> u64 {
        match self.fence_cursor.checked_add(1) {
            Some(next) => {
                self.fence_cursor = next;
                next
            }
            None => {
                self.quiesced = true;
                self.fence_cursor
            }
        }
    }

    fn source_fence(&self, id: SourceId) -> u64 {
        self.source_fences
            .iter()
            .find(|fence| fence.0 == id)
            .map(|fence| fence.1)
            .unwrap_or(self.direction_fence)
    }

    fn queue(&mut self, body: Body) -> Result<(), PeerError> {
        let session = match &body {
            Body::Offer { session, .. } | Body::Confirm { session, .. }
            | Body::Ready { session, .. } | Body::State { session, .. }
            | Body::Subscriptions { session, .. } => Some(*session),
            Body::Hello { .. } => None,
        };
        let bytes = wire::encode(&Control {
            from: self.local_id,
            to: self.remote_id,
            run: self.run,
            body,
        })?;
        if self.outgoing.iter().any(|queued| queued.bytes == bytes) {
            return Ok(());
        }
        if self.outgoing.len() == MAX_CONTROL_QUEUE {
            self.outgoing.pop_front();
        }
        self.outgoing.push_back(QueuedControl { bytes, session });
        Ok(())
    }

    fn queue_hello(&mut self) -> Result<(), PeerError> {
        self.hello_ms = self.now_ms;
        let state = self.local.clone();
        self.queue(Body::Hello {
            contact: self.contact,
            state,
        })
    }

    fn queue_pending(&mut self) -> Result<(), PeerError> {
        let pending = self.pending.as_ref().unwrap();
        let state = self.local.clone();
        let body = if let Some(nonce) = pending.nonce {
            Body::Confirm {
                session: pending.session,
                target_run: pending.remote_run,
                contact: self.contact,
                target_contact: pending.remote_contact,
                nonce,
                state,
            }
        } else {
            Body::Offer {
                session: pending.session,
                target_run: pending.remote_run,
                contact: self.contact,
                target_contact: pending.remote_contact,
                state,
            }
        };
        self.queue(body)
    }

    fn queue_state(&mut self) -> Result<(), PeerError> {
        if self.quiesced || self.active.is_none() {
            return Ok(());
        }
        let ack = self
            .remote_snapshot()
            .map(Snapshot::ack)
            .unwrap_or_default();
        let active = self.active.as_mut().unwrap();
        active.sent_ms = self.now_ms;
        let state = self.local.clone();
        let body = Body::State {
            session: active.id,
            target_run: active.remote_run,
            state,
            ack,
        };
        self.queue(body)?;
        self.queue_subscriptions()
    }

    fn queue_ready(&mut self) -> Result<(), PeerError> {
        if self.quiesced || self.active.is_none() {
            return Ok(());
        }
        let ack = self
            .remote_snapshot()
            .map(Snapshot::ack)
            .unwrap_or_default();
        let active = self.active.as_mut().unwrap();
        active.sent_ms = self.now_ms;
        let state = self.local.clone();
        let body = Body::Ready {
            session: active.id,
            target_run: active.remote_run,
            contact: self.contact,
            target_contact: active.remote_contact,
            nonce: active.ready_nonce.unwrap(),
            state,
            ack,
        };
        self.queue(body)?;
        self.queue_subscriptions()
    }

    fn activate(
        &mut self,
        pending: Pending,
        remote: Snapshot,
        ack: Ack,
        ready_nonce: Option<BindNonce>,
    ) -> Result<(), PeerError> {
        self.install_snapshot(pending.remote_run, remote)?;
        if self.quiesced {
            return Err(PeerError::Quiesced);
        }
        self.sync_source_fences();
        self.active = Some(Session {
            id: pending.session,
            remote_run: pending.remote_run,
            remote_contact: pending.remote_contact,
            peer_ack: ack,
            ready_nonce,
            last_seen_ms: self.now_ms,
            sent_ms: self.now_ms,
        });
        self.last_control_accepted = true;
        self.pending = None;
        self.remote_subscriptions = Subscriptions::default();
        self.subscriptions_ack = 0;
        self.bump_direction();
        if let Some(watermark) = self.call_watermark {
            self.replace_interval(watermark)?;
        }
        self.note_send_gate();
        Ok(())
    }

    fn install_snapshot(&mut self, run: RunId, snapshot: Snapshot) -> Result<Installed, PeerError> {
        let Some(current) = self.remembered.clone() else {
            self.remembered = Some(Remembered {
                windows: fresh_windows(&snapshot),
                run,
                snapshot,
            });
            self.bump_admission_revision();
            return Ok(Installed::Replaced);
        };
        if current.run != run {
            self.remembered = Some(Remembered {
                windows: fresh_windows(&snapshot),
                run,
                snapshot,
            });
            self.bump_admission_revision();
            return Ok(Installed::Replaced);
        }
        if snapshot.revision < current.snapshot.revision {
            return Ok(Installed::Stale);
        }
        if snapshot == current.snapshot {
            return Ok(Installed::Identical);
        }
        if !snapshot.follows(&current.snapshot) {
            return Err(PeerError::MalformedControl);
        }
        // Intermediate refusal snapshots may be lost. A new generation still
        // closes the prior permission interval when the final boolean is true.
        let send_changed = snapshot.send_generation != current.snapshot.send_generation;
        let receive_changed = snapshot.receive_generation != current.snapshot.receive_generation;
        let mut windows = current.windows;
        for entry in snapshot.sources.as_slice() {
            let reset = match current.snapshot.sources.source(entry.id) {
                Some(prior) => prior.epoch != entry.epoch,
                None => true,
            };
            if reset {
                if let Some(window) = windows.iter_mut().find(|window| window.id == entry.id) {
                    window.epoch = entry.epoch.get();
                    window.seen = Seen::default();
                } else {
                    windows.push(Window {
                        id: entry.id,
                        epoch: entry.epoch.get(),
                        seen: Seen::default(),
                    });
                }
                self.bump_source(entry.id);
            } else if snapshot.send_token(entry.id) != current.snapshot.send_token(entry.id) {
                self.bump_source(entry.id);
            }
        }
        self.remembered = Some(Remembered {
            run,
            snapshot,
            windows,
        });
        if send_changed {
            self.bump_direction();
        }
        if receive_changed {
            self.send_open = false;
            if let Some(watermark) = self.call_watermark {
                self.replace_interval(watermark)?;
            }
            self.note_send_gate();
        }
        self.bump_admission_revision();
        Ok(Installed::Updated)
    }

    fn sync_source_fences(&mut self) {
        let ids = self.remote_source_ids();
        for id in ids {
            if !self.source_fences.iter().any(|fence| fence.0 == id) {
                self.source_fences.push((id, self.direction_fence));
            }
        }
    }

    fn ingest_control(&mut self, message: Control) -> Result<(), PeerError> {
        self.last_control_revision = match &message.body {
            Body::Hello { state, .. } | Body::Offer { state, .. }
            | Body::Confirm { state, .. } | Body::Ready { state, .. }
            | Body::State { state, .. } => Some(state.revision),
            _ => None,
        };
        self.last_control_session = match &message.body {
            Body::Offer { session, .. } | Body::Confirm { session, .. }
            | Body::Ready { session, .. } | Body::State { session, .. }
            | Body::Subscriptions { session, .. } => Some(*session),
            _ => None,
        };
        let run = message.run;
        match message.body {
            Body::Hello { contact, state } => self.on_hello(run, contact, state),
            Body::Offer {
                session,
                target_run,
                contact,
                target_contact,
                state,
            } => self.on_offer(run, session, target_run, contact, target_contact, state),
            Body::Confirm {
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
            } => self.on_confirm(
                run,
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
            ),
            Body::Ready {
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
                ack,
            } => self.on_ready(
                run,
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
                ack,
            ),
            Body::State {
                session,
                target_run,
                state,
                ack,
            } => self.on_state(run, session, target_run, state, ack),
            Body::Subscriptions {
                session,
                target_run,
                revision,
                ack,
                grants,
            } => self.on_subscriptions(run, session, target_run, revision, ack, grants),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_subscriptions(
        &mut self,
        run: RunId,
        session: DirectSessionId,
        target_run: RunId,
        revision: u64,
        ack: u64,
        grants: Vec<ReceiveGrant>,
    ) -> Result<(), PeerError> {
        if target_run != self.run
            || !self
                .active
                .as_ref()
                .is_some_and(|active| active.id == session && active.remote_run == run)
        {
            return Ok(());
        }
        if ack > self.subscriptions.revision {
            return Err(PeerError::MalformedControl);
        }
        let current = self.remote_subscriptions.revision;
        if revision == current && grants != self.remote_subscriptions.grants {
            return Err(PeerError::MalformedControl);
        }
        if revision > current
            && grants
                .iter()
                .any(|grant| self.local.sources.source(grant.source).is_none())
        {
            return Err(PeerError::MalformedControl);
        }
        self.subscriptions_ack = self.subscriptions_ack.max(ack);
        self.touch();
        if revision > current {
            for grant in &grants {
                if self.remote_grant(grant.source) != Some(grant) {
                    self.replace_source_interval(grant.source);
                }
            }
            self.remote_subscriptions = Subscriptions { revision, grants };
            self.note_send_gate();
            self.queue_subscriptions()?;
        }
        Ok(())
    }

    fn on_hello(
        &mut self,
        run: RunId,
        contact: HelloNonce,
        state: Snapshot,
    ) -> Result<(), PeerError> {
        if self.handshake_state_stale(run, &state) { return Ok(()); }
        if self
            .active
            .as_ref()
            .is_some_and(|session| session.remote_run == run && session.remote_contact == contact)
        {
            self.touch();
            return self.queue_state();
        }
        if self.leader() {
            self.note_leader_candidate(run, contact, state)?;
            self.last_control_accepted = true;
            if self.pending.is_some() { self.queue_pending()?; }
        } else {
            // A fresh leader may be trying another address while our prior
            // offer is pending. Reply with the same local contact identity;
            // resetting it on each path would chase in-flight offers forever.
            self.last_control_accepted = true;
            self.queue_hello()?;
        }
        Ok(())
    }

    fn on_offer(
        &mut self,
        run: RunId,
        session: DirectSessionId,
        target_run: RunId,
        contact: HelloNonce,
        target_contact: HelloNonce,
        state: Snapshot,
    ) -> Result<(), PeerError> {
        if self.leader() || target_run != self.run || target_contact != self.contact {
            return Ok(());
        }
        if self.handshake_state_stale(run, &state) { return Ok(()); }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.id == session && active.remote_run == run)
        {
            self.touch();
            return self.queue_state();
        }
        self.note_follower_candidate(session, run, contact, state)?;
        self.last_control_accepted = true;
        self.queue_pending()
    }

    #[allow(clippy::too_many_arguments)]
    fn on_confirm(
        &mut self,
        run: RunId,
        session: DirectSessionId,
        target_run: RunId,
        contact: HelloNonce,
        target_contact: HelloNonce,
        nonce: BindNonce,
        state: Snapshot,
    ) -> Result<(), PeerError> {
        if !self.leader() || target_run != self.run || target_contact != self.contact {
            return Ok(());
        }
        if self.active.as_ref().is_some_and(|active| {
            active.id == session && active.remote_run == run && active.ready_nonce == Some(nonce)
        }) {
            self.touch();
            return self.queue_ready();
        }
        let Some(pending) = self.pending.clone() else {
            return Ok(());
        };
        if pending.session != session
            || pending.remote_run != run
            || pending.remote_contact != contact
        {
            return Ok(());
        }
        if !self.accept_completion(&state, &pending)? {
            return Ok(());
        }
        self.activate(pending, state, Ack::default(), Some(nonce))?;
        self.queue_ready()
    }

    #[allow(clippy::too_many_arguments)]
    fn on_ready(
        &mut self,
        run: RunId,
        session: DirectSessionId,
        target_run: RunId,
        contact: HelloNonce,
        target_contact: HelloNonce,
        nonce: BindNonce,
        state: Snapshot,
        ack: Ack,
    ) -> Result<(), PeerError> {
        if self.leader() || target_run != self.run || target_contact != self.contact {
            return Ok(());
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.id == session && active.remote_run == run)
        {
            self.touch();
            return self.queue_state();
        }
        let Some(pending) = self.pending.clone() else {
            return Ok(());
        };
        if pending.session != session
            || pending.remote_run != run
            || pending.remote_contact != contact
            || pending.nonce != Some(nonce)
        {
            return Ok(());
        }
        let ack = self.interpret_ack(ack)?;
        if !self.accept_completion(&state, &pending)? {
            return Ok(());
        }
        self.activate(pending, state, ack.unwrap_or_default(), None)?;
        self.queue_state()
    }

    fn on_state(
        &mut self,
        run: RunId,
        session: DirectSessionId,
        target_run: RunId,
        state: Snapshot,
        ack: Ack,
    ) -> Result<(), PeerError> {
        if target_run != self.run
            || !self
                .active
                .as_ref()
                .is_some_and(|active| active.id == session && active.remote_run == run)
        {
            return Ok(());
        }
        let ack = self.interpret_ack(ack)?;
        let effect = self.install_snapshot(run, state)?;
        if let Some(ack) = ack {
            if let Some(active) = &mut self.active {
                merge_ack(&mut active.peer_ack, ack);
            }
        }
        if let Some(active) = &mut self.active {
            active.ready_nonce = None;
            active.last_seen_ms = self.now_ms;
        }
        self.note_send_gate();
        if matches!(effect, Installed::Updated | Installed::Replaced) {
            self.queue_state()?;
        }
        Ok(())
    }

    fn handshake_state_stale(&self, run: RunId, state: &Snapshot) -> bool {
        self.remembered.as_ref().is_some_and(|known| {
            known.run == run && state.revision < known.snapshot.revision
        }) || self.pending.as_ref().is_some_and(|pending| {
            pending.remote_run == run && state.revision < pending.remote.revision
        })
    }

    fn note_leader_candidate(
        &mut self,
        run: RunId,
        contact: HelloNonce,
        state: Snapshot,
    ) -> Result<(), PeerError> {
        if let Some(pending) = &self.pending {
            if pending.remote_run == run
                && pending.remote_contact == contact
                && pending.nonce.is_none()
            {
                if state.revision > pending.remote.revision {
                    if !state.follows(&pending.remote) {
                        return Err(PeerError::MalformedControl);
                    }
                    self.pending.as_mut().unwrap().remote = state;
                } else if state.revision == pending.remote.revision && state != pending.remote {
                    return Err(PeerError::MalformedControl);
                }
                return Ok(());
            }
        }
        self.pending = Some(Pending {
            session: DirectSessionId::generate()?,
            remote_run: run,
            remote_contact: contact,
            remote: state,
            nonce: None,
            started_ms: self.now_ms,
            retried_ms: self.now_ms,
        });
        Ok(())
    }

    fn note_follower_candidate(
        &mut self,
        session: DirectSessionId,
        run: RunId,
        contact: HelloNonce,
        state: Snapshot,
    ) -> Result<(), PeerError> {
        if let Some(pending) = &self.pending {
            if pending.session == session
                && pending.remote_run == run
                && pending.remote_contact == contact
            {
                if state.revision > pending.remote.revision {
                    if !state.follows(&pending.remote) {
                        return Err(PeerError::MalformedControl);
                    }
                    self.pending.as_mut().unwrap().remote = state;
                } else if state.revision == pending.remote.revision && state != pending.remote {
                    return Err(PeerError::MalformedControl);
                }
                return Ok(());
            }
        }
        self.pending = Some(Pending {
            session,
            remote_run: run,
            remote_contact: contact,
            remote: state,
            nonce: Some(BindNonce::generate()?),
            started_ms: self.now_ms,
            retried_ms: self.now_ms,
        });
        Ok(())
    }

    fn accept_completion(&self, state: &Snapshot, pending: &Pending) -> Result<bool, PeerError> {
        if !passes_floor(state, &pending.remote)? {
            return Ok(false);
        }
        if let Some(remembered) = &self.remembered {
            if remembered.run == pending.remote_run && !passes_floor(state, &remembered.snapshot)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn interpret_ack(&self, ack: Ack) -> Result<Option<Ack>, PeerError> {
        let zeros = u8::from(ack.send_generation == 0)
            + u8::from(ack.receive_generation == 0)
            + u8::from(ack.state_revision == 0);
        if zeros == 3 {
            return Ok(None);
        }
        if zeros != 0
            || ack.send_generation > ack.state_revision
            || ack.receive_generation > ack.state_revision
            || ack.state_revision > self.local.revision
            || ack.send_generation > self.local.send_generation
            || ack.receive_generation > self.local.receive_generation
            || (ack.state_revision == self.local.revision
                && (ack.send_generation != self.local.send_generation
                    || ack.receive_generation != self.local.receive_generation))
        {
            return Err(PeerError::MalformedControl);
        }
        Ok(Some(ack))
    }

    fn touch(&mut self) {
        self.last_control_accepted = true;
        if let Some(active) = &mut self.active {
            active.last_seen_ms = self.now_ms;
        }
    }

    fn ingest_audio(
        &mut self,
        bytes: &[u8],
        arrived_at_ms: u64,
    ) -> Result<Option<AdmittedOriginal>, PeerError> {
        let Some(candidate) = self.audio_candidate(bytes)? else {
            return Ok(None);
        };
        let format = candidate.format;
        let samples = media::decode_pcm(
            &bytes[MEDIA_HEADER_BYTES..],
            format,
            candidate.header.frames,
        )?;
        let Some(metadata) = self.commit_audio(candidate, arrived_at_ms)? else {
            return Ok(None);
        };
        Ok(Some(AdmittedOriginal {
            member_id: metadata.member_id,
            remote_run: metadata.remote_run,
            source_id: metadata.entry.id,
            epoch: metadata.entry.epoch,
            format,
            sequence: metadata.sequence,
            start_frame: metadata.start_frame,
            frames: metadata.frames,
            session: metadata.session,
            send_generation: metadata.send_generation,
            receive_generation: metadata.receive_generation,
            admission_token: metadata.source_fence,
            arrived_at_ms: metadata.arrived_at_ms,
            peer_instance: metadata.peer_instance,
            admission_revision: metadata.admission_revision,
            samples,
        }))
    }

    fn ingest_audio_into(
        &mut self,
        bytes: &[u8],
        arrived_at_ms: u64,
        scratch: &mut [f64],
    ) -> Result<Option<AdmittedMetadata>, PeerError> {
        let Some(candidate) = self.audio_candidate(bytes)? else {
            return Ok(None);
        };
        let sample_count = usize::from(candidate.header.frames)
            .checked_mul(usize::from(candidate.format.channels()))
            .ok_or(PeerError::InvalidAudio)?;
        if scratch.len() < sample_count {
            return Err(PeerError::StorageTooSmall);
        }
        media::decode_pcm_into(
            &bytes[MEDIA_HEADER_BYTES..],
            candidate.format,
            candidate.header.frames,
            scratch,
        )?;
        self.commit_audio(candidate, arrived_at_ms)
    }

    fn audio_candidate(&self, bytes: &[u8]) -> Result<Option<AudioCandidate>, PeerError> {
        let header = media::header(bytes)?;
        let Some(active_id) = self.active.as_ref().map(|session| session.id) else {
            return Ok(None);
        };
        let Some(remote_run) = self.active.as_ref().map(|session| session.remote_run) else {
            return Ok(None);
        };
        let generations_match = self.remote_snapshot().is_some_and(|remote| {
            self.source_receive_ready(header.source)
                && header.session == active_id
                && header.send_generation == u64::from(remote.send_token(header.source))
                && header.receive_generation == self.source_fence(header.source)
        });
        if !generations_match {
            return Ok(None);
        }
        let Some(entry) = self
            .remote_snapshot()
            .and_then(|remote| remote.sources.source(header.source).copied())
        else {
            return Ok(None);
        };
        if entry.state != PublicationState::Active || entry.epoch.get() != header.epoch {
            return Ok(None);
        }
        let Some(format) = entry.format else {
            return Ok(None);
        };
        if media::expected_payload(format, header.frames)? != usize::from(header.payload_bytes) {
            return Err(PeerError::InvalidAudio);
        }
        if !supports_format(self.local.receive_encodings, Some(format)) {
            return Ok(None);
        }
        Ok(Some(AudioCandidate {
            header,
            remote_run,
            entry,
            format,
        }))
    }

    fn commit_audio(
        &mut self,
        candidate: AudioCandidate,
        arrived_at_ms: u64,
    ) -> Result<Option<AdmittedMetadata>, PeerError> {
        let admitted = self.insert_seen(
            candidate.header.source,
            candidate.header.epoch,
            candidate.header.sequence,
        );
        if admitted != Some(true) {
            return Ok(None);
        }
        self.touch();
        Ok(Some(AdmittedMetadata {
            peer_instance: self.peer_instance,
            admission_revision: self.admission_revision,
            member_id: self.remote_id,
            remote_run: candidate.remote_run,
            entry: candidate.entry,
            sequence: candidate.header.sequence,
            start_frame: candidate.header.start_frame,
            frames: candidate.header.frames,
            session: candidate.header.session,
            send_generation: candidate.header.send_generation,
            receive_generation: candidate.header.receive_generation,
            source_fence: self.source_fence(candidate.header.source),
            arrived_at_ms,
        }))
    }

    fn insert_seen(&mut self, id: SourceId, epoch: u64, sequence: u64) -> Option<bool> {
        let remembered = self.remembered.as_mut()?;
        let window = remembered
            .windows
            .iter_mut()
            .find(|window| window.id == id && window.epoch == epoch)?;
        Some(window.seen.insert(sequence))
    }

    #[cfg(test)]
    pub(crate) fn has_seen(&self, id: SourceId, sequence: u64) -> bool {
        self.remembered
            .as_ref()
            .and_then(|remembered| remembered.windows.iter().find(|window| window.id == id))
            .is_some_and(|window| window.seen.contains(sequence))
    }

    #[cfg(test)]
    pub(crate) fn reopen_session_for_test(
        &mut self,
        watermark: CaptureWatermark,
    ) -> Result<bool, PeerError> {
        let ready_before = self.member_send_ready();
        let active = self.active.clone().ok_or(PeerError::WrongPeer)?;
        let remote = self
            .remote_snapshot()
            .cloned()
            .ok_or(PeerError::WrongPeer)?;
        let pending = Pending {
            session: DirectSessionId::generate()?,
            remote_run: active.remote_run,
            remote_contact: active.remote_contact,
            remote: remote.clone(),
            nonce: None,
            started_ms: self.now_ms,
            retried_ms: self.now_ms,
        };
        self.call_watermark = Some(watermark);
        self.interval_replaced = false;
        self.activate(pending, remote, active.peer_ack, None)?;
        self.call_watermark = None;
        Ok(ready_before && self.member_send_ready())
    }

    #[cfg(test)]
    pub(crate) fn advance_remote_receive_for_test(
        &mut self,
        watermark: CaptureWatermark,
    ) -> Result<(), PeerError> {
        let remembered = self.remembered.clone().ok_or(PeerError::WrongPeer)?;
        let mut snapshot = remembered.snapshot;
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.receive_generation = snapshot.receive_generation.saturating_add(1);
        self.call_watermark = Some(watermark);
        self.interval_replaced = false;
        self.install_snapshot(remembered.run, snapshot)?;
        self.note_send_gate();
        self.call_watermark = None;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn repeat_remote_snapshot_for_test(
        &mut self,
        watermark: CaptureWatermark,
    ) -> Result<u64, PeerError> {
        let remembered = self.remembered.clone().ok_or(PeerError::WrongPeer)?;
        self.call_watermark = Some(watermark);
        self.interval_replaced = false;
        self.install_snapshot(remembered.run, remembered.snapshot)?;
        self.call_watermark = None;
        Ok(self.outgoing_revision)
    }

    #[cfg(test)]
    pub(crate) fn exhaust_admission_on_next_change_for_test(&mut self) {
        self.admission_revision = u64::MAX - 1;
    }

    #[cfg(test)]
    pub(crate) fn exhaust_outgoing_for_test(&mut self) {
        self.outgoing_revision = u64::MAX;
    }
}

fn fresh_windows(snapshot: &Snapshot) -> Vec<Window> {
    snapshot
        .sources
        .as_slice()
        .iter()
        .map(|entry| Window {
            id: entry.id,
            epoch: entry.epoch.get(),
            seen: Seen::default(),
        })
        .collect()
}

fn passes_floor(candidate: &Snapshot, floor: &Snapshot) -> Result<bool, PeerError> {
    if candidate.revision < floor.revision {
        Ok(false)
    } else if candidate == floor || candidate.follows(floor) {
        Ok(true)
    } else {
        Err(PeerError::MalformedControl)
    }
}

fn merge_ack(current: &mut Ack, incoming: Ack) {
    if current.state_revision == 0 || incoming.state_revision > current.state_revision {
        *current = incoming;
    }
}

fn supports_format(mask: u8, format: Option<PcmFormat>) -> bool {
    match format.map(PcmFormat::encoding) {
        Some(PcmEncoding::F32Le) => mask & F32_ENCODING_BIT != 0,
        Some(PcmEncoding::Signed32Le) => mask & (1 << 1) != 0,
        Some(PcmEncoding::F64Le) => mask & (1 << 2) != 0,
        None => false,
    }
}
