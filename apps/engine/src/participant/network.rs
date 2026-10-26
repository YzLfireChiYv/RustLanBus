//! Application-owned network selection. No system routes or adapter settings
//! are changed. A hard choice never falls through to another interface.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

use super::discovery::{DiscoveryConfig, InterfaceSelector};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkPolicy {
    #[default]
    Auto,
    Interface {
        name: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NetworkInterface {
    /// Windows adapter GUID; interface name on other systems.
    pub name: String,
    pub display_name: String,
    pub index: Option<u32>,
    pub ipv4_index: Option<u32>,
    pub ipv6_index: Option<u32>,
    pub up: bool,
    pub loopback: bool,
    pub addresses: Vec<IpAddr>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NetworkBinding {
    pub bind: SocketAddr,
    pub interface: Option<String>,
    pub interface_index: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct NetworkState {
    pub policy: NetworkPolicy,
    pub interfaces: Vec<NetworkInterface>,
    pub selection: Option<NetworkBinding>,
    pub selections: Vec<NetworkBinding>,
    pub error: Option<String>,
}

/// Cold native enumeration. Callers keep it outside the media reactor.
pub fn catalogue() -> Result<Vec<NetworkInterface>, String> {
    let entries = if_addrs::get_if_addrs()
        .map_err(|error| format!("Network interfaces could not be enumerated: {error}"))?;
    let mut result: BTreeMap<String, NetworkInterface> = BTreeMap::new();
    for entry in entries {
        #[cfg(windows)]
        let name = if entry.adapter_name.is_empty() {
            entry.name.clone()
        } else {
            entry.adapter_name.clone()
        };
        #[cfg(not(windows))]
        let name = entry.name.clone();
        let address = entry.ip();
        let interface = result
            .entry(name.clone())
            .or_insert_with(|| NetworkInterface {
                name,
                display_name: entry.name.clone(),
                index: entry.index,
                ipv4_index: None,
                ipv6_index: None,
                up: false,
                loopback: true,
                addresses: Vec::new(),
            });
        interface.up |= entry.is_oper_up();
        interface.loopback &= entry.is_loopback();
        if address.is_ipv4() {
            interface.ipv4_index = entry.index;
        } else {
            interface.ipv6_index = entry.index;
        }
        if !interface.addresses.contains(&address) {
            interface.addresses.push(address);
        }
    }
    let mut interfaces: Vec<_> = result.into_values().collect();
    for interface in &mut interfaces {
        interface.index = interface.ipv4_index.or(interface.ipv6_index);
        interface.addresses.sort();
    }
    interfaces.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then(a.name.cmp(&b.name))
    });
    Ok(interfaces)
}

impl NetworkPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Interface { name } = self {
            if name.trim().is_empty()
                || name.chars().count() > 512
                || name.chars().any(char::is_control)
            {
                return Err("A network interface choice needs a nonempty interface identifier without control characters.".into());
            }
        }
        Ok(())
    }

    pub fn resolve_bind(&self, requested: SocketAddr) -> Result<NetworkBinding, String> {
        if matches!(self, Self::Auto) {
            return self.resolve_from(requested, &[]);
        }
        self.resolve_from(requested, &catalogue()?)
    }

    pub(crate) fn resolve_from(
        &self,
        requested: SocketAddr,
        interfaces: &[NetworkInterface],
    ) -> Result<NetworkBinding, String> {
        self.validate()?;
        let Self::Interface { name } = self else {
            return Ok(NetworkBinding {
                bind: requested,
                interface: None,
                interface_index: None,
            });
        };
        let mut candidates = interfaces.iter().filter(|interface| {
            interface.name.eq_ignore_ascii_case(name) || interface.display_name == *name
        });
        let interface = candidates
            .next()
            .ok_or_else(|| format!("Waiting for the selected network interface {name}."))?;
        if candidates.next().is_some() {
            return Err(
                "The network interface name is ambiguous; choose its stable identifier.".into(),
            );
        }
        if !interface.up {
            return Err(format!(
                "Waiting for the selected network interface {} to come up.",
                interface.display_name
            ));
        }
        let index = if requested.is_ipv4() {
            interface.ipv4_index
        } else {
            interface.ipv6_index
        }
        .filter(|index| *index != 0)
        .ok_or_else(|| {
            format!(
                "The selected network interface {} has no usable index for this address family.",
                interface.display_name
            )
        })?;
        let mut addresses: Vec<_> = interface
            .addresses
            .iter()
            .copied()
            .filter(|address| {
                address.is_ipv4() == requested.is_ipv4()
                    && !address.is_unspecified()
                    && !address.is_multicast()
                    && (requested.ip().is_unspecified() || *address == requested.ip())
            })
            .collect();
        addresses.sort_by_key(|address| (link_local(*address), *address));
        let address = addresses.first().copied().ok_or_else(|| {
            format!(
                "Waiting for an address on the selected network interface {} matching {requested}.",
                interface.display_name
            )
        })?;
        let mut bind = SocketAddr::new(address, requested.port());
        if let SocketAddr::V6(value) = &mut bind {
            value.set_scope_id(if link_local(address) { index } else { 0 });
        }
        Ok(NetworkBinding {
            bind,
            interface: Some(interface.name.clone()),
            interface_index: Some(index),
        })
    }

    /// Bind real local source addresses so a proven peer path has one
    /// unambiguous local source. A wildcard permits both families, cap16.
    pub(crate) fn resolve_all_from(
        &self,
        requested: SocketAddr,
        interfaces: &[NetworkInterface],
    ) -> Result<Vec<NetworkBinding>, String> {
        self.validate()?;
        if let Self::Interface { name } = self {
            let count = interfaces
                .iter()
                .filter(|interface| self.allows(interface))
                .count();
            if count == 0 {
                return Err(format!("Waiting for selected network interface {name}."));
            }
            if count > 1 {
                return Err(
                    "The interface name is ambiguous; choose its stable identifier.".into(),
                );
            }
        }
        if !requested.ip().is_unspecified() {
            let mut binding = self.resolve_from(requested, interfaces)?;
            if binding.interface_index.is_none() {
                binding.interface_index = interfaces
                    .iter()
                    .find(|interface| interface.up && interface.addresses.contains(&requested.ip()))
                    .and_then(|interface| {
                        if requested.is_ipv4() {
                            interface.ipv4_index
                        } else {
                            interface.ipv6_index
                        }
                    });
            }
            if let SocketAddr::V6(address) = &mut binding.bind {
                if address.ip().is_unicast_link_local() && address.scope_id() == 0 {
                    address.set_scope_id(binding.interface_index.unwrap_or(0));
                }
            }
            return Ok(vec![binding]);
        }
        let eligible: Vec<_> = interfaces
            .iter()
            .filter(|interface| {
                interface.up
                    && self.allows(interface)
                    && (!matches!(self, Self::Auto) || !interface.loopback)
            })
            .collect();
        let mut per_interface = Vec::new();
        for interface in eligible {
            let mut addresses = interface.addresses.clone();
            addresses.retain(|ip| !ip.is_unspecified() && !ip.is_multicast());
            addresses.sort_by_key(|ip| (ip.is_loopback(), ip.is_ipv6(), link_local(*ip), *ip));
            // Start each NIC with one address of each family. Privacy aliases
            // on one interface cannot crowd all other networks out of the set.
            let mut ordered = Vec::new();
            for family in [true, false] {
                if let Some(ip) = addresses.iter().find(|ip| ip.is_ipv4() == family) {
                    ordered.push(*ip);
                }
            }
            for ip in addresses {
                if !ordered.contains(&ip) {
                    ordered.push(ip);
                }
            }
            let mut bindings = Vec::new();
            for ip in ordered {
                let index = if ip.is_ipv4() {
                    interface.ipv4_index
                } else {
                    interface.ipv6_index
                };
                if matches!(self, Self::Interface { .. }) && index.is_none() {
                    continue;
                }
                let mut bind = SocketAddr::new(ip, requested.port());
                if let SocketAddr::V6(address) = &mut bind {
                    if address.ip().is_unicast_link_local() {
                        address.set_scope_id(index.unwrap_or(0));
                    }
                }
                bindings.push(NetworkBinding {
                    bind,
                    interface: if matches!(self, Self::Interface { .. }) {
                        Some(interface.name.clone())
                    } else {
                        None
                    },
                    interface_index: index,
                });
            }
            per_interface.push(bindings);
        }
        let mut result = Vec::new();
        let cap = if matches!(self, Self::Auto) { 14 } else { 16 };
        for at in 0..16 {
            for choices in &per_interface {
                if let Some(binding) = choices.get(at) {
                    if result.len() < cap
                        && !result
                            .iter()
                            .any(|existing: &NetworkBinding| existing.bind == binding.bind)
                    {
                        result.push(binding.clone());
                    }
                }
            }
        }
        if matches!(self, Self::Auto) {
            for ip in [
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ] {
                result.push(NetworkBinding {
                    bind: SocketAddr::new(ip, requested.port()),
                    interface: None,
                    interface_index: None,
                });
            }
        }
        if result.is_empty() {
            Err("Waiting for real addresses on the selected network interface.".into())
        } else {
            Ok(result)
        }
    }

    /// mdns-sd applies these selectors in order: disabled families first,
    /// then the exact currently allowed family/index choices.
    pub fn for_discovery(&self, media_bind: SocketAddr) -> Result<DiscoveryConfig, String> {
        if media_bind.ip().is_loopback() {
            return Ok(DiscoveryConfig {
                enable_interfaces: Vec::new(),
                disable_interfaces: vec![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6],
            });
        }
        if matches!(self, Self::Auto) {
            if let SocketAddr::V6(address) = media_bind {
                if address.ip().is_unicast_link_local() && address.scope_id() != 0 {
                    return Ok(DiscoveryConfig {
                        enable_interfaces: vec![InterfaceSelector::IndexV6(address.scope_id())],
                        disable_interfaces: vec![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6],
                    });
                }
            }
            return if media_bind.ip().is_unspecified() {
                Ok(DiscoveryConfig::default())
            } else {
                Ok(DiscoveryConfig {
                    enable_interfaces: vec![InterfaceSelector::Address(media_bind.ip())],
                    disable_interfaces: vec![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6],
                })
            };
        }
        let interfaces = catalogue()?;
        let choices = self.resolve_all_from(media_bind, &interfaces)?;
        let enable_interfaces = choices
            .iter()
            .filter_map(|choice| {
                choice.interface_index.map(|index| {
                    if choice.bind.is_ipv4() {
                        InterfaceSelector::IndexV4(index)
                    } else {
                        InterfaceSelector::IndexV6(index)
                    }
                })
            })
            .collect();
        Ok(DiscoveryConfig {
            enable_interfaces,
            disable_interfaces: vec![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6],
        })
    }

    pub(crate) fn local_v6_indices(&self, interfaces: &[NetworkInterface]) -> Vec<u32> {
        let mut result = Vec::new();
        for interface in interfaces
            .iter()
            .filter(|interface| interface.up && !interface.loopback && self.allows(interface))
        {
            if let Some(index) = interface.ipv6_index.filter(|index| *index != 0) {
                if !result.contains(&index) {
                    result.push(index);
                }
            }
        }
        result
    }

    fn allows(&self, interface: &NetworkInterface) -> bool {
        match self {
            Self::Auto => true,
            Self::Interface { name } => {
                interface.name.eq_ignore_ascii_case(name) || interface.display_name == *name
            }
        }
    }

    /// This performs the actual control connection, never an auxiliary probe.
    /// A hand-entered unscoped link-local address tries only permitted local
    /// IPv6 scopes. All attempts share the caller's total timeout budget.
    pub fn connect_tcp(&self, remote: SocketAddr, timeout: Duration) -> Result<TcpStream, String> {
        if timeout.is_zero() {
            return Err("The contact timeout must be positive.".into());
        }
        self.validate()?;
        let interfaces = if matches!(self, Self::Interface { .. })
            || matches!(remote,SocketAddr::V6(ip) if ip.ip().is_unicast_link_local() && ip.scope_id()==0)
        {
            catalogue()?
        } else {
            Vec::new()
        };
        let mut targets = vec![remote];
        if let SocketAddr::V6(address) = remote {
            if address.ip().is_unicast_link_local() {
                let indices = self.local_v6_indices(&interfaces);
                if address.scope_id() == 0 {
                    targets = indices
                        .into_iter()
                        .take(16)
                        .map(|index| {
                            let mut target = address;
                            target.set_scope_id(index);
                            SocketAddr::V6(target)
                        })
                        .collect();
                } else if matches!(self, Self::Interface { .. })
                    && !indices.contains(&address.scope_id())
                {
                    return Err(
                        "The remote IPv6 scope conflicts with the selected network interface."
                            .into(),
                    );
                }
            }
        }
        if targets.is_empty() {
            return Err(
                "Waiting for a permitted IPv6 interface for the link-local room address.".into(),
            );
        }
        let started = std::time::Instant::now();
        let count = targets.len();
        let mut last = "The room contact connection could not be established.".to_owned();
        for (at, target) in targets.into_iter().enumerate() {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            let share = (remaining / (u32::try_from(count - at).unwrap_or(1).max(1)))
                .max(Duration::from_nanos(1));
            let attempt = if matches!(self, Self::Auto) {
                TcpStream::connect_timeout(&target, share).map_err(|error| error.to_string())
            } else {
                self.connect_tcp_bound(target, share, &interfaces)
            };
            match attempt {
                Ok(stream) => return Ok(stream),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn connect_tcp_bound(
        &self,
        remote: SocketAddr,
        timeout: Duration,
        interfaces: &[NetworkInterface],
    ) -> Result<TcpStream, String> {
        let wildcard = SocketAddr::new(
            if remote.is_ipv4() {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            },
            0,
        );
        let mut binding = self.resolve_from(wildcard, interfaces)?;
        // UNICAST_IF chooses the constrained NIC; its kernel route chooses
        // the suitable source address on that NIC for this particular peer.
        binding.bind = wildcard;
        let socket = Socket::new(
            Domain::for_address(remote),
            Type::STREAM,
            Some(Protocol::TCP),
        )
        .map_err(|e| e.to_string())?;
        constrain(&socket, &binding, false)?;
        socket
            .bind(&binding.bind.into())
            .map_err(|e| e.to_string())?;
        socket
            .connect_timeout(&remote.into(), timeout)
            .map_err(|e| e.to_string())?;
        Ok(socket.into())
    }

    /// One room owner accepts all these listeners. A wildcard creates v4 and
    /// v6-only listeners on a common port; a hard NIC uses its actual addresses.
    pub fn listen_tcp_all(&self, requested: SocketAddr) -> Result<Vec<TcpListener>, String> {
        let interfaces = if matches!(self, Self::Auto) {
            Vec::new()
        } else {
            catalogue()?
        };
        let bindings = if requested.ip().is_unspecified() && matches!(self, Self::Interface { .. })
        {
            let mut bindings = Vec::new();
            for interface in interfaces
                .iter()
                .filter(|interface| interface.up && self.allows(interface))
            {
                for ip in &interface.addresses {
                    if ip.is_multicast() || ip.is_unspecified() {
                        continue;
                    }
                    let index = if ip.is_ipv4() {
                        interface.ipv4_index
                    } else {
                        interface.ipv6_index
                    };
                    let Some(index) = index.filter(|index| *index != 0) else {
                        continue;
                    };
                    let mut bind = SocketAddr::new(*ip, requested.port());
                    if let SocketAddr::V6(address) = &mut bind {
                        if address.ip().is_unicast_link_local() {
                            address.set_scope_id(index);
                        }
                    }
                    bindings.push(NetworkBinding {
                        bind,
                        interface: Some(interface.name.clone()),
                        interface_index: Some(index),
                    });
                    if bindings.len() == 16 {
                        break;
                    }
                }
            }
            if bindings.is_empty() {
                return Err(
                    "Waiting for usable addresses on the selected network interface.".into(),
                );
            }
            bindings
        } else if requested.ip().is_unspecified() {
            vec![
                NetworkBinding {
                    bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), requested.port()),
                    interface: None,
                    interface_index: None,
                },
                NetworkBinding {
                    bind: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), requested.port()),
                    interface: None,
                    interface_index: None,
                },
            ]
        } else {
            vec![self.resolve_from(requested, &interfaces)?]
        };
        let mut last = "No room contact listener could be opened.".to_owned();
        let mut retained = Vec::new();
        for _ in 0..if requested.port() == 0 { 4 } else { 1 } {
            let mut port = requested.port();
            let mut listeners = Vec::new();
            for original in &bindings {
                let mut binding = original.clone();
                binding.bind.set_port(port);
                let result = (|| -> Result<TcpListener, String> {
                    let socket = Socket::new(
                        Domain::for_address(binding.bind),
                        Type::STREAM,
                        Some(Protocol::TCP),
                    )
                    .map_err(|e| e.to_string())?;
                    if binding.bind.is_ipv6() {
                        socket.set_only_v6(true).map_err(|e| e.to_string())?;
                    }
                    constrain(&socket, &binding, false)?;
                    socket
                        .bind(&binding.bind.into())
                        .map_err(|e| e.to_string())?;
                    socket.listen(128).map_err(|e| e.to_string())?;
                    let listener: TcpListener = socket.into();
                    if port == 0 {
                        port = listener.local_addr().map_err(|e| e.to_string())?.port();
                    }
                    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                    Ok(listener)
                })();
                match result {
                    Ok(listener) => listeners.push(listener),
                    Err(error) => last = error,
                }
            }
            if listeners.len() == bindings.len() {
                return Ok(listeners);
            }
            if listeners.len() > retained.len() {
                retained = listeners;
            } else {
                drop(listeners);
            }
            if !retained.is_empty() && requested.port() != 0 {
                break;
            }
        }
        if retained.is_empty() {
            Err(last)
        } else {
            Ok(retained)
        }
    }
}

