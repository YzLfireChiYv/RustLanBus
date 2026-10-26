use std::net::Ipv4Addr;

use rlb_control::{
    CommandStatus, Field, RoomCommandArgs, RoomCommandExpected, RoomCommandName, RoomCommandScope,
    Version2,
};
use rlb_media::{
    AckStatus as MediaAckStatus, BoundedRejections, BoundedSourceIds, DirectCommon, DirectDatagram,
    SubscribeAck, SubscribeSet,
};
use rlb_model::{
    AdmissionState, Capability, CaptureMethod, CatalogRevision, DirectSessionId, Endpoint,
    EndpointMode, ErrorCode, GainDb, GrantRevision, IdentityRecord, ManualBufferTarget,
    MemberConfigRevision, MemberId, MemberMix, PresenceRevision, QualityProfile,
    ReceiveBufferConfig, RegistrySessionId, RequestId, RoomId, RoomRevision, RunId,
    ServerGeneration, SourceDescriptor, SourceFormat, SourceId, SourceKind, SourceMix, SourceState,
    StreamEpoch, SubscriptionRevision,
};
use rlb_registry::{
    ControlPresence, DecimalU64, MediaPresence, RoomEvent, RoomEventValue, RoomSnapshot, RoomView,
    SelfView, ServerMessage, SnapshotMember,
};

use super::*;

const LOCAL: u128 = 1;
const PEER_A: u128 = 2;
const PEER_B: u128 = 3;
const REGISTRY_SESSION: u128 = 700;
const GENERATION: u128 = 800;

fn room_id() -> RoomId {
    RoomId::from_bytes(900_u128.to_be_bytes()).expect("room")
}

fn member_id(value: u128) -> MemberId {
    MemberId::from_bytes(value.to_be_bytes()).expect("member")
}

fn run_id(value: u128) -> RunId {
    RunId::from_bytes((value + 100).to_be_bytes()).expect("run")
}

fn source_id(member: u128, kind: SourceKind) -> SourceId {
    let kind_offset = match kind {
        SourceKind::SystemAudio => 1,
        SourceKind::Microphone => 2,
    };
    SourceId::from_bytes((member * 10 + kind_offset).to_be_bytes()).expect("source")
}

fn epoch(value: u128) -> StreamEpoch {
    StreamEpoch::from_bytes((1_000 + value).to_be_bytes()).expect("epoch")
}

fn request_id(value: u128) -> RequestId {
    RequestId::from_bytes((2_000 + value).to_be_bytes()).expect("request")
}

fn registry_session(value: u128) -> RegistrySessionId {
    RegistrySessionId::from_bytes(value.to_be_bytes()).expect("registry session")
}

fn direct_session(value: u128) -> DirectSessionId {
    DirectSessionId::from_bytes((3_000 + value).to_be_bytes()).expect("direct session")
}

fn endpoint(member: u128) -> Endpoint {
    Endpoint::new(
        Ipv4Addr::new(192, 168, 8, member as u8),
        17_000 + member as u16,
    )
    .expect("endpoint")
}

fn identity() -> IdentityRecord {
    IdentityRecord::new(
        member_id(LOCAL),
        source_id(LOCAL, SourceKind::SystemAudio),
        source_id(LOCAL, SourceKind::Microphone),
    )
    .expect("identity")
}

fn descriptor(member: u128, kind: SourceKind, active: bool) -> SourceDescriptor {
    let profile = QualityProfile::PcmF32V1;
    SourceDescriptor::new(
        source_id(member, kind),
        kind,
        match kind {
            SourceKind::SystemAudio => "System audio",
            SourceKind::Microphone => "Microphone",
        },
        SourceState::new(true, true, true, profile, profile, true).expect("source state"),
        active.then(|| {
            (
                SourceFormat::for_source(kind, profile),
                epoch(member * 10 + u128::from(kind == SourceKind::Microphone)),
            )
        }),
    )
    .expect("descriptor")
}

fn snapshot_member(member: u128) -> SnapshotMember {
    SnapshotMember {
        member_id: member_id(member),
        display_name: format!("member-{member}"),
        admission: AdmissionState::Approved,
        control_presence: ControlPresence::Online,
        media_presence: MediaPresence::Bound,
        run_id: Some(run_id(member)),
        media_endpoint: Some(endpoint(member)),
        catalog_revision: Some(CatalogRevision::new(1)),
        sources: vec![
            descriptor(member, SourceKind::SystemAudio, true),
            descriptor(member, SourceKind::Microphone, true),
        ],
    }
}

fn authority_snapshot(
    generation: u128,
    room_revision: u64,
    presence_revision: u64,
    event_cursor: u64,
    room_send_muted: bool,
    system_viewers: &[u128],
    microphone_viewers: &[u128],
) -> AuthoritySnapshot {
    AuthoritySnapshot {
        room: RoomSnapshot {
            server_generation: ServerGeneration::from_bytes(generation.to_be_bytes())
                .expect("generation"),
            room_id: room_id(),
            room_revision: RoomRevision::new(room_revision),
            presence_revision: PresenceRevision::new(presence_revision),
            event_cursor: DecimalU64::new(event_cursor),
            room: RoomView {
                name: "家庭房间".to_owned(),
                quality_cap: QualityProfile::PcmF32V1,
                qos_policy: rlb_model::QosPolicy::AudioPriority,
                room_send_muted,
                max_approved_members: 32,
            },
            self_view: SelfView {
                member_id: member_id(LOCAL),
                admission: AdmissionState::Approved,
            },
            members: vec![
                snapshot_member(LOCAL),
                snapshot_member(PEER_A),
                snapshot_member(PEER_B),
            ],
        },
        local_source_visibility: vec![
            LocalSourceVisibility {
                source_id: source_id(LOCAL, SourceKind::SystemAudio),
                viewer_member_ids: system_viewers.iter().copied().map(member_id).collect(),
            },
            LocalSourceVisibility {
                source_id: source_id(LOCAL, SourceKind::Microphone),
                viewer_member_ids: microphone_viewers.iter().copied().map(member_id).collect(),
            },
        ],
    }
}

fn room_event(
    event_seq: u64,
    room_revision: u64,
    presence_revision: u64,
    event: RoomEventValue,
) -> RoomEvent {
    RoomEvent {
        event_seq: DecimalU64::new(event_seq),
        server_generation: ServerGeneration::from_bytes(GENERATION.to_be_bytes())
            .expect("generation"),
        room_id: room_id(),
        room_revision: RoomRevision::new(room_revision),
        presence_revision: PresenceRevision::new(presence_revision),
        event,
    }
}

