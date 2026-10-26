//! Persistent engine composition root for the Windows audio stage.
//!
//! This process owns the WASAPI device supervisor, mix-graph publisher, source
//! encoders, UDP media path, member config files, Control-A, and
//! [`MemberSessionCoordinator`]. `run` honors persisted capture defaults.
//! `media-loop` enables capture for two seconds and fans encoded datagrams
//! back to this process so assembly and jitter readers can be counted.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rlb_audio::{
    AudioMetrics, EndpointDirection, EndpointState, MixGraphCommand, MixGraphPublisher,
};
use rlb_audio_windows::{
    CaptureWorker, DeviceBindings, DeviceSupervisor, EndpointEnumerator, EndpointSelection,
    SlotStatus, SupervisorSources,
};
use rlb_clock::StdMonotonic;
use rlb_control::{
    CommandStatus, DeviceFlow, LocalCommandName, LocalEventKind, LocalQueryName, ReceiveBufferMode,
    Version2, confirmation_bind_hash,
};
use rlb_media::{DirectDatagram, MediaIdentity, SenderMetrics};
use rlb_model::{
    AdmissionState, CatalogRevision, EndpointMode, ErrorCode, ErrorEntity, ErrorImpact,
    IdentityRecord, ManualBufferTarget, MemberMix, PresenceRevision, QualityProfile,
    ReceiveBufferConfig, RegistrySessionId, RequestId, RoomId, RoomRevision, RunId,
    ServerGeneration, SourceDescriptor, SourceFormat, SourceId, SourceKind, SourceMix, SourceState,
    StreamEpoch, StructuredError,
};
use rlb_registry::{
    AppliedRoomRevisions, ClientMessage, DecimalU64, DriverSigningState, MemberProjection,
    RoomSnapshot, SafeLocalFields, ServerMessage, SnapshotReason, SourceSlot, TokenDigest,
};
use rlb_session::{
    AudioMixPlan, AuthoritySnapshot, CoordinatorEffect, CoordinatorEvent, DeviceBindingConfig,
    EngineState, EpochChangeReason, JoinTarget, LocalConfigMutation, LocalConfigRequest,
    LocalSourceVisibility, LoggingConfig, MemberCommandDelivery, MemberConfig,
    MemberSessionCoordinator, PersistenceResult, ShutdownStage, SourceEpochStartPlan,
    SourceMonitorConfig, SourceRestartPlan, SourceStopReason, SourceStreamStarted, StartupConfig,
};

use crate::control::{
    ControlServer, QueuedCommand, command_result_json, command_result_json_with_effective,
    get_json, query_error_json, query_result_json,
};
use crate::encoder::SourceEncoder;
use crate::error::EngineError;
use crate::media::MediaPath;
use crate::persist::{
    ConfigLoad, ControlEndpointDescriptor, IdentityLoad, PersistStore, RUN_DESCRIPTOR_SCHEMA,
    RunDescriptor, generate_ipc_token, process_start_marker, rfc3339_utc_now,
};
use crate::registry::RegistryClient;
use crate::snapshot::{SnapshotLive, build_local_snapshot, canonical_revisions};

const LOOP_SELF_CONFIGURATION_ERROR: &str =
    "loop_self is reserved for the explicit media-loop test command";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EngineCommand {
    Run,
    MediaLoop,
}

/// How one engine process should run.
pub struct EngineOptions {
    command: EngineCommand,
    /// Wait for Enter on stdin before shutdown.
    pub wait_enter: bool,
    /// Fan encoded datagrams to this process's bound UDP endpoint.
    pub loop_self: bool,
    /// Force master capture on for this process without rewriting config.json.
    pub force_capture: bool,
    /// Optional timed stop used by `media-loop`.
    pub duration: Option<Duration>,
}

/// Runs the coordinator until standard input receives a newline.
pub fn run_engine() -> Result<(), EngineError> {
    run_with(EngineOptions {
        command: EngineCommand::Run,
        wait_enter: true,
        loop_self: false,
        force_capture: false,
        duration: None,
    })
}

/// Encodes local capture, fans it to the bound UDP socket, and counts assembly.
pub fn run_media_loop() -> Result<(), EngineError> {
    run_with(EngineOptions {
        command: EngineCommand::MediaLoop,
        wait_enter: false,
        loop_self: true,
        force_capture: true,
        duration: Some(Duration::from_secs(2)),
    })
}

fn run_with(options: EngineOptions) -> Result<(), EngineError> {
    validate_engine_options(&options)?;
    let store = PersistStore::user_default()?;
    let identity = match store.load_or_create_identity() {
        Ok(IdentityLoad::Existing(identity) | IdentityLoad::Created(identity)) => identity,
        Err(EngineError::IdentityCorrupt) => {
            eprintln!(
                "identity.json is unreadable; Control-A will start in a faulted run and identity.json was left unchanged"
            );
            return run_faulted_control(store);
        }
        Err(error) => return Err(error),
    };
    let mut config = match store.load_or_create_config(identity) {
        Ok(ConfigLoad::Existing(config) | ConfigLoad::Created(config)) => config,
        Err(EngineError::ConfigCorrupt) => {
            eprintln!(
                "config.json is unreadable; Control-A will start in a faulted run and config.json was left unchanged"
            );
            return run_faulted_control(store);
        }
        Err(error) => return Err(error),
    };
    if options.force_capture {
        for source in &mut config.sources {
            source.config.master_capture_enabled = true;
        }
    }

    let run_id = RunId::generate()?;
    let ipc_token = generate_ipc_token()?;
    let control = ControlServer::bind(identity.member_id(), run_id, ipc_token.clone())?;
    let descriptor = RunDescriptor {
        schema: RUN_DESCRIPTOR_SCHEMA,
        member_id: identity.member_id(),
        run_id,
        pid: std::process::id(),
        process_start_marker: process_start_marker(),
        endpoint: ControlEndpointDescriptor {
            host: "127.0.0.1".to_owned(),
            port: control.port,
            api_root: "/rlb/local/v2".to_owned(),
        },
        ipc_token: ipc_token.clone(),
        created_at: rfc3339_utc_now(),
    };
    store.write_run_descriptor(&descriptor)?;

    let metrics = Arc::new(AudioMetrics::new());
    let mut devices = DeviceSupervisor::start(
        device_bindings_from_config(&config),
        SupervisorSources {
            system_audio: identity.source_id(SourceKind::SystemAudio),
            microphone: identity.source_id(SourceKind::Microphone),
        },
        Arc::clone(&metrics),
    )?;
    devices.set_system_capture_method(config.system_capture_method);
    let _ = devices.poll()?;
    let attached_room_id = config.attached_room_id();
    let coordinator =
        MemberSessionCoordinator::new(identity, run_id, config, CatalogRevision::new(1))?;
    let clock = StdMonotonic::new();
    let media = MediaPath::bind(
        attached_room_id,
        identity.member_id(),
        run_id,
        options.loop_self,
        clock,
    )?;

    let stop = Arc::new(AtomicBool::new(false));
    if options.wait_enter {
        spawn_enter_watcher(Arc::clone(&stop));
    }

    let mut runtime = EngineRuntime {
        identity,
        room_id: attached_room_id,
        run_id,
        store,
        devices: Some(devices),
        coordinator,
        metrics,
        encoders: BTreeMap::new(),
        pending_starts: BTreeMap::new(),
        mirrors: BTreeMap::new(),
        last_mix: None,
        last_mix_epoch: 0,
        last_send: None,
        media: Some(media),
        registry: None,
        registry_session_id: None,
        registry_generation: None,
        reconnect_at: None,
        peer_endpoints: BTreeMap::new(),
        control: Some(control),
        loop_self: options.loop_self,
        pending_ops: BTreeMap::new(),
        shutting_down: false,
        open_errors: Vec::new(),
        last_engine_state: None,
        last_room_connected: None,
        last_playback_available: None,
    };
    runtime.last_mix_epoch = runtime
        .devices
        .as_ref()
        .map(DeviceSupervisor::mix_epoch)
        .unwrap_or(0);

    println!("RustLanBus engine — persistent coordinator");
    println!("member_id={}", identity.member_id());
    println!("run_id={run_id}");
    match runtime.room_id {
        Some(room_id) => println!("room_id={room_id} (joined target present)"),
        None => println!("room_id=<unjoined> (no durable authority room identity)"),
    }
    println!(
        "media udp {}:{}",
        runtime.media.as_ref().expect("media").local_address().ip(),
        runtime
            .media
            .as_ref()
            .expect("media")
            .local_address()
            .port()
    );
    println!("descriptor written under %LOCALAPPDATA%\\RustLanBus\\run");
    if options.wait_enter {
        println!("press Enter to stop");
    }
    print_poll(runtime.devices.as_ref().expect("devices").snapshot());

    runtime.apply_event(CoordinatorEvent::EngineStarted)?;
    runtime.publish_control_snapshot();
    if let Some(control) = runtime.control.as_ref() {
        control.emit_event(
            LocalEventKind::EngineStateChanged,
            canonical_revisions(&runtime.coordinator, runtime.coordinator.config()),
            serde_json::json!({
                "state": "running",
                "build": env!("CARGO_PKG_VERSION"),
            }),
        );
    }

    if options.loop_self {
        let hello = get_json(
            runtime.control.as_ref().expect("control").port,
            &ipc_token,
            "/rlb/local/v2/hello",
        )?;
        if hello.contains("\"type\":\"hello\"") || hello.contains("\"type\": \"hello\"") {
            println!("control hello: ok");
        } else {
            println!("control hello: unexpected response");
        }
    }

    let deadline = options.duration.map(|duration| Instant::now() + duration);
    let mut last_report = Instant::now();
    while !stop.load(Ordering::Relaxed)
        && runtime.coordinator.engine_state() == EngineState::Running
        && deadline.is_none_or(|deadline| Instant::now() < deadline)
    {
        runtime.poll_devices()?;
        runtime.poll_control()?;
        runtime.poll_registry()?;
        runtime.poll_media()?;
        runtime.publish_metrics()?;
        runtime.publish_control_snapshot();
        if last_report.elapsed() >= Duration::from_secs(1) {
            runtime.print_status();
            last_report = Instant::now();
        }
        thread::sleep(Duration::from_millis(10));
    }

    if runtime.coordinator.engine_state() == EngineState::Running {
        runtime.apply_event(CoordinatorEvent::ShutdownRequested)?;
        runtime.drive_shutdown()?;
    }
    Ok(())
}