/// Bounded real media endpoints. Wildcards expand only into addresses of an
/// actually bound family and permitted NIC; the current TCP source is first.
/// Scope metadata always belongs to this host, never the receiving host.
pub fn media_candidates(
    policy: &NetworkPolicy,
    local_addresses: &[SocketAddr],
    interfaces: &[NetworkInterface],
    tcp_local_ip: Option<IpAddr>,
) -> Vec<rlb_contact::MediaSocket> {
    let Some(port) = local_addresses
        .iter()
        .find(|address| address.port() != 0)
        .map(|address| address.port())
    else {
        return Vec::new();
    };
    let mut result = Vec::with_capacity(16);
    let allowed = |ip: IpAddr| {
        interfaces.iter().any(|interface| {
            interface.up && policy.allows(interface) && interface.addresses.contains(&ip)
        })
    };
    let mut push = |ip: IpAddr, scope: u32| {
        if result.len() == 16 || ip.is_unspecified() || ip.is_multicast() {
            return;
        }
        if let Ok(socket) = rlb_contact::MediaSocket::explicit(ip, port, scope) {
            if !result.contains(&socket) {
                result.push(socket);
            }
        }
    };
    if let Some(ip) = tcp_local_ip.filter(|ip| !ip.is_loopback() && !ip.is_unspecified()) {
        if local_addresses.iter().any(|bound| {
            bound.port() == port
                && bound.is_ipv4() == ip.is_ipv4()
                && (bound.ip().is_unspecified() || bound.ip() == ip)
        }) && (matches!(policy, NetworkPolicy::Auto) || allowed(ip))
        {
            let scope = interfaces
                .iter()
                .find(|interface| interface.addresses.contains(&ip))
                .and_then(|interface| interface.ipv6_index)
                .unwrap_or(0);
            push(
                ip,
                if link_local(ip) && ip.is_ipv6() {
                    scope
                } else {
                    0
                },
            );
        }
    }
    let has_external = local_addresses.iter().any(|address| {
        address.port() == port && !address.ip().is_unspecified() && !address.ip().is_loopback()
    });
    for bound in local_addresses
        .iter()
        .filter(|address| address.port() == port && (!has_external || !address.ip().is_loopback()))
    {
        if !bound.ip().is_unspecified() {
            let scope = match bound {
                SocketAddr::V6(address) => address.scope_id(),
                _ => 0,
            };
            push(bound.ip(), scope);
            continue;
        }
        for interface in interfaces.iter().filter(|interface| {
            interface.up
                && (!interface.loopback || matches!(policy, NetworkPolicy::Interface { .. }))
                && policy.allows(interface)
        }) {
            for ip in interface
                .addresses
                .iter()
                .filter(|ip| ip.is_ipv4() == bound.is_ipv4())
            {
                push(
                    *ip,
                    if link_local(*ip) && ip.is_ipv6() {
                        interface.ipv6_index.unwrap_or(0)
                    } else {
                        0
                    },
                );
            }
        }
    }
    result
}