fn local_stream(kind: SourceKind, epoch_value: u128) -> SourceStreamStarted {
    SourceStreamStarted {
        source_id: source_id(LOCAL, kind),
        stream_epoch: epoch(epoch_value),
        endpoint_id: format!("endpoint-{kind:?}"),
        capture_method: (kind == SourceKind::SystemAudio).then_some(CaptureMethod::WasapiLoopback),
        format: SourceFormat::for_source(kind, QualityProfile::PcmF32V1),
        reason: EpochChangeReason::EngineRun,
    }
}

fn initial_config() -> MemberConfig {
    let mut config = MemberConfig::defaults(identity(), room_id());
    for source in &mut config.sources {
        source.config.master_capture_enabled = true;
        source.config.publish_requested = true;
    }
    config
}

fn extract_subscribe_set(effects: &[CoordinatorEffect]) -> SubscribeSet {
    effects
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::SendDirect(DirectDatagram::SubscribeSet(request)) => Some(*request),
            _ => None,
        })
        .expect("subscribe set effect")
}

fn acknowledge_outbound(coordinator: &mut MemberSessionCoordinator, request: SubscribeSet) {
    let ack = SubscribeAck::new(
        DirectCommon {
            room_id: request.common.room_id,
            sender_member_id: request.common.receiver_member_id,
            sender_run_id: request.common.receiver_run_id,
            receiver_member_id: request.common.sender_member_id,
            receiver_run_id: request.common.sender_run_id,
            direct_session_id: request.common.direct_session_id,
        },
        request.subscription_revision,
        request.catalog_revision,
        MediaAckStatus::Ok,
        request.source_ids,
        BoundedRejections::empty(),
    )
    .expect("ack");
    coordinator
        .handle(CoordinatorEvent::SubscribeAckReceived(ack))
        .expect("accept ack");
}

fn subscribe_to_local(
    coordinator: &mut MemberSessionCoordinator,
    peer: u128,
    direct: DirectSessionId,
    revision: u64,
) {
    let mut sources = [
        source_id(LOCAL, SourceKind::SystemAudio),
        source_id(LOCAL, SourceKind::Microphone),
    ];
    sources.sort_unstable();
    let request = SubscribeSet::new(
        DirectCommon {
            room_id: room_id(),
            sender_member_id: member_id(peer),
            sender_run_id: run_id(peer),
            receiver_member_id: member_id(LOCAL),
            receiver_run_id: run_id(LOCAL),
            direct_session_id: direct,
        },
        SubscriptionRevision::new(revision),
        coordinator.catalog_revision(),
        BoundedSourceIds::new(&sources).expect("source set"),
    )
    .expect("subscription");
    coordinator
        .handle(CoordinatorEvent::SubscribeSetReceived(request))
        .expect("accept subscription");
}

fn ready_coordinator() -> MemberSessionCoordinator {
    let mut coordinator = MemberSessionCoordinator::new(
        identity(),
        run_id(LOCAL),
        initial_config(),
        CatalogRevision::new(1),
    )
    .expect("coordinator");
    coordinator
        .handle(CoordinatorEvent::EngineStarted)
        .expect("start engine");
    coordinator
        .handle(CoordinatorEvent::RegistryConnectRequested)
        .expect("connect request");
    coordinator
        .handle(CoordinatorEvent::RegistrySessionReady {
            registry_session_id: registry_session(REGISTRY_SESSION),
        })
        .expect("registry ready");
    coordinator
        .handle(CoordinatorEvent::AuthoritySnapshotReceived(
            authority_snapshot(
                GENERATION,
                1,
                1,
                0,
                false,
                &[PEER_A, PEER_B],
                &[PEER_A, PEER_B],
            ),
        ))
        .expect("room snapshot");
    coordinator
        .handle(CoordinatorEvent::SourceStreamStarted(local_stream(
            SourceKind::SystemAudio,
            1,
        )))
        .expect("system stream");
    coordinator
        .handle(CoordinatorEvent::SourceStreamStarted(local_stream(
            SourceKind::Microphone,
            2,
        )))
        .expect("microphone stream");

    for peer in [PEER_A, PEER_B] {
        let direct = direct_session(peer);
        let effects = coordinator
            .handle(CoordinatorEvent::DirectSessionReady {
                member_id: member_id(peer),
                run_id: run_id(peer),
                direct_session_id: direct,
            })
            .expect("direct ready");
        acknowledge_outbound(&mut coordinator, extract_subscribe_set(&effects));
        subscribe_to_local(&mut coordinator, peer, direct, 1);
    }
    coordinator
}

fn persist_transaction(effects: &[CoordinatorEffect]) -> ConfigTransactionId {
    effects
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::PersistMemberConfig { transaction_id, .. } => Some(*transaction_id),
            _ => None,
        })
        .expect("persistence effect")
}

fn assert_same_member_result(
    expected: &rlb_registry::ClientMessage,
    actual: &rlb_registry::ClientMessage,
) {
    let rlb_registry::ClientMessage::MemberCommandResult {
        request_id: expected_request,
        capability: expected_capability,
        status: expected_status,
        revisions: expected_revisions,
        code: expected_code,
        effective: expected_effective,
        warnings: expected_warnings,
        ..
    } = expected
    else {
        panic!("expected member command result");
    };
    let rlb_registry::ClientMessage::MemberCommandResult {
        request_id: actual_request,
        capability: actual_capability,
        status: actual_status,
        revisions: actual_revisions,
        code: actual_code,
        effective: actual_effective,
        warnings: actual_warnings,
        ..
    } = actual
    else {
        panic!("actual member command result");
    };
    assert_eq!(actual_request, expected_request);
    assert_eq!(actual_capability, expected_capability);
    assert_eq!(actual_status, expected_status);
    assert_eq!(actual_revisions, expected_revisions);
    assert_eq!(actual_code, expected_code);
    assert_eq!(actual_effective, expected_effective);
    assert_eq!(actual_warnings, expected_warnings);
}

fn commit_local(
    coordinator: &mut MemberSessionCoordinator,
    request: LocalConfigRequest,
) -> Vec<CoordinatorEffect> {
    let effects = coordinator
        .handle(CoordinatorEvent::LocalConfigRequested(request))
        .expect("local reduction");
    let transaction_id = persist_transaction(&effects);
    coordinator
        .handle(CoordinatorEvent::ConfigPersistenceCompleted {
            transaction_id,
            result: PersistenceResult::Committed,
        })
        .expect("persist completion")
}

