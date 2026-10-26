use rlb_codec::{WireCodec, encoded_len};
use rlb_model::{MEDIA_UNIT_FRAMES, MemberId, RoomId, RunId, SourceId, StreamEpoch};
use thiserror::Error;

use crate::{
    FIXED_HEADER_BYTES, MAX_FRAGMENT_DATA_BYTES, MAX_FRAGMENTS, MAX_UDP_APPLICATION_BYTES,
};

const MAGIC: &[u8; 4] = b"RLB2";
const VERSION_MAJOR: u8 = 2;
const VERSION_MINOR: u8 = 0;

/// Identity tuple carried by every RLB2 datagram.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaIdentity {
    /// Active room.
    pub room_id: RoomId,
    /// Actual datagram sender.
    pub sender_member_id: MemberId,
    /// Current sender process run.
    pub sender_run_id: RunId,
    /// Source owned by the publisher.
    pub source_id: SourceId,
    /// Active source timeline identity.
    pub stream_epoch: StreamEpoch,
}

/// Exact RLB2 kind value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PacketKind {
    /// One deterministic fragment of a 480-frame unit.
    Media = 1,
    /// Sender-declared source-time gap.
    Gap = 2,
    /// Missing-fragment request.
    Nack = 3,
    /// Exact source timeline end.
    EpochEnd = 4,
}

impl TryFrom<u8> for PacketKind {
    type Error = PacketError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Media),
            2 => Ok(Self::Gap),
            3 => Ok(Self::Nack),
            4 => Ok(Self::EpochEnd),
            _ => Err(PacketError::UnknownKind),
        }
    }
}

/// Exact sender GAP reason.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum GapReason {
    /// Known capture adapter overrun.
    CaptureOverrun = 1,
    /// Bounded send queue removed an indexed unit.
    SendQueueOverflow = 2,
    /// Sender missed the ten-millisecond unit deadline.
    SendDeadlineDrop = 3,
    /// Other known-length adapter discontinuity.
    KnownAdapterGap = 4,
}

impl TryFrom<u8> for GapReason {
    type Error = PacketError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::CaptureOverrun),
            2 => Ok(Self::SendQueueOverflow),
            3 => Ok(Self::SendDeadlineDrop),
            4 => Ok(Self::KnownAdapterGap),
            _ => Err(PacketError::GapReasonInvalid),
        }
    }
}

/// Exact normal epoch-end reason.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum EpochEndReason {
    /// Capture was intentionally stopped.
    SourceStopped = 1,
    /// A replacement format or timeline became active.
    EpochReplaced = 2,
    /// The configured source became unavailable.
    SourceUnavailable = 3,
    /// The member engine is shutting down.
    EngineShutdown = 4,
}

impl TryFrom<u8> for EpochEndReason {
    type Error = PacketError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::SourceStopped),
            2 => Ok(Self::EpochReplaced),
            3 => Ok(Self::SourceUnavailable),
            4 => Ok(Self::EngineShutdown),
            _ => Err(PacketError::EpochEndInvalid),
        }
    }
}

/// Descriptor shared by every fragment of one MEDIA unit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaUnitHeader {
    /// Common identity tuple.
    pub identity: MediaIdentity,
    /// Epoch record sequence shared by all fragments.
    pub unit_sequence: u32,
    /// Inclusive source sample start.
    pub start_sample: u64,
    /// Fragment index.
    pub fragment_index: u8,
    /// Exact deterministic fragment count.
    pub fragment_count: u8,
    /// Independent wire codec value.
    pub codec: WireCodec,
    /// One or two interleaved channels.
    pub channels: u8,
    /// Exact complete encoded payload bytes.
    pub unit_payload_bytes: u32,
}

impl MediaUnitHeader {
    /// Calculates the exact complete unit payload for the fixed V2 format.
    pub fn expected_unit_payload_bytes(codec: WireCodec, channels: u8) -> Result<u32, PacketError> {
        let bytes = encoded_len(codec, channels, usize::from(MEDIA_UNIT_FRAMES))
            .map_err(|_| PacketError::SourceFormatMismatch)?;
        u32::try_from(bytes).map_err(|_| PacketError::UnitPayloadMismatch)
    }

    /// Calculates the exact fragment count for a complete payload.
    #[must_use]
    pub const fn expected_fragment_count(unit_payload_bytes: u32) -> u8 {
        unit_payload_bytes.div_ceil(MAX_FRAGMENT_DATA_BYTES as u32) as u8
    }