fn validate_engine_options(options: &EngineOptions) -> Result<(), EngineError> {
    if options.loop_self && options.command != EngineCommand::MediaLoop {
        return Err(EngineError::Persist(LOOP_SELF_CONFIGURATION_ERROR));
    }
    Ok(())
}

fn run_faulted_control(store: PersistStore) -> Result<(), EngineError> {
    let run_id = RunId::generate()?;
    let token = generate_ipc_token()?;
    let mut control = ControlServer::bind_faulted(run_id, token)?;
    println!(
        "control http://127.0.0.1:{}/rlb/local/v2 (faulted; identity/config files were not replaced; no run descriptor was written)",
        control.port
    );
    println!("press Enter to stop");
    let stop = Arc::new(AtomicBool::new(false));
    spawn_enter_watcher(Arc::clone(&stop));
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(50));
        control.reject_queued_as_faulted();
    }
    control.shutdown();
    drop(store);
    Ok(())
}

struct EngineRuntime {
    identity: IdentityRecord,
    room_id: Option<RoomId>,
    run_id: RunId,
    store: PersistStore,
    devices: Option<DeviceSupervisor>,
    coordinator: MemberSessionCoordinator,
    metrics: Arc<AudioMetrics>,
    encoders: BTreeMap<SourceId, SourceEncoder>,
    pending_starts: BTreeMap<SourceId, EpochChangeReason>,
    mirrors: BTreeMap<SourceId, SourceMirror>,
    last_mix: Option<AudioMixPlan>,
    last_mix_epoch: u64,
    last_send: Option<rlb_session::SendPlan>,
    media: Option<MediaPath>,
    registry: Option<RegistryClient>,
    registry_session_id: Option<RegistrySessionId>,
    registry_generation: Option<ServerGeneration>,
    reconnect_at: Option<Instant>,
    peer_endpoints: BTreeMap<rlb_model::MemberId, rlb_model::Endpoint>,
    control: Option<ControlServer>,
    loop_self: bool,
    pending_ops: BTreeMap<RequestId, rlb_model::OperationId>,
    shutting_down: bool,
    open_errors: Vec<rlb_model::StructuredError>,
    last_engine_state: Option<rlb_control::EngineState>,
    last_room_connected: Option<bool>,
    last_playback_available: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct SourceMirror {
    endpoint_id: Option<String>,
    epoch: Option<StreamEpoch>,
    stream_started: bool,
}

impl EngineRuntime {
    fn apply_event(&mut self, event: CoordinatorEvent) -> Result<(), EngineError> {
        let effects = self.coordinator.handle(event)?;
        self.synchronize_media_path()?;
        self.execute_effects(effects)?;
        self.synchronize_media_path()?;
        Ok(())
    }

    fn execute_effects(&mut self, effects: Vec<CoordinatorEffect>) -> Result<(), EngineError> {
        for effect in effects {
            self.execute_effect(effect)?;
        }
        Ok(())
    }

    fn execute_effect(&mut self, effect: CoordinatorEffect) -> Result<(), EngineError> {
        match effect {
            CoordinatorEffect::SetEmergencySendGate { stopped } => {
                if let Some(media) = self.media.as_mut() {
                    media.set_send_gate(stopped);
                }
            }
            CoordinatorEffect::StartOrRestartSource(plan) => {
                self.pending_starts.insert(plan.source_id, plan.reason);
                self.try_start_pending(plan)?;
            }
            CoordinatorEffect::StartSourceEpoch(plan) => {
                self.start_encoder(plan)?;
            }
            CoordinatorEffect::EndSourceEpoch(plan) => {
                self.stop_encoder(plan.source_id);
            }
            CoordinatorEffect::StopSource { source_id, .. } => {
                self.stop_encoder(source_id);
            }
            CoordinatorEffect::ApplyAudioMix(plan) => {
                self.apply_mix_plan(plan);
            }
            CoordinatorEffect::ApplySendPlan(plan) => {
                if let Some(media) = self.media.as_mut() {
                    media.set_send_plan(plan.clone());
                }
                self.last_send = Some(plan);
            }
            CoordinatorEffect::PersistMemberConfig {
                transaction_id,
                candidate,
            } => {
                let result = match self.store.write_config(&candidate) {
                    Ok(()) => PersistenceResult::Committed,
                    Err(error) => {
                        eprintln!("config persist: {error}");
                        PersistenceResult::Failed
                    }
                };
                self.apply_event(CoordinatorEvent::ConfigPersistenceCompleted {
                    transaction_id,
                    result,
                })?;
            }
            CoordinatorEffect::LocalConfigCompleted(outcome) => {
                if let Some(operation_id) = self.pending_ops.remove(&outcome.request_id)
                    && let Some(control) = self.control.as_ref()
                {
                    let body = if outcome.status == CommandStatus::Rejected {
                        command_result_json(
                            outcome.request_id,
                            operation_id,
                            outcome.status,
                            outcome.revisions,
                            outcome.code,
                            self.identity.member_id(),
                            self.run_id,
                        )
                    } else {
                        let config = self.coordinator.config();
                        let effective = serde_json::json!({
                            "member_config_revision": config.member_config_revision,
                            "room_id": config.attached_room_id(),
                            "join": config.join_target().map(|target| serde_json::json!({
                                "authority_url": target.authority_url,
                                "member_registry_endpoint": target.member_registry_endpoint,
                            })),
                            "local_emergency_stop": config.local_emergency_stop,
                            "logging": {
                                "enabled": config.logging.enabled,
                                "directory": config.logging.directory,
                                "max_bytes": config.logging.max_bytes,
                                "retention_count": config.logging.retention_count,
                            },
                            "startup": {
                                "start_with_os": config.startup.start_with_os,
                                "start_engine_on_ui_open": config.startup.start_engine_on_ui_open,
                            },
                            "driver": {
                                "package_id": config.driver.package_id,
                                "enabled": config.driver.enabled,
                                "available": false,
                            },
                        });
                        command_result_json_with_effective(
                            outcome.request_id,
                            operation_id,
                            outcome.status,
                            outcome.revisions,
                            effective,
                            self.identity.member_id(),
                            self.run_id,
                        )
                    };
                    control.store_result(outcome.request_id, body);
                }
            }
            CoordinatorEffect::RunShutdownStage(stage) => {
                self.run_shutdown_stage(stage)?;
                self.apply_event(CoordinatorEvent::ShutdownStageCompleted(stage))?;
            }
            CoordinatorEffect::ConnectRegistry => {
                self.connect_registry();
            }
            CoordinatorEffect::LeaveRegistry => {
                self.registry = None;
                self.registry_session_id = None;
                self.registry_generation = None;
                self.reconnect_at = None;
            }
            CoordinatorEffect::RequestFullRoomSnapshot { reason } => {
                self.request_room_snapshot(reason);
            }
            CoordinatorEffect::ScheduleRegistryReconnect { base_delay_ms } => {
                self.reconnect_at =
                    Some(Instant::now() + Duration::from_millis(u64::from(base_delay_ms)));
            }
            CoordinatorEffect::SendRegistryMessage(message) => {
                if let Some(registry) = self.registry.as_mut() {
                    if let Err(error) = registry.send(&message) {
                        eprintln!("registry send: {error}");
                        self.note_open_error(ErrorCode::MemberDisconnected);
                    }
                } else {
                    self.note_open_error(ErrorCode::RoomServiceUnreachable);
                }
            }
            CoordinatorEffect::PublishRegistryProjection(_) => {
                if self.registry.is_none() {
                    self.note_open_error(ErrorCode::RoomServiceUnreachable);
                }
            }
            CoordinatorEffect::ConnectPeer(plan) => {
                self.peer_endpoints.insert(plan.member_id, plan.endpoint);
                if let Some(media) = self.media.as_mut() {
                    media.bind_peer(plan.member_id, plan.run_id, plan.endpoint);
                }
            }
            CoordinatorEffect::DisconnectPeer { member_id } => {
                self.peer_endpoints.remove(&member_id);
                if let Some(media) = self.media.as_mut() {
                    media.forget_peer(member_id);
                }
            }
            CoordinatorEffect::SendDirect(datagram) => {
                self.send_direct(datagram);
            }
        }
        Ok(())
    }