fn publication_delivery(
    request: u128,
    member_config_revision: MemberConfigRevision,
    catalog_revision: CatalogRevision,
    grant_revision: GrantRevision,
    publish_requested: bool,
) -> MemberCommandDelivery {
    let message = ServerMessage::MemberCommand {
        v: Version2,
        request_id: request_id(request),
        registry_session_id: registry_session(REGISTRY_SESSION),
        capability: Capability::SourcePublication,
        command: RoomCommandName::MemberSourcePublicationSet,
        scope: RoomCommandScope {
            room_id: Field::Present(room_id()),
            member_id: Field::Present(member_id(LOCAL)),
            source_id: Field::Present(source_id(LOCAL, SourceKind::SystemAudio)),
            ..RoomCommandScope::default()
        },
        expected: RoomCommandExpected {
            room_revision: Field::Present(RoomRevision::new(1)),
            member_config_revision: Field::Present(member_config_revision),
            catalog_revision: Field::Present(catalog_revision),
            grant_revision: Field::Present(grant_revision),
        },
        args: RoomCommandArgs {
            publish_requested: Field::Present(publish_requested),
            ..RoomCommandArgs::default()
        },
        delivery_ttl_ms: 5_000,
    };
    MemberCommandDelivery::from_server_message(message, 100).expect("wire delivery")
}

#[test]
fn media_receive_plan_requires_current_subscription_ack_and_catalog_epoch() {
    let mut coordinator = ready_coordinator();
    let peer_source = source_id(PEER_A, SourceKind::SystemAudio);
    let initial = coordinator.media_receive_plan();
    assert_eq!(initial.room_id, Some(room_id()));
    assert!(
        initial
            .sources
            .iter()
            .filter(|source| source.member_id == member_id(PEER_A))
            .all(|source| source.subscription_accepted)
    );

    coordinator
        .handle(CoordinatorEvent::DirectSessionLost {
            member_id: member_id(PEER_A),
            direct_session_id: direct_session(PEER_A),
        })
        .expect("direct session lost");
    let effects = coordinator
        .handle(CoordinatorEvent::DirectSessionReady {
            member_id: member_id(PEER_A),
            run_id: run_id(PEER_A),
            direct_session_id: direct_session(PEER_A + 100),
        })
        .expect("replacement direct session");
    let pending = extract_subscribe_set(&effects);
    assert!(
        coordinator
            .media_receive_plan()
            .sources
            .iter()
            .filter(|source| source.member_id == member_id(PEER_A))
            .all(|source| !source.subscription_accepted)
    );

    acknowledge_outbound(&mut coordinator, pending);
    let acknowledged = coordinator
        .media_receive_plan()
        .sources
        .into_iter()
        .find(|source| source.source_id == peer_source)
        .expect("acknowledged source projection");
    assert!(acknowledged.subscription_accepted);
    assert_eq!(
        acknowledged.subscription_revision,
        Some(pending.subscription_revision)
    );
    assert_eq!(acknowledged.catalog_revision, pending.catalog_revision);

    let next_epoch = epoch(9_999);
    let replacement = SourceDescriptor::new(
        peer_source,
        SourceKind::SystemAudio,
        "System audio",
        SourceState::new(
            true,
            true,
            true,
            QualityProfile::PcmF32V1,
            QualityProfile::PcmF32V1,
            true,
        )
        .expect("source state"),
        Some((
            SourceFormat::for_source(SourceKind::SystemAudio, QualityProfile::PcmF32V1),
            next_epoch,
        )),
    )
    .expect("replacement descriptor");
    let effects = coordinator
        .handle(CoordinatorEvent::AuthorityEventReceived(room_event(
            1,
            1,
            2,
            RoomEventValue::CatalogReplaced {
                member_id: member_id(PEER_A),
                catalog_revision: CatalogRevision::new(2),
                sources: vec![
                    replacement,
                    descriptor(PEER_A, SourceKind::Microphone, true),
                ],
            },
        )))
        .expect("catalog replacement");
    let next_request = extract_subscribe_set(&effects);
    let before_ack = coordinator
        .media_receive_plan()
        .sources
        .into_iter()
        .find(|source| source.source_id == peer_source)
        .expect("replacement source projection");
    assert_eq!(before_ack.active_epoch, next_epoch);
    assert_eq!(before_ack.catalog_revision, CatalogRevision::new(2));
    assert!(!before_ack.subscription_accepted);

    acknowledge_outbound(&mut coordinator, next_request);
    let after_ack = coordinator
        .media_receive_plan()
        .sources
        .into_iter()
        .find(|source| source.source_id == peer_source)
        .expect("current source projection");
    assert_eq!(after_ack.active_epoch, next_epoch);
    assert!(after_ack.subscription_accepted);
    assert_eq!(
        after_ack.subscription_revision,
        Some(next_request.subscription_revision)
    );
}

#[test]
fn three_members_two_sources_enforce_visibility_and_room_mute() {
    let mut coordinator = ready_coordinator();
    let initial = coordinator.effective_send_eligibility();
    assert_eq!(initial.len(), 4);
    assert!(initial.iter().all(|edge| edge.media_emitter_eligible));

    coordinator
        .handle(CoordinatorEvent::AuthorityEventReceived(room_event(
            1,
            2,
            1,
            RoomEventValue::SourceVisibilityChanged {
                publisher_member_id: member_id(LOCAL),
                source_id: source_id(LOCAL, SourceKind::SystemAudio),
                viewer_member_ids: vec![member_id(PEER_A)],
            },
        )))
        .expect("visibility event");
    let hidden = coordinator.effective_send_eligibility();
    let hidden_system = hidden
        .iter()
        .find(|edge| {
            edge.source_id == source_id(LOCAL, SourceKind::SystemAudio)
                && edge.recipient_member_id == member_id(PEER_B)
        })
        .expect("hidden edge");
    assert!(!hidden_system.media_emitter_eligible);
    assert!(
        hidden_system
            .blocking_reasons
            .contains(&EffectiveSendBlock::SourceHidden)
    );
    let still_audible_microphone = hidden
        .iter()
        .find(|edge| {
            edge.source_id == source_id(LOCAL, SourceKind::Microphone)
                && edge.recipient_member_id == member_id(PEER_B)
        })
        .expect("microphone edge");
    assert!(still_audible_microphone.media_emitter_eligible);

    let epochs_before: Vec<_> = coordinator
        .diagnostics_snapshot()
        .sources
        .iter()
        .map(|source| source.stream_epoch)
        .collect();
    coordinator
        .handle(CoordinatorEvent::AuthorityEventReceived(room_event(
            2,
            3,
            1,
            RoomEventValue::RoomSendMutedChanged {
                room_send_muted: true,
            },
        )))
        .expect("room mute");
    let muted = coordinator.effective_send_eligibility();
    assert!(muted.iter().all(|edge| {
        !edge.media_emitter_eligible
            && edge
                .blocking_reasons
                .contains(&EffectiveSendBlock::RoomSendMuted)
    }));
    let epochs_after: Vec<_> = coordinator
        .diagnostics_snapshot()
        .sources
        .iter()
        .map(|source| source.stream_epoch)
        .collect();
    assert_eq!(epochs_before, epochs_after);
}