    /// Calculates one deterministic fragment payload length.
    pub fn expected_fragment_payload_bytes(&self) -> Result<usize, PacketError> {
        expected_fragment_payload_bytes(
            self.unit_payload_bytes,
            self.fragment_index,
            self.fragment_count,
        )
    }

    /// Returns this fragment's deterministic offset in the complete payload.
    #[must_use]
    pub const fn fragment_offset(&self) -> usize {
        self.fragment_index as usize * MAX_FRAGMENT_DATA_BYTES
    }

    /// Returns the descriptor fields which all fragments must share.
    #[must_use]
    pub const fn descriptor_without_index(self) -> MediaDescriptor {
        MediaDescriptor {
            identity: self.identity,
            unit_sequence: self.unit_sequence,
            start_sample: self.start_sample,
            fragment_count: self.fragment_count,
            codec: self.codec,
            channels: self.channels,
            unit_payload_bytes: self.unit_payload_bytes,
        }
    }
}

/// Fragment-invariant MEDIA descriptor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaDescriptor {
    /// Common identity tuple.
    pub identity: MediaIdentity,
    /// Epoch record sequence.
    pub unit_sequence: u32,
    /// Inclusive source sample start.
    pub start_sample: u64,
    /// Deterministic fragment count.
    pub fragment_count: u8,
    /// Payload codec.
    pub codec: WireCodec,
    /// Interleaved channels.
    pub channels: u8,
    /// Complete payload bytes.
    pub unit_payload_bytes: u32,
}

/// Borrowed MEDIA fragment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaFragment<'a> {
    /// Parsed fixed header.
    pub header: MediaUnitHeader,
    /// Exact borrowed fragment bytes.
    pub payload: &'a [u8],
}

/// Parsed sender-declared GAP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GapRecord {
    /// Common identity tuple.
    pub identity: MediaIdentity,
    /// Sequence of the first record represented by this interval.
    pub record_sequence: u32,
    /// Inclusive source sample start.
    pub start_sample: u64,
    /// Positive source-frame count.
    pub frame_count: u32,
    /// Stable sender classification.
    pub reason: GapReason,
}

/// Parsed receiver NACK request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NackRecord {
    /// Common identity tuple; sender is the receiver.
    pub identity: MediaIdentity,
    /// Exact history sequence.
    pub target_unit_sequence: u32,
    /// Exact history source sample start.
    pub unit_start_sample: u64,
    /// Requested fragment bits.
    pub missing_fragment_mask: u32,
    /// Receiver observation sequence.
    pub nack_id: u32,
}

/// Parsed exact epoch end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpochEndRecord {
    /// Common identity tuple.
    pub identity: MediaIdentity,
    /// Sequence allocator's next unassigned value.
    pub next_record_sequence: u32,
    /// Exclusive source sample end.
    pub final_sample: u64,
    /// Normal end classification.
    pub reason: EpochEndReason,
}

/// Borrowed result of strict RLB2 syntax parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParsedDatagram<'a> {
    /// MEDIA fragment.
    Media(MediaFragment<'a>),
    /// GAP record.
    Gap(GapRecord),
    /// NACK request.
    Nack(NackRecord),
    /// EPOCH_END record.
    EpochEnd(EpochEndRecord),
}

/// Active catalog format checks available to the syntax parser.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParseContext {
    /// Expected active room.
    pub expected_room_id: Option<RoomId>,
    /// Expected datagram sender member.
    pub expected_sender_member_id: Option<MemberId>,
    /// Expected current sender process run.
    pub expected_sender_run_id: Option<RunId>,
    /// Expected source owned by the sender.
    pub expected_source_id: Option<SourceId>,
    /// Expected active catalog epoch.
    pub expected_stream_epoch: Option<StreamEpoch>,
    /// Optional effective-subscription gate.
    pub subscription_allowed: Option<bool>,
    /// Expected source channel count.
    pub expected_channels: Option<u8>,
    /// Expected active epoch codec.
    pub expected_codec: Option<WireCodec>,
}

