use rlb_model::{
    CatalogRevision, DirectSessionId, HelloNonce, MemberId, RoomId, RunId, ServerGeneration,
    SourceId, SubscriptionRevision,
};
use thiserror::Error;

const MAGIC: &[u8; 4] = b"RLBD";
const VERSION_MAJOR: u8 = 2;
const VERSION_MINOR: u8 = 0;
const COMMON_BYTES: usize = 108;
const MAX_BYTES: usize = 168;

/// Exact RLBD direct-control kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum DirectKind {
    /// Nonce and liveness exchange.
    Hello = 1,
    /// Complete desired source set.
    SubscribeSet = 2,
    /// Complete accepted and rejected result.
    SubscribeAck = 3,
}

impl TryFrom<u8> for DirectKind {
    type Error = DirectError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::SubscribeSet),
            3 => Ok(Self::SubscribeAck),
            _ => Err(DirectError::UnknownKind),
        }
    }
}

/// Identity and direct-session tuple shared by every RLBD datagram.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectCommon {
    /// Current room.
    pub room_id: RoomId,
    /// Datagram sender.
    pub sender_member_id: MemberId,
    /// Sender's current process run.
    pub sender_run_id: RunId,
    /// Unique destination member.
    pub receiver_member_id: MemberId,
    /// Destination's current process run.
    pub receiver_run_id: RunId,
    /// Current peer-to-peer direct session.
    pub direct_session_id: DirectSessionId,
}

/// Exact 156-byte DIRECT_HELLO.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectHello {
    /// Common peer tuple.
    pub common: DirectCommon,
    /// Sender's latest complete registry snapshot.
    pub server_generation: ServerGeneration,
    /// Sender's direct-session nonce.
    pub hello_nonce: HelloNonce,
    /// Peer nonce echo; `None` encodes sixteen zero bytes.
    pub echo_nonce: Option<HelloNonce>,
}

/// Fixed-capacity, sorted source identity set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedSourceIds {
    ids: [Option<SourceId>; 2],
    len: u8,
}

impl BoundedSourceIds {
    /// Creates a sorted, duplicate-free set with at most two identities.
    pub fn new(ids: &[SourceId]) -> Result<Self, DirectError> {
        if ids.len() > 2 {
            return Err(DirectError::SourceCountOutOfRange);
        }
        if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(DirectError::SourceOrderInvalid);
        }
        let mut storage = [None; 2];
        for (slot, source_id) in storage.iter_mut().zip(ids.iter().copied()) {
            *slot = Some(source_id);
        }
        Ok(Self {
            ids: storage,
            len: ids.len() as u8,
        })
    }

    /// Returns an empty complete set.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            ids: [None; 2],
            len: 0,
        }
    }

    /// Returns the number of source identities.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len as usize
    }

    /// Returns whether the complete set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Returns one source by sorted index.
    #[must_use]
    pub const fn get(self, index: usize) -> Option<SourceId> {
        if index < self.len as usize {
            self.ids[index]
        } else {
            None
        }
    }

    /// Iterates source identities in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.ids[..self.len as usize].iter().copied().flatten()
    }

    /// Tests membership in the complete set.
    #[must_use]
    pub fn contains(&self, source_id: SourceId) -> bool {
        self.iter().any(|candidate| candidate == source_id)
    }
}

impl Default for BoundedSourceIds {
    fn default() -> Self {
        Self::empty()
    }
}

/// Exact SUBSCRIBE_SET body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscribeSet {
    /// Common peer tuple; sender is the subscriber.
    pub common: DirectCommon,
    /// Complete-set revision, at least one.
    pub subscription_revision: SubscriptionRevision,
    /// Complete publisher catalog held by the subscriber.
    pub catalog_revision: CatalogRevision,
    /// Complete desired source set.
    pub source_ids: BoundedSourceIds,
}

impl SubscribeSet {
    /// Creates a validated complete subscription request.
    pub fn new(
        common: DirectCommon,
        subscription_revision: SubscriptionRevision,
        catalog_revision: CatalogRevision,
        source_ids: BoundedSourceIds,
    ) -> Result<Self, DirectError> {
        if subscription_revision.get() == 0 {
            return Err(DirectError::SubscriptionRevisionZero);
        }
        Ok(Self {
            common,
            subscription_revision,
            catalog_revision,
            source_ids,
        })
    }
}