#[test]
fn registry_outage_and_reconnect_preserve_healthy_direct_state() {
    let mut coordinator = ready_coordinator();
    let before = coordinator.diagnostics_snapshot();
    assert!(
        before
            .peers
            .iter()
            .all(|peer| matches!(peer.connection, PeerConnectionState::Ready { .. }))
    );
    assert!(
        coordinator
            .effective_send_eligibility()
            .iter()
            .all(|edge| edge.media_emitter_eligible)
    );

    let effects = coordinator
        .handle(CoordinatorEvent::RegistryDisconnected)
        .expect("registry outage");
    assert_eq!(coordinator.registry_state(), RegistryState::Outage);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::ScheduleRegistryReconnect {
            base_delay_ms: 1_000
        }
    )));
    let outage = coordinator.diagnostics_snapshot();
    assert_eq!(outage.peers, before.peers);
    assert!(
        coordinator
            .effective_send_eligibility()
            .iter()
            .all(|edge| edge.media_emitter_eligible)
    );

    coordinator
        .handle(CoordinatorEvent::ReconnectTimerElapsed)
        .expect("reconnect timer");
    coordinator
        .handle(CoordinatorEvent::RegistrySessionReady {
            registry_session_id: registry_session(REGISTRY_SESSION + 1),
        })
        .expect("new registry session");
    coordinator
        .handle(CoordinatorEvent::AuthoritySnapshotReceived(
            authority_snapshot(
                GENERATION + 1,
                1,
                1,
                0,
                false,
                &[PEER_A, PEER_B],
                &[PEER_A, PEER_B],
            ),
        ))
        .expect("reconnect snapshot");
    let reconnected = coordinator.diagnostics_snapshot();
    assert_eq!(reconnected.registry_state, RegistryState::Online);
    assert_eq!(reconnected.registry_reconnects_total, 1);
    assert!(
        reconnected
            .peers
            .iter()
            .all(|peer| matches!(peer.connection, PeerConnectionState::Ready { .. }))
    );
}

#[test]
fn delegated_command_persists_then_revoke_and_offline_drop_are_immediate() {
    let mut coordinator = ready_coordinator();
    let grant_effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(10),
            expected_member_config_revision: MemberConfigRevision::new(1),
            expected_grant_revision: Some(GrantRevision::new(1)),
            mutation: LocalConfigMutation::SetCapabilityGrant {
                room_id: room_id(),
                capability: Capability::SourcePublication,
                enabled: true,
            },
        },
    );
    assert!(grant_effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::LocalConfigCompleted(LocalConfigOutcome {
            status: CommandStatus::Applied,
            ..
        })
    )));
    assert_eq!(
        coordinator.config().room.grant_revision,
        GrantRevision::new(2)
    );

    let delivery = publication_delivery(
        11,
        coordinator.config().member_config_revision,
        coordinator.catalog_revision(),
        coordinator.config().room.grant_revision,
        false,
    );
    let replay_delivery = delivery.clone();
    let effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery,
            now_ms: 200,
        })
        .expect("delegated reduction");
    assert!(coordinator.has_pending_persistence());
    let persisted_candidate = effects
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::PersistMemberConfig { candidate, .. } => Some(candidate),
            _ => None,
        })
        .expect("delegated persistent candidate");
    assert!(
        !persisted_candidate
            .source(source_id(LOCAL, SourceKind::SystemAudio))
            .expect("candidate source")
            .config
            .publish_requested
    );
    assert_eq!(
        persisted_candidate.room.member_command_cache.records.len(),
        1
    );
    assert_eq!(
        persisted_candidate
            .room
            .member_command_cache
            .policy
            .capacity,
        256
    );
    assert!(
        persisted_candidate.room.member_command_cache.records[0]
            .response
            .is_some()
    );
    let transaction_id = persist_transaction(&effects);
    let effects = coordinator
        .handle(CoordinatorEvent::ConfigPersistenceCompleted {
            transaction_id,
            result: PersistenceResult::Committed,
        })
        .expect("delegated persist");
    let committed_config = coordinator.config().clone();
    let committed_catalog_revision = coordinator.catalog_revision();
    let committed_member_revision = coordinator.config().member_config_revision;
    assert!(
        !coordinator
            .config()
            .source(source_id(LOCAL, SourceKind::SystemAudio))
            .expect("system source")
            .config
            .publish_requested
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::SendRegistryMessage(rlb_registry::ClientMessage::MemberCommandResult {
            status: rlb_registry::MemberCommandStatus::Applied,
            ..
        })
    )));
    let original_result = effects
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::SendRegistryMessage(message) => Some(message.clone()),
            _ => None,
        })
        .expect("original delegated result");

    let replay_effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: replay_delivery.clone(),
            now_ms: 250,
        })
        .expect("same request replays");
    assert!(
        !replay_effects
            .iter()
            .any(|effect| matches!(effect, CoordinatorEffect::PersistMemberConfig { .. }))
    );
    assert_eq!(
        coordinator.config().member_config_revision,
        committed_member_revision
    );
    assert_eq!(
        coordinator.config().room.member_command_cache.records.len(),
        1
    );
    let replayed_result = replay_effects
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::SendRegistryMessage(message) => Some(message),
            _ => None,
        })
        .expect("replayed delegated result");
    assert_same_member_result(&original_result, replayed_result);

    let persisted_delivery = replay_delivery.clone();
    let mut conflict = replay_delivery;
    conflict.command.mutation = rlb_control::DelegatedMutation::SetSourcePublication {
        source_id: source_id(LOCAL, SourceKind::SystemAudio),
        publish_requested: true,
    };
    let conflict_effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: conflict,
            now_ms: 251,
        })
        .expect("hash conflict is a terminal rejection");
    assert!(
        !conflict_effects
            .iter()
            .any(|effect| matches!(effect, CoordinatorEffect::PersistMemberConfig { .. }))
    );
    assert!(conflict_effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::SendRegistryMessage(rlb_registry::ClientMessage::MemberCommandResult {
            status: rlb_registry::MemberCommandStatus::Rejected,
            code: Some(ErrorCode::RequestIdConflict),
            ..
        })
    )));

    let restarted_session = registry_session(REGISTRY_SESSION + 1);
    let mut restarted = MemberSessionCoordinator::new(
        identity(),
        run_id(LOCAL + 100),
        committed_config,
        committed_catalog_revision,
    )
    .expect("restart from persisted config");
    restarted
        .handle(CoordinatorEvent::EngineStarted)
        .expect("restart engine");
    restarted
        .handle(CoordinatorEvent::RegistryConnectRequested)
        .expect("restart connect");
    restarted
        .handle(CoordinatorEvent::RegistrySessionReady {
            registry_session_id: restarted_session,
        })
        .expect("restart registry session");
    restarted
        .handle(CoordinatorEvent::AuthoritySnapshotReceived(
            authority_snapshot(
                GENERATION + 1,
                1,
                1,
                0,
                false,
                &[PEER_A, PEER_B],
                &[PEER_A, PEER_B],
            ),
        ))
        .expect("restart snapshot");
    let mut restarted_delivery = persisted_delivery;
    restarted_delivery.registry_session_id = restarted_session;
    let restart_replay = restarted
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: restarted_delivery,
            now_ms: 300,
        })
        .expect("restart replay");
    assert!(
        !restart_replay
            .iter()
            .any(|effect| matches!(effect, CoordinatorEffect::PersistMemberConfig { .. }))
    );
    let restart_result = restart_replay
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::SendRegistryMessage(message) => Some(message),
            _ => None,
        })
        .expect("restart cached result");
    assert_same_member_result(&original_result, restart_result);

    let member_revision = coordinator.config().member_config_revision;
    let grant_revision = coordinator.config().room.grant_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(12),
            expected_member_config_revision: member_revision,
            expected_grant_revision: Some(grant_revision),
            mutation: LocalConfigMutation::SetCapabilityGrant {
                room_id: room_id(),
                capability: Capability::SourcePublication,
                enabled: false,
            },
        },
    );
    let revoked = publication_delivery(
        13,
        coordinator.config().member_config_revision,
        coordinator.catalog_revision(),
        coordinator.config().room.grant_revision,
        true,
    );
    let effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: revoked.clone(),
            now_ms: 300,
        })
        .expect("revoked command");
    assert!(coordinator.has_pending_persistence());
    let rejected_transaction = persist_transaction(&effects);
    let effects = coordinator
        .handle(CoordinatorEvent::ConfigPersistenceCompleted {
            transaction_id: rejected_transaction,
            result: PersistenceResult::Committed,
        })
        .expect("persist rejected idempotency result");
    assert!(!coordinator.has_pending_persistence());
    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::SendRegistryMessage(rlb_registry::ClientMessage::MemberCommandResult {
            status: rlb_registry::MemberCommandStatus::Rejected,
            code: Some(ErrorCode::CapabilityDenied),
            ..
        })
    )));

    coordinator
        .handle(CoordinatorEvent::RegistryDisconnected)
        .expect("disconnect");
    let offline_effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: revoked,
            now_ms: 400,
        })
        .expect("offline delivery is dropped");
    assert!(offline_effects.is_empty());
    assert!(!coordinator.has_pending_persistence());
    assert_eq!(
        coordinator
            .diagnostics_snapshot()
            .commands_dropped_without_session_total,
        1
    );
}