/// Stable primary parser rejection.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PacketError {
    /// Datagram had fewer than twelve bytes.
    #[error("datagram_too_short")]
    DatagramTooShort,
    /// Datagram exceeded 1,200 bytes.
    #[error("datagram_too_large")]
    DatagramTooLarge,
    /// Magic was not `RLB2`.
    #[error("bad_magic")]
    BadMagic,
    /// Version was not exactly 2.0.
    #[error("unsupported_version")]
    UnsupportedVersion,
    /// Kind was outside 1–4.
    #[error("unknown_kind")]
    UnknownKind,
    /// Fixed header length was not 120.
    #[error("bad_header_length")]
    BadHeaderLength,
    /// Header payload length did not equal the datagram length.
    #[error("length_mismatch")]
    LengthMismatch,
    /// Flags or kind-specific reserved bytes were nonzero.
    #[error("reserved_nonzero")]
    ReservedNonzero,
    /// At least one required identity was all zero.
    #[error("zero_identity")]
    ZeroIdentity,
    /// Room identity differed from the active room.
    #[error("room_mismatch")]
    RoomMismatch,
    /// Sender member identity differed from the bound peer.
    #[error("member_mismatch")]
    MemberMismatch,
    /// Sender run was replaced by the direct session.
    #[error("stale_run_packet")]
    StaleRunPacket,
    /// Source was not owned by the bound publisher.
    #[error("source_unknown")]
    SourceUnknown,
    /// Current local subscription denied the source.
    #[error("subscription_denied")]
    SubscriptionDenied,
    /// Epoch was absent from the active catalog descriptor.
    #[error("media_epoch_unannounced")]
    MediaEpochUnannounced,
    /// The fixed unit frame count was not 480.
    #[error("unit_frames_invalid")]
    UnitFramesInvalid,
    /// MEDIA codec was outside the closed first-release set.
    #[error("unsupported_codec")]
    UnsupportedCodec,
    /// Channels contradicted the active source descriptor.
    #[error("source_format_mismatch")]
    SourceFormatMismatch,
    /// Complete payload arithmetic did not match the descriptor.
    #[error("unit_payload_mismatch")]
    UnitPayloadMismatch,
    /// Fragment count was zero, above 32, or non-deterministic.
    #[error("fragment_count_out_of_range")]
    FragmentCountOutOfRange,
    /// Fragment index was outside the declared count.
    #[error("fragment_index_out_of_range")]
    FragmentIndexOutOfRange,
    /// Fragment payload did not use the deterministic slice length.
    #[error("fragment_length_mismatch")]
    FragmentLengthMismatch,
    /// GAP source range was invalid.
    #[error("gap_range_invalid")]
    GapRangeInvalid,
    /// GAP reason was outside 1–4.
    #[error("gap_reason_invalid")]
    GapReasonInvalid,
    /// NACK mask contained no requested fragment.
    #[error("nack_mask_empty")]
    NackMaskEmpty,
    /// NACK selected a bit outside the target fragment count.
    #[error("nack_mask_out_of_range")]
    NackMaskOutOfRange,
    /// EPOCH_END fields were invalid.
    #[error("epoch_end_invalid")]
    EpochEndInvalid,
    /// A source sample range overflowed.
    #[error("sample_range_overflow")]
    SampleRangeOverflow,
}

impl PacketError {
    /// Returns the normative stable ASCII reason.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DatagramTooShort => "datagram_too_short",
            Self::DatagramTooLarge => "datagram_too_large",
            Self::BadMagic => "bad_magic",
            Self::UnsupportedVersion => "unsupported_version",
            Self::UnknownKind => "unknown_kind",
            Self::BadHeaderLength => "bad_header_length",
            Self::LengthMismatch => "length_mismatch",
            Self::ReservedNonzero => "reserved_nonzero",
            Self::ZeroIdentity => "zero_identity",
            Self::RoomMismatch => "room_mismatch",
            Self::MemberMismatch => "member_mismatch",
            Self::StaleRunPacket => "stale_run_packet",
            Self::SourceUnknown => "source_unknown",
            Self::SubscriptionDenied => "subscription_denied",
            Self::MediaEpochUnannounced => "media_epoch_unannounced",
            Self::UnitFramesInvalid => "unit_frames_invalid",
            Self::UnsupportedCodec => "unsupported_codec",
            Self::SourceFormatMismatch => "source_format_mismatch",
            Self::UnitPayloadMismatch => "unit_payload_mismatch",
            Self::FragmentCountOutOfRange => "fragment_count_out_of_range",
            Self::FragmentIndexOutOfRange => "fragment_index_out_of_range",
            Self::FragmentLengthMismatch => "fragment_length_mismatch",
            Self::GapRangeInvalid => "gap_range_invalid",
            Self::GapReasonInvalid => "gap_reason_invalid",
            Self::NackMaskEmpty => "nack_mask_empty",
            Self::NackMaskOutOfRange => "nack_mask_out_of_range",
            Self::EpochEndInvalid => "epoch_end_invalid",
            Self::SampleRangeOverflow => "sample_range_overflow",
        }
    }
}

