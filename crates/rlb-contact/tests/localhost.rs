//! Localhost control checks for the contact service.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use rlb_contact::{
    CONTACT_PREFACE, ContactClient, ContactCode, ContactConfig, ContactError, ContactEvent,
    ContactLossCause, ContactService, ContactVisibility, DEFAULT_CONTACT_PORT, DEFAULT_IO_TIMEOUT,
    JoinParams, encode_payload, snapshot_gaps,
};
use rlb_model::{MemberId, RunId};

const SECRET: &str = "contact-test-secret";

#[test]
fn bound_port_is_reported_and_a_second_bind_keeps_the_first_socket() {
    let (service, address) = start("Headphones", "");
    assert_ne!(address.port(), 0);
    assert_eq!(service.room_info().port, address.port());
    assert_eq!(service.room_info().protocol_version, 1);
    assert_eq!(DEFAULT_CONTACT_PORT, 47_101);
    let error = occupied_bind(address);
    let ContactError::Bind {
        address: failed, ..
    } = error
    else {
        panic!("expected bind failure");
    };
    assert_eq!(failed, address);
    let text = occupied_bind(address).to_string();
    assert!(text.contains(&address.port().to_string()));
    assert!(!text.contains(SECRET));
    let mut probe = connect(address);
    let info = probe.inspect().expect("inspect");
    assert_eq!(info.room_name, "Headphones");
    assert_eq!(info.port, address.port());
    assert_eq!(info.instance_id, service.room_info().instance_id);
    service.stop().expect("stop");
}

#[test]
fn inspect_then_join_and_wrong_credentials_leave_membership_unchanged() {
    let (service, address) = start("Headphones", SECRET);
    let mut member = connect(address);
    let info = member.inspect().expect("inspect");
    assert_eq!(info.room_name, "Headphones");
    let before = service.room_info().instance_id;
    assert_eq!(info.instance_id, before);

    let wrong_password = join_result(
        &mut member,
        params("Headphones", "nope", 1, "甲", 9_001, None),
    );
    assert_eq!(
        rejection_code(wrong_password),
        ContactCode::PasswordRejected
    );
    assert!(
        !rejection_message(join_result(
            &mut connect(address),
            params("Headphones", "contact-test-secret-no", 2, "乙", 9_002, None),
        ))
        .contains(SECRET)
    );

    let wrong_name = join_result(
        &mut connect(address),
        params("Headphones ", SECRET, 3, "丙", 9_003, None),
    );
    assert_eq!(rejection_code(wrong_name), ContactCode::RoomNameMismatch);

    let joined = member
        .join(params("Headphones", SECRET, 1, "甲", 9_001, None))
        .expect("join after inspect")
        .value;
    assert_eq!(joined.members.len(), 1);
    assert_eq!(joined.you.display_name, "甲");
    assert!(
        joined
            .members
            .iter()
            .any(|member| member.member_id == joined.you.member_id)
    );
    service.stop().expect("stop");
}

#[test]
fn duplicate_names_keep_declared_udp_ports() {
    let (service, address) = start("Headphones", "");
    let mut first = connect(address);
    let first_tcp = first.local_addr().expect("tcp").port();
    let first_media = if first_tcp == 9_001 { 9_002 } else { 9_001 };
    let joined = first
        .join(params("Headphones", "", 1, "同名", first_media, None))
        .expect("first")
        .value;
    assert_eq!(joined.you.media.port(), first_media);
    assert_ne!(joined.you.media.port(), first_tcp);
    assert_eq!(joined.you.media.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));

    let mut second = connect(address);
    let second_tcp = second.local_addr().expect("tcp").port();
    let second_media = if second_tcp == 9_003 { 9_004 } else { 9_003 };
    second
        .join(params("Headphones", "", 2, "同名", second_media, None))
        .expect("second");
    let visible = next_visible(&mut first);
    assert_eq!(visible.display_name, "同名");
    assert_ne!(visible.member_id, joined.you.member_id);
    assert_eq!(visible.media.port(), second_media);
    assert_ne!(visible.media.port(), second_tcp);

    let snapshot = first.refresh().expect("snapshot").value;
    assert_eq!(snapshot.members.len(), 2);
    assert!(
        snapshot
            .members
            .iter()
            .all(|member| member.display_name == "同名")
    );
    service.stop().expect("stop");
}

