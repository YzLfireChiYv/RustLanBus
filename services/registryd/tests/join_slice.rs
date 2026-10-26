//! Live TCP/UDP 17099 join slice against `rustlanbus-registryd`.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use rlb_control::Version2;
use rlb_model::{
    CapabilityGrants, CaptureMethod, CatalogRevision, EndpointMode, GrantRevision,
    MemberConfigRevision, MemberId, QualityProfile, RequestId, RunId, SourceDescriptor, SourceId,
    SourceKind, SourceState,
};
use rlb_registry::{
    AppliedRoomRevisions, BIND_OK_BYTES, BindMedia, BindOk, ClaimSecret, ClientMessage,
    DriverSigningState, FrameDecoder, LeaveReason, MemberProjection, RLBREG2_PREFACE,
    SafeLocalFields, ServerMessage, SourceSlot, TokenDigest, encode_json_frame,
    parse_server_message,
};
use rustlanbus_registryd::{RegistryService, ServerConfig};

#[test]
fn enroll_claim_hello_heartbeat_and_bind_media() {
    let state_dir = std::env::temp_dir().join(format!(
        "rlb-registryd-{}-{}",
        std::process::id(),
        now_unique()
    ));
    let service = RegistryService::start(ServerConfig {
        listen: "127.0.0.1:0".parse().expect("listen"),
        state_dir,
        auto_approve: true,
        room_name: "家庭房间".to_owned(),
    })
    .expect("start registry");
    let addr = service.local_addr();
    let room_id = service.room_id();

    let member_id = MemberId::generate().expect("member");
    let system_audio = SourceId::generate().expect("system audio");
    let microphone = SourceId::generate().expect("microphone");
    let slots = vec![
        SourceSlot {
            source_id: system_audio,
            kind: SourceKind::SystemAudio,
        },
        SourceSlot {
            source_id: microphone,
            kind: SourceKind::Microphone,
        },
    ];
    let claim = ClaimSecret::generate().expect("claim");

    let pending = exchange(
        addr,
        ClientMessage::EnrollmentBegin {
            v: Version2,
            request_id: RequestId::generate().expect("request"),
            room_id,
            member_id,
            display_name: "member".to_owned(),
            source_slots: slots.clone(),
            claim_sha256: claim.digest(),
            request_readmission: false,
        },
    );
    let ServerMessage::EnrollmentPending {
        admission,
        room_name,
        ..
    } = pending
    else {
        panic!("expected enrollment_pending, got {pending:?}");
    };
    assert_eq!(admission, rlb_model::AdmissionState::Approved);
    assert_eq!(room_name, "家庭房间");

    let claimed = exchange(
        addr,
        ClientMessage::EnrollmentClaim {
            v: Version2,
            request_id: RequestId::generate().expect("request"),
            room_id,
            member_id,
            claim_secret: claim,
        },
    );
    let ServerMessage::EnrollmentClaimResult {
        member_token,
        token_generation,
        admission,
        ..
    } = claimed
    else {
        panic!("expected enrollment_claim_result, got {claimed:?}");
    };
    assert_eq!(admission, rlb_model::AdmissionState::Approved);

    let mut session = Session::connect(addr);
    let hello = ClientMessage::EngineHello {
        v: Version2,
        request_id: RequestId::generate().expect("request"),
        member_token: member_token.clone(),
        token_generation,
        token_sha256: TokenDigest::of(member_token.expose_bytes()),
        room_id,
        member_id,
        run_id: RunId::generate().expect("run"),
        display_name: "member".to_owned(),
        client_build: "rustlanbus-test".to_owned(),
        catalog_revision: CatalogRevision::new(1),
        sources: catalog(system_audio, microphone),
        member_config_revision: MemberConfigRevision::new(1),
        member_projection: projection(),
        grant_revision: GrantRevision::new(1),
        capability_grants: CapabilityGrants::default(),
    };
    session.send(&hello);
    let ready = session.recv();
    let ServerMessage::SessionReady {
        registry_session_id,
        bind_nonce,
        server_generation,
        lease_ms,
        heartbeat_ms,
        ..
    } = ready
    else {
        panic!("expected session_ready, got {ready:?}");
    };
    assert_eq!(lease_ms, 30_000);
    assert_eq!(heartbeat_ms, 10_000);
    let snapshot = session.recv();
    assert!(matches!(snapshot, ServerMessage::RoomSnapshot { .. }));

    session.send(&ClientMessage::Heartbeat {
        v: Version2,
        request_id: RequestId::generate().expect("request"),
        room_id,
        server_generation,
        registry_session_id,
        applied_revisions: AppliedRoomRevisions {
            room_revision: rlb_model::RoomRevision::new(2),
            presence_revision: rlb_model::PresenceRevision::new(1),
        },
    });
    let ack = session.recv();
    assert!(matches!(ack, ServerMessage::HeartbeatAck { .. }));

    let udp = UdpSocket::bind("127.0.0.1:0").expect("udp");
    udp.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("udp timeout");
    let bind = BindMedia {
        server_generation,
        registry_session_id,
        bind_nonce,
    };
    udp.send_to(&bind.encode(), addr).expect("send BIND_MEDIA");
    let mut packet = [0_u8; BIND_OK_BYTES];
    let (length, _) = udp.recv_from(&mut packet).expect("BIND_OK");
    assert_eq!(length, BIND_OK_BYTES);
    BindOk::decode(&packet).expect("decode BIND_OK");

    let bound = session.recv();
    assert!(matches!(bound, ServerMessage::MediaBound { .. }));

    session.send(&ClientMessage::Leave {
        v: Version2,
        request_id: RequestId::generate().expect("request"),
        room_id,
        server_generation,
        registry_session_id,
        reason: LeaveReason::UserLeave,
    });

    service.shutdown().expect("shutdown");
}