#[test]
fn publication_policy_does_not_change_epoch_while_method_and_format_do() {
    let mut coordinator = ready_coordinator();
    let system_id = source_id(LOCAL, SourceKind::SystemAudio);
    let first_epoch = coordinator
        .diagnostics_snapshot()
        .sources
        .iter()
        .find(|source| source.source_id == system_id)
        .expect("system diagnostics")
        .stream_epoch;

    let member_revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(20),
            expected_member_config_revision: member_revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourcePublication {
                room_id: room_id(),
                source_id: system_id,
                publish_requested: false,
            },
        },
    );
    assert_eq!(
        coordinator
            .diagnostics_snapshot()
            .sources
            .iter()
            .find(|source| source.source_id == system_id)
            .expect("system diagnostics")
            .stream_epoch,
        first_epoch
    );

    let member_revision = coordinator.config().member_config_revision;
    let effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(21),
            expected_member_config_revision: member_revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetCaptureMethod {
                source_id: system_id,
                method: CaptureMethod::InstalledCableCapture,
            },
        },
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
            source_id,
            reason: EpochChangeReason::CaptureMethodChanged,
        }) if *source_id == system_id
    )));

    let reused = SourceStreamStarted {
        source_id: system_id,
        stream_epoch: first_epoch.expect("first epoch"),
        endpoint_id: "installed-cable".to_owned(),
        capture_method: Some(CaptureMethod::InstalledCableCapture),
        format: SourceFormat::for_source(SourceKind::SystemAudio, QualityProfile::PcmF32V1),
        reason: EpochChangeReason::CaptureMethodChanged,
    };
    assert_eq!(
        coordinator
            .handle(CoordinatorEvent::SourceStreamStarted(reused))
            .expect_err("epoch reuse rejected")
            .code,
        ErrorCode::MediaEpochConflict
    );
    coordinator
        .handle(CoordinatorEvent::SourceStreamStarted(SourceStreamStarted {
            source_id: system_id,
            stream_epoch: epoch(30),
            endpoint_id: "installed-cable".to_owned(),
            capture_method: Some(CaptureMethod::InstalledCableCapture),
            format: SourceFormat::for_source(SourceKind::SystemAudio, QualityProfile::PcmF32V1),
            reason: EpochChangeReason::CaptureMethodChanged,
        }))
        .expect("new method epoch");

    let member_revision = coordinator.config().member_config_revision;
    let effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(22),
            expected_member_config_revision: member_revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSendQuality {
                room_id: room_id(),
                source_id: system_id,
                selected_profile: QualityProfile::PcmS16V1,
            },
        },
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
            source_id,
            reason: EpochChangeReason::FormatChanged,
        }) if *source_id == system_id
    )));
    coordinator
        .handle(CoordinatorEvent::SourceStreamStarted(SourceStreamStarted {
            source_id: system_id,
            stream_epoch: epoch(31),
            endpoint_id: "installed-cable".to_owned(),
            capture_method: Some(CaptureMethod::InstalledCableCapture),
            format: SourceFormat::for_source(SourceKind::SystemAudio, QualityProfile::PcmS16V1),
            reason: EpochChangeReason::FormatChanged,
        }))
        .expect("new format epoch");
    assert_eq!(
        coordinator
            .diagnostics_snapshot()
            .sources
            .iter()
            .find(|source| source.source_id == system_id)
            .expect("system diagnostics")
            .stream_epoch,
        Some(epoch(31))
    );
}

