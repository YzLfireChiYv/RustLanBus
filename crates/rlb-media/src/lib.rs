#![forbid(unsafe_code)]

//! RLB2 media datagrams, RLBD direct control, bounded assembly, and sender
//! history for the V2 first executable media slice.

mod assembly;
mod direct;
mod sender;
mod wire;

pub use assembly::{
    AssembledUnit, AssemblerMetrics, AssemblyError, AssemblyOutcome, FragmentAssembler,
    NackCandidate, NackPlannerConfig,
};
pub use direct::{
    AckStatus, BoundedRejections, BoundedSourceIds, DirectCommon, DirectDatagram, DirectError,
    DirectHello, DirectKind, Rejection, RejectionCode, SubscribeAck, SubscribeSet,
    encode_direct_into, parse_direct,
};
pub use sender::{
    DatagramFanout, DatagramSet, FanoutDatagram, HistoryKey, HistoryMetrics, MediaPacketizer,
    OutboundRecord, PacketizeError, RecordSequence, ResendDecision, ResendHistory, SenderMetrics,
};
pub use wire::{
    EncodeError, EpochEndReason, EpochEndRecord, GapReason, GapRecord, MediaFragment,
    MediaIdentity, MediaUnitHeader, NackRecord, PacketError, PacketKind, ParseContext,
    ParsedDatagram, encode_epoch_end_into, encode_gap_into, encode_media_into, encode_nack_into,
    parse_datagram, validate_nack_mask,
};

/// Maximum UDP application payload for every RLB2 media datagram.
pub const MAX_UDP_APPLICATION_BYTES: usize = 1_200;
/// Fixed RLB2 header size.
pub const FIXED_HEADER_BYTES: usize = 120;
/// Maximum deterministic MEDIA fragment payload.
pub const MAX_FRAGMENT_DATA_BYTES: usize = 1_080;
/// Maximum fragments in one first-release MEDIA unit.
pub const MAX_FRAGMENTS: u8 = 32;
/// Fixed first-release sender deadline after unit readiness.
pub const SENDER_DEADLINE_NS: u64 = 10_000_000;
/// Receiver decode/publication margin used by the NACK budget.
pub const NACK_PUBLICATION_MARGIN_NS: u64 = 2_000_000;

#[cfg(test)]
mod tests;
