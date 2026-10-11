#![forbid(unsafe_code)]

//! Deterministic, device-independent LAN peer foundation.
//!
//! The socket owner supplies monotonically increasing milliseconds and routes
//! datagrams only from the configured peer endpoint. Each instance owns one
//! directed-permission pair. Received audio has no automatic publication path.
//! Phase 1 permits one local PCM format and forward-only source timeline per
//! process run; device/source epoch switching belongs to a subsequent slice.

mod media;
mod peer;
mod wire;

pub use media::{MAX_DATAGRAM_BYTES, OriginalAudio};
pub use peer::{Peer, PeerError, PeerStatus};
pub use wire::Intent;

#[cfg(test)]
mod tests;