#[test]
fn persisted_mix_is_emitted_as_rlb_audio_render_configuration() {
    let mut coordinator = ready_coordinator();
    let desired = MemberMix {
        gain_db: GainDb::new(-6.0).expect("gain"),
        mute: true,
        solo: false,
    };
    let member_revision = coordinator.config().member_config_revision;
    let effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(40),
            expected_member_config_revision: member_revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetMemberMix {
                room_id: room_id(),
                remote_member_id: member_id(PEER_A),
                mix: desired,
            },
        },
    );
    let plan = effects
        .iter()
        .find_map(|effect| match effect {
            CoordinatorEffect::ApplyAudioMix(plan) => Some(plan),
            _ => None,
        })
        .expect("audio mix plan");
    let peer_sources: Vec<_> = plan
        .sources
        .iter()
        .filter(|source| source.member_id == member_id(PEER_A))
        .collect();
    assert_eq!(peer_sources.len(), 2);
    assert!(
        peer_sources
            .iter()
            .all(|source| source.member_mix == desired)
    );
}

#[test]
fn engine_and_shutdown_state_machine_rejects_skipped_edges() {
    let mut coordinator = MemberSessionCoordinator::new(
        identity(),
        run_id(LOCAL),
        MemberConfig::defaults(identity(), room_id()),
        CatalogRevision::new(1),
    )
    .expect("coordinator");
    assert_eq!(
        coordinator
            .handle(CoordinatorEvent::RegistryConnectRequested)
            .expect_err("cannot connect before start")
            .code,
        ErrorCode::InvalidStateTransition
    );
    coordinator
        .handle(CoordinatorEvent::EngineStarted)
        .expect("engine start");
    coordinator
        .handle(CoordinatorEvent::ShutdownRequested)
        .expect("shutdown request");
    assert_eq!(
        coordinator
            .handle(CoordinatorEvent::ShutdownStageCompleted(
                ShutdownStage::StopMedia,
            ))
            .expect_err("cannot skip shutdown stages")
            .code,
        ErrorCode::InvalidStateTransition
    );
    let mut stage = ShutdownStage::CloseSendGate;
    loop {
        coordinator
            .handle(CoordinatorEvent::ShutdownStageCompleted(stage))
            .expect("ordered shutdown stage");
        let Some(next) = stage.next() else {
            break;
        };
        stage = next;
    }
    assert_eq!(coordinator.engine_state(), EngineState::Stopped);
}

#[test]
fn source_binding_persists_and_microphone_does_not_change_capture_method() {
    let mut coordinator = ready_coordinator();
    let system_id = source_id(LOCAL, SourceKind::SystemAudio);
    let mic_id = source_id(LOCAL, SourceKind::Microphone);
    let member_revision = coordinator.config().member_config_revision;
    let mic_effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(50),
            expected_member_config_revision: member_revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourceBinding {
                source_id: mic_id,
                capture_method: CaptureMethod::InstalledCableCapture,
                binding: DeviceBindingConfig::from_choice(
                    EndpointMode::Pinned,
                    Some("mic-endpoint".to_owned()),
                ),
            },
        },
    );
    assert_eq!(
        coordinator.config().system_capture_method,
        CaptureMethod::WasapiLoopback
    );
    assert_eq!(
        coordinator
            .config()
            .microphone_binding
            .endpoint_id
            .as_deref(),
        Some("mic-endpoint")
    );
    assert!(mic_effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
            source_id,
            reason: EpochChangeReason::EndpointChanged,
        }) if *source_id == mic_id
    )));

    let member_revision = coordinator.config().member_config_revision;
    let effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(51),
            expected_member_config_revision: member_revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourceBinding {
                source_id: system_id,
                capture_method: CaptureMethod::InstalledCableCapture,
                binding: DeviceBindingConfig::from_choice(
                    EndpointMode::Pinned,
                    Some("sys-endpoint".to_owned()),
                ),
            },
        },
    );
    assert_eq!(
        coordinator.config().system_capture_method,
        CaptureMethod::InstalledCableCapture
    );
    assert_eq!(
        coordinator
            .config()
            .system_audio_binding
            .endpoint_id
            .as_deref(),
        Some("sys-endpoint")
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::StartOrRestartSource(SourceRestartPlan {
            source_id,
            reason: EpochChangeReason::CaptureMethodChanged,
        }) if *source_id == system_id
    )));
}

#[test]
fn unjoined_config_has_no_durable_room_id() {
    let config = MemberConfig::unjoined(identity());
    config.validate(identity()).expect("valid unjoined");
    assert_eq!(config.attached_room_id(), None);
    assert!(config.join_target().is_none());
}

#[test]
fn join_persists_target_and_connects_registry() {
    let mut coordinator = MemberSessionCoordinator::new(
        identity(),
        run_id(LOCAL),
        MemberConfig::unjoined(identity()),
        CatalogRevision::new(1),
    )
    .expect("coordinator");
    coordinator
        .handle(CoordinatorEvent::EngineStarted)
        .expect("start");
    let target = crate::JoinTarget {
        authority_url: "http://192.0.2.1/gw-ctrl".to_owned(),
        member_registry_endpoint: Endpoint::new(Ipv4Addr::new(10, 23, 4, 7), 17_099)
            .expect("endpoint"),
    };
    let revision = coordinator.config().member_config_revision;
    let effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(80),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetJoinTarget {
                room_id: room_id(),
                target: target.clone(),
            },
        },
    );
    assert_eq!(coordinator.config().attached_room_id(), Some(room_id()));
    assert_eq!(coordinator.config().join_target(), Some(&target));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, CoordinatorEffect::ConnectRegistry))
    );
}