fn catalog(system_audio: SourceId, microphone: SourceId) -> Vec<SourceDescriptor> {
    [
        (system_audio, SourceKind::SystemAudio, "System audio"),
        (microphone, SourceKind::Microphone, "Microphone"),
    ]
    .into_iter()
    .map(|(source_id, kind, name)| {
        SourceDescriptor::new(
            source_id,
            kind,
            name,
            SourceState::new(
                false,
                false,
                false,
                QualityProfile::PcmF32V1,
                QualityProfile::PcmF32V1,
                false,
            )
            .expect("source state"),
            None,
        )
        .expect("source descriptor")
    })
    .collect()
}

fn projection() -> MemberProjection {
    MemberProjection {
        local_emergency_stop: false,
        auto_reconnect: true,
        effective_room_send_muted: false,
        effective_qos_policy: rlb_model::QosPolicy::AudioPriority,
        safe_local_fields: SafeLocalFields {
            capture_method: CaptureMethod::WasapiLoopback,
            capture_endpoint_mode: EndpointMode::FollowDefault,
            capture_friendly_label: "Default".to_owned(),
            capture_available: true,
            playback_endpoint_mode: EndpointMode::FollowDefault,
            playback_friendly_label: "Default".to_owned(),
            playback_available: true,
            driver_version: None,
            driver_signing_state: DriverSigningState::NotInstalled,
            driver_available: false,
            master_capture_enabled: false,
            local_emergency_stop: false,
            startup_enabled: false,
            file_logging_enabled: false,
        },
    }
}

fn exchange(addr: std::net::SocketAddr, message: ClientMessage) -> ServerMessage {
    let mut session = Session::connect(addr);
    session.send(&message);
    session.recv()
}

struct Session {
    stream: TcpStream,
    decoder: FrameDecoder,
}

impl Session {
    fn connect(addr: std::net::SocketAddr) -> Self {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("tcp");
        stream.set_nodelay(true).expect("nodelay");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream.write_all(RLBREG2_PREFACE).expect("client preface");
        stream.flush().expect("flush preface");
        let mut preface = [0_u8; 8];
        stream.read_exact(&mut preface).expect("server preface");
        assert_eq!(&preface, RLBREG2_PREFACE);
        let mut decoder = FrameDecoder::new();
        let advanced = decoder.feed(RLBREG2_PREFACE).expect("advance decoder");
        assert!(advanced.is_empty());
        Self { stream, decoder }
    }

    fn send(&mut self, message: &ClientMessage) {
        let frame = encode_json_frame(message).expect("encode");
        self.stream.write_all(&frame).expect("write frame");
        self.stream.flush().expect("flush frame");
    }

    fn recv(&mut self) -> ServerMessage {
        let mut buffer = [0_u8; 8192];
        loop {
            let length = self.stream.read(&mut buffer).expect("read");
            assert_ne!(length, 0, "registry closed the TCP session");
            let frames = self.decoder.feed(&buffer[..length]).expect("decode");
            if let Some(payload) = frames.into_iter().next() {
                return parse_server_message(&payload).unwrap_or_else(|error| {
                    panic!(
                        "server frame: {error}; payload {}",
                        String::from_utf8_lossy(&payload)
                    )
                });
            }
        }
    }
}

fn now_unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}