    fn try_start_pending(&mut self, plan: SourceRestartPlan) -> Result<(), EngineError> {
        let kind = self.source_kind(plan.source_id);
        let started = {
            let Some(devices) = self.devices.as_ref() else {
                return Ok(());
            };
            let snapshot = devices.snapshot();
            match kind {
                SourceKind::SystemAudio => match snapshot.system_audio {
                    SlotStatus::Running {
                        endpoint_id,
                        friendly_name,
                    } => Some((devices.system_audio_epoch(), endpoint_id, friendly_name)),
                    SlotStatus::Unavailable { .. } => None,
                },
                SourceKind::Microphone => match snapshot.microphone {
                    SlotStatus::Running {
                        endpoint_id,
                        friendly_name,
                    } => Some((devices.microphone_epoch(), endpoint_id, friendly_name)),
                    SlotStatus::Unavailable { .. } => None,
                },
            }
        };
        let Some((epoch, endpoint_id, friendly)) = started else {
            self.note_open_error(ErrorCode::SourceUnavailable);
            return Ok(());
        };
        self.pending_starts.remove(&plan.source_id);
        self.report_stream_started(plan.source_id, epoch, endpoint_id, plan.reason, &friendly)
    }

    fn report_stream_started(
        &mut self,
        source_id: SourceId,
        stream_epoch: StreamEpoch,
        endpoint_id: String,
        reason: EpochChangeReason,
        friendly_name: &str,
    ) -> Result<(), EngineError> {
        let kind = self.source_kind(source_id);
        let profile = self
            .coordinator
            .config()
            .source(source_id)
            .map(|source| source.config.selected_profile)
            .unwrap_or(QualityProfile::PcmF32V1);
        println!("source {kind:?} started {friendly_name} epoch={stream_epoch} reason={reason:?}");
        self.mirrors.insert(
            source_id,
            SourceMirror {
                endpoint_id: Some(endpoint_id.clone()),
                epoch: Some(stream_epoch),
                stream_started: true,
            },
        );
        self.apply_event(CoordinatorEvent::SourceStreamStarted(SourceStreamStarted {
            source_id,
            stream_epoch,
            endpoint_id,
            capture_method: match kind {
                SourceKind::SystemAudio => Some(self.coordinator.config().system_capture_method),
                SourceKind::Microphone => None,
            },
            format: SourceFormat::for_source(kind, profile),
            reason,
        }))
    }

    fn start_encoder(&mut self, plan: SourceEpochStartPlan) -> Result<(), EngineError> {
        let Some(room_id) = self.coordinator.config().attached_room_id() else {
            return Ok(());
        };
        self.stop_encoder(plan.source_id);
        let kind = self.source_kind(plan.source_id);
        let consumer = self.devices.as_mut().and_then(|devices| {
            let worker = match kind {
                SourceKind::SystemAudio => devices.system_audio(),
                SourceKind::Microphone => devices.microphone(),
            };
            worker.and_then(CaptureWorker::take_consumer)
        });
        let Some(consumer) = consumer else {
            println!(
                "encoder skipped: capture queue unavailable for {}",
                plan.source_id
            );
            self.note_open_error(ErrorCode::SourceUnavailable);
            return Ok(());
        };
        let identity = MediaIdentity {
            room_id,
            sender_member_id: self.identity.member_id(),
            sender_run_id: self.run_id,
            source_id: plan.source_id,
            stream_epoch: plan.stream_epoch,
        };
        if self.loop_self
            && let (Some(media), Some(devices)) = (self.media.as_mut(), self.devices.as_ref())
        {
            let publisher = devices.mix_publisher();
            media.prepare_local_loop(identity, plan.format, &publisher)?;
        }
        let thread_name = match self.source_kind(plan.source_id) {
            SourceKind::SystemAudio => "rlb-enc-sys",
            SourceKind::Microphone => "rlb-enc-mic",
        };
        let encoder = SourceEncoder::spawn(identity, plan.format, consumer, thread_name)?;
        self.encoders.insert(plan.source_id, encoder);
        Ok(())
    }

    fn stop_encoder(&mut self, source_id: SourceId) {
        if let Some(mut encoder) = self.encoders.remove(&source_id) {
            let queued = encoder.drain_outbound();
            if let Some(media) = self.media.as_mut() {
                for record in queued {
                    media.submit(record);
                }
            }
            match encoder.stop() {
                Ok((report, leftover)) => {
                    if let Some(media) = self.media.as_mut() {
                        for record in leftover {
                            media.submit(record);
                        }
                    }
                    println!(
                        "encoder {} stopped units={} gaps={}",
                        source_id, report.encoded_units, report.gap_records
                    );
                }
                Err(error) => eprintln!("encoder {source_id} stop: {error}"),
            }
        }
    }

    fn apply_mix_plan(&mut self, plan: AudioMixPlan) {
        let Some((publisher, mix_epoch)) = self
            .devices
            .as_ref()
            .map(|devices| (devices.mix_publisher(), devices.mix_epoch()))
        else {
            self.last_mix = Some(plan);
            return;
        };
        if !publish_mix_plan(&publisher, self.last_mix.as_ref(), &plan) {
            self.note_open_error(ErrorCode::ServerBusy);
        }
        self.last_mix = Some(plan);
        self.last_mix_epoch = mix_epoch;
    }

    fn republish_mix_if_rebound(&mut self) {
        let Some((publisher, mix_epoch)) = self
            .devices
            .as_ref()
            .map(|devices| (devices.mix_publisher(), devices.mix_epoch()))
        else {
            return;
        };
        if mix_epoch == self.last_mix_epoch {
            return;
        }
        self.last_mix_epoch = mix_epoch;
        if let Some(plan) = self.last_mix.clone()
            && !publish_mix_plan(&publisher, None, &plan)
        {
            self.note_open_error(ErrorCode::ServerBusy);
        }
    }

    fn poll_devices(&mut self) -> Result<(), EngineError> {
        let (poll, system_epoch, microphone_epoch) = {
            let Some(devices) = self.devices.as_mut() else {
                return Ok(());
            };
            let poll = devices.poll()?;
            devices.reclaim_mix_readers();
            (
                poll,
                devices.system_audio_epoch(),
                devices.microphone_epoch(),
            )
        };
        let system_id = self.identity.source_id(SourceKind::SystemAudio);
        let microphone_id = self.identity.source_id(SourceKind::Microphone);
        self.sync_slot(system_id, &poll.system_audio, system_epoch)?;
        self.sync_slot(microphone_id, &poll.microphone, microphone_epoch)?;
        self.republish_mix_if_rebound();
        self.flush_pending_starts()?;
        Ok(())
    }

    fn poll_media(&mut self) -> Result<(), EngineError> {
        let records: Vec<_> = self
            .encoders
            .values_mut()
            .flat_map(SourceEncoder::drain_outbound)
            .collect();
        let publisher = self.devices.as_ref().map(DeviceSupervisor::mix_publisher);
        let mix_failed = if let Some(media) = self.media.as_mut() {
            for record in records {
                media.submit(record);
            }
            media.poll_receive(publisher.as_ref());
            if !media.take_mix_refresh().is_empty()
                && let (Some(publisher), Some(plan)) = (publisher.as_ref(), self.last_mix.clone())
            {
                !publish_mix_plan(publisher, None, &plan)
            } else {
                false
            }
        } else {
            false
        };
        if mix_failed {
            self.note_open_error(ErrorCode::ServerBusy);
        }
        Ok(())
    }

    fn poll_control(&mut self) -> Result<(), EngineError> {
        let commands = self
            .control
            .as_ref()
            .map(ControlServer::take_commands)
            .unwrap_or_default();
        for command in commands {
            self.dispatch_command(command)?;
        }
        let queries = self
            .control
            .as_ref()
            .map(ControlServer::take_queries)
            .unwrap_or_default();
        for query in queries {
            self.dispatch_query(query.envelope)?;
        }
        Ok(())
    }

    fn poll_registry(&mut self) -> Result<(), EngineError> {
        if let Some(at) = self.reconnect_at
            && Instant::now() >= at
        {
            self.reconnect_at = None;
            self.apply_event(CoordinatorEvent::ReconnectTimerElapsed)?;
        }
        let poll = match self.registry.as_mut() {
            Some(registry) => match registry.poll() {
                Ok(messages) => Ok(messages),
                Err(error) => {
                    eprintln!("registry poll: {error}");
                    Err(error)
                }
            },
            None => Ok(Vec::new()),
        };
        match poll {
            Ok(messages) => {
                for message in messages {
                    self.ingest_registry_message(message)?;
                }
            }
            Err(_) => {
                self.registry = None;
                self.registry_session_id = None;
                self.registry_generation = None;
                self.note_open_error(ErrorCode::MemberDisconnected);
                let _ = self.apply_event(CoordinatorEvent::RegistryDisconnected);
            }
        }
        Ok(())
    }