#[test]
fn join_target_rejects_non_private_or_ambiguous_endpoints() {
    let valid = crate::JoinTarget {
        authority_url: "https://203.0.113.1/gw-ctrl".to_owned(),
        member_registry_endpoint: Endpoint::new(Ipv4Addr::new(192, 168, 90, 8), 17_099)
            .expect("registry endpoint"),
    };
    let mut config = MemberConfig::defaults(identity(), room_id());
    config.room.join = Some(valid);
    config.validate(identity()).expect("distinct private hosts");

    let invalid_cases = [
        (
            "https://8.8.8.8/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "public authority",
        ),
        (
            "https://198.51.100.1/gw-ctrl",
            Ipv4Addr::new(8, 8, 8, 8),
            17_099,
            "public registry",
        ),
        (
            "http://127.0.0.1/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "loopback authority",
        ),
        (
            "http://198.51.100.1/gw-ctrl",
            Ipv4Addr::new(127, 0, 0, 1),
            17_099,
            "loopback registry",
        ),
        (
            "http://router.lan/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "authority hostname",
        ),
        (
            "ftp://198.51.100.1/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "authority scheme",
        ),
        (
            "http://operator:secret@198.51.100.1/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "authority credentials",
        ),
        (
            "http://@198.51.100.1/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "empty authority user information",
        ),
        (
            "http://198.51.100.1/gw-ctrl?room=family",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "authority query",
        ),
        (
            "http://198.51.100.1/gw-ctrl#settings",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "authority fragment",
        ),
        (
            "198.51.100.1/gw-ctrl",
            Ipv4Addr::new(192, 168, 90, 8),
            17_099,
            "unparsed authority",
        ),
        (
            "http://198.51.100.1/gw-ctrl",
            Ipv4Addr::new(10, 23, 4, 7),
            17_100,
            "registry port",
        ),
    ];

    for (authority_url, registry_address, registry_port, case) in invalid_cases {
        let mut config = MemberConfig::defaults(identity(), room_id());
        config.room.join = Some(crate::JoinTarget {
            authority_url: authority_url.to_owned(),
            member_registry_endpoint: Endpoint::new(registry_address, registry_port)
                .expect("candidate endpoint"),
        });
        assert_eq!(
            config.validate(identity()),
            Err(MemberConfigError::JoinTarget),
            "{case}"
        );
    }
}

#[test]
fn clear_join_persists_detached_room_settings_residue() {
    let mut config = MemberConfig::unjoined(identity());
    config.room.grant_revision = GrantRevision::new(7);
    config.room.settings.auto_reconnect = false;
    config.room.settings.grants.receive_mix = true;
    config.room.settings.receive_buffer = ReceiveBufferConfig::Manual {
        target_frames: ManualBufferTarget::new(1_440).expect("manual buffer"),
    };
    let mut coordinator =
        MemberSessionCoordinator::new(identity(), run_id(LOCAL), config, CatalogRevision::new(1))
            .expect("coordinator");
    coordinator
        .handle(CoordinatorEvent::EngineStarted)
        .expect("start");

    let effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(84),
            expected_member_config_revision: MemberConfigRevision::new(1),
            expected_grant_revision: None,
            mutation: LocalConfigMutation::ClearJoin,
        },
    );

    assert!(effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::LocalConfigCompleted(LocalConfigOutcome {
            status: CommandStatus::Applied,
            ..
        })
    )));
    assert_eq!(
        coordinator.config().room.grant_revision,
        GrantRevision::new(8)
    );
    assert!(coordinator.config().room.settings.auto_reconnect);
    assert_eq!(
        coordinator.config().room.settings.receive_buffer,
        ReceiveBufferConfig::Automatic
    );
    let grants = coordinator.config().room.settings.grants;
    assert!(!grants.source_publication);
    assert!(!grants.send_quality);
    assert!(!grants.receive_mix);
    assert!(!grants.auto_reconnect);
}