/// Whole-request ACK status.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum AckStatus {
    /// Per-source accepted and rejected entries cover the requested set.
    Ok = 0,
    /// An equal revision carried different bytes.
    RevisionConflict = 1,
    /// Publisher catalog differs from the requested catalog.
    CatalogStale = 2,
}

impl TryFrom<u8> for AckStatus {
    type Error = DirectError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::RevisionConflict),
            2 => Ok(Self::CatalogStale),
            _ => Err(DirectError::UnknownStatus),
        }
    }
}

/// Stable per-source rejection code.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u16)]
pub enum RejectionCode {
    /// Source does not appear in the current catalog.
    SourceUnknown = 1,
    /// Source is hidden from this receiver.
    SourceHidden = 2,
    /// Source backend is unavailable.
    SourceUnavailable = 3,
    /// Source publication is disabled.
    SourceUnpublished = 4,
    /// Authority room send mute is active.
    RoomSendMuted = 5,
    /// One of the members lacks approval.
    MemberNotApproved = 6,
    /// Publisher emergency stop is active.
    LocalEmergencyStop = 7,
}

impl TryFrom<u16> for RejectionCode {
    type Error = DirectError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::SourceUnknown),
            2 => Ok(Self::SourceHidden),
            3 => Ok(Self::SourceUnavailable),
            4 => Ok(Self::SourceUnpublished),
            5 => Ok(Self::RoomSendMuted),
            6 => Ok(Self::MemberNotApproved),
            7 => Ok(Self::LocalEmergencyStop),
            _ => Err(DirectError::UnknownRejectionCode),
        }
    }
}

/// One canonical rejected source entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rejection {
    /// Rejected stable source.
    pub source_id: SourceId,
    /// Stable product reason.
    pub code: RejectionCode,
}

/// Fixed-capacity, source-sorted rejection entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedRejections {
    entries: [Option<Rejection>; 2],
    len: u8,
}

impl BoundedRejections {
    /// Creates a canonical rejection list with at most two entries.
    pub fn new(entries: &[Rejection]) -> Result<Self, DirectError> {
        if entries.len() > 2 {
            return Err(DirectError::SourceCountOutOfRange);
        }
        if entries
            .windows(2)
            .any(|pair| pair[0].source_id >= pair[1].source_id)
        {
            return Err(DirectError::SourceOrderInvalid);
        }
        let mut storage = [None; 2];
        for (slot, entry) in storage.iter_mut().zip(entries.iter().copied()) {
            *slot = Some(entry);
        }
        Ok(Self {
            entries: storage,
            len: entries.len() as u8,
        })
    }

    /// Returns an empty list.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: [None; 2],
            len: 0,
        }
    }

    /// Returns the entry count.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len as usize
    }

    /// Returns whether no entries are present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Iterates entries in canonical source order.
    pub fn iter(&self) -> impl Iterator<Item = Rejection> + '_ {
        self.entries[..self.len as usize].iter().copied().flatten()
    }

    /// Tests whether a source appears in this list.
    #[must_use]
    pub fn contains(&self, source_id: SourceId) -> bool {
        self.iter().any(|entry| entry.source_id == source_id)
    }
}

impl Default for BoundedRejections {
    fn default() -> Self {
        Self::empty()
    }
}

/// Exact SUBSCRIBE_ACK body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscribeAck {
    /// Common peer tuple; sender is the publisher.
    pub common: DirectCommon,
    /// Confirmed or current subscription revision.
    pub subscription_revision: SubscriptionRevision,
    /// Publisher's current complete catalog revision.
    pub catalog_revision: CatalogRevision,
    /// Whole-request status.
    pub status: AckStatus,
    /// Canonical accepted source set.
    pub accepted_source_ids: BoundedSourceIds,
    /// Canonical rejected source entries.
    pub rejections: BoundedRejections,
}

