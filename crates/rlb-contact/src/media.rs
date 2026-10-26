//! Media socket addresses carried by the contact service.
//!
//! The shared `rlb_model::Endpoint` value is IPv4-only. Contact members publish
//! an IPv4 or IPv6 socket, so this crate keeps its own address type. Discovery
//! already stores runtime endpoints as `SocketAddr`.

use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::num::NonZeroU16;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// One preferred endpoint and at most fifteen ordered alternatives.
pub const MAX_MEDIA_CANDIDATES: usize = 16;

/// Why a media socket was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MediaError {
    /// The UDP port was zero.
    #[error("a media port must be in 1..=65535")]
    Port,
    /// The address was unspecified, multicast, or IPv4 broadcast.
    #[error("a media address must be an individual unicast address")]
    Address,
    /// An IPv4 address carried an IPv6 scope identifier.
    #[error("an IPv4 media address has no scope identifier")]
    Scope,
    /// An IPv6 link-local destination needs this receiving machine's interface.
    #[error("an IPv6 link-local media address requires a nonzero local interface scope")]
    LocalScopeRequired,
    /// The preferred endpoint plus supplied alternatives exceeded the bound.
    #[error("a member may publish at most 16 media candidates")]
    CandidateCount,
    /// All candidates must describe the same UDP socket port.
    #[error("all media candidates must use the declared UDP port")]
    CandidatePort,
}

/// One member's direct-media UDP socket.
///
/// `port` is the member's UDP port. An accepted TCP source port is never stored here.
/// `scope_id` records the sender's explicit scope or the server's observed scope.
/// Interface indices belong to the originating machine; receiving clients must
/// supply their own scope to [`Self::socket_addr_on`] for link-local addresses.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaSocket {
    ip: IpAddr,
    port: NonZeroU16,
    scope_id: u32,
}

impl MediaSocket {
    /// Creates a socket from an explicit client-owned endpoint.
    pub fn explicit(ip: IpAddr, port: u16, scope_id: u32) -> Result<Self, MediaError> {
        let (ip, scope_id) = normalize(ip, scope_id)?;
        let port = NonZeroU16::new(port).ok_or(MediaError::Port)?;
        Ok(Self { ip, port, scope_id })
    }

    /// Returns the IP address.
    #[must_use]
    pub const fn ip(self) -> IpAddr {
        self.ip
    }

    /// Returns the UDP port.
    #[must_use]
    pub const fn port(self) -> u16 {
        self.port.get()
    }

    /// Returns the sender/server-local scope metadata. IPv4 uses zero.
    ///
    /// This value cannot select an interface on a different receiving machine.
    #[must_use]
    pub const fn scope_id(self) -> u32 {
        self.scope_id
    }

    /// Builds a destination that requires no local interface selection.
    ///
    /// IPv6 link-local addresses return [`MediaError::LocalScopeRequired`]. Use
    /// [`Self::socket_addr_on`] with the receiving machine's interface index.
    pub fn socket_addr(self) -> Result<SocketAddr, MediaError> {
        self.socket_addr_on(0)
    }

    /// Builds a destination using this receiving machine's interface scope.
    ///
    /// IPv6 link-local addresses require a nonzero `local_scope`; stored
    /// sender/server scope metadata is never used for local interface selection.
    /// IPv4 and other IPv6 destinations use scope zero.
    pub fn socket_addr_on(self, local_scope: u32) -> Result<SocketAddr, MediaError> {
        match self.ip {
            IpAddr::V4(ip) => Ok(SocketAddr::V4(SocketAddrV4::new(ip, self.port.get()))),
            IpAddr::V6(ip) => {
                let scope = if (ip.segments()[0] & 0xffc0) == 0xfe80 {
                    if local_scope == 0 {
                        return Err(MediaError::LocalScopeRequired);
                    }
                    local_scope
                } else {
                    0
                };
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    self.port.get(),
                    0,
                    scope,
                )))
            }
        }
    }
}

/// Validates one bounded alternate list and removes repeats without changing
/// preference order. `primary` is absent until an observed TCP IP is resolved.
/// The declared primary always reserves one of the sixteen candidate slots.
pub fn normalize_media_alternates(
    media_port: u16,
    primary: Option<MediaSocket>,
    alternates: &[MediaSocket],
) -> Result<Vec<MediaSocket>, MediaError> {
    if media_port == 0 {
        return Err(MediaError::Port);
    }
    if alternates.len() >= MAX_MEDIA_CANDIDATES {
        return Err(MediaError::CandidateCount);
    }
    if primary.is_some_and(|candidate| candidate.port() != media_port) {
        return Err(MediaError::CandidatePort);
    }
    let mut unique = Vec::with_capacity(alternates.len());
    for &candidate in alternates {
        if candidate.port() != media_port {
            return Err(MediaError::CandidatePort);
        }
        if primary == Some(candidate) || unique.contains(&candidate) {
            continue;
        }
        unique.push(candidate);
    }
    Ok(unique)
}

/// Resolves the media socket for one accepted member.
///
/// `remote` supplies the peer IP and, for IPv6, the scope. Its port is the
/// ephemeral TCP source port and is discarded. `media_port` is the UDP port
/// declared by the member. `explicit_ip` replaces the observed IP when the
/// member has a pinned local bind. The resulting scope remains sender/server
/// metadata; it must be replaced with the receiving machine's own interface
/// index when constructing an IPv6 link-local destination.
pub fn resolve_media(
    remote: SocketAddr,
    media_port: u16,
    explicit_ip: Option<IpAddr>,
    explicit_scope: u32,
) -> Result<MediaSocket, MediaError> {
    if let Some(ip) = explicit_ip {
        return MediaSocket::explicit(ip, media_port, explicit_scope);
    }
    if explicit_scope != 0 {
        return Err(MediaError::Scope);
    }
    if remote.port() == 0 {
        return Err(MediaError::Port);
    }
    let (ip, scope_id) = match remote {
        SocketAddr::V4(address) => (IpAddr::V4(*address.ip()), 0),
        SocketAddr::V6(address) => (IpAddr::V6(*address.ip()), address.scope_id()),
    };
    MediaSocket::explicit(ip, media_port, scope_id)
}

