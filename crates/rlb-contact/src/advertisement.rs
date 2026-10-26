//! Advertisement contract for `_rlbroom._tcp.local.`.
//!
//! The text record carries the protocol generation, the original room caption,
//! and the run-lifetime instance marker. The service port is the bound contact
//! port. Passwords, rules, and saved addresses are absent.

use std::fmt;
use std::net::IpAddr;

use mdns_sd::{IfKind, IfPredicate, ServiceInfo};

use rlb_model::RoomId;

use crate::wire::validate_room_name;

/// DNS-SD service type for a running room contact service.
pub const SERVICE_TYPE: &str = "_rlbroom._tcp.local.";

/// Advertised contact-introduction generation.
///
/// The contact service carries the same generation. The current value is `2`.
pub const PROTOCOL_VERSION: &str = "2";

/// Text-record key for [`PROTOCOL_VERSION`].
pub const TXT_VERSION: &str = "ver";

/// Text-record key for the original room caption.
pub const TXT_NAME: &str = "name";

/// Text-record key for the run-lifetime [`RoomId`], lowercase hexadecimal.
pub const TXT_INSTANCE: &str = "id";

/// Optional actual listener IP. A shared mDNS host can have unrelated A/AAAA
/// addresses; this hint restricts runtime candidates, never saved room rules.
pub const TXT_ADDRESS: &str = "addr";

/// Longest room caption the text record can carry, in bytes.
///
/// A DNS-SD text string is at most 255 bytes, including the key and the
/// separating `=`.
pub(crate) const NAME_TEXT_LIMIT: usize = 250;

/// One running contact service, supplied by its owner.
///
/// `port` is the port the service actually bound. This value is independent
/// of any saved card.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct AdvertisedRoom {
    pub instance_id: RoomId,
    pub display_name: String,
    pub port: u16,
}

/// Optional pass-through to the mDNS library's interface hooks.
///
/// An empty list leaves the library defaults in place. This selector does not
/// enumerate adapters or change system configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryConfig {
    pub enable_interfaces: Vec<InterfaceSelector>,
    pub disable_interfaces: Vec<InterfaceSelector>,
}

/// One interface choice accepted by the mDNS library.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterfaceSelector {
    Ipv4,
    Ipv6,
    Name(String),
    Address(IpAddr),
    IndexV4(u32),
    IndexV6(u32),
}

/// Failure while preparing or operating discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryError {
    InvalidAdvertisement(String),
    InvalidConfiguration(String),
    Service(String),
    Stopped,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAdvertisement(message)
            | Self::InvalidConfiguration(message)
            | Self::Service(message) => formatter.write_str(message),
            Self::Stopped => formatter.write_str("Room discovery is stopped."),
        }
    }
}

impl std::error::Error for DiscoveryError {}