impl SubscribeAck {
    /// Creates a structurally valid ACK.
    pub fn new(
        common: DirectCommon,
        subscription_revision: SubscriptionRevision,
        catalog_revision: CatalogRevision,
        status: AckStatus,
        accepted_source_ids: BoundedSourceIds,
        rejections: BoundedRejections,
    ) -> Result<Self, DirectError> {
        if subscription_revision.get() == 0 {
            return Err(DirectError::SubscriptionRevisionZero);
        }
        if accepted_source_ids.len() + rejections.len() > 2 {
            return Err(DirectError::SourceCountOutOfRange);
        }
        if status != AckStatus::Ok && (!accepted_source_ids.is_empty() || !rejections.is_empty()) {
            return Err(DirectError::AckStatusCountsInvalid);
        }
        if accepted_source_ids
            .iter()
            .any(|source_id| rejections.contains(source_id))
        {
            return Err(DirectError::AckSourceOverlap);
        }
        Ok(Self {
            common,
            subscription_revision,
            catalog_revision,
            status,
            accepted_source_ids,
            rejections,
        })
    }

    /// Verifies the status-OK union against its exact SET.
    pub fn validate_against(&self, request: &SubscribeSet) -> Result<(), DirectError> {
        if self.status != AckStatus::Ok {
            return Ok(());
        }
        if self.subscription_revision != request.subscription_revision
            || self.catalog_revision != request.catalog_revision
        {
            return Err(DirectError::AckCoverageInvalid);
        }
        let covered = self.accepted_source_ids.len() + self.rejections.len();
        if covered != request.source_ids.len() {
            return Err(DirectError::AckCoverageInvalid);
        }
        for source_id in request.source_ids.iter() {
            let matches = usize::from(self.accepted_source_ids.contains(source_id))
                + usize::from(self.rejections.contains(source_id));
            if matches != 1 {
                return Err(DirectError::AckCoverageInvalid);
            }
        }
        Ok(())
    }
}

/// Owned direct-control packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectDatagram {
    /// DIRECT_HELLO.
    Hello(DirectHello),
    /// SUBSCRIBE_SET.
    SubscribeSet(SubscribeSet),
    /// SUBSCRIBE_ACK.
    SubscribeAck(SubscribeAck),
}

/// Stable closed RLBD parser and encoder failure set.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DirectError {
    /// Input had fewer than twelve bytes.
    #[error("datagram_too_short")]
    DatagramTooShort,
    /// Input exceeded the 168-byte direct limit.
    #[error("datagram_too_large")]
    DatagramTooLarge,
    /// Magic was not `RLBD`.
    #[error("bad_magic")]
    BadMagic,
    /// Version was not exactly 2.0.
    #[error("unsupported_version")]
    UnsupportedVersion,
    /// Kind was outside 1–3.
    #[error("unknown_kind")]
    UnknownKind,
    /// Common header length was not 108.
    #[error("bad_header_length")]
    BadHeaderLength,
    /// `total_bytes` or a kind-specific exact length was wrong.
    #[error("length_mismatch")]
    LengthMismatch,
    /// Flags or kind-specific reserved bytes were nonzero.
    #[error("reserved_nonzero")]
    ReservedNonzero,
    /// A required identity or nonce was zero.
    #[error("zero_identity")]
    ZeroIdentity,
    /// A source count exceeded two.
    #[error("source_count_out_of_range")]
    SourceCountOutOfRange,
    /// Source entries were duplicated or out of order.
    #[error("source_order_invalid")]
    SourceOrderInvalid,
    /// New direct revision domains begin at one.
    #[error("subscription_revision_zero")]
    SubscriptionRevisionZero,
    /// ACK status was outside 0–2.
    #[error("unknown_status")]
    UnknownStatus,
    /// Rejection code was outside 1–7.
    #[error("unknown_rejection_code")]
    UnknownRejectionCode,
    /// A non-OK ACK carried per-source entries.
    #[error("ack_status_counts_invalid")]
    AckStatusCountsInvalid,
    /// A source appeared in both accepted and rejected entries.
    #[error("ack_source_overlap")]
    AckSourceOverlap,
    /// Status-OK entries did not exactly cover the corresponding SET.
    #[error("ack_coverage_invalid")]
    AckCoverageInvalid,
    /// Caller-owned output storage was too short.
    #[error("output_too_small")]
    OutputTooSmall,
}

