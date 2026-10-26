#![forbid(unsafe_code)]

//! Deterministic, device-independent LAN peer foundation.
//!
//! One participant owns a single source publisher for its process run. Each
//! `Peer` is one member pair: it carries that publisher's current catalogue,
//! admits remote catalogues, and wraps already prepared original fragments.
//! The socket owner supplies monotonic milliseconds and a separate receive
//! arrival. Playback, capture and host policy stay outside this crate.

mod media;
mod peer;
mod policy;
mod publisher;
mod relationship;
mod routing;
mod source;
mod wire;

pub use media::MAX_DATAGRAM_BYTES;
pub use peer::{
    AdmissionProjection, AdmissionSnapshot, AdmissionView, AdmittedMetadata, AdmittedOriginal,
    AdmittedPcm, OutgoingInterval, Peer, PeerError, PeerInstance, PeerOperation, PeerStatus,
    SourceAdmission, WrittenDatagram, QueuedControl,
};
pub use policy::{SourceIntent, SourcePolicy};
pub use publisher::{
    BindingRequest, CaptureTicket, CaptureWatermark, LocalOriginal, PreparedBatch,
    PreparedFragment, PreparedOriginalPacket, PreparedStorage, PreparedView, PublicationBinding,
    ReceivedBinding, SourcePublisher, SourceUpdate, TicketedOriginal,
};
pub use relationship::{
    DeliveryRoute, MemberRun, PublicationInput, PublicationRef, SourceRef, UpstreamMapping,
};
pub use routing::{DatagramRoute, datagram_route};
pub use source::{
    max_packet_frames, Catalogue, ChannelLayout, MAX_ORIGINAL_FRAMES, MAX_SOURCES, MEDIA_HEADER_BYTES, PcmEncoding,
    PcmFormat, PublicationEpoch, PublicationState, SOURCE_RECORD_BYTES, SourceEntry, SourceError,
    SourceKind,
};
pub use wire::Intent;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod return_tests;

#[cfg(test)]
mod stream_tests;