/// Closed failure set for caller-owned wire encoding.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EncodeError {
    /// Caller output storage was too short.
    #[error("output requires {required} bytes, only {available} available")]
    OutputTooSmall {
        /// Exact required bytes.
        required: usize,
        /// Available bytes.
        available: usize,
    },
    /// A supplied record violated strict wire invariants.
    #[error("record is invalid: {0}")]
    InvalidRecord(PacketError),
}

/// Strictly parses one RLB2 datagram and borrows MEDIA payload storage.
pub fn parse_datagram(
    bytes: &[u8],
    context: ParseContext,
) -> Result<ParsedDatagram<'_>, PacketError> {
    if bytes.len() < 12 {
        return Err(PacketError::DatagramTooShort);
    }
    if bytes.len() > MAX_UDP_APPLICATION_BYTES {
        return Err(PacketError::DatagramTooLarge);
    }
    if &bytes[0..4] != MAGIC {
        return Err(PacketError::BadMagic);
    }
    if bytes[4] != VERSION_MAJOR || bytes[5] != VERSION_MINOR {
        return Err(PacketError::UnsupportedVersion);
    }
    let kind = PacketKind::try_from(bytes[6])?;
    if usize::from(bytes[7]) != FIXED_HEADER_BYTES {
        return Err(PacketError::BadHeaderLength);
    }
    let payload_bytes = usize::from(read_u16(bytes, 10));
    let exact_length = FIXED_HEADER_BYTES
        .checked_add(payload_bytes)
        .ok_or(PacketError::LengthMismatch)?;
    if bytes.len() != exact_length {
        return Err(PacketError::LengthMismatch);
    }
    if read_u16(bytes, 8) != 0 || reserved_nonzero(bytes, kind) {
        return Err(PacketError::ReservedNonzero);
    }
    let identity = parse_identity(bytes)?;
    validate_identity(identity, context)?;

    match kind {
        PacketKind::Media => parse_media(bytes, identity, payload_bytes, context),
        PacketKind::Gap => parse_gap(bytes, identity, payload_bytes),
        PacketKind::Nack => parse_nack(bytes, identity, payload_bytes),
        PacketKind::EpochEnd => parse_epoch_end(bytes, identity, payload_bytes),
    }
}

/// Validates a parsed NACK against the exact history fragment count.
pub fn validate_nack_mask(nack: NackRecord, fragment_count: u8) -> Result<(), PacketError> {
    if nack.missing_fragment_mask == 0 {
        return Err(PacketError::NackMaskEmpty);
    }
    if !(1..=MAX_FRAGMENTS).contains(&fragment_count) {
        return Err(PacketError::FragmentCountOutOfRange);
    }
    let allowed = if fragment_count == 32 {
        u32::MAX
    } else {
        (1_u32 << fragment_count) - 1
    };
    if nack.missing_fragment_mask & !allowed != 0 {
        return Err(PacketError::NackMaskOutOfRange);
    }
    Ok(())
}

/// Encodes one exact MEDIA fragment into caller-owned storage.
pub fn encode_media_into(
    fragment: MediaFragment<'_>,
    output: &mut [u8],
) -> Result<usize, EncodeError> {
    validate_media_for_encode(fragment)?;
    let required = FIXED_HEADER_BYTES + fragment.payload.len();
    require_output(output, required)?;
    write_common(
        output,
        PacketKind::Media,
        fragment.payload.len(),
        fragment.header.identity,
    );
    output[92..96].copy_from_slice(&fragment.header.unit_sequence.to_be_bytes());
    output[96..104].copy_from_slice(&fragment.header.start_sample.to_be_bytes());
    output[104..106].copy_from_slice(&MEDIA_UNIT_FRAMES.to_be_bytes());
    output[106] = fragment.header.fragment_index;
    output[107] = fragment.header.fragment_count;
    output[108] = fragment.header.codec.as_u8();
    output[109] = fragment.header.channels;
    output[112..116].copy_from_slice(&fragment.header.unit_payload_bytes.to_be_bytes());
    output[FIXED_HEADER_BYTES..required].copy_from_slice(fragment.payload);
    Ok(required)
}