/// Strictly parses one RLBD direct-control datagram.
pub fn parse_direct(bytes: &[u8]) -> Result<DirectDatagram, DirectError> {
    if bytes.len() < 12 {
        return Err(DirectError::DatagramTooShort);
    }
    if bytes.len() > MAX_BYTES {
        return Err(DirectError::DatagramTooLarge);
    }
    if &bytes[0..4] != MAGIC {
        return Err(DirectError::BadMagic);
    }
    if bytes[4] != VERSION_MAJOR || bytes[5] != VERSION_MINOR {
        return Err(DirectError::UnsupportedVersion);
    }
    let kind = DirectKind::try_from(bytes[6])?;
    if usize::from(bytes[7]) != COMMON_BYTES {
        return Err(DirectError::BadHeaderLength);
    }
    if usize::from(read_u16(bytes, 8)) != bytes.len() {
        return Err(DirectError::LengthMismatch);
    }
    if read_u16(bytes, 10) != 0 {
        return Err(DirectError::ReservedNonzero);
    }
    if bytes.len() < COMMON_BYTES {
        return Err(DirectError::LengthMismatch);
    }
    let common = parse_common(bytes)?;

    match kind {
        DirectKind::Hello => parse_hello(bytes, common),
        DirectKind::SubscribeSet => parse_set(bytes, common),
        DirectKind::SubscribeAck => parse_ack(bytes, common),
    }
}

/// Encodes one exact RLBD datagram into caller-owned storage.
pub fn encode_direct_into(
    datagram: DirectDatagram,
    output: &mut [u8],
) -> Result<usize, DirectError> {
    match datagram {
        DirectDatagram::Hello(hello) => {
            let required = 156;
            prepare_output(output, required, DirectKind::Hello, hello.common)?;
            output[108..124].copy_from_slice(hello.server_generation.as_bytes());
            output[124..140].copy_from_slice(hello.hello_nonce.as_bytes());
            if let Some(echo) = hello.echo_nonce {
                output[140..156].copy_from_slice(echo.as_bytes());
            }
            Ok(required)
        }
        DirectDatagram::SubscribeSet(request) => {
            let request = SubscribeSet::new(
                request.common,
                request.subscription_revision,
                request.catalog_revision,
                request.source_ids,
            )?;
            let required = 128 + request.source_ids.len() * 16;
            prepare_output(output, required, DirectKind::SubscribeSet, request.common)?;
            output[108..116].copy_from_slice(&request.subscription_revision.get().to_be_bytes());
            output[116..124].copy_from_slice(&request.catalog_revision.get().to_be_bytes());
            output[124] = request.source_ids.len() as u8;
            for (index, source_id) in request.source_ids.iter().enumerate() {
                let offset = 128 + index * 16;
                output[offset..offset + 16].copy_from_slice(source_id.as_bytes());
            }
            Ok(required)
        }
        DirectDatagram::SubscribeAck(ack) => {
            let ack = SubscribeAck::new(
                ack.common,
                ack.subscription_revision,
                ack.catalog_revision,
                ack.status,
                ack.accepted_source_ids,
                ack.rejections,
            )?;
            let required = 128 + ack.accepted_source_ids.len() * 16 + ack.rejections.len() * 20;
            prepare_output(output, required, DirectKind::SubscribeAck, ack.common)?;
            output[108..116].copy_from_slice(&ack.subscription_revision.get().to_be_bytes());
            output[116..124].copy_from_slice(&ack.catalog_revision.get().to_be_bytes());
            output[124] = ack.status as u8;
            output[125] = ack.accepted_source_ids.len() as u8;
            output[126] = ack.rejections.len() as u8;
            let mut offset = 128;
            for source_id in ack.accepted_source_ids.iter() {
                output[offset..offset + 16].copy_from_slice(source_id.as_bytes());
                offset += 16;
            }
            for rejection in ack.rejections.iter() {
                output[offset..offset + 16].copy_from_slice(rejection.source_id.as_bytes());
                output[offset + 16..offset + 18]
                    .copy_from_slice(&(rejection.code as u16).to_be_bytes());
                offset += 20;
            }
            Ok(required)
        }
    }
}

