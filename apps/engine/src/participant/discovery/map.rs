//! Pure browse-event to [`DiscoveredRoom`](super::super::rooms::DiscoveredRoom) mapping.
//!
//! The candidate name is the text-record caption. A DNS conflict label stays
//! beside that caption and is never written into the room name. The same
//! instance marker contributes one candidate, with its usable contact
//! addresses combined. IPv6 link-local addresses keep their interface scope.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use rlb_model::RoomId;

use super::super::rooms::DiscoveredRoom;
use super::contract::{PROTOCOL_VERSION, SERVICE_TYPE, display_name_ok};

/// One resolved address before it becomes a contact endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SuppliedAddress {
    pub ip: IpAddr,
    pub interface_index: u32,
}

/// One resolved advertisement, already separated from the mDNS types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedRecord {
    pub fullname: String,
    pub port: u16,
    pub addresses: Vec<SuppliedAddress>,
    pub version: Option<String>,
    pub display_name: Option<String>,
    pub instance_text: Option<String>,
    pub bound_address: Option<String>,
}

/// Browse add/update or removal/expiry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrowseSignal {
    Resolved(ResolvedRecord),
    Removed { fullname: String },
}

#[derive(Clone, Debug)]
struct Observation {
    instance_id: RoomId,
    display_name: String,
    #[cfg(test)]
    dns_label: String,
    addresses: Vec<SocketAddr>,
    sequence: u64,
}

#[derive(Default)]
struct Group {
    sequence: u64,
    display_name: String,
    addresses: Vec<SocketAddr>,
}

/// Transient observations keyed by DNS full name.
#[derive(Default)]
pub(crate) struct CandidatePool {
    observations: BTreeMap<String, Observation>,
    sequence: u64,
}

impl CandidatePool {
    pub(crate) fn apply(&mut self, signal: BrowseSignal) -> Vec<DiscoveredRoom> {
        match signal {
            BrowseSignal::Resolved(record) => self.resolve(record),
            BrowseSignal::Removed { fullname } => {
                if let Some((key, _)) = split_service_name(&fullname) {
                    self.observations.remove(&key);
                }
            }
        }
        self.candidates()
    }

    pub(crate) fn clear(&mut self) {
        self.observations.clear();
    }