/// Encodes one exact GAP record into caller-owned storage.
pub fn encode_gap_into(record: GapRecord, output: &mut [u8]) -> Result<usize, EncodeError> {
    validate_gap(record).map_err(EncodeError::InvalidRecord)?;
    require_output(output, FIXED_HEADER_BYTES)?;
    write_common(output, PacketKind::Gap, 0, record.identity);
    output[92..96].copy_from_slice(&record.record_sequence.to_be_bytes());
    output[96..104].copy_from_slice(&record.start_sample.to_be_bytes());
    output[104..108].copy_from_slice(&record.frame_count.to_be_bytes());
    output[108] = record.reason as u8;
    Ok(FIXED_HEADER_BYTES)
}

/// Encodes one exact NACK record into caller-owned storage.
pub fn encode_nack_into(record: NackRecord, output: &mut [u8]) -> Result<usize, EncodeError> {
    if record.missing_fragment_mask == 0 {
        return Err(EncodeError::InvalidRecord(PacketError::NackMaskEmpty));
    }
    require_output(output, FIXED_HEADER_BYTES)?;
    write_common(output, PacketKind::Nack, 0, record.identity);
    output[92..96].copy_from_slice(&record.target_unit_sequence.to_be_bytes());
    output[96..104].copy_from_slice(&record.unit_start_sample.to_be_bytes());
    output[104..108].copy_from_slice(&record.missing_fragment_mask.to_be_bytes());
    output[108..112].copy_from_slice(&record.nack_id.to_be_bytes());
    Ok(FIXED_HEADER_BYTES)
}

/// Encodes one exact EPOCH_END record into caller-owned storage.
pub fn encode_epoch_end_into(
    record: EpochEndRecord,
    output: &mut [u8],
) -> Result<usize, EncodeError> {
    require_output(output, FIXED_HEADER_BYTES)?;
    write_common(output, PacketKind::EpochEnd, 0, record.identity);
    output[92..96].copy_from_slice(&record.next_record_sequence.to_be_bytes());
    output[96..104].copy_from_slice(&record.final_sample.to_be_bytes());
    output[104] = record.reason as u8;
    Ok(FIXED_HEADER_BYTES)
}

fn parse_media(
    bytes: &[u8],
    identity: MediaIdentity,
    payload_bytes: usize,
    context: ParseContext,
) -> Result<ParsedDatagram<'_>, PacketError> {
    if read_u16(bytes, 104) != MEDIA_UNIT_FRAMES {
        return Err(PacketError::UnitFramesInvalid);
    }
    let fragment_index = bytes[106];
    let fragment_count = bytes[107];
    if fragment_count == 0 || fragment_count > MAX_FRAGMENTS {
        return Err(PacketError::FragmentCountOutOfRange);
    }
    if fragment_index >= fragment_count {
        return Err(PacketError::FragmentIndexOutOfRange);
    }
    let codec = WireCodec::try_from(bytes[108]).map_err(|_| PacketError::UnsupportedCodec)?;
    let channels = bytes[109];
    if !matches!(channels, 1 | 2)
        || context
            .expected_channels
            .is_some_and(|expected| expected != channels)
        || context
            .expected_codec
            .is_some_and(|expected| expected != codec)
    {
        return Err(PacketError::SourceFormatMismatch);
    }
    let unit_payload_bytes = read_u32(bytes, 112);
    let expected = MediaUnitHeader::expected_unit_payload_bytes(codec, channels)?;
    if unit_payload_bytes != expected {
        return Err(PacketError::UnitPayloadMismatch);
    }
    if fragment_count != MediaUnitHeader::expected_fragment_count(unit_payload_bytes) {
        return Err(PacketError::FragmentCountOutOfRange);
    }
    let header = MediaUnitHeader {
        identity,
        unit_sequence: read_u32(bytes, 92),
        start_sample: read_u64(bytes, 96),
        fragment_index,
        fragment_count,
        codec,
        channels,
        unit_payload_bytes,
    };
    header
        .start_sample
        .checked_add(u64::from(MEDIA_UNIT_FRAMES))
        .ok_or(PacketError::SampleRangeOverflow)?;
    if payload_bytes != header.expected_fragment_payload_bytes()? {
        return Err(PacketError::FragmentLengthMismatch);
    }
    Ok(ParsedDatagram::Media(MediaFragment {
        header,
        payload: &bytes[FIXED_HEADER_BYTES..],
    }))
}