pub(crate) fn bind_udp(binding: &NetworkBinding) -> Result<UdpSocket, String> {
    let socket = Socket::new(
        Domain::for_address(binding.bind),
        Type::DGRAM,
        Some(Protocol::UDP),
    )
    .map_err(|error| error.to_string())?;
    if binding.bind.is_ipv6() {
        socket
            .set_only_v6(true)
            .map_err(|error| error.to_string())?;
    }
    constrain(&socket, binding, true)?;
    socket.bind(&binding.bind.into()).map_err(|error| {
        format!(
            "The media socket could not bind to {}: {error}",
            binding.bind
        )
    })?;
    socket
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    Ok(socket.into())
}

fn link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unicast_link_local(),
    }
}

#[cfg(windows)]
fn constrain(socket: &Socket, binding: &NetworkBinding, datagram: bool) -> Result<(), String> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{
        IP_ADD_IFLIST, IP_IFLIST, IP_UNICAST_IF, IPPROTO_IP, IPPROTO_IPV6, IPV6_ADD_IFLIST,
        IPV6_IFLIST, IPV6_UNICAST_IF, SOCKET, SOCKET_ERROR, WSAGetLastError, setsockopt,
    };
    if binding.interface.is_none() {
        return Ok(());
    }
    let Some(index) = binding.interface_index else {
        return Ok(());
    };
    let (level, option, bytes) = if binding.bind.is_ipv4() {
        (IPPROTO_IP.0, IP_UNICAST_IF, index.to_be_bytes())
    } else {
        (IPPROTO_IPV6.0, IPV6_UNICAST_IF, index.to_ne_bytes())
    };
    // Microsoft documents IPv4 UNICAST_IF in network byte order and IPv6 in
    // host order. IFLIST restricts datagram receive interfaces as well.
    // https://learn.microsoft.com/windows/win32/winsock/ipproto-ip-socket-options
    // https://learn.microsoft.com/windows/win32/winsock/ipproto-ipv6-socket-options
    let set = |option, bytes: &[u8]| -> Result<(), String> {
        // The slice remains live through this synchronous call and has the
        // documented DWORD width. The socket stays owned by the caller.
        let result = unsafe {
            setsockopt(
                SOCKET(socket.as_raw_socket() as usize),
                level,
                option,
                Some(bytes),
            )
        };
        if result == SOCKET_ERROR {
            let error = unsafe { WSAGetLastError() };
            Err(format!(
                "The strict network interface could not be applied: {}",
                std::io::Error::from_raw_os_error(error.0)
            ))
        } else {
            Ok(())
        }
    };
    set(option, &bytes)?;
    if datagram && binding.interface.is_some() {
        set(
            if binding.bind.is_ipv4() {
                IP_ADD_IFLIST
            } else {
                IPV6_ADD_IFLIST
            },
            &index.to_ne_bytes(),
        )?;
        set(
            if binding.bind.is_ipv4() {
                IP_IFLIST
            } else {
                IPV6_IFLIST
            },
            &1u32.to_ne_bytes(),
        )?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn constrain(_: &Socket, binding: &NetworkBinding, _: bool) -> Result<(), String> {
    if binding.interface.is_some() {
        Err("Strict interface binding currently targets Windows.".into())
    } else {
        Ok(())
    }
}