pub fn validate_config(config: &DiscoveryConfig) -> Result<(), DiscoveryError> {
    for selector in config
        .enable_interfaces
        .iter()
        .chain(config.disable_interfaces.iter())
    {
        if let InterfaceSelector::Name(name) = selector {
            if name.is_empty() || name.chars().any(char::is_control) {
                return Err(DiscoveryError::InvalidConfiguration(
                    "An interface name must contain at least one character and no control characters."
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

pub fn validate_advertised(room: &AdvertisedRoom) -> Result<(), DiscoveryError> {
    if room.port == 0 {
        return Err(DiscoveryError::InvalidAdvertisement(
            "The bound service port must be in the range 1 through 65535.".into(),
        ));
    }
    validate_room_name(&room.display_name)
        .map_err(|message| DiscoveryError::InvalidAdvertisement(message.to_owned()))?;
    if room.display_name.len() > NAME_TEXT_LIMIT {
        return Err(DiscoveryError::InvalidAdvertisement(format!(
            "The room name is {} bytes. Discovery text can carry {NAME_TEXT_LIMIT} bytes of the name.",
            room.display_name.len()
        )));
    }
    Ok(())
}

pub fn display_name_ok(name: &str) -> bool {
    validate_advertised(&AdvertisedRoom {
        instance_id: RoomId::from_bytes([1; 16]).expect("nonzero instance marker"),
        display_name: name.to_owned(),
        port: 1,
    })
    .is_ok()
}


pub fn to_if_kind(selector: &InterfaceSelector) -> IfKind {
    match selector {
        InterfaceSelector::Ipv4 => IfKind::IPv4,
        InterfaceSelector::Ipv6 => IfKind::IPv6,
        InterfaceSelector::Name(name) => IfKind::Name(name.clone()),
        InterfaceSelector::Address(address) => IfKind::Addr(*address),
        InterfaceSelector::IndexV4(index) => IfKind::IndexV4(*index),
        InterfaceSelector::IndexV6(index) => IfKind::IndexV6(*index),
    }
}

pub fn build_service_info(
    room: &AdvertisedRoom,
    enable_interfaces: &[InterfaceSelector],
) -> Result<ServiceInfo, DiscoveryError> {
    build_service_info_bound(room, enable_interfaces, None)
}

pub fn build_service_info_bound(
    room: &AdvertisedRoom,
    enable_interfaces: &[InterfaceSelector],
    bound_ip: Option<IpAddr>,
) -> Result<ServiceInfo, DiscoveryError> {
    build_service_info_bound_all(
        room,
        enable_interfaces,
        &bound_ip.into_iter().collect::<Vec<_>>(),
    )
}

pub fn build_service_info_bound_all(
    room: &AdvertisedRoom,
    enable_interfaces: &[InterfaceSelector],
    bound_ips: &[IpAddr],
) -> Result<ServiceInfo, DiscoveryError> {
    validate_advertised(room)?;
    let instance = room.instance_id.to_string();
    let host = format!("rlb-{instance}.local.");
    let manual: Vec<IpAddr> = bound_ips
        .iter()
        .copied()
        .filter(|ip| !ip.is_unspecified())
        .collect();
    let wildcard4 = bound_ips.is_empty()
        || bound_ips
            .iter()
            .any(|ip| ip.is_ipv4() && ip.is_unspecified());
    let wildcard6 = bound_ips.is_empty()
        || bound_ips
            .iter()
            .any(|ip| ip.is_ipv6() && ip.is_unspecified());
    let address = manual
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut properties = vec![
        (TXT_VERSION, PROTOCOL_VERSION),
        (TXT_NAME, room.display_name.as_str()),
        (TXT_INSTANCE, instance.as_str()),
    ];
    if manual.len() == 1 && !wildcard4 && !wildcard6 {
        properties.push((TXT_ADDRESS, address.as_str()));
    }
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host,
        address.as_str(),
        room.port,
        &properties[..],
    )
    .map_err(|error| DiscoveryError::Service(error.to_string()))?;
    if wildcard4 || wildcard6 {
        info = info.enable_addr_auto();
    }
    let selectors = enable_interfaces.to_vec();
    // One predicate expresses the intersection. Appending positive IfKinds
    // would otherwise OR the family restriction with a hard interface choice.
    info.set_interfaces(vec![IfKind::Predicate(IfPredicate::new(
        move |interface| {
            let ip = interface.ip();
            let bound =
                (ip.is_ipv4() && wildcard4) || (ip.is_ipv6() && wildcard6) || manual.contains(&ip);
            let enabled = selectors.is_empty()
                || selectors.iter().any(|selector| match selector {
                    InterfaceSelector::Ipv4 => ip.is_ipv4(),
                    InterfaceSelector::Ipv6 => ip.is_ipv6(),
                    InterfaceSelector::Name(name) => *name == interface.name,
                    InterfaceSelector::Address(address) => *address == ip,
                    InterfaceSelector::IndexV4(index) => {
                        ip.is_ipv4() && interface.index == Some(*index)
                    }
                    InterfaceSelector::IndexV6(index) => {
                        ip.is_ipv6() && interface.index == Some(*index)
                    }
                });
            bound && enabled
        },
    ))]);
    Ok(info)
}

