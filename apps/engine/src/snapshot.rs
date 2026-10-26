//! Builds the closed A `LocalSnapshot` from coordinator and device state.

use rlb_audio_windows::SlotStatus;
use rlb_control::{
    CanonicalRevisions, DiagnosticsProjection, DriverProjection, EngineProjection, EngineState,
    LifecycleProjection, LocalRoomProjection, LocalSnapshot, LoggingProjection, MixerProjection,
    OperationProjection, PermissionProjection, PlaybackProjection, Version2,
};
use rlb_model::{IdentityRecord, RunId, StructuredError};
use rlb_session::{MemberConfig, MemberSessionCoordinator, RegistryState};

use crate::persist::{PersistStore, rfc3339_utc_now};

/// Optional live diagnostics folded into an A snapshot.
pub struct SnapshotLive {
    pub event_cursor: u64,
    pub engine_faulted: bool,
    pub open_errors: Vec<StructuredError>,
    pub dropped_ingress: u64,
    pub malformed_messages: u64,
    pub control_reconnects: u64,
    pub operations: Vec<OperationProjection>,
}

/// Assembles one complete A snapshot. `ipc_token` is never included.
pub fn build_local_snapshot(
    coordinator: &MemberSessionCoordinator,
    identity: IdentityRecord,
    run_id: RunId,
    _store: &PersistStore,
    render: &SlotStatus,
    live: SnapshotLive,
) -> LocalSnapshot {
    let config = coordinator.config();
    let revisions = canonical_revisions(coordinator, config);
    let mut errors = live.open_errors;
    let sources = match coordinator.source_catalog() {
        Ok(sources) => sources,
        Err(error) => {
            errors.push(StructuredError {
                code: error.code,
                entity: rlb_model::ErrorEntity::Engine {
                    member_id: identity.member_id(),
                    run_id,
                },
                impact: rlb_model::ErrorImpact::DiagnosticOnly,
                retryable: true,
                diagnostic_id: None,
            });
            Vec::new()
        }
    };
    let (effective_endpoint_id, playback_available) = match render {
        SlotStatus::Running { endpoint_id, .. } => (Some(endpoint_id.clone()), true),
        SlotStatus::Unavailable { endpoint_id } => (Some(endpoint_id.clone()), false),
    };
    let (room_id, permissions) = room_identity_and_permissions(config);
    let registry_connected = coordinator.registry_state() == RegistryState::Online;
    LocalSnapshot {
        schema: Version2,
        captured_at: rfc3339_utc_now(),
        member_id: identity.member_id(),
        run_id,
        engine: EngineProjection {
            state: if live.engine_faulted {
                EngineState::Faulted
            } else {
                map_engine_state(coordinator.engine_state())
            },
            build: env!("CARGO_PKG_VERSION").to_owned(),
        },
        revisions,
        room: LocalRoomProjection {
            room_id,
            admission: coordinator.local_admission(),
            connected: registry_connected,
            policy: coordinator.room_policy(),
        },
        sources,
        playback: PlaybackProjection {
            endpoint_mode: config.playback_binding.endpoint_mode,
            endpoint_id: config.playback_binding.endpoint_id.clone(),
            effective_endpoint_id,
            available: playback_available,
        },
        receive_buffer: config.room.settings.receive_buffer,
        mixer: mix_projection(config),
        permissions,
        local_emergency_stop: config.local_emergency_stop,
        diagnostics: DiagnosticsProjection {
            control_reconnects: live.control_reconnects,
            malformed_messages: live.malformed_messages,
            ingress_dropped: live.dropped_ingress,
            active_alert_codes: errors.iter().map(|error| error.code).collect(),
        },
        logging: LoggingProjection {
            enabled: config.logging.enabled,
            directory: config.logging.directory.clone(),
            max_bytes: config.logging.max_bytes,
            retention_count: config.logging.retention_count,
        },
        driver: DriverProjection {
            package_id: config.driver.package_id.clone(),
            version: None,
            enabled: config.driver.enabled,
            available: false,
        },
        lifecycle: LifecycleProjection {
            start_with_os: config.startup.start_with_os,
            start_engine_on_ui_open: config.startup.start_engine_on_ui_open,
        },
        operations: live.operations,
        errors,
        event_cursor: live.event_cursor,
    }
}

fn room_identity_and_permissions(
    config: &MemberConfig,
) -> (Option<rlb_model::RoomId>, Vec<PermissionProjection>) {
    let room_id = config.attached_room_id();
    let permissions = room_id
        .map(|room_id| PermissionProjection {
            room_id,
            grant_revision: config.room.grant_revision,
            grants: config.room.settings.grants,
        })
        .into_iter()
        .collect();
    (room_id, permissions)
}