#[test]
fn explicit_media_address_replaces_the_observed_peer() {
    let (service, address) = start("Headphones", "");
    let mut observer = connect(address);
    observer
        .join(params("Headphones", "", 1, "观察", 8_001, None))
        .expect("observer");
    let mut pinned = connect(address);
    let explicit = Ipv4Addr::new(198, 51, 100, 23);
    pinned
        .join(params(
            "Headphones",
            "",
            2,
            "固定",
            5_004,
            Some(explicit.into()),
        ))
        .expect("pinned");
    let visible = next_visible(&mut observer);
    assert_eq!(visible.media.ip(), IpAddr::from(explicit));
    assert_eq!(visible.media.port(), 5_004);
    assert_ne!(visible.media.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
    service.stop().expect("stop");
}

#[test]
fn replacement_survives_the_old_connection_closing() {
    let (service, address) = start("Headphones", "");
    let mut observer = connect(address);
    observer
        .join(params("Headphones", "", 1, "观察", 8_010, None))
        .expect("observer");
    let mut first = connect(address);
    first
        .join(params("Headphones", "", 2, "成员", 8_011, None))
        .expect("first");
    let _ = next_visible(&mut observer);
    let mut second = connect(address);
    second
        .join(params("Headphones", "", 2, "成员", 8_012, None))
        .expect("replacement");
    let superseded = first
        .poll(Duration::from_secs(1))
        .expect("poll")
        .expect("event");
    assert!(superseded.explicit_departure().is_none());
    assert!(matches!(superseded, ContactEvent::Superseded));
    let leave = first.leave();
    assert!(leave.is_err());
    drop(first);
    assert_no_departure(&mut observer);
    let snapshot = observer.refresh().expect("snapshot").value;
    let member = snapshot
        .members
        .iter()
        .find(|member| member.display_name == "成员")
        .expect("replacement remains");
    assert_eq!(member.media.port(), 8_012);
    assert_eq!(
        snapshot
            .members
            .iter()
            .filter(|member| member.display_name == "成员")
            .count(),
        1
    );
    second
        .keepalive()
        .expect("newer registration still answers");
    service.stop().expect("stop");
}

#[test]
fn explicit_leave_names_the_member_and_disconnect_stays_visibility() {
    let (service, address) = start("Headphones", "");
    let mut observer = connect(address);
    observer
        .join(params("Headphones", "", 1, "观察", 8_020, None))
        .expect("observer");
    let mut leaving = connect(address);
    let joined = leaving
        .join(params("Headphones", "", 2, "离开", 8_021, None))
        .expect("leaving")
        .value;
    let _ = next_visible(&mut observer);
    leaving.leave().expect("leave");
    let event = observer
        .poll(Duration::from_secs(1))
        .expect("event")
        .expect("leave event");
    assert_eq!(
        event.explicit_departure(),
        Some((joined.you.member_id, joined.you.run_id))
    );
    assert!(event.contact_visibility().is_none());
    let after_leave = observer.refresh().expect("refresh").value;
    assert!(
        after_leave
            .members
            .iter()
            .all(|member| member.member_id != joined.you.member_id)
    );
    assert_eq!(
        snapshot_gaps(&joined.members, &after_leave.members).len(),
        1
    );

    let mut dropped = connect(address);
    let hidden = dropped
        .join(params("Headphones", "", 3, "断开", 8_022, None))
        .expect("dropped")
        .value;
    let _ = next_visible(&mut observer);
    drop(dropped);
    let event = observer
        .poll(Duration::from_secs(1))
        .expect("poll")
        .expect("visibility");
    assert!(event.explicit_departure().is_none());
    assert_eq!(
        event.contact_visibility(),
        Some(ContactVisibility::Member {
            member_id: hidden.you.member_id,
            run_id: hidden.you.run_id,
            cause: ContactLossCause::ControlDisconnect,
        })
    );
    assert!(ContactError::ControlLost.is_local_control_failure());
    service.stop().expect("stop");
}

#[test]
fn service_stop_is_visibility_and_releases_the_port() {
    let (service, address) = start("Headphones", SECRET);
    let mut member = connect(address);
    member
        .join(params("Headphones", SECRET, 1, "甲", 8_030, None))
        .expect("join");
    let instance = service.room_info().instance_id;
    service.stop().expect("stop");
    let event = member
        .poll(Duration::from_secs(1))
        .expect("poll")
        .expect("service stop");
    assert!(event.explicit_departure().is_none());
    assert_eq!(
        event.contact_visibility(),
        Some(ContactVisibility::ServiceStop)
    );
    let restarted = ContactService::start(
        ContactConfig::try_new(address, "Headphones", "next-secret").expect("config"),
    )
    .expect("rebind");
    assert_eq!(restarted.local_addr(), address);
    assert_ne!(restarted.room_info().instance_id, instance);
    let mut again = connect(address);
    let rejected = join_result(
        &mut again,
        params("Headphones", SECRET, 1, "甲", 8_030, None),
    );
    assert_eq!(rejection_code(rejected), ContactCode::PasswordRejected);
    again
        .join(params("Headphones", "next-secret", 1, "甲", 8_031, None))
        .expect("new password");
    restarted.stop().expect("stop");
}

#[test]
fn silence_hides_a_member_without_an_explicit_departure() {
    let config =
        ContactConfig::try_new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), "Headphones", "")
            .expect("config")
            .with_contact_timeout(Duration::from_millis(500))
            .expect("timeout");
    let service = ContactService::start(config).expect("start");
    let address = service.local_addr();
    let mut idle = connect(address);
    let idle_joined = idle
        .join(params("Headphones", "", 1, "静默", 8_040, None))
        .expect("idle")
        .value;
    let mut observer = connect(address);
    observer
        .join(params("Headphones", "", 2, "观察", 8_041, None))
        .expect("observer");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = None;
    while Instant::now() < deadline {
        observer.keepalive().expect("keepalive");
        if let Some(event) = observer.poll(Duration::from_millis(100)).expect("poll") {
            seen = Some(event);
            break;
        }
    }
    let event = seen.expect("timeout notice");
    assert!(event.explicit_departure().is_none());
    assert_eq!(
        event.contact_visibility(),
        Some(ContactVisibility::Member {
            member_id: idle_joined.you.member_id,
            run_id: idle_joined.you.run_id,
            cause: ContactLossCause::Timeout,
        })
    );
    drop(idle);
    service.stop().expect("stop");
}

