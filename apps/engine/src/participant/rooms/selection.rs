//! Deterministic name-first room selection from a saved target and discovery.
//!
//! Uses the single persisted [`RoomTarget`] from [`super::profile`]. This module
//! does not persist, discover, probe, or join a room, and does not print passwords.

use std::net::{IpAddr, SocketAddr};

use rlb_model::RoomId;

use super::profile::RoomTarget;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct DiscoveredRoom {
    pub name: String,
    /// Run-lifetime marker for this contact-service instance. Same id => same instance, merge candidates.
    pub instance_id: RoomId,
    pub candidates: Vec<SocketAddr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    Manual,
    Automatic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionPlan {
    /// Concrete instance chosen. `candidates` are runtime addresses of that instance.
    ///
    /// A choice justified by an explicit IP keeps only endpoints on that
    /// normalized IP. The ordinary single same-name match may keep every
    /// advertised address of that instance.
    Selected {
        instance_id: RoomId,
        name: String,
        candidates: Vec<SocketAddr>,
    },
    /// Ambiguous. `rooms` are the actual same-name (or exact-match) candidates with their addresses.
    NeedsChoice {
        expected_name: String,
        rooms: Vec<DiscoveredRoom>,
    },
    /// Instruct later networking to probe this address; not proof of join success.
    ProbeAddress {
        ip: IpAddr,
        port: u16,
        expected_name: String,
    },
    NotFound {
        expected_name: String,
    },
    /// Automatic + strict_auto_join with no explicit address.
    AutoJoinBlocked {
        expected_name: String,
        reason: AutoJoinBlockReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoJoinBlockReason {
    MissingAddress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionError {
    InvalidTarget(String),
}

/// Merge advertisements of the same instance_id: union of candidate addrs (stable order, no dups).
/// Distinct instance_ids stay distinct even with the same display name.
/// The first advertisement supplies the display name.
pub fn deduplicate_instances(rooms: &[DiscoveredRoom]) -> Vec<DiscoveredRoom> {
    let mut merged: Vec<DiscoveredRoom> = Vec::new();
    for room in rooms {
        if let Some(existing) = merged
            .iter_mut()
            .find(|seen| seen.instance_id == room.instance_id)
        {
            for candidate in &room.candidates {
                push_unique_candidate(&mut existing.candidates, *candidate);
            }
        } else {
            let mut candidates = Vec::with_capacity(room.candidates.len());
            for candidate in &room.candidates {
                push_unique_candidate(&mut candidates, *candidate);
            }
            merged.push(DiscoveredRoom {
                name: room.name.clone(),
                instance_id: room.instance_id,
                candidates,
            });
        }
    }
    merged
}

/// Choose a join plan from a saved target and currently discovered rooms.
///
/// Does not compare or fall back on `target.password`. A probe decision is not a join.
pub fn choose_plan(
    target: &RoomTarget,
    discovered: &[DiscoveredRoom],
    mode: JoinMode,
) -> Result<SelectionPlan, SelectionError> {
    target.validate().map_err(SelectionError::InvalidTarget)?;
    let same_name: Vec<DiscoveredRoom> = deduplicate_instances(discovered)
        .into_iter()
        .filter(|room| room.name == target.name)
        .collect();

    if mode == JoinMode::Automatic && target.strict_auto_join {
        Ok(strict_automatic_plan(target, same_name))
    } else {
        Ok(normal_plan(target, same_name))
    }
}

fn strict_automatic_plan(target: &RoomTarget, same_name: Vec<DiscoveredRoom>) -> SelectionPlan {
    let Some(ip) = target.address else {
        return SelectionPlan::AutoJoinBlocked {
            expected_name: target.name.clone(),
            reason: AutoJoinBlockReason::MissingAddress,
        };
    };

    let exact: Vec<DiscoveredRoom> = same_name
        .into_iter()
        .filter(|room| room_contains_ip(room, ip))
        .map(|room| room_on_ip(room, ip))
        .collect();
    match exact.len() {
        0 => SelectionPlan::ProbeAddress {
            ip,
            port: target.effective_port(),
            expected_name: target.name.clone(),
        },
        1 => selected_plan(take_one(exact)),
        _ => SelectionPlan::NeedsChoice {
            expected_name: target.name.clone(),
            rooms: exact,
        },
    }
}

fn normal_plan(target: &RoomTarget, mut same_name: Vec<DiscoveredRoom>) -> SelectionPlan {
    match same_name.len() {
        0 => match target.address {
            Some(ip) => SelectionPlan::ProbeAddress {
                ip,
                port: target.effective_port(),
                expected_name: target.name.clone(),
            },
            None => SelectionPlan::NotFound {
                expected_name: target.name.clone(),
            },
        },
        1 => selected_plan(take_one(same_name)),
        _ => {
            if let Some(ip) = target.address {
                if let Some(index) = unique_ip_match(&same_name, ip) {
                    return selected_plan(room_on_ip(same_name.swap_remove(index), ip));
                }
            }
            SelectionPlan::NeedsChoice {
                expected_name: target.name.clone(),
                rooms: same_name,
            }
        }
    }
}

fn selected_plan(room: DiscoveredRoom) -> SelectionPlan {
    SelectionPlan::Selected {
        instance_id: room.instance_id,
        name: room.name,
        candidates: room.candidates,
    }
}

fn take_one(mut rooms: Vec<DiscoveredRoom>) -> DiscoveredRoom {
    debug_assert_eq!(rooms.len(), 1);
    rooms
        .pop()
        .expect("selection helper requires exactly one room")
}

fn room_on_ip(mut room: DiscoveredRoom, ip: IpAddr) -> DiscoveredRoom {
    let ip = ip.to_canonical();
    room.candidates
        .retain(|candidate| candidate.ip().to_canonical() == ip);
    room
}

fn unique_ip_match(rooms: &[DiscoveredRoom], ip: IpAddr) -> Option<usize> {
    let mut found = None;
    for (index, room) in rooms.iter().enumerate() {
        if room_contains_ip(room, ip) {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

fn room_contains_ip(room: &DiscoveredRoom, ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    room.candidates
        .iter()
        .any(|candidate| candidate.ip().to_canonical() == ip)
}

fn push_unique_candidate(candidates: &mut Vec<SocketAddr>, candidate: SocketAddr) {
    if !candidates
        .iter()
        .any(|seen| same_endpoint(*seen, candidate))
    {
        candidates.push(candidate);
    }
}

fn same_endpoint(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() == right.port()
        && left.ip().to_canonical() == right.ip().to_canonical()
        && match (left, right) {
            (SocketAddr::V6(left), SocketAddr::V6(right)) => left.scope_id() == right.scope_id(),
            _ => true,
        }
}

#[cfg(test)]
mod tests {
    use super::super::profile::DEFAULT_CONTACT_PORT;
    use super::*;

    fn room_id(byte: u8) -> RoomId {
        RoomId::from_bytes([byte; 16]).expect("nonzero room id")
    }

    fn room(name: &str, instance: u8, candidates: &[&str]) -> DiscoveredRoom {
        DiscoveredRoom {
            name: name.to_owned(),
            instance_id: room_id(instance),
            candidates: candidates
                .iter()
                .map(|candidate| candidate.parse().expect("socket addr"))
                .collect(),
        }
    }

    fn saved(
        name: &str,
        address: Option<&str>,
        port: Option<u16>,
        strict_auto_join: bool,
    ) -> RoomTarget {
        RoomTarget {
            name: name.to_owned(),
            address: address.map(|ip| ip.parse().expect("ip")),
            port,
            password: String::new(),
            strict_auto_join,
        }
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().expect("ip")
    }

    fn sock(value: &str) -> SocketAddr {
        value.parse().expect("socket addr")
    }

    #[test]
    fn one_same_name_room_is_selected_even_when_stored_ip_differs() {
        let discovered = [
            room("studio", 2, &["192.0.2.99:47101"]),
            room("Studio", 1, &["192.0.2.10:47101"]),
        ];
        let mut card = saved("Studio", Some("192.0.2.99"), Some(12345), false);
        card.password = "unused".to_owned();

        let expected = SelectionPlan::Selected {
            instance_id: room_id(1),
            name: "Studio".to_owned(),
            candidates: vec![sock("192.0.2.10:47101")],
        };
        assert_eq!(
            choose_plan(&card, &discovered, JoinMode::Manual).unwrap(),
            expected
        );
        assert_eq!(
            choose_plan(&card, &discovered, JoinMode::Automatic).unwrap(),
            expected
        );
    }

    #[test]
    fn multiple_same_name_uses_unique_ip_else_needs_choice() {
        let discovered = [
            room(
                "Studio",
                1,
                &["192.0.2.10:47101", "192.0.2.10:48000", "192.0.2.11:47101"],
            ),
            room("Studio", 2, &["192.0.2.20:9", "192.0.2.21:9"]),
        ];

        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.20"), Some(47101), false),
                &discovered,
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::Selected {
                instance_id: room_id(2),
                name: "Studio".to_owned(),
                candidates: vec![sock("192.0.2.20:9")],
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("::ffff:192.0.2.10"), None, false),
                &discovered,
                JoinMode::Automatic,
            )
            .unwrap(),
            SelectionPlan::Selected {
                instance_id: room_id(1),
                name: "Studio".to_owned(),
                candidates: vec![sock("192.0.2.10:47101"), sock("192.0.2.10:48000")],
            }
        );

        let choice = SelectionPlan::NeedsChoice {
            expected_name: "Studio".to_owned(),
            rooms: discovered.to_vec(),
        };
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.30"), None, false),
                &discovered,
                JoinMode::Manual,
            )
            .unwrap(),
            choice
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", None, None, false),
                &discovered,
                JoinMode::Manual,
            )
            .unwrap(),
            choice
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.10"), None, false),
                &[
                    room("Studio", 1, &["192.0.2.10:47101"]),
                    room("Studio", 2, &["192.0.2.10:48000"]),
                ],
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::NeedsChoice {
                expected_name: "Studio".to_owned(),
                rooms: vec![
                    room("Studio", 1, &["192.0.2.10:47101"]),
                    room("Studio", 2, &["192.0.2.10:48000"]),
                ],
            }
        );
    }

    #[test]
    fn zero_same_name_probes_explicit_ip_or_reports_not_found() {
        let other = [room("Other", 1, &["192.0.2.1:47101"])];

        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.50"), None, false),
                &other,
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::ProbeAddress {
                ip: ip("192.0.2.50"),
                port: DEFAULT_CONTACT_PORT,
                expected_name: "Studio".to_owned(),
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.50"), Some(0), false),
                &[],
                JoinMode::Automatic,
            )
            .unwrap(),
            SelectionPlan::ProbeAddress {
                ip: ip("192.0.2.50"),
                port: DEFAULT_CONTACT_PORT,
                expected_name: "Studio".to_owned(),
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.50"), Some(48000), false),
                &[],
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::ProbeAddress {
                ip: ip("192.0.2.50"),
                port: 48000,
                expected_name: "Studio".to_owned(),
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", None, Some(48000), false),
                &[],
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::NotFound {
                expected_name: "Studio".to_owned(),
            }
        );
    }

    #[test]
    fn strict_automatic_join_requires_name_and_explicit_address() {
        let discovered = [
            room("Studio", 1, &["192.0.2.10:1234"]),
            room("Studio", 2, &["192.0.2.20:9", "192.0.2.21:9"]),
            room("Other", 3, &["192.0.2.99:47101"]),
        ];

        assert_eq!(
            choose_plan(
                &saved("Studio", None, None, true),
                &discovered,
                JoinMode::Automatic,
            )
            .unwrap(),
            SelectionPlan::AutoJoinBlocked {
                expected_name: "Studio".to_owned(),
                reason: AutoJoinBlockReason::MissingAddress,
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.20"), Some(47101), true),
                &discovered,
                JoinMode::Automatic,
            )
            .unwrap(),
            SelectionPlan::Selected {
                instance_id: room_id(2),
                name: "Studio".to_owned(),
                candidates: vec![sock("192.0.2.20:9")],
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.99"), None, true),
                &discovered,
                JoinMode::Automatic,
            )
            .unwrap(),
            SelectionPlan::ProbeAddress {
                ip: ip("192.0.2.99"),
                port: DEFAULT_CONTACT_PORT,
                expected_name: "Studio".to_owned(),
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.10"), None, true),
                &[
                    room("Studio", 1, &["192.0.2.10:1"]),
                    room("Studio", 4, &["192.0.2.10:2"]),
                ],
                JoinMode::Automatic,
            )
            .unwrap(),
            SelectionPlan::NeedsChoice {
                expected_name: "Studio".to_owned(),
                rooms: vec![
                    room("Studio", 1, &["192.0.2.10:1"]),
                    room("Studio", 4, &["192.0.2.10:2"]),
                ],
            }
        );
    }

    #[test]
    fn manual_ignores_strict_auto_join_missing_address_block() {
        let discovered = [room("Studio", 1, &["192.0.2.10:47101"])];
        let no_address = saved("Studio", None, None, true);

        assert_eq!(
            choose_plan(&no_address, &[], JoinMode::Manual).unwrap(),
            SelectionPlan::NotFound {
                expected_name: "Studio".to_owned(),
            }
        );
        assert_eq!(
            choose_plan(&no_address, &discovered, JoinMode::Manual).unwrap(),
            SelectionPlan::Selected {
                instance_id: room_id(1),
                name: "Studio".to_owned(),
                candidates: vec![sock("192.0.2.10:47101")],
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.50"), None, true),
                &[],
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::ProbeAddress {
                ip: ip("192.0.2.50"),
                port: DEFAULT_CONTACT_PORT,
                expected_name: "Studio".to_owned(),
            }
        );
        assert!(matches!(
            choose_plan(&no_address, &[], JoinMode::Automatic).unwrap(),
            SelectionPlan::AutoJoinBlocked {
                reason: AutoJoinBlockReason::MissingAddress,
                ..
            }
        ));
    }

    #[test]
    fn same_instance_merges_addresses_distinct_instances_remain() {
        let discovered = [
            room("Studio", 1, &["192.0.2.10:47101", "192.0.2.10:47101"]),
            room(
                "Studio",
                1,
                &[
                    "[::ffff:192.0.2.10]:47101",
                    "192.0.2.11:47101",
                    "[fe80::1%3]:47101",
                    "[fe80::1%4]:47101",
                    "[fe80::1%3]:47101",
                ],
            ),
            room("Studio", 2, &["192.0.2.12:47101"]),
        ];

        let merged = deduplicate_instances(&discovered);
        assert_eq!(
            merged,
            vec![
                DiscoveredRoom {
                    name: "Studio".to_owned(),
                    instance_id: room_id(1),
                    candidates: vec![
                        sock("192.0.2.10:47101"),
                        sock("192.0.2.11:47101"),
                        sock("[fe80::1%3]:47101"),
                        sock("[fe80::1%4]:47101"),
                    ],
                },
                room("Studio", 2, &["192.0.2.12:47101"]),
            ]
        );

        assert_eq!(
            choose_plan(
                &saved("Studio", None, None, false),
                &discovered,
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::NeedsChoice {
                expected_name: "Studio".to_owned(),
                rooms: merged,
            }
        );
        assert_eq!(
            choose_plan(
                &saved("Studio", Some("192.0.2.12"), None, false),
                &discovered,
                JoinMode::Manual,
            )
            .unwrap(),
            SelectionPlan::Selected {
                instance_id: room_id(2),
                name: "Studio".to_owned(),
                candidates: vec![sock("192.0.2.12:47101")],
            }
        );
    }

    #[test]
    fn invalid_target_name_is_rejected_before_matching() {
        let discovered = [room("Studio", 1, &["192.0.2.10:47101"])];
        for name in ["", "   ", "bad\nname", &"x".repeat(81)] {
            let card = saved(name, Some("192.0.2.10"), None, false);
            assert!(
                matches!(
                    choose_plan(&card, &discovered, JoinMode::Manual),
                    Err(SelectionError::InvalidTarget(_))
                ),
                "name length {}",
                name.chars().count()
            );
        }

        let max_name = "x".repeat(80);
        assert_eq!(
            choose_plan(&saved(&max_name, None, None, false), &[], JoinMode::Manual,).unwrap(),
            SelectionPlan::NotFound {
                expected_name: max_name,
            }
        );
    }
}