fn normalize(ip: IpAddr, scope_id: u32) -> Result<(IpAddr, u32), MediaError> {
    let (ip, scope_id) = match ip {
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                if scope_id != 0 {
                    return Err(MediaError::Scope);
                }
                (IpAddr::V4(mapped), 0)
            } else {
                (IpAddr::V6(address), scope_id)
            }
        }
        IpAddr::V4(address) => (IpAddr::V4(address), scope_id),
    };
    if !usable_unicast(ip) {
        return Err(MediaError::Address);
    }
    if matches!(ip, IpAddr::V4(_)) && scope_id != 0 {
        return Err(MediaError::Scope);
    }
    Ok((ip, scope_id))
}

fn usable_unicast(ip: IpAddr) -> bool {
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(address) => !address.is_broadcast(),
        IpAddr::V6(_) => true,
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MediaWire {
    ip: IpAddr,
    port: u16,
    scope_id: u32,
}

impl Serialize for MediaSocket {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        MediaWire {
            ip: self.ip,
            port: self.port.get(),
            scope_id: self.scope_id,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MediaSocket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MediaWire::deserialize(deserializer)?;
        Self::explicit(wire.ip, wire.port, wire.scope_id).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn observed_tcp_port_is_replaced_by_the_declared_udp_port() {
        let remote = SocketAddr::from((Ipv4Addr::LOCALHOST, 55_555));
        let media = resolve_media(remote, 9_001, None, 0).unwrap();
        assert_eq!(media.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(media.port(), 9_001);
        assert_ne!(media.port(), remote.port());
        assert_eq!(media.scope_id(), 0);
    }

    #[test]
    fn explicit_address_replaces_the_observed_peer() {
        let remote = SocketAddr::from((Ipv4Addr::LOCALHOST, 40_000));
        let pinned = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 23));
        let media = resolve_media(remote, 5_004, Some(pinned), 0).unwrap();
        assert_eq!(media.ip(), pinned);
        assert_eq!(media.port(), 5_004);
    }

    #[test]
    fn ipv4_mapped_peers_are_stored_as_ipv4() {
        let mapped = Ipv4Addr::new(203, 0, 113, 10).to_ipv6_mapped();
        let remote = SocketAddr::V6(SocketAddrV6::new(mapped, 44_444, 0, 0));
        let media = resolve_media(remote, 7_001, None, 0).unwrap();
        assert_eq!(media.ip(), IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)));
        assert_eq!(media.port(), 7_001);
        assert_eq!(media.socket_addr().unwrap().port(), 7_001);
    }

    #[test]
    fn unusable_addresses_and_ipv4_scopes_are_rejected() {
        assert!(resolve_media(SocketAddr::from((Ipv4Addr::LOCALHOST, 1)), 0, None, 0).is_err());
        assert!(MediaSocket::explicit(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1, 0).is_err());
        assert!(MediaSocket::explicit(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 0).is_err());
        assert!(MediaSocket::explicit(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)), 1, 0).is_err());
        assert!(MediaSocket::explicit(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 1, 0).is_err());
        assert!(MediaSocket::explicit(IpAddr::V4(Ipv4Addr::LOCALHOST), 9, 4).is_err());
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let scoped = MediaSocket::explicit(IpAddr::V6(link_local), 9, 12).unwrap();
        assert_eq!(scoped.scope_id(), 12);
        assert_eq!(scoped.socket_addr(), Err(MediaError::LocalScopeRequired));
        assert_eq!(
            scoped.socket_addr_on(0),
            Err(MediaError::LocalScopeRequired)
        );
    }

    #[test]
    fn link_local_destinations_use_each_receivers_local_scope() {
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let remote = SocketAddr::V6(SocketAddrV6::new(link_local, 44_444, 0, 12));
        let published = resolve_media(remote, 9_001, None, 0).unwrap();
        let bytes = serde_json::to_vec(&published).unwrap();
        let received: MediaSocket = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(received.scope_id(), 12);
        assert_eq!(received.socket_addr(), Err(MediaError::LocalScopeRequired));
        let first = received.socket_addr_on(3).unwrap();
        let second = received.socket_addr_on(7).unwrap();
        assert_eq!(
            first,
            SocketAddr::V6(SocketAddrV6::new(link_local, 9_001, 0, 3))
        );
        assert_eq!(
            second,
            SocketAddr::V6(SocketAddrV6::new(link_local, 9_001, 0, 7))
        );
        assert_ne!(first, second);
    }

    #[test]
    fn ipv4_and_non_link_local_ipv6_need_no_scope_translation() {
        let ipv4 = MediaSocket::explicit(IpAddr::V4(Ipv4Addr::LOCALHOST), 9, 0).unwrap();
        assert_eq!(
            ipv4.socket_addr().unwrap(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 9))
        );
        assert_eq!(ipv4.socket_addr_on(7), ipv4.socket_addr());
        for address in [
            Ipv6Addr::LOCALHOST,
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
        ] {
            let media = MediaSocket::explicit(IpAddr::V6(address), 9, 12).unwrap();
            let expected = SocketAddr::V6(SocketAddrV6::new(address, 9, 0, 0));
            assert_eq!(media.socket_addr().unwrap(), expected);
            assert_eq!(media.socket_addr_on(7).unwrap(), expected);
        }
    }
}