#[test]
fn leave_then_rejoin_same_room_revokes_old_delegated_authority() {
    let mut coordinator = ready_coordinator();
    let system_source_id = source_id(LOCAL, SourceKind::SystemAudio);
    let microphone_source_id = source_id(LOCAL, SourceKind::Microphone);
    let join_target = crate::JoinTarget {
        authority_url: "https://192.0.2.1/gw-ctrl".to_owned(),
        member_registry_endpoint: Endpoint::new(Ipv4Addr::new(10, 44, 5, 9), 17_099)
            .expect("registry endpoint"),
    };

    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(90),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetJoinTarget {
                room_id: room_id(),
                target: join_target.clone(),
            },
        },
    );

    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(91),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourceBinding {
                source_id: system_source_id,
                capture_method: CaptureMethod::InstalledCableCapture,
                binding: DeviceBindingConfig::from_choice(
                    EndpointMode::Pinned,
                    Some("system-device".to_owned()),
                ),
            },
        },
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(92),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourceBinding {
                source_id: microphone_source_id,
                capture_method: CaptureMethod::WasapiLoopback,
                binding: DeviceBindingConfig::from_choice(
                    EndpointMode::Pinned,
                    Some("microphone-device".to_owned()),
                ),
            },
        },
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(93),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetPlaybackBinding {
                binding: DeviceBindingConfig::from_choice(
                    EndpointMode::Pinned,
                    Some("playback-device".to_owned()),
                ),
            },
        },
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(94),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetEmergencyStop { enabled: true },
        },
    );

    for (offset, capability) in [
        Capability::SourcePublication,
        Capability::SendQuality,
        Capability::ReceiveMix,
        Capability::AutoReconnect,
    ]
    .into_iter()
    .enumerate()
    {
        let member_revision = coordinator.config().member_config_revision;
        let grant_revision = coordinator.config().room.grant_revision;
        commit_local(
            &mut coordinator,
            LocalConfigRequest {
                request_id: request_id(100 + offset as u128),
                expected_member_config_revision: member_revision,
                expected_grant_revision: Some(grant_revision),
                mutation: LocalConfigMutation::SetCapabilityGrant {
                    room_id: room_id(),
                    capability,
                    enabled: true,
                },
            },
        );
    }

    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(104),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetReceiveBuffer {
                room_id: room_id(),
                buffer: ReceiveBufferConfig::Manual {
                    target_frames: ManualBufferTarget::new(2_400).expect("manual buffer"),
                },
            },
        },
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(105),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetAutoReconnect {
                room_id: room_id(),
                enabled: false,
            },
        },
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(106),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetMemberMix {
                room_id: room_id(),
                remote_member_id: member_id(PEER_A),
                mix: MemberMix {
                    mute: true,
                    ..MemberMix::default()
                },
            },
        },
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(107),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourceMix {
                room_id: room_id(),
                remote_member_id: member_id(PEER_A),
                source_id: source_id(PEER_A, SourceKind::Microphone),
                mix: SourceMix {
                    mute: true,
                    ..SourceMix::default()
                },
            },
        },
    );

    let old_delivery = publication_delivery(
        108,
        coordinator.config().member_config_revision,
        coordinator.catalog_revision(),
        coordinator.config().room.grant_revision,
        false,
    );
    let delegated_effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: old_delivery.clone(),
            now_ms: 200,
        })
        .expect("delegated publication");
    let delegated_transaction = persist_transaction(&delegated_effects);
    coordinator
        .handle(CoordinatorEvent::ConfigPersistenceCompleted {
            transaction_id: delegated_transaction,
            result: PersistenceResult::Committed,
        })
        .expect("delegated persistence");
    assert_eq!(
        coordinator.config().room.member_command_cache.records.len(),
        1
    );

    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(109),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourcePublication {
                room_id: room_id(),
                source_id: system_source_id,
                publish_requested: true,
            },
        },
    );

    let old_grant_revision = coordinator.config().room.grant_revision;
    let old_catalog_revision = coordinator.catalog_revision();
    let system_binding = coordinator.config().system_audio_binding.clone();
    let microphone_binding = coordinator.config().microphone_binding.clone();
    let playback_binding = coordinator.config().playback_binding.clone();
    let revision = coordinator.config().member_config_revision;
    let leave_effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(110),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::ClearJoin,
        },
    );

    assert!(
        leave_effects
            .iter()
            .any(|effect| matches!(effect, CoordinatorEffect::LeaveRegistry))
    );
    assert_eq!(coordinator.registry_state(), RegistryState::Left);
    assert_eq!(coordinator.config().attached_room_id(), None);
    assert!(coordinator.config().join_target().is_none());
    assert_eq!(
        coordinator.config().room.grant_revision,
        old_grant_revision
            .checked_next()
            .expect("next grant revision")
    );
    assert_eq!(
        coordinator.catalog_revision(),
        old_catalog_revision
            .checked_next()
            .expect("next catalog revision")
    );
    let grants = coordinator.config().room.settings.grants;
    assert!(!grants.source_publication);
    assert!(!grants.send_quality);
    assert!(!grants.receive_mix);
    assert!(!grants.auto_reconnect);
    assert!(coordinator.config().room.settings.auto_reconnect);
    assert_eq!(
        coordinator.config().room.settings.receive_buffer,
        ReceiveBufferConfig::Automatic
    );
    assert!(coordinator.config().room.member_mix.is_empty());
    assert!(coordinator.config().room.source_mix.is_empty());
    assert!(
        coordinator
            .config()
            .room
            .member_command_cache
            .records
            .is_empty()
    );
    assert!(coordinator.config().sources.iter().all(|source| {
        source.config.master_capture_enabled && !source.config.publish_requested
    }));
    assert!(coordinator.config().local_emergency_stop);
    assert_eq!(
        coordinator.config().system_capture_method,
        CaptureMethod::InstalledCableCapture
    );
    assert_eq!(coordinator.config().system_audio_binding, system_binding);
    assert_eq!(coordinator.config().microphone_binding, microphone_binding);
    assert_eq!(coordinator.config().playback_binding, playback_binding);

    let detached_grant_revision = coordinator.config().room.grant_revision;
    let revision = coordinator.config().member_config_revision;
    let rejoin_effects = commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(111),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetJoinTarget {
                room_id: room_id(),
                target: join_target.clone(),
            },
        },
    );
    assert!(
        rejoin_effects
            .iter()
            .any(|effect| matches!(effect, CoordinatorEffect::ConnectRegistry))
    );
    assert_eq!(coordinator.config().join_target(), Some(&join_target));
    assert_eq!(
        coordinator.config().room.grant_revision,
        detached_grant_revision
    );
    let grants = coordinator.config().room.settings.grants;
    assert!(!grants.source_publication);
    assert!(!grants.send_quality);
    assert!(!grants.receive_mix);
    assert!(!grants.auto_reconnect);

    let rejoined_session = registry_session(REGISTRY_SESSION + 10);
    coordinator
        .handle(CoordinatorEvent::RegistrySessionReady {
            registry_session_id: rejoined_session,
        })
        .expect("rejoined registry session");
    coordinator
        .handle(CoordinatorEvent::AuthoritySnapshotReceived(
            authority_snapshot(
                GENERATION + 10,
                1,
                1,
                0,
                false,
                &[PEER_A, PEER_B],
                &[PEER_A, PEER_B],
            ),
        ))
        .expect("rejoined room snapshot");

    let mut stale_delivery = old_delivery;
    stale_delivery.registry_session_id = rejoined_session;
    let rejection_effects = coordinator
        .handle(CoordinatorEvent::MemberCommandReceived {
            delivery: stale_delivery,
            now_ms: 300,
        })
        .expect("old authority command");
    let rejection_transaction = persist_transaction(&rejection_effects);
    let rejection_effects = coordinator
        .handle(CoordinatorEvent::ConfigPersistenceCompleted {
            transaction_id: rejection_transaction,
            result: PersistenceResult::Committed,
        })
        .expect("persist rejection");
    assert!(rejection_effects.iter().any(|effect| matches!(
        effect,
        CoordinatorEffect::SendRegistryMessage(rlb_registry::ClientMessage::MemberCommandResult {
            status: rlb_registry::MemberCommandStatus::Rejected,
            code: Some(ErrorCode::CapabilityRevoked),
            ..
        })
    )));
    assert!(
        !coordinator
            .config()
            .source(system_source_id)
            .expect("system source")
            .config
            .publish_requested
    );
}

#[test]
fn monitor_startup_and_logging_mutations_persist() {
    let mut coordinator = ready_coordinator();
    let source_id = source_id(LOCAL, SourceKind::Microphone);
    let monitor = crate::SourceMonitorConfig {
        enabled: true,
        gain_db: GainDb::new(-6.0).expect("gain"),
    };
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(81),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetSourceMonitor { source_id, monitor },
        },
    );
    assert_eq!(
        coordinator
            .config()
            .source(source_id)
            .expect("source")
            .monitor,
        monitor
    );
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(82),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetStartup {
                startup: crate::StartupConfig {
                    start_with_os: true,
                    start_engine_on_ui_open: false,
                },
            },
        },
    );
    assert!(coordinator.config().startup.start_with_os);
    assert!(!coordinator.config().startup.start_engine_on_ui_open);
    let revision = coordinator.config().member_config_revision;
    commit_local(
        &mut coordinator,
        LocalConfigRequest {
            request_id: request_id(83),
            expected_member_config_revision: revision,
            expected_grant_revision: None,
            mutation: LocalConfigMutation::SetLogging {
                logging: crate::LoggingConfig {
                    enabled: true,
                    directory: Some(r"C:\logs\rlb".to_owned()),
                    max_bytes: 2_000_000,
                    retention_count: 4,
                },
            },
        },
    );
    assert_eq!(
        coordinator.config().logging.directory.as_deref(),
        Some(r"C:\logs\rlb")
    );
}