fn parse_hello(bytes: &[u8], common: DirectCommon) -> Result<DirectDatagram, DirectError> {
    if bytes.len() != 156 {
        return Err(DirectError::LengthMismatch);
    }
    let server_generation =
        ServerGeneration::from_bytes(read_id(bytes, 108)).map_err(|_| DirectError::ZeroIdentity)?;
    let hello_nonce =
        HelloNonce::from_bytes(read_id(bytes, 124)).map_err(|_| DirectError::ZeroIdentity)?;
    let echo_bytes = read_id(bytes, 140);
    let echo_nonce = if echo_bytes == [0; 16] {
        None
    } else {
        Some(HelloNonce::from_bytes(echo_bytes).map_err(|_| DirectError::ZeroIdentity)?)
    };
    Ok(DirectDatagram::Hello(DirectHello {
        common,
        server_generation,
        hello_nonce,
        echo_nonce,
    }))
}

fn parse_set(bytes: &[u8], common: DirectCommon) -> Result<DirectDatagram, DirectError> {
    if bytes.len() < 128 {
        return Err(DirectError::LengthMismatch);
    }
    if bytes[125..128].iter().any(|byte| *byte != 0) {
        return Err(DirectError::ReservedNonzero);
    }
    let count = usize::from(bytes[124]);
    if count > 2 {
        return Err(DirectError::SourceCountOutOfRange);
    }
    if bytes.len() != 128 + count * 16 {
        return Err(DirectError::LengthMismatch);
    }
    let revision = SubscriptionRevision::new(read_u64(bytes, 108));
    if revision.get() == 0 {
        return Err(DirectError::SubscriptionRevisionZero);
    }
    let mut parsed = [None; 2];
    for (index, destination) in parsed.iter_mut().take(count).enumerate() {
        let offset = 128 + index * 16;
        *destination = Some(
            SourceId::from_bytes(read_id(bytes, offset)).map_err(|_| DirectError::ZeroIdentity)?,
        );
    }
    let ids = source_ids_from_options(parsed, count)?;
    Ok(DirectDatagram::SubscribeSet(SubscribeSet::new(
        common,
        revision,
        CatalogRevision::new(read_u64(bytes, 116)),
        ids,
    )?))
}

fn parse_ack(bytes: &[u8], common: DirectCommon) -> Result<DirectDatagram, DirectError> {
    if bytes.len() < 128 {
        return Err(DirectError::LengthMismatch);
    }
    if bytes[127] != 0 {
        return Err(DirectError::ReservedNonzero);
    }
    let status = AckStatus::try_from(bytes[124])?;
    let accepted_count = usize::from(bytes[125]);
    let rejected_count = usize::from(bytes[126]);
    if accepted_count > 2 || rejected_count > 2 || accepted_count + rejected_count > 2 {
        return Err(DirectError::SourceCountOutOfRange);
    }
    let expected = 128 + accepted_count * 16 + rejected_count * 20;
    if bytes.len() != expected {
        return Err(DirectError::LengthMismatch);
    }
    if status != AckStatus::Ok && accepted_count + rejected_count != 0 {
        return Err(DirectError::AckStatusCountsInvalid);
    }
    let revision = SubscriptionRevision::new(read_u64(bytes, 108));
    if revision.get() == 0 {
        return Err(DirectError::SubscriptionRevisionZero);
    }

    let mut accepted = [None; 2];
    let mut offset = 128;
    for destination in accepted.iter_mut().take(accepted_count) {
        *destination = Some(
            SourceId::from_bytes(read_id(bytes, offset)).map_err(|_| DirectError::ZeroIdentity)?,
        );
        offset += 16;
    }
    let accepted = source_ids_from_options(accepted, accepted_count)?;

    let mut rejections = [None; 2];
    for destination in rejections.iter_mut().take(rejected_count) {
        let source_id =
            SourceId::from_bytes(read_id(bytes, offset)).map_err(|_| DirectError::ZeroIdentity)?;
        let code = RejectionCode::try_from(read_u16(bytes, offset + 16))?;
        if read_u16(bytes, offset + 18) != 0 {
            return Err(DirectError::ReservedNonzero);
        }
        *destination = Some(Rejection { source_id, code });
        offset += 20;
    }
    let rejections = rejections_from_options(rejections, rejected_count)?;
    Ok(DirectDatagram::SubscribeAck(SubscribeAck::new(
        common,
        revision,
        CatalogRevision::new(read_u64(bytes, 116)),
        status,
        accepted,
        rejections,
    )?))
}