#[test]
fn ipv6_loopback_uses_the_observed_address_and_declared_port() {
    let config =
        ContactConfig::try_new(SocketAddr::from((Ipv6Addr::LOCALHOST, 0)), "Headphones", "")
            .expect("config");
    let service = ContactService::start(config).expect("ipv6 bind");
    let address = service.local_addr();
    assert!(address.is_ipv6());
    let mut member = connect(address);
    let joined = member
        .join(params("Headphones", "", 1, "环回", 8_050, None))
        .expect("join")
        .value;
    assert_eq!(joined.you.media.ip(), IpAddr::from(Ipv6Addr::LOCALHOST));
    assert_eq!(joined.you.media.port(), 8_050);
    assert_eq!(
        joined.you.media.socket_addr().expect("IPv6 loopback").ip(),
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    );
    assert_ne!(
        joined.you.media.port(),
        member.local_addr().expect("tcp").port()
    );
    service.stop().expect("stop");
}

#[test]
fn unknown_preface_and_unknown_message_do_not_register() {
    let (service, address) = start("Headphones", "");
    let mut observer = connect(address);
    observer
        .join(params("Headphones", "", 1, "观察", 8_060, None))
        .expect("observer");

    let mut registry = TcpStream::connect(address).expect("connect");
    registry.set_nodelay(true).expect("nodelay");
    registry.write_all(b"RLBREG2\n").expect("write");

    let mut raw = TcpStream::connect(address).expect("connect");
    raw.set_nodelay(true).expect("nodelay");
    raw.write_all(CONTACT_PREFACE).expect("preface");
    let frame = encode_payload(br#"{"v":2,"type":"kick"}"#).expect("frame");
    raw.write_all(&frame).expect("write");
    raw.set_read_timeout(Some(Duration::from_secs(1)))
        .expect("timeout");
    let mut buffer = [0_u8; 256];
    let _ = raw.read(&mut buffer);

    thread::sleep(Duration::from_millis(50));
    let snapshot = observer.refresh().expect("snapshot").value;
    assert_eq!(snapshot.members.len(), 1);
    service.stop().expect("stop");
}

#[test]
fn a_full_room_still_accepts_replacement_of_the_current_member() {
    let config =
        ContactConfig::try_new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), "Headphones", "")
            .expect("config")
            .with_max_members(1)
            .expect("limit");
    let service = ContactService::start(config).expect("start");
    let address = service.local_addr();
    let mut member = connect(address);
    member
        .join(params("Headphones", "", 1, "甲", 8_070, None))
        .expect("join");
    let rejected = join_result(
        &mut connect(address),
        params("Headphones", "", 2, "乙", 8_071, None),
    );
    assert_eq!(rejection_code(rejected), ContactCode::RoomFull);
    let mut replacement = connect(address);
    replacement
        .join(params("Headphones", "", 1, "甲", 8_072, None))
        .expect("replacement");
    let snapshot = replacement.refresh().expect("snapshot").value;
    assert_eq!(snapshot.members.len(), 1);
    assert_eq!(snapshot.members[0].media.port(), 8_072);
    service.stop().expect("stop");
}