    fn ingest_registry_message(&mut self, message: ServerMessage) -> Result<(), EngineError> {
        match message {
            ServerMessage::SessionReady {
                registry_session_id,
                server_generation,
                ..
            } => {
                self.registry_session_id = Some(registry_session_id);
                self.registry_generation = Some(server_generation);
                self.apply_event(CoordinatorEvent::RegistrySessionReady {
                    registry_session_id,
                })
            }
            ServerMessage::RoomSnapshot {
                server_generation,
                room_id,
                room_revision,
                presence_revision,
                event_cursor,
                room,
                self_view,
                members,
                ..
            } => {
                let snapshot = RoomSnapshot {
                    server_generation,
                    room_id,
                    room_revision,
                    presence_revision,
                    event_cursor,
                    room,
                    self_view,
                    members,
                };
                let local_source_visibility = self.local_visibility_from(&snapshot);
                self.apply_event(CoordinatorEvent::AuthoritySnapshotReceived(
                    AuthoritySnapshot {
                        room: snapshot,
                        local_source_visibility,
                    },
                ))
            }
            ServerMessage::RoomEvent {
                event_seq,
                server_generation,
                room_id,
                room_revision,
                presence_revision,
                event,
                ..
            } => self.apply_event(CoordinatorEvent::AuthorityEventReceived(
                rlb_registry::RoomEvent {
                    event_seq,
                    server_generation,
                    room_id,
                    room_revision,
                    presence_revision,
                    event,
                },
            )),
            ServerMessage::MemberCommand { .. } => {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                match MemberCommandDelivery::from_server_message(message, now_ms) {
                    Ok(delivery) => self
                        .apply_event(CoordinatorEvent::MemberCommandReceived { delivery, now_ms }),
                    Err(_) => {
                        self.note_open_error(ErrorCode::MalformedMessage);
                        Ok(())
                    }
                }
            }
            ServerMessage::Error { code, .. } => {
                self.note_open_error(code);
                Ok(())
            }
            ServerMessage::HeartbeatAck { .. }
            | ServerMessage::MediaBound { .. }
            | ServerMessage::AdmissionUpdate { .. } => Ok(()),
            _ => {
                self.note_open_error(ErrorCode::OperationNotSupported);
                Ok(())
            }
        }
    }

    fn request_room_snapshot(&mut self, reason: SnapshotReason) {
        let Some(registry) = self.registry.as_mut() else {
            self.note_open_error(ErrorCode::RoomServiceUnreachable);
            return;
        };
        let Some(room_id) = self.coordinator.config().attached_room_id() else {
            self.note_open_error(ErrorCode::InvalidStateTransition);
            return;
        };
        let Some(registry_session_id) = self.registry_session_id else {
            self.note_open_error(ErrorCode::UnknownSession);
            return;
        };
        let Some(server_generation) = self.registry_generation else {
            self.note_open_error(ErrorCode::StaleGeneration);
            return;
        };
        let message = ClientMessage::SnapshotRequest {
            v: Version2,
            request_id: match RequestId::generate() {
                Ok(id) => id,
                Err(_) => {
                    self.note_open_error(ErrorCode::UnexpectedError);
                    return;
                }
            },
            room_id,
            server_generation,
            registry_session_id,
            applied_revisions: AppliedRoomRevisions {
                room_revision: RoomRevision::new(1),
                presence_revision: PresenceRevision::new(1),
            },
            reason,
        };
        if let Err(error) = registry.send(&message) {
            eprintln!("registry snapshot request: {error}");
            self.note_open_error(ErrorCode::MemberDisconnected);
        }
    }

    fn ensure_member_token(
        &mut self,
        endpoint: rlb_model::Endpoint,
        room_id: RoomId,
    ) -> Result<(), ErrorCode> {
        let mut credentials = self
            .store
            .load_or_create_registry_credentials(room_id)
            .map_err(|_| ErrorCode::ConfigPersistFailed)?;
        if credentials.member_token.is_some() {
            return Ok(());
        }
        let slots = vec![
            SourceSlot {
                source_id: self.identity.source_id(SourceKind::SystemAudio),
                kind: SourceKind::SystemAudio,
            },
            SourceSlot {
                source_id: self.identity.source_id(SourceKind::Microphone),
                kind: SourceKind::Microphone,
            },
        ];
        let begin = ClientMessage::EnrollmentBegin {
            v: Version2,
            request_id: RequestId::generate().map_err(|_| ErrorCode::UnexpectedError)?,
            room_id,
            member_id: self.identity.member_id(),
            display_name: "Windows member".to_owned(),
            source_slots: slots,
            claim_sha256: credentials.claim_secret.digest(),
            request_readmission: false,
        };
        let pending = RegistryClient::provisioning_roundtrip(endpoint, &begin)
            .map_err(|_| ErrorCode::RoomServiceUnreachable)?;
        match pending {
            ServerMessage::EnrollmentPending {
                admission: AdmissionState::Approved,
                ..
            } => {}
            ServerMessage::EnrollmentPending {
                admission: AdmissionState::Pending,
                ..
            } => return Err(ErrorCode::AdmissionPending),
            ServerMessage::Error { code, .. } => return Err(code),
            _ => return Err(ErrorCode::InvalidStateTransition),
        }
        let claim = ClientMessage::EnrollmentClaim {
            v: Version2,
            request_id: RequestId::generate().map_err(|_| ErrorCode::UnexpectedError)?,
            room_id,
            member_id: self.identity.member_id(),
            claim_secret: credentials.claim_secret.clone(),
        };
        match RegistryClient::provisioning_roundtrip(endpoint, &claim)
            .map_err(|_| ErrorCode::RoomServiceUnreachable)?
        {
            ServerMessage::EnrollmentClaimResult {
                member_token,
                token_generation,
                ..
            } => {
                credentials.member_token = Some(member_token);
                credentials.token_generation = Some(token_generation);
                self.store
                    .write_registry_credentials(&credentials)
                    .map_err(|_| ErrorCode::ConfigPersistFailed)?;
                Ok(())
            }
            ServerMessage::Error { code, .. } => Err(code),
            _ => Err(ErrorCode::ClaimInvalid),
        }
    }

    fn engine_hello_message(&self, room_id: RoomId) -> Result<ClientMessage, ErrorCode> {
        let credentials = self
            .store
            .load_or_create_registry_credentials(room_id)
            .map_err(|_| ErrorCode::ConfigPersistFailed)?;
        let token = credentials
            .member_token
            .ok_or(ErrorCode::MemberTokenInvalid)?;
        let token_generation = credentials
            .token_generation
            .unwrap_or_else(|| DecimalU64::new(1));
        let config = self.coordinator.config();
        Ok(ClientMessage::EngineHello {
            v: Version2,
            request_id: RequestId::generate().map_err(|_| ErrorCode::UnexpectedError)?,
            member_token: token.clone(),
            token_generation,
            token_sha256: TokenDigest::of(token.expose_bytes()),
            room_id,
            member_id: self.identity.member_id(),
            run_id: self.run_id,
            display_name: "Windows member".to_owned(),
            client_build: "rustlanbus-engine-0.2.0".to_owned(),
            catalog_revision: config
                .sources
                .first()
                .map(|_| CatalogRevision::new(1))
                .unwrap_or_else(|| CatalogRevision::new(1)),
            sources: self.hello_catalog(config),
            member_config_revision: config.member_config_revision,
            member_projection: self.hello_projection(config),
            grant_revision: config.room.grant_revision,
            capability_grants: config.room.settings.grants,
        })
    }

    fn hello_catalog(&self, config: &MemberConfig) -> Vec<SourceDescriptor> {
        config
            .sources
            .iter()
            .filter_map(|source| {
                let name = match source.kind {
                    SourceKind::SystemAudio => "System audio",
                    SourceKind::Microphone => "Microphone",
                };
                let selected = source.config.selected_profile;
                let effective = selected.capped_by(QualityProfile::PcmF32V1);
                let state = SourceState::new(
                    false,
                    source.config.master_capture_enabled,
                    source.config.publish_requested,
                    selected,
                    effective,
                    false,
                )
                .ok()?;
                SourceDescriptor::new(source.source_id, source.kind, name, state, None).ok()
            })
            .collect()
    }

    fn hello_projection(&self, config: &MemberConfig) -> MemberProjection {
        MemberProjection {
            local_emergency_stop: config.local_emergency_stop,
            auto_reconnect: config.room.settings.auto_reconnect,
            effective_room_send_muted: false,
            effective_qos_policy: rlb_model::QosPolicy::AudioPriority,
            safe_local_fields: SafeLocalFields {
                capture_method: config.system_capture_method,
                capture_endpoint_mode: config.system_audio_binding.endpoint_mode,
                capture_friendly_label: "Default".to_owned(),
                capture_available: true,
                playback_endpoint_mode: config.playback_binding.endpoint_mode,
                playback_friendly_label: "Default".to_owned(),
                playback_available: true,
                driver_version: None,
                driver_signing_state: DriverSigningState::NotInstalled,
                driver_available: false,
                master_capture_enabled: config
                    .sources
                    .iter()
                    .any(|source| source.config.master_capture_enabled),
                local_emergency_stop: config.local_emergency_stop,
                startup_enabled: config.startup.start_with_os,
                file_logging_enabled: config.logging.enabled,
            },
        }
    }

    fn local_visibility_from(&self, snapshot: &RoomSnapshot) -> Vec<LocalSourceVisibility> {
        let viewers: Vec<_> = snapshot
            .members
            .iter()
            .map(|member| member.member_id)
            .filter(|member_id| *member_id != self.identity.member_id())
            .collect();
        [SourceKind::SystemAudio, SourceKind::Microphone]
            .into_iter()
            .map(|kind| LocalSourceVisibility {
                source_id: self.identity.source_id(kind),
                viewer_member_ids: viewers.clone(),
            })
            .collect()
    }

