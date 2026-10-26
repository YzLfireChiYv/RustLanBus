#![forbid(unsafe_code)]

//! Room contact service.
//!
//! One process serves one room. Members learn each other's names and direct-media
//! UDP sockets here, and the optional room password rejects an accidental join.
//! PCM, capture, playback and permission matrices stay outside this crate.
//! Optional `mdns` supplies the common room-advertisement contract.
//!
//! # Bytes
//!
//! Each direction starts with the eight bytes `RLBCT1`, carriage return, line feed
//! ([`CONTACT_PREFACE`]). Every following frame is a little-endian `u32` length and
//! a JSON object of that many bytes. The length is from 2 to 262144 inclusive.
//! JSON version2 carries one preferred media address plus at most15 alternatives
//! on the same UDP port. RLC5/RLP5 audio is separate and unchanged. The RLBCT1
//! framing marker remains stable so older JSON versions receive an explicit
//! unsupported-version result. JSON uses the shared strict object parser.
//!
//! # Addresses
//!
//! [`ContactConfig`] binds the socket the caller supplies. Port 0 selects an
//! ephemeral port. [`ContactService::local_addr`] and [`RoomInfo::port`] report
//! the bound socket. A failed bind returns that requested address with the
//! operating-system error.
//!
//! The published media port is the member's declared UDP port. The accepted TCP
//! source port is discarded. An omitted `media_ip` uses the peer IP on that TCP
//! connection; an IPv4-mapped IPv6 peer is stored as IPv4, and an IPv6 scope is
//! kept as sender/server-local metadata. Receiving clients supply their own
//! interface scope through [`MediaSocket::socket_addr_on`] for IPv6 link-local
//! destinations. A present `media_ip` is the member's pinned media address. The
//! service binds the one requested socket and leaves route selection to discovery.
//!
//! # Departure and visibility
//!
//! [`ContactEvent::explicit_departure`] names a member who sent `leave`.
//! [`ContactEvent::contact_visibility`] and [`snapshot_gaps`] describe directory
//! visibility: control disconnect, timeout, service stop, or absence from a later
//! snapshot. Healthy direct media continues across those notices. Stopping the
//! service closes its control sockets and records service-stop visibility.
//!
//! # Integration
//!
//! Direct-address probe: [`ContactClient::connect`] then [`ContactClient::inspect`].
//! Membership begins at [`ContactClient::join`]. Discovery can publish
//! `room_name`, `instance_id` ([`rlb_model::RoomId`]), and `port`. A new service
//! start generates a new instance identity for advertisement deduplication.
//! The participant adapter joins with [`JoinParams`],
//! refreshes with [`ContactClient::refresh`], and treats only
//! [`ContactEvent::explicit_departure`] as a member leaving direct media.
//! [`ContactError::is_local_control_failure`] covers loss of this control session.
//! Join and refresh return [`OrderedContactReply`]; apply its `before` events
//! in order before applying its directory `value`.

use std::time::Duration;

mod client;
#[cfg(feature = "mdns")]
pub mod advertisement;
mod directory;
mod error;
mod event;
mod framing;
mod media;
mod server;
mod wire;

pub use client::ContactClient;
pub use error::ContactError;
pub use event::{
    ContactEvent, ContactVisibility, DirectorySnapshot, JoinedRoom, OrderedContactReply,
    SnapshotGap, snapshot_gaps,
};
pub use framing::{
    CONTACT_JSON_LIMITS, CONTACT_PREFACE, DecoderPhase, FrameDecoder, FrameError, MAX_FRAME_BYTES,
    MIN_FRAME_BYTES, decode_json_frame, encode_json_frame, encode_payload,
};
pub use media::{
    MAX_MEDIA_CANDIDATES, MediaError, MediaSocket, normalize_media_alternates, resolve_media,
};
pub use server::{ContactConfig, ContactService, ReturnRequest, ReturnRequests};
pub use wire::{
    CONTACT_PROTOCOL_VERSION, ClientMessage, ContactCode, ContactLossCause, ContactVersion,
    JoinParams, MAX_NAME_CHARS, MAX_PASSWORD_CHARS, MemberContact, PresenceChange, ReturnAction,
    ReturnMapping, ReturnState, RoomInfo, ServerMessage, ServiceStopEffect, decode_client,
    decode_server, encode_client, encode_server,
};

/// TCP port used when an operator selects the ordinary contact port.
pub const DEFAULT_CONTACT_PORT: u16 = 47101;
/// Maximum simultaneous member registrations in one room.
pub const MAX_MEMBERS: usize = 64;
/// Maximum simultaneous control TCP connections, including replacements.
pub const MAX_CONTROL_CONNECTIONS: usize = 128;
/// Interval at which a joined client should send keepalive.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
/// Silence after which the service hides a member.
pub const DEFAULT_CONTACT_TIMEOUT: Duration = Duration::from_secs(8);
/// Default wait for a connect or a single request.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(2);