/// Canonical revisions currently known to this engine run.
#[must_use]
pub fn canonical_revisions(
    coordinator: &MemberSessionCoordinator,
    config: &MemberConfig,
) -> CanonicalRevisions {
    CanonicalRevisions {
        room_revision: coordinator.room_revision(),
        presence_revision: coordinator.presence_revision(),
        catalog_revision: Some(coordinator.catalog_revision()),
        member_config_revision: Some(config.member_config_revision),
        grant_revision: Some(config.room.grant_revision),
        subscription_revision: coordinator.snapshot_subscription_revision(),
    }
}

fn mix_projection(config: &MemberConfig) -> MixerProjection {
    let Some(room_id) = config.attached_room_id() else {
        return MixerProjection::default();
    };
    MixerProjection {
        members: config
            .room
            .member_mix
            .iter()
            .map(|entry| rlb_control::MemberMixProjection {
                room_id,
                remote_member_id: entry.remote_member_id,
                gain_db: entry.mix.gain_db,
                mute: entry.mix.mute,
                solo: entry.mix.solo,
            })
            .collect(),
        sources: config
            .room
            .source_mix
            .iter()
            .map(|entry| rlb_control::SourceMixProjection {
                room_id,
                remote_member_id: entry.remote_member_id,
                source_id: entry.source_id,
                gain_db: entry.mix.gain_db,
                pan: entry.mix.pan,
                mute: entry.mix.mute,
            })
            .collect(),
    }
}