fn parse_gap(
    bytes: &[u8],
    identity: MediaIdentity,
    payload_bytes: usize,
) -> Result<ParsedDatagram<'_>, PacketError> {
    if payload_bytes != 0 {
        return Err(PacketError::LengthMismatch);
    }
    let record = GapRecord {
        identity,
        record_sequence: read_u32(bytes, 92),
        start_sample: read_u64(bytes, 96),
        frame_count: read_u32(bytes, 104),
        reason: GapReason::try_from(bytes[108])?,
    };
    validate_gap(record)?;
    Ok(ParsedDatagram::Gap(record))
}

fn parse_nack(
    bytes: &[u8],
    identity: MediaIdentity,
    payload_bytes: usize,
) -> Result<ParsedDatagram<'_>, PacketError> {
    if payload_bytes != 0 {
        return Err(PacketError::LengthMismatch);
    }
    let record = NackRecord {
        identity,
        target_unit_sequence: read_u32(bytes, 92),
        unit_start_sample: read_u64(bytes, 96),
        missing_fragment_mask: read_u32(bytes, 104),
        nack_id: read_u32(bytes, 108),
    };
    if record.missing_fragment_mask == 0 {
        return Err(PacketError::NackMaskEmpty);
    }
    Ok(ParsedDatagram::Nack(record))
}

fn parse_epoch_end(
    bytes: &[u8],
    identity: MediaIdentity,
    payload_bytes: usize,
) -> Result<ParsedDatagram<'_>, PacketError> {
    if payload_bytes != 0 {
        return Err(PacketError::LengthMismatch);
    }
    let record = EpochEndRecord {
        identity,
        next_record_sequence: read_u32(bytes, 92),
        final_sample: read_u64(bytes, 96),
        reason: EpochEndReason::try_from(bytes[104])?,
    };
    Ok(ParsedDatagram::EpochEnd(record))
}

fn parse_identity(bytes: &[u8]) -> Result<MediaIdentity, PacketError> {
    Ok(MediaIdentity {
        room_id: RoomId::from_bytes(read_id(bytes, 12)).map_err(|_| PacketError::ZeroIdentity)?,
        sender_member_id: MemberId::from_bytes(read_id(bytes, 28))
            .map_err(|_| PacketError::ZeroIdentity)?,
        sender_run_id: RunId::from_bytes(read_id(bytes, 44))
            .map_err(|_| PacketError::ZeroIdentity)?,
        source_id: SourceId::from_bytes(read_id(bytes, 60))
            .map_err(|_| PacketError::ZeroIdentity)?,
        stream_epoch: StreamEpoch::from_bytes(read_id(bytes, 76))
            .map_err(|_| PacketError::ZeroIdentity)?,
    })
}

fn validate_identity(identity: MediaIdentity, context: ParseContext) -> Result<(), PacketError> {
    if context
        .expected_room_id
        .is_some_and(|expected| expected != identity.room_id)
    {
        return Err(PacketError::RoomMismatch);
    }
    if context
        .expected_sender_member_id
        .is_some_and(|expected| expected != identity.sender_member_id)
    {
        return Err(PacketError::MemberMismatch);
    }
    if context
        .expected_sender_run_id
        .is_some_and(|expected| expected != identity.sender_run_id)
    {
        return Err(PacketError::StaleRunPacket);
    }
    if context
        .expected_source_id
        .is_some_and(|expected| expected != identity.source_id)
    {
        return Err(PacketError::SourceUnknown);
    }
    if context.subscription_allowed == Some(false) {
        return Err(PacketError::SubscriptionDenied);
    }
    if context
        .expected_stream_epoch
        .is_some_and(|expected| expected != identity.stream_epoch)
    {
        return Err(PacketError::MediaEpochUnannounced);
    }
    Ok(())
}