fn occupied_bind(address: SocketAddr) -> ContactError {
    match ContactService::start(ContactConfig::try_new(address, "Other", SECRET).expect("config")) {
        Err(error) => error,
        Ok(extra) => {
            let _ = extra.stop();
            panic!("the occupied address accepted another listener");
        }
    }
}

fn start(name: &str, password: &str) -> (ContactService, SocketAddr) {
    let service = ContactService::start(
        ContactConfig::try_new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), name, password)
            .expect("config"),
    )
    .expect("start");
    let address = service.local_addr();
    (service, address)
}

fn connect(address: SocketAddr) -> ContactClient {
    ContactClient::connect(address, DEFAULT_IO_TIMEOUT).expect("connect")
}

fn params(
    room: &str,
    password: &str,
    number: u8,
    name: &str,
    media_port: u16,
    media_ip: Option<std::net::IpAddr>,
) -> JoinParams {
    JoinParams {
        room_name: room.to_owned(),
        password: password.to_owned(),
        member_id: opaque_member(number),
        run_id: opaque_run(number),
        display_name: name.to_owned(),
        media_port,
        media_ip,
        media_scope_id: 0,
        media_alternates: vec![],
    }
}

fn opaque_member(number: u8) -> MemberId {
    let mut bytes = [0_u8; 16];
    bytes[15] = number;
    MemberId::from_bytes(bytes).expect("member")
}

fn opaque_run(number: u8) -> RunId {
    let mut bytes = [0_u8; 16];
    bytes[0] = number;
    RunId::from_bytes(bytes).expect("run")
}

fn join_result(
    client: &mut ContactClient,
    params: JoinParams,
) -> Result<rlb_contact::JoinedRoom, ContactError> {
    client.join(params).map(|reply| reply.value)
}

fn rejection_code(result: Result<rlb_contact::JoinedRoom, ContactError>) -> ContactCode {
    match result {
        Err(ContactError::Rejected { code, .. }) => code,
        other => panic!("expected rejection, got {other:?}"),
    }
}

fn rejection_message(result: Result<rlb_contact::JoinedRoom, ContactError>) -> String {
    match result {
        Err(ContactError::Rejected { message, .. }) => message,
        other => panic!("expected rejection, got {other:?}"),
    }
}

fn next_visible(client: &mut ContactClient) -> rlb_contact::MemberContact {
    match client.poll(Duration::from_secs(1)).expect("poll") {
        Some(ContactEvent::MemberVisible(member)) => member,
        Some(other) => panic!("expected a visible member, got {other:?}"),
        None => panic!("timed out waiting for a visible member"),
    }
}

fn assert_no_departure(client: &mut ContactClient) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match client.poll(Duration::from_millis(50)).expect("poll") {
            None => {}
            Some(event)
                if event.explicit_departure().is_none() && event.contact_visibility().is_none() => {
            }
            Some(event) => panic!("unexpected directory loss: {event:?}"),
        }
    }
}