    fn note_open_error(&mut self, code: ErrorCode) {
        if self.open_errors.iter().any(|error| error.code == code) {
            return;
        }
        self.open_errors.push(StructuredError {
            code,
            entity: ErrorEntity::Engine {
                member_id: self.identity.member_id(),
                run_id: self.run_id,
            },
            impact: ErrorImpact::DiagnosticOnly,
            retryable: false,
            diagnostic_id: None,
        });
    }

    fn dispatch_command(&mut self, queued: QueuedCommand) -> Result<(), EngineError> {
        let envelope = queued.envelope;
        self.pending_ops
            .insert(envelope.request_id, queued.operation_id);
        if matches!(envelope.command, LocalCommandName::ConfirmationPrepare) {
            return self.dispatch_confirmation_prepare(envelope, queued.operation_id);
        }
        if matches!(
            envelope.command,
            LocalCommandName::EngineShutdown | LocalCommandName::EngineRestart
        ) {
            return self.dispatch_confirmed_lifecycle(envelope, queued.operation_id);
        }
        if envelope.command == LocalCommandName::RoomReconnect {
            return self.dispatch_reconnect(envelope.request_id, queued.operation_id);
        }
        if envelope.command == LocalCommandName::RoomReconnectCancel {
            self.reconnect_at = None;
            self.finish_command(
                envelope.request_id,
                queued.operation_id,
                CommandStatus::Applied,
                None,
            );
            return Ok(());
        }
        let expected = match envelope.expected.member_config_revision.as_ref().copied() {
            Some(revision) => revision,
            None => {
                self.finish_command(
                    envelope.request_id,
                    queued.operation_id,
                    CommandStatus::Rejected,
                    Some(ErrorCode::InvalidValue),
                );
                return Ok(());
            }
        };
        let mutation = match self.mutation_from_command(&envelope) {
            Ok(mutation) => mutation,
            Err(code) => {
                self.finish_command(
                    envelope.request_id,
                    queued.operation_id,
                    CommandStatus::Rejected,
                    Some(code),
                );
                return Ok(());
            }
        };
        self.apply_event(CoordinatorEvent::LocalConfigRequested(LocalConfigRequest {
            request_id: envelope.request_id,
            expected_member_config_revision: expected,
            expected_grant_revision: envelope.expected.grant_revision.as_ref().copied(),
            mutation,
        }))?;
        self.sync_devices_from_config();
        Ok(())
    }

