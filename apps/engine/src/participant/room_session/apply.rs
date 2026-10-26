//! Pure contact, selection, and card decisions.
//!
//! These functions call the shared room selector and the contact address
//! helpers. They do not open sockets.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use rlb_contact::{ContactEvent, ContactLossCause, MediaError, MediaSocket, MemberContact};
use rlb_model::MemberId;

use super::super::rooms::{
    DiscoveredRoom, JoinMode, RoomProfile, RoomTarget, SCHEMA, SelectionError, SelectionPlan,
    choose_plan, create_room_profile_file,
};
use super::types::{MemberView, ProjectedEndpoint};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryView {
    epoch: u64,
    members: BTreeMap<MemberId, KnownMember>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KnownMember {
    view: MemberView,
    epoch: u64,
}

impl DirectoryView {
    pub(crate) fn new() -> Self {
        Self {
            epoch: 0,
            members: BTreeMap::new(),
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn members(&self) -> Vec<MemberView> {
        self.members
            .values()
            .map(|member| member.view.clone())
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        self.members.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Signal {
    Visible(MemberView),
    Visibility {
        member_id: MemberId,
        run_id: rlb_model::RunId,
        display_name: String,
        cause: ContactLossCause,
    },
    Departure {
        member_id: MemberId,
        run_id: rlb_model::RunId,
        display_name: String,
    },
    DirectoryLost,
}

/// What the worker should do with one [`SelectionPlan`].
///
/// Ambiguous plans stay on [`NextStep::WaitForChoice`]. The first candidate
/// is left in the list for the caller to display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NextStep {
    Connect {
        instance_id: Option<rlb_model::RoomId>,
        candidates: Vec<SocketAddr>,
    },
    Probe {
        address: SocketAddr,
    },
    WaitForChoice {
        rooms: Vec<DiscoveredRoom>,
    },
    Blocked {
        message: String,
    },
    NotFound,
}

pub(crate) fn step_from_plan(plan: SelectionPlan) -> NextStep {
    match plan {
        SelectionPlan::Selected {
            instance_id,
            candidates,
            ..
        } => NextStep::Connect {
            instance_id: Some(instance_id),
            candidates,
        },
        SelectionPlan::ProbeAddress { ip, port, .. } => NextStep::Probe {
            address: SocketAddr::new(ip, port),
        },
        SelectionPlan::NeedsChoice { rooms, .. } => NextStep::WaitForChoice { rooms },
        SelectionPlan::AutoJoinBlocked { expected_name, .. } => NextStep::Blocked {
            message: format!(
                "Automatic entry to {expected_name} needs both the room name and an entered address."
            ),
        },
        SelectionPlan::NotFound { .. } => NextStep::NotFound,
    }
}

/// Recovery is automatic entry. Previously confirmed endpoints are useful only
/// while same-name discovery is unavailable, subject to the target's IP rule.
/// Ambiguity and a missing strict address remain explicit waiting decisions.
pub(crate) fn recovery_step(
    target: &RoomTarget,
    discovered: &[DiscoveredRoom],
    last_endpoints: &[SocketAddr],
) -> Result<NextStep, SelectionError> {
    let step = step_from_plan(choose_plan(target, discovered, JoinMode::Automatic)?);
    let same_name_discovered = discovered.iter().any(|room| room.name == target.name);
    let known: Vec<SocketAddr> = last_endpoints
        .iter()
        .copied()
        .filter(|endpoint| {
            !target.strict_auto_join
                || target
                    .address
                    .is_some_and(|ip| ip.to_canonical() == endpoint.ip().to_canonical())
        })
        .collect();
    match step {
        NextStep::NotFound if !known.is_empty() => Ok(NextStep::Connect {
            instance_id: None,
            candidates: known,
        }),
        NextStep::Probe { address } if !same_name_discovered => {
            let mut candidates = vec![address];
            for endpoint in known {
                if !candidates.contains(&endpoint) {
                    candidates.push(endpoint);
                }
            }
            if candidates.len() == 1 {
                Ok(NextStep::Probe { address })
            } else {
                Ok(NextStep::Connect {
                    instance_id: None,
                    candidates,
                })
            }
        }
        other => Ok(other),
    }
}

pub(crate) fn project_media(media: MediaSocket, local_scope: u32) -> ProjectedEndpoint {
    match media.socket_addr_on(local_scope) {
        Ok(address) => ProjectedEndpoint::Ready(address),
        Err(MediaError::LocalScopeRequired) => link_local(media),
        Err(_) => match media.ip() {
            IpAddr::V4(ip) => ProjectedEndpoint::Ready(SocketAddr::from((ip, media.port()))),
            IpAddr::V6(_) => link_local(media),
        },
    }
}

fn link_local(media: MediaSocket) -> ProjectedEndpoint {
    match media.ip() {
        IpAddr::V6(ip) => ProjectedEndpoint::LocalScopeRequired {
            ip,
            port: media.port(),
        },
        IpAddr::V4(ip) => ProjectedEndpoint::Ready(SocketAddr::from((ip, media.port()))),
    }
}

pub(crate) fn usable_lan_media(media: MediaSocket) -> bool {
    match media.ip() {
        IpAddr::V4(ip) => !ip.is_loopback(),
        IpAddr::V6(ip) => !ip.is_loopback(),
    }
}

pub(crate) fn apply_ordered(
    view: &mut DirectoryView,
    before: &[ContactEvent],
    members: &[MemberContact],
    local_scope: u32,
) -> Vec<Signal> {
    let mut signals = Vec::new();
    for event in before {
        if let Some(signal) = apply_event(view, event, view.epoch, local_scope) {
            signals.push(signal);
        }
    }
    let next = view.epoch.saturating_add(1);
    signals.extend(apply_snapshot(view, members, next, local_scope));
    view.epoch = next;
    signals
}

pub(crate) fn apply_event(
    view: &mut DirectoryView,
    event: &ContactEvent,
    epoch: u64,
    local_scope: u32,
) -> Option<Signal> {
    if let Some((member_id, run_id)) = event.explicit_departure() {
        return Some(apply_departure(view, member_id, run_id, event));
    }
    if let Some(visibility) = event.contact_visibility() {
        return Some(apply_visibility(view, visibility));
    }
    match event {
        ContactEvent::MemberVisible(contact) => install_member(view, contact, epoch, local_scope),
        ContactEvent::Joined(joined) => {
            let _ = apply_snapshot(view, &joined.members, epoch.saturating_add(1), local_scope);
            view.epoch = epoch.saturating_add(1);
            None
        }
        ContactEvent::Snapshot(snapshot) => {
            let _ = apply_snapshot(
                view,
                &snapshot.members,
                epoch.saturating_add(1),
                local_scope,
            );
            view.epoch = epoch.saturating_add(1);
            None
        }
        _ => None,
    }
}

fn apply_departure(
    view: &mut DirectoryView,
    member_id: MemberId,
    run_id: rlb_model::RunId,
    event: &ContactEvent,
) -> Signal {
    let display_name = match event {
        ContactEvent::ExplicitLeave { display_name, .. } => display_name.clone(),
        _ => String::new(),
    };
    if let Some(member) = view.members.get(&member_id) {
        if member.view.run_id == run_id {
            let display_name = member.view.display_name.clone();
            view.members.remove(&member_id);
            return Signal::Departure {
                member_id,
                run_id,
                display_name,
            };
        }
    }
    Signal::Departure {
        member_id,
        run_id,
        display_name,
    }
}

fn apply_visibility(
    view: &mut DirectoryView,
    visibility: rlb_contact::ContactVisibility,
) -> Signal {
    match visibility {
        rlb_contact::ContactVisibility::ServiceStop => {
            for member in view.members.values_mut() {
                member.view.visible = false;
            }
            Signal::DirectoryLost
        }
        rlb_contact::ContactVisibility::Member {
            member_id,
            run_id,
            cause,
        } => {
            if let Some(member) = view.members.get_mut(&member_id) {
                if member.view.run_id == run_id {
                    member.view.visible = false;
                    return Signal::Visibility {
                        member_id,
                        run_id,
                        display_name: member.view.display_name.clone(),
                        cause,
                    };
                }
            }
            Signal::Visibility {
                member_id,
                run_id,
                display_name: String::new(),
                cause,
            }
        }
    }
}

fn install_member(
    view: &mut DirectoryView,
    contact: &MemberContact,
    epoch: u64,
    local_scope: u32,
) -> Option<Signal> {
    if let Some(existing) = view.members.get(&contact.member_id) {
        if existing.epoch > epoch {
            return None;
        }
    }
    let view_member = MemberView {
        member_id: contact.member_id,
        run_id: contact.run_id,
        display_name: contact.display_name.clone(),
        endpoint: project_media(contact.media, local_scope),
        alternatives: contact.media_alternates.iter().map(|media| project_media(*media, local_scope)).collect(),
        visible: true,
    };
    view.members.insert(
        contact.member_id,
        KnownMember {
            view: view_member.clone(),
            epoch,
        },
    );
    Some(Signal::Visible(view_member))
}

fn apply_snapshot(
    view: &mut DirectoryView,
    members: &[MemberContact],
    epoch: u64,
    local_scope: u32,
) -> Vec<Signal> {
    let mut signals = Vec::new();
    let mut seen = BTreeSet::new();
    for contact in members {
        seen.insert(contact.member_id);
        if let Some(signal) = install_member(view, contact, epoch, local_scope) {
            signals.push(signal);
        }
    }
    let missing: Vec<MemberId> = view
        .members
        .keys()
        .copied()
        .filter(|member_id| !seen.contains(member_id))
        .collect();
    for member_id in missing {
        let Some(member) = view.members.get_mut(&member_id) else {
            continue;
        };
        if member.view.visible {
            member.view.visible = false;
            member.epoch = epoch;
            signals.push(Signal::Visibility {
                member_id,
                run_id: member.view.run_id,
                display_name: member.view.display_name.clone(),
                cause: ContactLossCause::AbsentFromSnapshot,
            });
        }
    }
    signals
}

pub(crate) fn mark_all_hidden(view: &mut DirectoryView) -> Vec<Signal> {
    let mut signals = vec![Signal::DirectoryLost];
    for member in view.members.values_mut() {
        if member.view.visible {
            member.view.visible = false;
            signals.push(Signal::Visibility {
                member_id: member.view.member_id,
                run_id: member.view.run_id,
                display_name: member.view.display_name.clone(),
                cause: ContactLossCause::ServiceStop,
            });
        }
    }
    signals
}

/// A card for a room that was not already saved.
///
/// `entered_address` is the user's field. `runtime` is ignored so a connected
/// endpoint cannot fill an empty saved address.
pub(crate) fn unknown_card(
    name: &str,
    password: &str,
    entered_address: Option<IpAddr>,
    entered_port: Option<u16>,
    strict_auto_join: bool,
    rules: Vec<super::super::rooms::Rule>,
    _runtime: SocketAddr,
) -> Result<RoomProfile, String> {
    let profile = RoomProfile {
        schema: SCHEMA,
        room: RoomTarget {
            name: name.to_owned(),
            address: entered_address,
            port: entered_port,
            password: password.to_owned(),
            strict_auto_join,
        },
        rules,
        playback: Vec::new(),
    };
    profile.validate()?;
    Ok(profile)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CardWrite {
    Saved,
    Superseded,
    Unchanged(String),
}

pub(crate) fn persist_new_card(
    path: &Path,
    profile: &RoomProfile,
    result_generation: u64,
    current_generation: u64,
) -> CardWrite {
    if result_generation != current_generation {
        return CardWrite::Superseded;
    }
    match create_room_profile_file(path, profile) {
        Ok(()) => CardWrite::Saved,
        Err(message) => CardWrite::Unchanged(message),
    }
}

pub(crate) fn targets_differ(current: &RoomTarget, loaded: &RoomTarget) -> bool {
    current.name != loaded.name
        || current.address != loaded.address
        || current.port != loaded.port
        || current.password != loaded.password
        || current.strict_auto_join != loaded.strict_auto_join
}

pub(crate) fn same_generation(result_generation: u64, current_generation: u64) -> bool {
    result_generation == current_generation
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use rlb_contact::{ContactLossCause, MediaSocket};
    use rlb_model::{MemberId, RoomId, RunId};

    use super::super::super::rooms::{JoinMode, Rule, choose_plan};
    use super::*;

    fn ids(member: u8, run: u8) -> (MemberId, RunId) {
        let mut member_bytes = [0_u8; 16];
        member_bytes[15] = member;
        let mut run_bytes = [0_u8; 16];
        run_bytes[0] = run;
        (
            MemberId::from_bytes(member_bytes).unwrap(),
            RunId::from_bytes(run_bytes).unwrap(),
        )
    }

    fn contact(member: u8, run: u8, port: u16) -> MemberContact {
        let (member_id, run_id) = ids(member, run);
        MemberContact {
            member_id,
            run_id,
            display_name: "同名".to_owned(),
            media_alternates: Vec::new(),
            media: MediaSocket::explicit(IpAddr::V4(Ipv4Addr::new(198, 51, 100, member)), port, 0)
                .unwrap(),
        }
    }

    fn target(name: &str, address: Option<&str>, strict: bool) -> RoomTarget {
        RoomTarget {
            name: name.to_owned(),
            address: address.map(|value| value.parse().unwrap()),
            port: None,
            password: String::new(),
            strict_auto_join: strict,
        }
    }

    fn discovered(name: &str, id: u8, ip: &str, port: u16) -> DiscoveredRoom {
        let mut bytes = [0_u8; 16];
        bytes[1] = id;
        DiscoveredRoom {
            name: name.to_owned(),
            instance_id: RoomId::from_bytes(bytes).unwrap(),
            candidates: vec![SocketAddr::new(ip.parse().unwrap(), port)],
        }
    }

    #[test]
    fn automatic_recovery_keeps_strict_ip_and_ambiguity_constraints() {
        let previous: SocketAddr = "192.0.2.20:47101".parse().unwrap();
        let other = discovered("Room", 1, "192.0.2.20", 47101);
        let strict = target("Room", Some("192.0.2.10"), true);
        assert_eq!(
            recovery_step(&strict, std::slice::from_ref(&other), &[previous]).unwrap(),
            NextStep::Probe {
                address: "192.0.2.10:47101".parse().unwrap()
            }
        );
        assert_eq!(
            recovery_step(&strict, &[], &[previous]).unwrap(),
            NextStep::Probe {
                address: "192.0.2.10:47101".parse().unwrap()
            }
        );
        assert!(matches!(
            recovery_step(&target("Room", None, true), &[], &[previous]).unwrap(),
            NextStep::Blocked { .. }
        ));
        let ambiguous = [
            discovered("Room", 2, "192.0.2.10", 47101),
            discovered("Room", 3, "192.0.2.10", 47102),
        ];
        assert!(matches!(
            recovery_step(&strict, &ambiguous, &[previous]).unwrap(),
            NextStep::WaitForChoice { rooms } if rooms.len() == 2
        ));
        assert!(matches!(
            recovery_step(&target("Room", Some("192.0.2.10"), false), &[other], &[previous]).unwrap(),
            NextStep::Connect { candidates, .. } if candidates == vec![previous]
        ));
        assert!(matches!(
            recovery_step(&target("Room", None, false), &[], &[previous]).unwrap(),
            NextStep::Connect { candidates, .. } if candidates == vec![previous]
        ));
    }

    #[test]
    fn selection_keeps_name_first_choice_and_strict_auto() {
        let one = discovered("Room", 1, "192.0.2.9", 47101);
        let plan = choose_plan(
            &target("Room", Some("198.51.100.8"), false),
            std::slice::from_ref(&one),
            JoinMode::Manual,
        )
        .unwrap();
        match step_from_plan(plan) {
            NextStep::Connect {
                instance_id,
                candidates,
            } => {
                assert_eq!(instance_id, Some(one.instance_id));
                assert_eq!(candidates, one.candidates);
            }
            other => panic!("expected the single discovered instance, got {other:?}"),
        }

        let other = discovered("Room", 2, "192.0.2.10", 47101);
        let plan = choose_plan(
            &target("Room", None, false),
            &[one.clone(), other.clone()],
            JoinMode::Automatic,
        )
        .unwrap();
        match step_from_plan(plan) {
            NextStep::WaitForChoice { rooms } => {
                assert_eq!(rooms.len(), 2);
            }
            other => panic!("expected a waiting choice, got {other:?}"),
        }

        let plan = choose_plan(&target("Room", None, true), &[], JoinMode::Automatic).unwrap();
        assert!(matches!(step_from_plan(plan), NextStep::Blocked { .. }));
        let plan = choose_plan(
            &target("Room", Some("192.0.2.9"), false),
            &[],
            JoinMode::Manual,
        )
        .unwrap();
        assert!(matches!(
            step_from_plan(plan),
            NextStep::Probe { address } if address == SocketAddr::new("192.0.2.9".parse().unwrap(), 47101)
        ));
    }

    #[test]
    fn ordered_snapshot_keeps_the_newer_address_and_hides_absence() {
        let mut view = DirectoryView::new();
        let first = contact(1, 1, 1000);
        apply_ordered(&mut view, &[], std::slice::from_ref(&first), 0);
        let older = contact(1, 1, 1001);
        let newer = contact(1, 2, 2000);
        let signals = apply_ordered(
            &mut view,
            &[ContactEvent::MemberVisible(older)],
            std::slice::from_ref(&newer),
            0,
        );
        let installed = view.members.get(&newer.member_id).unwrap();
        assert_eq!(installed.view.run_id, newer.run_id);
        match installed.view.endpoint {
            ProjectedEndpoint::Ready(address) => assert_eq!(address.port(), 2000),
            other => panic!("expected a ready endpoint, got {other:?}"),
        }
        assert!(signals.iter().any(|signal| matches!(
            signal,
            Signal::Visible(member) if member.run_id == newer.run_id
        )));

        let gone = apply_ordered(&mut view, &[], &[], 0);
        assert!(gone.iter().any(|signal| matches!(
            signal,
            Signal::Visibility {
                cause: ContactLossCause::AbsentFromSnapshot,
                ..
            }
        )));
        assert!(
            !gone
                .iter()
                .any(|signal| matches!(signal, Signal::Departure { .. }))
        );
        assert!(!view.members.get(&newer.member_id).unwrap().view.visible);
        assert_eq!(
            view.members.get(&newer.member_id).unwrap().view.run_id,
            newer.run_id
        );
    }

    #[test]
    fn an_older_leave_keeps_the_newer_run() {
        let mut view = DirectoryView::new();
        let current = contact(1, 2, 2000);
        apply_ordered(&mut view, &[], std::slice::from_ref(&current), 0);
        let (member_id, old_run) = ids(1, 1);
        let event = ContactEvent::ExplicitLeave {
            member_id,
            run_id: old_run,
            display_name: "同名".to_owned(),
        };
        let epoch = view.epoch;
        let signal = apply_event(&mut view, &event, epoch, 0).unwrap();
        assert!(matches!(
            signal,
            Signal::Departure { run_id, .. } if run_id == old_run
        ));
        assert_eq!(
            view.members.get(&member_id).unwrap().view.run_id,
            current.run_id
        );
    }

    #[test]
    fn local_scope_is_this_machine_and_ordinary_addresses_stay_ready() {
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let media = MediaSocket::explicit(IpAddr::V6(link_local), 9, 99).unwrap();
        assert!(matches!(
            project_media(media, 0),
            ProjectedEndpoint::LocalScopeRequired { ip, port: 9 } if ip == link_local
        ));
        match project_media(media, 3) {
            ProjectedEndpoint::Ready(address) => assert_eq!(
                address,
                SocketAddr::V6(std::net::SocketAddrV6::new(link_local, 9, 0, 3))
            ),
            other => panic!("expected scope 3, got {other:?}"),
        }
        let scoped = media.socket_addr_on(3).unwrap();
        let other = media.socket_addr_on(4).unwrap();
        assert_eq!(
            scoped,
            SocketAddr::V6(std::net::SocketAddrV6::new(link_local, 9, 0, 3))
        );
        assert_eq!(
            other,
            SocketAddr::V6(std::net::SocketAddrV6::new(link_local, 9, 0, 4))
        );
        assert_ne!(scoped, other);

        let v4 = MediaSocket::explicit(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 10)), 5, 0).unwrap();
        assert_eq!(
            project_media(v4, 0),
            ProjectedEndpoint::Ready(SocketAddr::from((Ipv4Addr::new(198, 51, 100, 10), 5)))
        );
        let global = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let v6 = MediaSocket::explicit(IpAddr::V6(global), 7, 0).unwrap();
        assert_eq!(
            project_media(v6, 0),
            ProjectedEndpoint::Ready(SocketAddr::from((global, 7)))
        );
    }

    #[test]
    fn unknown_card_keeps_an_absent_address_and_refuses_a_stale_or_occupied_path() {
        let runtime = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 9), 40000));
        let profile = unknown_card(
            "Headphones",
            "secret",
            None,
            None,
            false,
            Vec::<Rule>::new(),
            runtime,
        )
        .unwrap();
        assert_eq!(profile.room.address, None);
        assert!(!format!("{profile:?}").contains("secret"));
        let dir = std::env::temp_dir().join(format!(
            "rlb-c16-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("headphones.json");
        assert_eq!(
            persist_new_card(&path, &profile, 2, 3),
            CardWrite::Superseded
        );
        assert!(!path.exists());
        assert_eq!(persist_new_card(&path, &profile, 4, 4), CardWrite::Saved);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"address\": null"));
        assert!(!saved.contains("203.0.113.9"));
        let occupied = persist_new_card(&path, &profile, 4, 4);
        assert!(matches!(occupied, CardWrite::Unchanged(_)));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"address\": null")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_media_is_not_treated_as_a_lan_endpoint() {
        let loopback = MediaSocket::explicit(IpAddr::V4(Ipv4Addr::LOCALHOST), 1, 0).unwrap();
        assert!(!usable_lan_media(loopback));
        let lan = MediaSocket::explicit(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)), 1, 0).unwrap();
        assert!(usable_lan_media(lan));
    }

    #[test]
    fn generation_fence_rejects_a_late_result() {
        assert!(same_generation(3, 3));
        assert!(!same_generation(3, 4));
    }
}