fn parse_common(bytes: &[u8]) -> Result<DirectCommon, DirectError> {
    Ok(DirectCommon {
        room_id: RoomId::from_bytes(read_id(bytes, 12)).map_err(|_| DirectError::ZeroIdentity)?,
        sender_member_id: MemberId::from_bytes(read_id(bytes, 28))
            .map_err(|_| DirectError::ZeroIdentity)?,
        sender_run_id: RunId::from_bytes(read_id(bytes, 44))
            .map_err(|_| DirectError::ZeroIdentity)?,
        receiver_member_id: MemberId::from_bytes(read_id(bytes, 60))
            .map_err(|_| DirectError::ZeroIdentity)?,
        receiver_run_id: RunId::from_bytes(read_id(bytes, 76))
            .map_err(|_| DirectError::ZeroIdentity)?,
        direct_session_id: DirectSessionId::from_bytes(read_id(bytes, 92))
            .map_err(|_| DirectError::ZeroIdentity)?,
    })
}

fn source_ids_from_options(
    ids: [Option<SourceId>; 2],
    count: usize,
) -> Result<BoundedSourceIds, DirectError> {
    match count {
        0 => BoundedSourceIds::new(&[]),
        1 => BoundedSourceIds::new(&[ids[0].ok_or(DirectError::ZeroIdentity)?]),
        2 => BoundedSourceIds::new(&[
            ids[0].ok_or(DirectError::ZeroIdentity)?,
            ids[1].ok_or(DirectError::ZeroIdentity)?,
        ]),
        _ => Err(DirectError::SourceCountOutOfRange),
    }
}

fn rejections_from_options(
    entries: [Option<Rejection>; 2],
    count: usize,
) -> Result<BoundedRejections, DirectError> {
    match count {
        0 => BoundedRejections::new(&[]),
        1 => BoundedRejections::new(&[entries[0].ok_or(DirectError::ZeroIdentity)?]),
        2 => BoundedRejections::new(&[
            entries[0].ok_or(DirectError::ZeroIdentity)?,
            entries[1].ok_or(DirectError::ZeroIdentity)?,
        ]),
        _ => Err(DirectError::SourceCountOutOfRange),
    }
}

fn prepare_output(
    output: &mut [u8],
    required: usize,
    kind: DirectKind,
    common: DirectCommon,
) -> Result<(), DirectError> {
    if output.len() < required {
        return Err(DirectError::OutputTooSmall);
    }
    output[..required].fill(0);
    output[0..4].copy_from_slice(MAGIC);
    output[4] = VERSION_MAJOR;
    output[5] = VERSION_MINOR;
    output[6] = kind as u8;
    output[7] = COMMON_BYTES as u8;
    output[8..10].copy_from_slice(&(required as u16).to_be_bytes());
    output[12..28].copy_from_slice(common.room_id.as_bytes());
    output[28..44].copy_from_slice(common.sender_member_id.as_bytes());
    output[44..60].copy_from_slice(common.sender_run_id.as_bytes());
    output[60..76].copy_from_slice(common.receiver_member_id.as_bytes());
    output[76..92].copy_from_slice(common.receiver_run_id.as_bytes());
    output[92..108].copy_from_slice(common.direct_session_id.as_bytes());
    Ok(())
}

fn read_id(bytes: &[u8], offset: usize) -> [u8; 16] {
    bytes[offset..offset + 16]
        .try_into()
        .expect("validated direct length")
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated direct length"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated direct length"),
    )
}