    fn mutation_from_command(
        &self,
        envelope: &rlb_control::LocalCommandEnvelope,
    ) -> Result<LocalConfigMutation, ErrorCode> {
        match envelope.command {
            LocalCommandName::SourceCaptureSet => {
                let source_id = envelope
                    .scope
                    .source_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let enabled = envelope
                    .args
                    .master_capture_enabled
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetMasterCapture { source_id, enabled })
            }
            LocalCommandName::SourcePublicationSet => {
                let source_id = envelope
                    .scope
                    .source_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let publish_requested = envelope
                    .args
                    .publish_requested
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetSourcePublication {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    source_id,
                    publish_requested,
                })
            }
            LocalCommandName::SourceBindingSet => {
                let source_id = envelope
                    .scope
                    .source_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let capture_method = envelope
                    .args
                    .capture_method
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let endpoint_mode = envelope
                    .args
                    .endpoint_mode
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let endpoint_id = envelope.args.endpoint_id.as_ref().cloned();
                let binding = DeviceBindingConfig::from_choice(endpoint_mode, endpoint_id);
                binding.validate().map_err(|_| ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetSourceBinding {
                    source_id,
                    capture_method,
                    binding,
                })
            }
            LocalCommandName::PlaybackBindingSet => {
                let endpoint_mode = envelope
                    .args
                    .endpoint_mode
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let endpoint_id = envelope.args.endpoint_id.as_ref().cloned();
                let binding = DeviceBindingConfig::from_choice(endpoint_mode, endpoint_id);
                binding.validate().map_err(|_| ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetPlaybackBinding { binding })
            }
            LocalCommandName::ReceiveBufferSet => {
                let mode = envelope
                    .args
                    .mode
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let target_ms = envelope.args.target_ms.as_ref().copied();
                Ok(LocalConfigMutation::SetReceiveBuffer {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    buffer: receive_buffer_from_command(mode, target_ms)?,
                })
            }
            LocalCommandName::SourceQualitySet => {
                let source_id = envelope
                    .scope
                    .source_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let selected_profile = envelope
                    .args
                    .selected_profile
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetSendQuality {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    source_id,
                    selected_profile,
                })
            }
            LocalCommandName::EmergencyStopSet => {
                let enabled = envelope
                    .args
                    .enabled
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetEmergencyStop { enabled })
            }
            LocalCommandName::CapabilityGrantSet => {
                let capability = envelope
                    .args
                    .capability
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let enabled = envelope
                    .args
                    .enabled
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetCapabilityGrant {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    capability,
                    enabled,
                })
            }
            LocalCommandName::MixMemberSet => {
                let remote_member_id = envelope
                    .scope
                    .remote_member_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetMemberMix {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    remote_member_id,
                    mix: MemberMix {
                        gain_db: envelope
                            .args
                            .gain_db
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                        mute: envelope
                            .args
                            .mute
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                        solo: envelope
                            .args
                            .solo
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                    },
                })
            }
            LocalCommandName::MixSourceSet => {
                let remote_member_id = envelope
                    .scope
                    .remote_member_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let source_id = envelope
                    .scope
                    .source_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetSourceMix {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    remote_member_id,
                    source_id,
                    mix: SourceMix {
                        gain_db: envelope
                            .args
                            .gain_db
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                        pan: envelope
                            .args
                            .pan
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                        mute: envelope
                            .args
                            .mute
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                    },
                })
            }
            LocalCommandName::RoomJoin => {
                let room_id = envelope
                    .scope
                    .room_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                let authority_url = envelope
                    .args
                    .authority_url
                    .as_ref()
                    .cloned()
                    .ok_or(ErrorCode::InvalidValue)?;
                let member_registry_endpoint = envelope
                    .args
                    .member_registry_endpoint
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetJoinTarget {
                    room_id,
                    target: JoinTarget {
                        authority_url,
                        member_registry_endpoint,
                    },
                })
            }
            LocalCommandName::RoomLeave => Ok(LocalConfigMutation::ClearJoin),
            LocalCommandName::SourceMonitorSet => {
                let source_id = envelope
                    .scope
                    .source_id
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetSourceMonitor {
                    source_id,
                    monitor: SourceMonitorConfig {
                        enabled: envelope
                            .args
                            .enabled
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                        gain_db: envelope
                            .args
                            .gain_db
                            .as_ref()
                            .copied()
                            .ok_or(ErrorCode::InvalidValue)?,
                    },
                })
            }
            LocalCommandName::StartupSet => Ok(LocalConfigMutation::SetStartup {
                startup: StartupConfig {
                    start_with_os: envelope
                        .args
                        .start_with_os
                        .as_ref()
                        .copied()
                        .ok_or(ErrorCode::InvalidValue)?,
                    start_engine_on_ui_open: envelope
                        .args
                        .start_engine_on_ui_open
                        .as_ref()
                        .copied()
                        .ok_or(ErrorCode::InvalidValue)?,
                },
            }),
            LocalCommandName::LoggingSet => Ok(LocalConfigMutation::SetLogging {
                logging: LoggingConfig {
                    enabled: envelope
                        .args
                        .enabled
                        .as_ref()
                        .copied()
                        .ok_or(ErrorCode::InvalidValue)?,
                    directory: envelope.args.directory.as_ref().cloned(),
                    max_bytes: envelope
                        .args
                        .max_bytes
                        .as_ref()
                        .copied()
                        .ok_or(ErrorCode::InvalidValue)?,
                    retention_count: envelope
                        .args
                        .retention_count
                        .as_ref()
                        .copied()
                        .ok_or(ErrorCode::InvalidValue)?,
                },
            }),
            LocalCommandName::DriverEnable | LocalCommandName::DriverDisable => {
                Ok(LocalConfigMutation::SetDriverSelection {
                    package_id: envelope.args.package_id.as_ref().cloned(),
                    enabled: matches!(envelope.command, LocalCommandName::DriverEnable),
                })
            }
            LocalCommandName::RoomReconnect
            | LocalCommandName::RoomReconnectCancel
            | LocalCommandName::RoomRequestReadmission
            | LocalCommandName::LoggingDelete
            | LocalCommandName::DiagnosticsExport
            | LocalCommandName::DiagnosticsAbStart
            | LocalCommandName::DiagnosticsAbCancel
            | LocalCommandName::DriverInstall
            | LocalCommandName::DriverUninstall
            | LocalCommandName::EngineRestart
            | LocalCommandName::EngineShutdown
            | LocalCommandName::ConfirmationPrepare => Err(ErrorCode::OperationNotSupported),
            LocalCommandName::RoomAutoReconnectSet => {
                let enabled = envelope
                    .args
                    .enabled
                    .as_ref()
                    .copied()
                    .ok_or(ErrorCode::InvalidValue)?;
                Ok(LocalConfigMutation::SetAutoReconnect {
                    room_id: Self::command_room(envelope, self.room_id)?,
                    enabled,
                })
            }
        }
    }

    fn command_room(
        envelope: &rlb_control::LocalCommandEnvelope,
        fallback: Option<RoomId>,
    ) -> Result<RoomId, ErrorCode> {
        envelope
            .scope
            .room_id
            .as_ref()
            .copied()
            .or(fallback)
            .ok_or(ErrorCode::RoomScopeMismatch)
    }

    fn dispatch_reconnect(
        &mut self,
        request_id: RequestId,
        operation_id: rlb_model::OperationId,
    ) -> Result<(), EngineError> {
        match self.apply_event(CoordinatorEvent::RegistryConnectRequested) {
            Ok(()) => {
                self.finish_command(request_id, operation_id, CommandStatus::Applied, None);
                Ok(())
            }
            Err(error) => {
                self.finish_command(
                    request_id,
                    operation_id,
                    CommandStatus::Rejected,
                    Some(ErrorCode::InvalidStateTransition),
                );
                Err(error)
            }
        }
    }

    fn connect_registry(&mut self) {
        let Some(target) = self.coordinator.config().join_target().cloned() else {
            self.note_open_error(ErrorCode::InvalidStateTransition);
            return;
        };
        let Some(room_id) = self.coordinator.config().attached_room_id() else {
            self.note_open_error(ErrorCode::InvalidStateTransition);
            return;
        };
        match self.ensure_member_token(target.member_registry_endpoint, room_id) {
            Ok(()) => {}
            Err(code) => {
                self.note_open_error(code);
                return;
            }
        }
        match RegistryClient::connect(target.member_registry_endpoint) {
            Ok(mut client) => match self.engine_hello_message(room_id) {
                Ok(hello) => {
                    if let Err(error) = client.send(&hello) {
                        eprintln!("registry hello: {error}");
                        self.note_open_error(ErrorCode::MemberDisconnected);
                        self.registry = None;
                        let _ = self.apply_event(CoordinatorEvent::RegistryDisconnected);
                        return;
                    }
                    eprintln!("registry tcp connected {:?}", client.endpoint());
                    self.registry = Some(client);
                }
                Err(code) => {
                    self.note_open_error(code);
                    self.registry = None;
                    let _ = self.apply_event(CoordinatorEvent::RegistryDisconnected);
                }
            },
            Err(error) => {
                eprintln!("registry connect: {error}");
                self.note_open_error(ErrorCode::RoomServiceUnreachable);
                self.registry = None;
                let _ = self.apply_event(CoordinatorEvent::RegistryDisconnected);
            }
        }
    }

    fn send_direct(&mut self, datagram: DirectDatagram) {
        let member_id = match &datagram {
            DirectDatagram::Hello(hello) => hello.common.receiver_member_id,
            DirectDatagram::SubscribeSet(set) => set.common.receiver_member_id,
            DirectDatagram::SubscribeAck(ack) => ack.common.receiver_member_id,
        };
        let Some(endpoint) = self.peer_endpoints.get(&member_id).copied() else {
            self.note_open_error(ErrorCode::MemberOffline);
            return;
        };
        if let Some(media) = self.media.as_ref() {
            media.send_direct(endpoint, datagram);
        }
    }

    fn synchronize_media_path(&mut self) -> Result<(), EngineError> {
        let plan = self.coordinator.media_receive_plan();
        let next_room = plan.room_id;
        let previous_room = self.media.as_ref().and_then(MediaPath::room_id);
        let publisher = self.devices.as_ref().map(DeviceSupervisor::mix_publisher);
        if let Some(media) = self.media.as_mut() {
            media.apply_receive_plan(plan, publisher.as_ref());
        }
        self.room_id = next_room;
        if previous_room == next_room {
            return Ok(());
        }

        self.peer_endpoints.clear();
        let source_ids: Vec<_> = self.encoders.keys().copied().collect();
        for source_id in source_ids {
            self.stop_encoder(source_id);
        }
        if next_room.is_some() {
            self.start_attached_encoders()?;
        }
        Ok(())
    }

    fn start_attached_encoders(&mut self) -> Result<(), EngineError> {
        let starts: Vec<_> = self
            .coordinator
            .diagnostics_snapshot()
            .sources
            .into_iter()
            .filter_map(|source| {
                let stream_epoch = source.stream_epoch?;
                (!self.encoders.contains_key(&source.source_id)).then_some(SourceEpochStartPlan {
                    source_id: source.source_id,
                    stream_epoch,
                    format: SourceFormat::for_source(
                        self.source_kind(source.source_id),
                        source.effective_profile,
                    ),
                })
            })
            .collect();
        for plan in starts {
            self.start_encoder(plan)?;
        }
        Ok(())
    }

    fn sync_devices_from_config(&mut self) {
        self.room_id = self.coordinator.config().attached_room_id();
        let config = self.coordinator.config();
        if let Some(media) = self.media.as_mut() {
            media.set_receive_buffer(config.room.settings.receive_buffer);
        }
        let Some(devices) = self.devices.as_mut() else {
            return;
        };
        devices.set_system_capture_method(config.system_capture_method);
        devices.set_bindings(device_bindings_from_config(config));
    }

    fn dispatch_confirmation_prepare(
        &mut self,
        envelope: rlb_control::LocalCommandEnvelope,
        operation_id: rlb_model::OperationId,
    ) -> Result<(), EngineError> {
        let Some(command) = envelope.args.command.as_ref().copied() else {
            self.finish_command(
                envelope.request_id,
                operation_id,
                CommandStatus::Rejected,
                Some(ErrorCode::InvalidValue),
            );
            return Ok(());
        };
        let Some(args_hash) = envelope.args.canonical_args_hash.as_ref().cloned() else {
            self.finish_command(
                envelope.request_id,
                operation_id,
                CommandStatus::Rejected,
                Some(ErrorCode::InvalidValue),
            );
            return Ok(());
        };
        let Some(control) = self.control.as_ref() else {
            self.finish_command(
                envelope.request_id,
                operation_id,
                CommandStatus::Rejected,
                Some(ErrorCode::UnexpectedError),
            );
            return Ok(());
        };
        match control.prepare_confirmation(command, args_hash) {
            Ok(token) => {
                self.pending_ops.remove(&envelope.request_id);
                control.store_result(
                    envelope.request_id,
                    command_result_json_with_effective(
                        envelope.request_id,
                        operation_id,
                        CommandStatus::Applied,
                        canonical_revisions(&self.coordinator, self.coordinator.config()),
                        serde_json::json!({
                            "confirmation_token": token,
                            "expires_in_s": 30,
                        }),
                        self.identity.member_id(),
                        self.run_id,
                    ),
                );
            }
            Err(_) => self.finish_command(
                envelope.request_id,
                operation_id,
                CommandStatus::Rejected,
                Some(ErrorCode::UnexpectedError),
            ),
        }
        Ok(())
    }

    fn dispatch_confirmed_lifecycle(
        &mut self,
        envelope: rlb_control::LocalCommandEnvelope,
        operation_id: rlb_model::OperationId,
    ) -> Result<(), EngineError> {
        let Some(token) = envelope.args.confirmation_token.as_ref() else {
            self.finish_command(
                envelope.request_id,
                operation_id,
                CommandStatus::Rejected,
                Some(ErrorCode::ConfirmationRequired),
            );
            return Ok(());
        };
        let Some(control) = self.control.as_ref() else {
            self.finish_command(
                envelope.request_id,
                operation_id,
                CommandStatus::Rejected,
                Some(ErrorCode::UnexpectedError),
            );
            return Ok(());
        };
        let bind_hash = match confirmation_bind_hash(envelope.command, &envelope.args) {
            Ok(hash) => hash,
            Err(_) => {
                self.finish_command(
                    envelope.request_id,
                    operation_id,
                    CommandStatus::Rejected,
                    Some(ErrorCode::UnexpectedError),
                );
                return Ok(());
            }
        };
        match control.consume_confirmation(token, envelope.command, bind_hash) {
            Ok(()) => {}
            Err(code) => {
                self.finish_command(
                    envelope.request_id,
                    operation_id,
                    CommandStatus::Rejected,
                    Some(code),
                );
                return Ok(());
            }
        }
        match envelope.command {
            LocalCommandName::EngineShutdown => {
                self.finish_command(
                    envelope.request_id,
                    operation_id,
                    CommandStatus::Applied,
                    None,
                );
                self.apply_event(CoordinatorEvent::ShutdownRequested)
            }
            LocalCommandName::EngineRestart => {
                self.finish_command(
                    envelope.request_id,
                    operation_id,
                    CommandStatus::Applied,
                    None,
                );
                self.store
                    .remove_run_descriptor(self.identity.member_id(), self.run_id)?;
                let exe = std::env::current_exe().map_err(EngineError::from)?;
                Command::new(exe)
                    .arg("run")
                    .spawn()
                    .map_err(EngineError::from)?;
                std::process::exit(0);
            }
            _ => Ok(()),
        }
    }

    fn dispatch_query(
        &mut self,
        envelope: rlb_control::LocalQueryEnvelope,
    ) -> Result<(), EngineError> {
        let revisions = canonical_revisions(&self.coordinator, self.coordinator.config());
        let Some(control) = self.control.as_ref() else {
            return Ok(());
        };
        match envelope.query {
            LocalQueryName::SnapshotGet | LocalQueryName::OperationGet => {}
            LocalQueryName::DevicesEnumerate => {
                let enumerator = EndpointEnumerator::new();
                let body = match enumerator.and_then(|enumerator| enumerator.enumerate()) {
                    Ok(catalog) => {
                        let flow = envelope.args.flow.as_ref().copied();
                        let endpoints: Vec<_> = catalog
                            .endpoints()
                            .iter()
                            .filter(|endpoint| match flow {
                                Some(DeviceFlow::Capture) => {
                                    endpoint.direction() == EndpointDirection::Capture
                                }
                                Some(DeviceFlow::Render) => {
                                    endpoint.direction() == EndpointDirection::Render
                                }
                                Some(DeviceFlow::All) | None => true,
                            })
                            .map(|endpoint| {
                                serde_json::json!({
                                    "endpoint_id": endpoint.endpoint_id(),
                                    "friendly_name": endpoint.friendly_name(),
                                    "direction": endpoint_direction_name(endpoint.direction()),
                                    "state": endpoint_state_name(endpoint.state()),
                                })
                            })
                            .collect();
                        query_result_json(
                            envelope.request_id,
                            envelope.query,
                            revisions,
                            serde_json::json!({
                                "flow": flow,
                                "endpoints": endpoints,
                            }),
                        )
                    }
                    Err(_) => query_error_json(
                        envelope.request_id,
                        ErrorCode::DeviceUnavailable,
                        self.identity.member_id(),
                        self.run_id,
                        revisions,
                    ),
                };
                control.store_result(envelope.request_id, body);
            }
            LocalQueryName::DriverInspect => {
                control.store_result(
                    envelope.request_id,
                    query_result_json(
                        envelope.request_id,
                        envelope.query,
                        revisions,
                        serde_json::json!({
                            "package_id": serde_json::Value::Null,
                            "version": serde_json::Value::Null,
                            "enabled": false,
                            "available": false,
                        }),
                    ),
                );
            }
            LocalQueryName::DiagnosticsCurrent => {
                let domain = envelope.args.domain.as_ref().map(String::as_str);
                let dropped = self.media.as_ref().map_or(0, |media| {
                    media
                        .metrics
                        .datagrams_dropped_ingress
                        .load(Ordering::Relaxed)
                });
                let codes: Vec<_> = self.open_errors.iter().map(|error| error.code).collect();
                let body = match domain {
                    Some("diagnostics") => query_result_json(
                        envelope.request_id,
                        envelope.query,
                        revisions,
                        serde_json::json!({
                            "control_reconnects": 0,
                            "malformed_messages": dropped,
                            "active_alert_codes": codes,
                        }),
                    ),
                    Some("control_reconnects") => query_result_json(
                        envelope.request_id,
                        envelope.query,
                        revisions,
                        serde_json::json!({ "control_reconnects": 0 }),
                    ),
                    Some("malformed_messages") => query_result_json(
                        envelope.request_id,
                        envelope.query,
                        revisions,
                        serde_json::json!({ "malformed_messages": dropped }),
                    ),
                    Some("active_alert_codes") => query_result_json(
                        envelope.request_id,
                        envelope.query,
                        revisions,
                        serde_json::json!({ "active_alert_codes": codes }),
                    ),
                    _ => query_error_json(
                        envelope.request_id,
                        ErrorCode::InvalidValue,
                        self.identity.member_id(),
                        self.run_id,
                        revisions,
                    ),
                };
                control.store_result(envelope.request_id, body);
            }
            LocalQueryName::AuditGet => {
                control.store_result(
                    envelope.request_id,
                    query_error_json(
                        envelope.request_id,
                        ErrorCode::OperationNotSupported,
                        self.identity.member_id(),
                        self.run_id,
                        revisions,
                    ),
                );
            }
        }
        Ok(())
    }

    fn finish_command(
        &mut self,
        request_id: RequestId,
        operation_id: rlb_model::OperationId,
        status: CommandStatus,
        code: Option<ErrorCode>,
    ) {
        self.pending_ops.remove(&request_id);
        if let Some(control) = self.control.as_ref() {
            control.store_result(
                request_id,
                command_result_json(
                    request_id,
                    operation_id,
                    status,
                    canonical_revisions(&self.coordinator, self.coordinator.config()),
                    code,
                    self.identity.member_id(),
                    self.run_id,
                ),
            );
        }
    }

    fn publish_control_snapshot(&mut self) {
        let live = {
            let Some(control) = self.control.as_ref() else {
                return;
            };
            SnapshotLive {
                event_cursor: control.state.event_cursor.load(Ordering::Relaxed),
                engine_faulted: false,
                open_errors: self.open_errors.clone(),
                dropped_ingress: self.media.as_ref().map_or(0, |media| {
                    media
                        .metrics
                        .datagrams_dropped_ingress
                        .load(Ordering::Relaxed)
                }),
                malformed_messages: control.state.malformed_messages.load(Ordering::Relaxed),
                control_reconnects: control.state.control_reconnects.load(Ordering::Relaxed),
                operations: control.operation_projections(),
            }
        };
        let render = self
            .devices
            .as_ref()
            .map(|devices| devices.snapshot().render)
            .unwrap_or(SlotStatus::Unavailable {
                endpoint_id: String::new(),
            });
        let snapshot = build_local_snapshot(
            &self.coordinator,
            self.identity,
            self.run_id,
            &self.store,
            &render,
            live,
        );
        let revisions = snapshot.revisions;
        let engine_state = snapshot.engine.state;
        let engine_build = snapshot.engine.build.clone();
        let room = snapshot.room.clone();
        let playback = snapshot.playback.clone();
        let emit_engine = self.last_engine_state != Some(engine_state);
        let emit_room =
            self.last_room_connected != Some(room.connected) || self.room_id != room.room_id;
        let emit_playback = self.last_playback_available != Some(playback.available);
        if let Some(control) = self.control.as_ref() {
            if emit_engine {
                control.emit_event(
                    LocalEventKind::EngineStateChanged,
                    revisions,
                    serde_json::json!({
                        "state": engine_state,
                        "build": engine_build,
                    }),
                );
            }
            if emit_room {
                control.emit_event(
                    LocalEventKind::RoomStateChanged,
                    revisions,
                    serde_json::to_value(&room).unwrap_or(serde_json::json!({})),
                );
            }
            if emit_playback {
                control.emit_event(
                    LocalEventKind::PlaybackStateChanged,
                    revisions,
                    serde_json::to_value(&playback).unwrap_or(serde_json::json!({})),
                );
            }
            control.publish_snapshot(snapshot, revisions);
        }
        self.last_engine_state = Some(engine_state);
        self.last_room_connected = Some(room.connected);
        self.last_playback_available = Some(playback.available);
    }

    fn sync_slot(
        &mut self,
        source_id: SourceId,
        status: &SlotStatus,
        epoch: StreamEpoch,
    ) -> Result<(), EngineError> {
        match status {
            SlotStatus::Running {
                endpoint_id,
                friendly_name,
            } => {
                let previous = self.mirrors.get(&source_id).cloned().unwrap_or_default();
                if previous.stream_started {
                    let endpoint_changed =
                        previous.endpoint_id.as_deref() != Some(endpoint_id.as_str());
                    let epoch_changed = previous.epoch != Some(epoch);
                    if endpoint_changed || epoch_changed {
                        self.stop_encoder(source_id);
                        let reason = if endpoint_changed {
                            EpochChangeReason::EndpointChanged
                        } else {
                            EpochChangeReason::TimelineDiscontinuity
                        };
                        self.apply_event(CoordinatorEvent::SourceStreamStopped {
                            source_id,
                            reason: SourceStopReason::Reconfigure(reason),
                        })?;
                        self.report_stream_started(
                            source_id,
                            epoch,
                            endpoint_id.clone(),
                            reason,
                            friendly_name,
                        )?;
                    }
                }
            }
            SlotStatus::Unavailable { .. } => {
                let previous = self.mirrors.get(&source_id).cloned().unwrap_or_default();
                if previous.stream_started {
                    self.stop_encoder(source_id);
                    self.mirrors.insert(source_id, SourceMirror::default());
                    self.apply_event(CoordinatorEvent::SourceStreamStopped {
                        source_id,
                        reason: SourceStopReason::SourceUnavailable,
                    })?;
                }
            }
        }
        Ok(())
    }

    fn flush_pending_starts(&mut self) -> Result<(), EngineError> {
        let pending: Vec<_> = self
            .pending_starts
            .iter()
            .map(|(source_id, reason)| SourceRestartPlan {
                source_id: *source_id,
                reason: *reason,
            })
            .collect();
        for plan in pending {
            self.try_start_pending(plan)?;
        }
        Ok(())
    }

    fn publish_metrics(&mut self) -> Result<(), EngineError> {
        self.apply_event(CoordinatorEvent::AudioMetricsUpdated(
            self.metrics.snapshot(),
        ))?;
        let updates: Vec<(SourceId, SenderMetrics)> = self
            .encoders
            .iter()
            .map(|(source_id, encoder)| (*source_id, encoder.metrics_snapshot()))
            .collect();
        for (source_id, metrics) in updates {
            self.apply_event(CoordinatorEvent::LocalSenderMetrics { source_id, metrics })?;
        }
        Ok(())
    }

    fn run_shutdown_stage(&mut self, stage: ShutdownStage) -> Result<(), EngineError> {
        self.shutting_down = true;
        match stage {
            ShutdownStage::CloseSendGate => {
                if let Some(media) = self.media.as_mut() {
                    media.set_send_gate(true);
                }
            }
            ShutdownStage::LeaveRegistry => {
                self.registry = None;
                self.reconnect_at = None;
            }
            ShutdownStage::StopCapture | ShutdownStage::StopMedia => {
                let source_ids: Vec<_> = self.encoders.keys().copied().collect();
                for source_id in source_ids {
                    self.stop_encoder(source_id);
                }
                if matches!(stage, ShutdownStage::StopMedia)
                    && let Some(media) = self.media.as_mut()
                {
                    media.shutdown();
                }
            }
            ShutdownStage::StopRender | ShutdownStage::ReleaseAudio => {
                if let Some(mut devices) = self.devices.take() {
                    devices.reclaim_mix_readers();
                    let _ = devices.shutdown();
                }
            }
            ShutdownStage::FlushPersistence => {}
            ShutdownStage::RemoveRuntimeState => {
                self.store
                    .remove_run_descriptor(self.identity.member_id(), self.run_id)?;
                if let Some(control) = self.control.as_mut() {
                    control.shutdown();
                }
            }
        }
        Ok(())
    }

    fn drive_shutdown(&mut self) -> Result<(), EngineError> {
        while self.coordinator.engine_state() == EngineState::ShuttingDown {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn source_kind(&self, source_id: SourceId) -> SourceKind {
        if source_id == self.identity.source_id(SourceKind::SystemAudio) {
            SourceKind::SystemAudio
        } else {
            SourceKind::Microphone
        }
    }

    fn print_status(&self) {
        let snapshot = self.coordinator.diagnostics_snapshot();
        let send_sources = self
            .last_send
            .as_ref()
            .map(|plan| plan.sources.len())
            .unwrap_or(0);
        let sent = self
            .media
            .as_ref()
            .map(|media| media.metrics.datagrams_sent.load(Ordering::Relaxed))
            .unwrap_or(0);
        let received = self
            .media
            .as_ref()
            .map(|media| media.metrics.datagrams_received.load(Ordering::Relaxed))
            .unwrap_or(0);
        let assembled = self
            .media
            .as_ref()
            .map(|media| media.metrics.units_assembled.load(Ordering::Relaxed))
            .unwrap_or(0);
        print!(
            "engine={:?} registry={:?} mix_gen={} send_sources={send_sources} udp_sent={sent} udp_recv={received} assembled={assembled} ",
            snapshot.engine_state, snapshot.registry_state, self.last_mix_epoch
        );
        for source in &snapshot.sources {
            let encoded = self
                .encoders
                .get(&source.source_id)
                .map(SourceEncoder::encoded_units)
                .unwrap_or(0);
            let gaps = self
                .encoders
                .get(&source.source_id)
                .map(SourceEncoder::gap_records)
                .unwrap_or(0);
            print!(
                "{:?} capture={:?} units={encoded} gaps={gaps} ",
                self.source_kind(source.source_id),
                source.capture_state
            );
        }
        let jitter_exact: u64 = snapshot
            .sources
            .iter()
            .filter_map(|source| {
                self.media
                    .as_ref()
                    .and_then(|media| media.reader_metrics(source.source_id))
                    .map(|metrics| metrics.exact_frames.load(Ordering::Relaxed))
            })
            .sum();
        println!(
            "audio_frames={} render_frames={} jitter_exact={jitter_exact}",
            snapshot.audio.capture_frames_total, snapshot.audio.render_frames_total
        );
    }
}

fn publish_mix_plan(
    publisher: &MixGraphPublisher,
    previous: Option<&AudioMixPlan>,
    plan: &AudioMixPlan,
) -> bool {
    let mut ok = true;
    let previous_ids: Vec<SourceId> = previous
        .map(|plan| plan.sources.iter().map(|source| source.source_id).collect())
        .unwrap_or_default();
    for source_id in previous_ids {
        if !plan
            .sources
            .iter()
            .any(|source| source.source_id == source_id)
        {
            ok &= publisher
                .try_publish(MixGraphCommand::RemoveSource { source_id })
                .is_ok();
        }
    }
    for source in &plan.sources {
        ok &= publisher
            .try_publish(MixGraphCommand::SetSourceMix {
                source_id: source.source_id,
                source_mix: source.source_mix,
            })
            .is_ok();
        ok &= publisher
            .try_publish(MixGraphCommand::SetMemberMix {
                member_id: source.member_id,
                member_mix: source.member_mix,
            })
            .is_ok();
        ok &= publisher
            .try_publish(MixGraphCommand::SetSourceRouted {
                source_id: source.source_id,
                routed: source.routed,
            })
            .is_ok();
        ok &= publisher
            .try_publish(MixGraphCommand::SetMemberSoloEligible {
                member_id: source.member_id,
                eligible: source.member_solo_eligible,
            })
            .is_ok();
    }
    ok
}

fn spawn_enter_watcher(stop: Arc<AtomicBool>) {
    thread::Builder::new()
        .name("rlb-engine-stdin".to_owned())
        .spawn(move || {
            let mut stdin = io::stdin();
            let mut byte = [0_u8; 1];
            while stdin.read(&mut byte).is_ok() {
                if byte[0] == b'\n' || byte[0] == b'\r' {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
            stop.store(true, Ordering::Relaxed);
        })
        .expect("stdin watcher");
}

fn device_bindings_from_config(config: &MemberConfig) -> DeviceBindings {
    DeviceBindings {
        system_audio: selection_from_binding(&config.system_audio_binding),
        microphone: selection_from_binding(&config.microphone_binding),
        render: selection_from_binding(&config.playback_binding),
    }
}

fn selection_from_binding(binding: &DeviceBindingConfig) -> EndpointSelection {
    match binding.endpoint_mode {
        EndpointMode::FollowDefault => EndpointSelection::follow_default(),
        EndpointMode::Pinned => {
            EndpointSelection::fixed(binding.endpoint_id.clone().unwrap_or_default())
        }
    }
}

fn receive_buffer_from_command(
    mode: ReceiveBufferMode,
    target_ms: Option<u16>,
) -> Result<ReceiveBufferConfig, ErrorCode> {
    match mode {
        ReceiveBufferMode::Automatic => Ok(ReceiveBufferConfig::Automatic),
        ReceiveBufferMode::Manual => {
            let ms = target_ms.ok_or(ErrorCode::InvalidValue)?;
            let frames = ms.checked_mul(48).ok_or(ErrorCode::InvalidValue)?;
            let target = ManualBufferTarget::new(frames).map_err(|_| ErrorCode::InvalidValue)?;
            Ok(ReceiveBufferConfig::Manual {
                target_frames: target,
            })
        }
    }
}

fn endpoint_direction_name(direction: EndpointDirection) -> &'static str {
    match direction {
        EndpointDirection::Capture => "capture",
        EndpointDirection::Render => "render",
        EndpointDirection::LoopbackCapture => "loopback_capture",
    }
}

fn endpoint_state_name(state: EndpointState) -> &'static str {
    match state {
        EndpointState::Active => "active",
        EndpointState::Disabled => "disabled",
        EndpointState::Unplugged => "unplugged",
        EndpointState::NotPresent => "not_present",
    }
}

fn print_poll(poll: rlb_audio_windows::SupervisorPoll) {
    print_slot("system_audio", &poll.system_audio);
    print_slot("microphone", &poll.microphone);
    print_slot("render", &poll.render);
}

fn print_slot(name: &str, status: &SlotStatus) {
    match status {
        SlotStatus::Running {
            endpoint_id,
            friendly_name,
        } => println!("{name}: running {friendly_name} ({endpoint_id})"),
        SlotStatus::Unavailable { endpoint_id } => {
            println!("{name}: unavailable ({endpoint_id})")
        }
    }
}

/// Shared helper used by the audio-smoke command.
pub fn drain_capture(worker: Option<&mut CaptureWorker>) -> u64 {
    let Some(worker) = worker else {
        return 0;
    };
    let Some(consumer) = worker.consumer() else {
        return 0;
    };
    let mut units = 0_u64;
    while let Some(block) = consumer.try_pop() {
        units = units.saturating_add(1);
        let _ = consumer.release(block);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_run_rejects_loop_self_configuration() {
        let options = EngineOptions {
            command: EngineCommand::Run,
            wait_enter: false,
            loop_self: true,
            force_capture: false,
            duration: None,
        };
        let error = run_with(options).expect_err("ordinary run must reject");
        assert!(matches!(
            error,
            EngineError::Persist(message) if message == LOOP_SELF_CONFIGURATION_ERROR
        ));
    }

    #[test]
    fn explicit_media_loop_command_allows_loop_self() {
        let options = EngineOptions {
            command: EngineCommand::MediaLoop,
            wait_enter: false,
            loop_self: true,
            force_capture: true,
            duration: Some(Duration::from_secs(2)),
        };
        validate_engine_options(&options).expect("media-loop command");
    }
}