    pub(crate) fn candidates(&self) -> Vec<DiscoveredRoom> {
        let mut grouped: BTreeMap<RoomId, Group> = BTreeMap::new();
        for observation in self.observations.values() {
            let group = grouped.entry(observation.instance_id).or_default();
            if observation.sequence > group.sequence {
                group.sequence = observation.sequence;
                group.display_name.clone_from(&observation.display_name);
            }
            for address in &observation.addresses {
                insert_address(&mut group.addresses, *address);
            }
        }
        grouped
            .into_iter()
            .map(|(instance_id, mut group)| {
                group.addresses.sort_by_key(address_key);
                DiscoveredRoom {
                    name: group.display_name,
                    instance_id,
                    candidates: group.addresses,
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn dns_labels(&self, instance_id: RoomId) -> Vec<String> {
        let mut labels: Vec<String> = self
            .observations
            .values()
            .filter(|observation| observation.instance_id == instance_id)
            .map(|observation| observation.dns_label.clone())
            .collect();
        labels.sort();
        labels
    }

    fn resolve(&mut self, record: ResolvedRecord) {
        let Some((key, dns_label)) = split_service_name(&record.fullname) else {
            return;
        };
        let Some(accepted) = accept(&record) else {
            return;
        };
        #[cfg(not(test))]
        let _ = dns_label;
        self.sequence = self.sequence.wrapping_add(1);
        self.observations.insert(
            key,
            Observation {
                instance_id: accepted.instance_id,
                display_name: accepted.display_name,
                #[cfg(test)]
                dns_label,
                addresses: accepted.addresses,
                sequence: self.sequence,
            },
        );
    }
}

struct Accepted {
    instance_id: RoomId,
    display_name: String,
    addresses: Vec<SocketAddr>,
}

fn accept(record: &ResolvedRecord) -> Option<Accepted> {
    if record.version.as_deref() != Some(PROTOCOL_VERSION) {
        return None;
    }
    let display_name = record.display_name.as_deref()?;
    if !display_name_ok(display_name) {
        return None;
    }
    let instance_id = record.instance_text.as_deref()?.parse::<RoomId>().ok()?;
    if record.port == 0 {
        return None;
    }
    let bound_address = record
        .bound_address
        .as_deref()
        .map(str::parse::<IpAddr>)
        .transpose()
        .ok()?
        .map(|ip| ip.to_canonical());
    let mut addresses = Vec::new();
    for supplied in &record.addresses {
        if bound_address.is_some_and(|bound| supplied.ip.to_canonical() != bound) {
            continue;
        }
        if let Some(address) = normalize_contact(supplied.ip, supplied.interface_index, record.port)
        {
            insert_address(&mut addresses, address);
        }
    }
    if addresses.is_empty() {
        return None;
    }
    addresses.sort_by_key(address_key);
    Some(Accepted {
        instance_id,
        display_name: display_name.to_owned(),
        addresses,
    })
}

/// Convert one discovered address into a contact endpoint.
///
/// IPv4-mapped IPv6 values become IPv4. IPv6 link-local values keep a
/// nonzero interface index as their scope. Other IPv6 addresses stay unscoped.
pub(crate) fn normalize_contact(ip: IpAddr, interface_index: u32, port: u16) -> Option<SocketAddr> {
    if port == 0 {
        return None;
    }
    match ip.to_canonical() {
        IpAddr::V4(address) => {
            if address.is_unspecified() || address.is_multicast() || address.is_broadcast() {
                return None;
            }
            Some(SocketAddr::V4(SocketAddrV4::new(address, port)))
        }
        IpAddr::V6(address) => {
            if address.is_unspecified() || address.is_multicast() {
                return None;
            }
            let scope = if is_unicast_link_local(address) {
                if interface_index == 0 {
                    return None;
                }
                interface_index
            } else {
                0
            };
            Some(SocketAddr::V6(SocketAddrV6::new(address, port, 0, scope)))
        }
    }
}

fn is_unicast_link_local(address: Ipv6Addr) -> bool {
    (address.segments()[0] & 0xffc0) == 0xfe80
}

fn split_service_name(fullname: &str) -> Option<(String, String)> {
    let suffix = format!(".{SERVICE_TYPE}");
    if fullname.len() <= suffix.len() {
        return None;
    }
    let (label, tail) = fullname.split_at(fullname.len() - suffix.len());
    if label.is_empty() || !tail.eq_ignore_ascii_case(&suffix) {
        return None;
    }
    Some((fullname.to_lowercase(), label.to_owned()))
}

fn insert_address(addresses: &mut Vec<SocketAddr>, candidate: SocketAddr) {
    if let Some(existing) = addresses
        .iter_mut()
        .find(|seen| same_endpoint(**seen, candidate))
    {
        *existing = prefer_endpoint(*existing, candidate);
        return;
    }
    addresses.push(candidate);
}

/// Endpoint identity shared with room selection: canonical address, port, and
/// IPv6 scope.
fn same_endpoint(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() == right.port()
        && left.ip().to_canonical() == right.ip().to_canonical()
        && match (left, right) {
            (SocketAddr::V6(left), SocketAddr::V6(right)) => left.scope_id() == right.scope_id(),
            _ => true,
        }
}

fn prefer_endpoint(left: SocketAddr, right: SocketAddr) -> SocketAddr {
    match (left, right) {
        (SocketAddr::V4(_), SocketAddr::V6(_)) => left,
        (SocketAddr::V6(_), SocketAddr::V4(_)) => right,
        _ => left,
    }
}

fn address_key(address: &SocketAddr) -> (u8, u128, u32, u16) {
    match *address {
        SocketAddr::V4(address) => (4, u128::from(u32::from(*address.ip())), 0, address.port()),
        SocketAddr::V6(address) => (
            6,
            u128::from(*address.ip()),
            address.scope_id(),
            address.port(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::super::super::rooms::{JoinMode, RoomTarget, SelectionPlan, choose_plan};
    use super::*;

    fn instance(byte: u8) -> RoomId {
        RoomId::from_bytes([byte; 16]).expect("nonzero instance marker")
    }

    fn supplied(ip: &str, interface_index: u32) -> SuppliedAddress {
        SuppliedAddress {
            ip: ip.parse().expect("address"),
            interface_index,
        }
    }

    fn socket(text: &str) -> SocketAddr {
        text.parse().expect("socket address")
    }

    fn fullname(label: &str) -> String {
        format!("{label}.{SERVICE_TYPE}")
    }

    fn record(
        label: &str,
        port: u16,
        version: &str,
        name: &str,
        instance_text: &str,
        addresses: Vec<SuppliedAddress>,
    ) -> ResolvedRecord {
        ResolvedRecord {
            fullname: fullname(label),
            port,
            addresses,
            version: Some(version.to_owned()),
            display_name: Some(name.to_owned()),
            instance_text: Some(instance_text.to_owned()),
            bound_address: None,
        }
    }

    #[test]
    fn same_name_instances_keep_separate_captions_and_merge_scoped_addresses() {
        let first = instance(1).to_string();
        let second = instance(2).to_string();
        let mut pool = CandidatePool::default();
        let merged = pool.apply(BrowseSignal::Resolved(record(
            "renamed (2)",
            47101,
            PROTOCOL_VERSION,
            "Studio",
            &first,
            vec![
                supplied("192.0.2.11", 0),
                supplied("192.0.2.10", 9),
                supplied("fe80::1", 3),
                supplied("fe80::1", 0),
            ],
        )));
        assert_eq!(merged.len(), 1);
        assert_eq!(
            pool.apply(BrowseSignal::Resolved(record(
                "also-renamed",
                47101,
                PROTOCOL_VERSION,
                "Studio",
                &first,
                vec![
                    supplied("::ffff:192.0.2.10", 0),
                    supplied("fe80::1", 4),
                    supplied("192.0.2.10", 0),
                ],
            ))),
            vec![DiscoveredRoom {
                name: "Studio".to_owned(),
                instance_id: instance(1),
                candidates: vec![
                    socket("192.0.2.10:47101"),
                    socket("192.0.2.11:47101"),
                    socket("[fe80::1%3]:47101"),
                    socket("[fe80::1%4]:47101"),
                ],
            }]
        );
        let both = pool.apply(BrowseSignal::Resolved(record(
            "second (2)",
            47101,
            PROTOCOL_VERSION,
            "Studio",
            &second,
            vec![supplied("192.0.2.20", 0)],
        )));
        assert_eq!(
            both,
            vec![
                DiscoveredRoom {
                    name: "Studio".to_owned(),
                    instance_id: instance(1),
                    candidates: vec![
                        socket("192.0.2.10:47101"),
                        socket("192.0.2.11:47101"),
                        socket("[fe80::1%3]:47101"),
                        socket("[fe80::1%4]:47101"),
                    ],
                },
                DiscoveredRoom {
                    name: "Studio".to_owned(),
                    instance_id: instance(2),
                    candidates: vec![socket("192.0.2.20:47101")],
                },
            ]
        );
        assert_eq!(
            pool.dns_labels(instance(1)),
            vec!["also-renamed".to_owned(), "renamed (2)".to_owned()]
        );
        let target = RoomTarget {
            name: "Studio".to_owned(),
            address: None,
            port: None,
            password: String::new(),
            strict_auto_join: false,
        };
        assert_eq!(
            choose_plan(&target, &both, JoinMode::Manual).expect("valid saved room"),
            SelectionPlan::NeedsChoice {
                expected_name: "Studio".to_owned(),
                rooms: both,
            }
        );
    }

    #[test]
    fn updates_replace_one_observation_and_expiry_removes_that_instance() {
        let first = instance(1).to_string();
        let second = instance(2).to_string();
        let mut pool = CandidatePool::default();
        pool.apply(BrowseSignal::Resolved(record(
            "alpha",
            47101,
            PROTOCOL_VERSION,
            "Studio",
            &first,
            vec![supplied("192.0.2.10", 0), supplied("192.0.2.11", 0)],
        )));
        pool.apply(BrowseSignal::Resolved(record(
            "beta",
            47101,
            PROTOCOL_VERSION,
            "Studio",
            &first,
            vec![supplied("fe80::1", 4)],
        )));
        pool.apply(BrowseSignal::Resolved(record(
            "gamma",
            48000,
            PROTOCOL_VERSION,
            "Studio",
            &second,
            vec![supplied("192.0.2.20", 0)],
        )));

        let replaced = pool.apply(BrowseSignal::Resolved(record(
            "alpha",
            47101,
            PROTOCOL_VERSION,
            "Studio Live",
            &first,
            vec![supplied("192.0.2.11", 0)],
        )));
        assert_eq!(
            replaced,
            vec![
                DiscoveredRoom {
                    name: "Studio Live".to_owned(),
                    instance_id: instance(1),
                    candidates: vec![socket("192.0.2.11:47101"), socket("[fe80::1%4]:47101"),],
                },
                DiscoveredRoom {
                    name: "Studio".to_owned(),
                    instance_id: instance(2),
                    candidates: vec![socket("192.0.2.20:48000")],
                },
            ]
        );

        let one_observation = pool.apply(BrowseSignal::Removed {
            fullname: fullname("beta"),
        });
        assert_eq!(
            one_observation[0].candidates,
            vec![socket("192.0.2.11:47101")]
        );
        assert_eq!(one_observation[0].instance_id, instance(1));
        let only_second = pool.apply(BrowseSignal::Removed {
            fullname: fullname("ALPHA"),
        });
        assert_eq!(
            only_second,
            vec![DiscoveredRoom {
                name: "Studio".to_owned(),
                instance_id: instance(2),
                candidates: vec![socket("192.0.2.20:48000")],
            }]
        );
        assert!(
            pool.apply(BrowseSignal::Removed {
                fullname: fullname("gamma"),
            })
            .is_empty()
        );
    }

    #[test]
    fn unresolved_records_do_not_create_or_erase_endpoints() {
        let mut pool = CandidatePool::default();
        let instance_text = instance(0x0a).to_string();
        let ignored = [
            record(
                "early",
                0,
                PROTOCOL_VERSION,
                "Studio",
                &instance_text,
                vec![supplied("192.0.2.10", 0)],
            ),
            record(
                "early",
                47101,
                PROTOCOL_VERSION,
                "Studio",
                &instance_text,
                Vec::new(),
            ),
            record(
                "early",
                47101,
                "2",
                "Studio",
                &instance_text,
                vec![supplied("192.0.2.10", 0)],
            ),
            record(
                "early",
                47101,
                PROTOCOL_VERSION,
                "Studio",
                &instance_text.to_uppercase(),
                vec![supplied("192.0.2.10", 0)],
            ),
            ResolvedRecord {
                fullname: fullname("early"),
                port: 47101,
                addresses: vec![supplied("192.0.2.10", 0)],
                version: Some(PROTOCOL_VERSION.to_owned()),
                display_name: None,
                instance_text: Some(instance_text.clone()),
                bound_address: None,
            },
            record(
                "early",
                47101,
                PROTOCOL_VERSION,
                "bad\nname",
                &instance_text,
                vec![supplied("192.0.2.10", 0)],
            ),
            record(
                "early",
                47101,
                PROTOCOL_VERSION,
                "Studio",
                &instance_text,
                vec![supplied("fe80::2", 0), supplied("0.0.0.0", 0)],
            ),
        ];
        for record in ignored {
            assert!(pool.apply(BrowseSignal::Resolved(record)).is_empty());
        }

        let present = pool.apply(BrowseSignal::Resolved(record(
            "early",
            47101,
            PROTOCOL_VERSION,
            "Studio",
            &instance_text,
            vec![supplied("192.0.2.10", 0)],
        )));
        assert_eq!(present[0].candidates, vec![socket("192.0.2.10:47101")]);
        let still_present = pool.apply(BrowseSignal::Resolved(record(
            "early",
            47101,
            "9",
            "Other",
            &instance(4).to_string(),
            vec![supplied("192.0.2.40", 0)],
        )));
        assert_eq!(still_present, present);
    }

    #[test]
    fn listener_hint_filters_host_addresses_and_keeps_receiver_scope() {
        let mut resolved = record(
            "contact",
            47101,
            PROTOCOL_VERSION,
            "Studio",
            &instance(1).to_string(),
            vec![
                supplied("192.0.2.10", 0),
                supplied("192.0.2.11", 0),
                supplied("2001:db8::10", 0),
                supplied("fe80::10", 7),
            ],
        );
        assert_eq!(accept(&resolved).unwrap().addresses.len(), 4);
        resolved.bound_address = Some("::ffff:192.0.2.10".to_owned());
        assert_eq!(
            accept(&resolved).unwrap().addresses,
            vec![socket("192.0.2.10:47101")]
        );
        resolved.bound_address = Some("fe80::10".to_owned());
        assert_eq!(
            accept(&resolved).unwrap().addresses,
            vec![socket("[fe80::10%7]:47101")]
        );
        for invalid in ["", "0.0.0.0", "192.0.2.99", "bad-address"] {
            resolved.bound_address = Some(invalid.to_owned());
            assert!(accept(&resolved).is_none());
        }
    }

    #[test]
    fn empty_candidate_list_leaves_an_explicit_address_probe_available() {
        let target = RoomTarget {
            name: "Studio".to_owned(),
            address: Some("192.0.2.50".parse::<IpAddr>().expect("address")),
            port: None,
            password: String::new(),
            strict_auto_join: false,
        };
        assert_eq!(
            choose_plan(&target, &[], JoinMode::Manual).expect("valid saved room"),
            SelectionPlan::ProbeAddress {
                ip: "192.0.2.50".parse().expect("address"),
                port: 47101,
                expected_name: "Studio".to_owned(),
            }
        );
    }
}