fn validate_media_for_encode(fragment: MediaFragment<'_>) -> Result<(), EncodeError> {
    let header = fragment.header;
    let expected = MediaUnitHeader::expected_unit_payload_bytes(header.codec, header.channels)
        .map_err(EncodeError::InvalidRecord)?;
    if header.unit_payload_bytes != expected {
        return Err(EncodeError::InvalidRecord(PacketError::UnitPayloadMismatch));
    }
    if header.fragment_count == 0
        || header.fragment_count > MAX_FRAGMENTS
        || header.fragment_count != MediaUnitHeader::expected_fragment_count(expected)
    {
        return Err(EncodeError::InvalidRecord(
            PacketError::FragmentCountOutOfRange,
        ));
    }
    if header.fragment_index >= header.fragment_count {
        return Err(EncodeError::InvalidRecord(
            PacketError::FragmentIndexOutOfRange,
        ));
    }
    let payload_bytes = header
        .expected_fragment_payload_bytes()
        .map_err(EncodeError::InvalidRecord)?;
    if fragment.payload.len() != payload_bytes {
        return Err(EncodeError::InvalidRecord(
            PacketError::FragmentLengthMismatch,
        ));
    }
    header
        .start_sample
        .checked_add(u64::from(MEDIA_UNIT_FRAMES))
        .ok_or(EncodeError::InvalidRecord(PacketError::SampleRangeOverflow))?;
    Ok(())
}

fn validate_gap(record: GapRecord) -> Result<(), PacketError> {
    if !(1..=48_000).contains(&record.frame_count) {
        return Err(PacketError::GapRangeInvalid);
    }
    record
        .start_sample
        .checked_add(u64::from(record.frame_count))
        .ok_or(PacketError::SampleRangeOverflow)?;
    Ok(())
}

fn expected_fragment_payload_bytes(
    unit_payload_bytes: u32,
    fragment_index: u8,
    fragment_count: u8,
) -> Result<usize, PacketError> {
    if fragment_count == 0 || fragment_count > MAX_FRAGMENTS {
        return Err(PacketError::FragmentCountOutOfRange);
    }
    if fragment_index >= fragment_count {
        return Err(PacketError::FragmentIndexOutOfRange);
    }
    let offset = u32::from(fragment_index)
        .checked_mul(MAX_FRAGMENT_DATA_BYTES as u32)
        .ok_or(PacketError::UnitPayloadMismatch)?;
    let remaining = unit_payload_bytes
        .checked_sub(offset)
        .ok_or(PacketError::FragmentLengthMismatch)?;
    Ok(remaining.min(MAX_FRAGMENT_DATA_BYTES as u32) as usize)
}

fn require_output(output: &[u8], required: usize) -> Result<(), EncodeError> {
    if output.len() < required {
        Err(EncodeError::OutputTooSmall {
            required,
            available: output.len(),
        })
    } else {
        Ok(())
    }
}

fn write_common(
    output: &mut [u8],
    kind: PacketKind,
    payload_bytes: usize,
    identity: MediaIdentity,
) {
    let required = FIXED_HEADER_BYTES + payload_bytes;
    output[..required].fill(0);
    output[0..4].copy_from_slice(MAGIC);
    output[4] = VERSION_MAJOR;
    output[5] = VERSION_MINOR;
    output[6] = kind as u8;
    output[7] = FIXED_HEADER_BYTES as u8;
    output[10..12].copy_from_slice(&(payload_bytes as u16).to_be_bytes());
    output[12..28].copy_from_slice(identity.room_id.as_bytes());
    output[28..44].copy_from_slice(identity.sender_member_id.as_bytes());
    output[44..60].copy_from_slice(identity.sender_run_id.as_bytes());
    output[60..76].copy_from_slice(identity.source_id.as_bytes());
    output[76..92].copy_from_slice(identity.stream_epoch.as_bytes());
}

fn reserved_nonzero(bytes: &[u8], kind: PacketKind) -> bool {
    match kind {
        PacketKind::Media => any_nonzero(&bytes[110..112]) || any_nonzero(&bytes[116..120]),
        PacketKind::Gap => any_nonzero(&bytes[109..120]),
        PacketKind::Nack => any_nonzero(&bytes[112..120]),
        PacketKind::EpochEnd => any_nonzero(&bytes[105..120]),
    }
}

fn any_nonzero(bytes: &[u8]) -> bool {
    bytes.iter().any(|byte| *byte != 0)
}

fn read_id(bytes: &[u8], offset: usize) -> [u8; 16] {
    bytes[offset..offset + 16]
        .try_into()
        .expect("validated fixed header")
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated fixed header"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated fixed header"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated fixed header"),
    )
}