fn map_engine_state(state: rlb_session::EngineState) -> EngineState {
    match state {
        rlb_session::EngineState::Starting => EngineState::Starting,
        rlb_session::EngineState::Running => EngineState::Running,
        rlb_session::EngineState::ShuttingDown | rlb_session::EngineState::Stopped => {
            EngineState::Stopping
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::Ipv4Addr;

    use rlb_model::{
        AdmissionState, CatalogRevision, Endpoint, GrantRevision, IdentityRecord, MemberId,
        PresenceRevision, QosPolicy, QualityProfile, RegistrySessionId, RoomId, RoomRevision,
        RunId, ServerGeneration, SourceId, SourceKind,
    };
    use rlb_registry::{
        ControlPresence, DecimalU64, MediaPresence, RoomSnapshot, RoomView, SelfView,
        SnapshotMember,
    };
    use rlb_session::{
        AuthoritySnapshot, CoordinatorEvent, JoinTarget, LocalSourceVisibility, MemberConfig,
        MemberConfigError,
    };

    use super::*;

    fn identity() -> IdentityRecord {
        IdentityRecord::new(
            MemberId::from_bytes(1_u128.to_be_bytes()).expect("member id"),
            SourceId::from_bytes(11_u128.to_be_bytes()).expect("system source id"),
            SourceId::from_bytes(12_u128.to_be_bytes()).expect("microphone source id"),
        )
        .expect("identity")
    }

    fn room_id() -> RoomId {
        RoomId::from_bytes(2_u128.to_be_bytes()).expect("room id")
    }

    fn run_id() -> RunId {
        RunId::from_bytes(3_u128.to_be_bytes()).expect("run id")
    }

    fn join_target() -> JoinTarget {
        JoinTarget {
            authority_url: "http://192.0.2.1/gw-ctrl".to_owned(),
            member_registry_endpoint: Endpoint::new(Ipv4Addr::new(192, 0, 2, 2), 17_099)
                .expect("registry endpoint"),
        }
    }

    fn coordinator(config: MemberConfig) -> MemberSessionCoordinator {
        MemberSessionCoordinator::new(identity(), run_id(), config, CatalogRevision::new(1))
            .expect("coordinator")
    }

    fn build_snapshot(coordinator: &MemberSessionCoordinator, case_name: &str) -> LocalSnapshot {
        let root =
            std::env::temp_dir().join(format!("rlb-snapshot-{}-{case_name}", std::process::id()));
        let store = PersistStore::open(root.clone()).expect("store");
        let render = SlotStatus::Unavailable {
            endpoint_id: String::new(),
        };
        let snapshot = build_local_snapshot(
            coordinator,
            identity(),
            run_id(),
            &store,
            &render,
            SnapshotLive {
                event_cursor: 0,
                engine_faulted: false,
                open_errors: Vec::new(),
                dropped_ingress: 0,
                malformed_messages: 0,
                control_reconnects: 0,
                operations: Vec::new(),
            },
        );
        fs::remove_dir_all(root).expect("remove test store");
        snapshot
    }

    fn authority_snapshot() -> AuthoritySnapshot {
        let identity = identity();
        AuthoritySnapshot {
            room: RoomSnapshot {
                server_generation: ServerGeneration::from_bytes(4_u128.to_be_bytes())
                    .expect("server generation"),
                room_id: room_id(),
                room_revision: RoomRevision::new(1),
                presence_revision: PresenceRevision::new(1),
                event_cursor: DecimalU64::new(0),
                room: RoomView {
                    name: "Room".to_owned(),
                    quality_cap: QualityProfile::PcmF32V1,
                    qos_policy: QosPolicy::AudioPriority,
                    room_send_muted: false,
                    max_approved_members: 32,
                },
                self_view: SelfView {
                    member_id: identity.member_id(),
                    admission: AdmissionState::Approved,
                },
                members: vec![SnapshotMember {
                    member_id: identity.member_id(),
                    display_name: "Local member".to_owned(),
                    admission: AdmissionState::Approved,
                    control_presence: ControlPresence::Online,
                    media_presence: MediaPresence::Unbound,
                    run_id: Some(run_id()),
                    media_endpoint: None,
                    catalog_revision: Some(CatalogRevision::new(1)),
                    sources: Vec::new(),
                }],
            },
            local_source_visibility: [SourceKind::SystemAudio, SourceKind::Microphone]
                .into_iter()
                .map(|kind| LocalSourceVisibility {
                    source_id: identity.source_id(kind),
                    viewer_member_ids: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn attached_only_snapshot_keeps_room_and_local_grants() {
        let mut config = MemberConfig::defaults(identity(), room_id());
        config.room.grant_revision = GrantRevision::new(7);
        config.room.settings.grants.receive_mix = true;
        config.validate(identity()).expect("attached-only config");
        assert!(config.join_target().is_none());
        let coordinator = coordinator(config);

        let snapshot = build_snapshot(&coordinator, "attached-only");

        assert_eq!(snapshot.room.room_id, Some(room_id()));
        assert!(!snapshot.room.connected);
        assert_eq!(snapshot.permissions.len(), 1);
        assert_eq!(snapshot.permissions[0].room_id, room_id());
        assert_eq!(
            snapshot.permissions[0].grant_revision,
            GrantRevision::new(7)
        );
        assert!(snapshot.permissions[0].grants.receive_mix);
    }

    #[test]
    fn join_only_config_is_invalid_and_has_no_room_projection() {
        let mut config = MemberConfig::unjoined(identity());
        config.room.join = Some(join_target());

        assert_eq!(
            config.validate(identity()),
            Err(MemberConfigError::JoinTarget)
        );
        let (projected_room_id, permissions) = room_identity_and_permissions(&config);
        assert_eq!(projected_room_id, None);
        assert!(permissions.is_empty());
    }

    #[test]
    fn complete_join_projects_registry_connection_independently() {
        let mut config = MemberConfig::defaults(identity(), room_id());
        config.room.join = Some(join_target());
        config.validate(identity()).expect("complete join config");
        let mut coordinator = coordinator(config);

        let disconnected = build_snapshot(&coordinator, "complete-join-disconnected");
        assert_eq!(disconnected.room.room_id, Some(room_id()));
        assert!(!disconnected.room.connected);
        assert_eq!(disconnected.permissions.len(), 1);

        coordinator
            .handle(CoordinatorEvent::EngineStarted)
            .expect("start engine");
        coordinator
            .handle(CoordinatorEvent::RegistryConnectRequested)
            .expect("request registry connection");
        coordinator
            .handle(CoordinatorEvent::RegistrySessionReady {
                registry_session_id: RegistrySessionId::from_bytes(5_u128.to_be_bytes())
                    .expect("registry session id"),
            })
            .expect("registry session");
        coordinator
            .handle(CoordinatorEvent::AuthoritySnapshotReceived(
                authority_snapshot(),
            ))
            .expect("authority snapshot");

        let connected = build_snapshot(&coordinator, "complete-join-connected");
        assert_eq!(connected.room.room_id, Some(room_id()));
        assert!(connected.room.connected);
        assert_eq!(connected.room.admission, Some(AdmissionState::Approved));
        assert_eq!(connected.permissions.len(), 1);
    }

    #[test]
    fn unjoined_snapshot_has_no_room_or_permissions() {
        let coordinator = coordinator(MemberConfig::unjoined(identity()));

        let snapshot = build_snapshot(&coordinator, "unjoined");

        assert_eq!(snapshot.room.room_id, None);
        assert!(!snapshot.room.connected);
        assert_eq!(snapshot.room.admission, None);
        assert_eq!(snapshot.room.policy, None);
        assert!(snapshot.permissions.is_empty());
    }
}
