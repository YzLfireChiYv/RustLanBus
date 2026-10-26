use rlb_model::{DirectSessionId, MemberId, RunId, SourceId, StreamEpoch};
use rlb_peer::{
    AdmissionSnapshot, AdmittedMetadata, AdmittedPcm, Catalogue, ChannelLayout, PcmEncoding,
    PcmFormat, PeerInstance, PublicationEpoch, PublicationState, SourceAdmission, SourceEntry,
    SourceKind,
};

use super::*;

fn ids() -> (MemberId, RunId, SourceId, DirectSessionId, StreamEpoch) {
    (
        MemberId::from_bytes([1; 16]).unwrap(),
        RunId::from_bytes([2; 16]).unwrap(),
        SourceId::from_bytes([3; 16]).unwrap(),
        DirectSessionId::from_bytes([4; 16]).unwrap(),
        StreamEpoch::from_bytes([5; 16]).unwrap(),
    )
}

fn format() -> PcmFormat {
    PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap()
}

fn remote(epoch: u64, _session: DirectSessionId, _fence: u64) -> TimelineId {
    let (member, run, source, _, _) = ids();
    TimelineId::Remote {
        member,
        run,
        source,
        epoch: PublicationEpoch::new(epoch).unwrap(),
    }
}

fn permission(session: DirectSessionId, fence: u64) -> Permission {
    Permission::Remote {
        peer_instance: PeerInstance::new(11).unwrap(),
        session,
        fence,
    }
}

fn source(
    epoch: u64,
    session: DirectSessionId,
    fence: u64,
    activity: SourceActivity,
) -> RenderSource {
    RenderSource {
        timeline: remote(epoch, session, fence),
        permission: permission(session, fence),
        activity,
        format: format(),
        kind: Some(SourceKind::Microphone),
    }
}

fn policy() -> TimingPolicy {
    TimingPolicy {
        brief_missing_frames: 4,
        delivery_guard_frames: 64,
        excess_discard_frames: 2_000,
        ..TimingPolicy::provisional()
    }
}

fn core() -> RenderCore {
    RenderCore::new(
        RenderCapacities {
            sources: 4,
            window_frames: 8_192,
            max_block_frames: 4_096,
            max_quantum_frames: 128,
            output_channels: 1,
            output_layout: ChannelLayout::Mono,
            output_rate_hz: 48_000,
            output_generation: 7,
            quantum_frames: 64,
        },
        policy(),
    )
    .unwrap()
}

fn block(
    epoch: u64,
    session: DirectSessionId,
    fence: u64,
    start: u64,
    samples: &[f64],
) -> SourceBlock<'_> {
    SourceBlock {
        timeline: remote(epoch, session, fence),
        permission: permission(session, fence),
        format: format(),
        start_frame: start,
        arrived_at_ms: 10,
        samples,
    }
}

fn service(padding: u32, missed: bool) -> ServiceObservation {
    ServiceObservation {
        generation: 7,
        selected: true,
        quantum_frames: 64,
        target_padding_frames: 64,
        writable_frames: 128,
        padding_frames: padding,
        missed_service: missed,
    }
}

fn admit(core: &mut RenderCore, sources: Vec<RenderSource>) {
    core.queue_admission(RenderAdmission {
        revision: core.applied_revision() + 1,
        sources,
    })
    .unwrap();
    core.apply_admission().unwrap();
}

fn tone(frames: usize, value: f64) -> Vec<f64> {
    vec![value; frames]
}

#[test]
fn a_late_join_and_a_large_gap_anchor_the_current_window() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active)],
    );
    for start in [100_000, 1_000_000] {
        let report = core
            .push_block(block(1, session, 1, start, &tone(1_024, 0.25)))
            .unwrap();
        assert_eq!(report.stored_frames, 1_024);
        assert_eq!(report.ahead_frames, 0);
        let mut heard = false;
        // A new sinc segment has an explicit filter startup delay. Feed a
        // normal continuing stream and require actual nonzero output after it.
        for step in 0..8 {
            core.push_block(block(1, session, 1, start + 1_024 + step * 64, &[0.25; 64]))
                .unwrap();
            let PrepareOutcome::Prepared(handle) =
                core.prepare_quantum(&service(0, false)).unwrap()
            else {
                panic!("current output");
            };
            heard |= handle.peak_abs > 0.2;
            assert!(
                core.source_status(remote(1, session, 1))
                    .unwrap()
                    .closed_before
                    >= start
            );
            let native_start = core.submitted_frames();
            core.complete(SubmissionReceipt {
                generation: 7,
                quantum_id: handle.id,
                success: true,
                start_frame: native_start,
                end_frame: native_start + u64::from(handle.frames),
            })
            .unwrap();
        }
        assert!(heard, "the current source span never reached output");
        let stale = core
            .push_block(block(1, session, 1, 0, &[0.9; 64]))
            .unwrap();
        assert_eq!(stale.stale_frames, 64);
    }
}

#[test]
fn obsolete_runs_release_history_while_same_run_retirement_stays() {
    let mut core = core();
    let (member, run, _, session, _) = ids();
    let old = source(1, session, 1, SourceActivity::Active);
    admit(&mut core, vec![old]);
    admit(&mut core, Vec::new());
    core.retire_obsolete_remote_runs(member, run);
    assert!(core.source_status(old.timeline).is_some());
    let new_run = RunId::from_bytes([9; 16]).unwrap();
    core.retire_obsolete_remote_runs(member, new_run);
    assert!(core.source_status(old.timeline).is_none());
    let next = RenderSource {
        timeline: TimelineId::Remote {
            member,
            run: new_run,
            source: SourceId::from_bytes([10; 16]).unwrap(),
            epoch: PublicationEpoch::new(1).unwrap(),
        },
        ..old
    };
    admit(&mut core, vec![next]);
    assert!(core.source_status(next.timeline).unwrap().active);
    assert!(
        core.push_block(block(1, session, 1, 0, &[0.9; 64]))
            .is_err()
    );
}

#[test]
fn closed_source_frames_survive_output_failure_reprime_and_permission_changes() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active)],
    );
    core.push_block(block(1, session, 1, 0, &tone(4_096, 0.25)))
        .unwrap();
    let prepared = core.prepare_quantum(&service(0, false)).unwrap();
    let PrepareOutcome::Prepared(handle) = prepared else {
        panic!("expected a prepared quantum");
    };
    let floor = core
        .source_status(remote(1, session, 1))
        .unwrap()
        .closed_before;
    assert!(floor > 0);
    let failed = core
        .complete(SubmissionReceipt {
            generation: 7,
            quantum_id: handle.id,
            success: false,
            start_frame: 0,
            end_frame: 0,
        })
        .unwrap();
    assert!(!failed.accepted);
    assert_eq!(failed.submitted_output_frames, 0);
    assert!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before
            >= floor
    );
    core.set_output(OutputSetup {
        generation: 8,
        sample_rate_hz: 48_000,
        channels: 1,
        layout: ChannelLayout::Mono,
        quantum_frames: 64,
    })
    .unwrap();

    let replacement = DirectSessionId::from_bytes([8; 16]).unwrap();
    admit(
        &mut core,
        vec![source(1, replacement, 1, SourceActivity::Active)],
    );
    assert!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before
            >= floor
    );
    admit(
        &mut core,
        vec![source(1, replacement, 1, SourceActivity::Closed)],
    );
    admit(
        &mut core,
        vec![source(1, replacement, 1, SourceActivity::Active)],
    );
    assert!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before
            >= floor
    );

    core.fail_next_process(remote(1, session, 1));
    let mut resumed_service = service(0, false);
    resumed_service.generation = 8;
    let again = core.prepare_quantum(&resumed_service).unwrap();
    let PrepareOutcome::Prepared(handle) = again else {
        panic!("silence is still a prepared quantum");
    };
    assert!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before
            >= floor
    );
    assert!(
        core.complete(SubmissionReceipt {
            generation: 1,
            quantum_id: handle.id,
            success: true,
            start_frame: 0,
            end_frame: u64::from(handle.frames),
        })
        .is_err()
    );
    assert_eq!(core.submitted_frames(), 0);
    core.complete(SubmissionReceipt {
        generation: 8,
        quantum_id: handle.id,
        success: true,
        start_frame: 100,
        end_frame: 100 + u64::from(handle.frames),
    })
    .unwrap();
    assert_eq!(core.submitted_frames(), 100 + u64::from(handle.frames));
}

#[test]
fn a_new_epoch_is_a_new_timeline_and_a_stale_fence_does_not_extend_it() {
    let mut core = core();
    let (_, _, source_id, session, _) = ids();
    admit(
        &mut core,
        vec![source(1, session, 4, SourceActivity::Active)],
    );
    core.push_block(block(1, session, 4, 0, &tone(128, 0.2)))
        .unwrap();
    let before = core
        .source_status(remote(1, session, 4))
        .unwrap()
        .high_water;
    assert!(
        core.push_block(block(1, session, 2, 128, &tone(16, 0.2)))
            .is_err()
    );
    assert_eq!(
        core.source_status(remote(1, session, 4))
            .unwrap()
            .high_water,
        before
    );
    admit(
        &mut core,
        vec![source(2, session, 4, SourceActivity::Active)],
    );
    assert!(core.source_status(remote(1, session, 4)).is_none());
    core.push_block(block(2, session, 4, 0, &tone(32, 0.2)))
        .unwrap();
    let fresh = core.source_status(remote(2, session, 4)).unwrap();
    assert_eq!(fresh.high_water, 32);
    assert_eq!(fresh.closed_before, 0);
    assert_ne!(source_id, SourceId::from_bytes([9; 16]).unwrap());
}

#[test]
fn one_unprimed_source_stays_silent_while_another_can_prepare() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let mut second = source(1, session, 1, SourceActivity::Active);
    if let TimelineId::Remote { source, .. } = &mut second.timeline {
        *source = SourceId::from_bytes([6; 16]).unwrap();
    }
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active), second],
    );
    core.push_block(block(1, session, 1, 0, &tone(4_096, 0.4)))
        .unwrap();
    let prepared = core.prepare_quantum(&service(0, false)).unwrap();
    let PrepareOutcome::Prepared(handle) = prepared else {
        panic!("output starts while one source is still priming");
    };
    assert!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before
            > 0
    );
    assert_eq!(
        core.source_status(second.timeline).unwrap().closed_before,
        0
    );
    assert!(
        core.output_interleaved()
            .iter()
            .all(|sample| sample.is_finite())
    );
    assert_eq!(core.output_interleaved().len(), handle.frames as usize);
    let preset = core.preset();
    assert_eq!(preset.sinc_taps, 256);
    assert_eq!(preset.phases, 128);
    assert_eq!(preset.window, "BlackmanHarris2");
    assert!(preset.cutoff.is_some());
    assert!(preset.output_delay.unwrap_or(0) > 0);
}

#[test]
fn waiting_does_not_close_source_time_and_a_stall_reprimes_forward() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active)],
    );
    core.push_block(block(1, session, 1, 0, &tone(4_096, 0.3)))
        .unwrap();
    assert!(matches!(
        core.prepare_quantum(&service(64, false)).unwrap(),
        PrepareOutcome::Waiting { .. }
    ));
    assert_eq!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before,
        0
    );
    let primed = core.prepare_quantum(&service(0, false)).unwrap();
    assert!(matches!(primed, PrepareOutcome::Prepared(_)));
    let floor = core
        .source_status(remote(1, session, 1))
        .unwrap()
        .closed_before;
    core.complete(SubmissionReceipt {
        generation: 7,
        quantum_id: 1,
        success: true,
        start_frame: 0,
        end_frame: 64,
    })
    .unwrap();
    let stalled = core.prepare_quantum(&service(0, true)).unwrap();
    assert!(matches!(stalled, PrepareOutcome::Prepared(_)));
    let after = core.source_status(remote(1, session, 1)).unwrap();
    assert!(after.closed_before >= floor);
    assert!(after.ratio <= 1.0 + 1_000.0 / 1_000_000.0 + 1.0e-9);
}

#[test]
fn closing_one_stream_retires_only_its_render_lane_and_preserves_the_catalogue() {
    let (member, run, system_id, session, _) = ids();
    let system = SourceEntry {
        id: system_id,
        epoch: PublicationEpoch::new(1).unwrap(),
        kind: SourceKind::System,
        state: PublicationState::Active,
        format: Some(format()),
    };
    let microphone = SourceEntry {
        id: SourceId::from_bytes([6; 16]).unwrap(),
        kind: SourceKind::Microphone,
        ..system
    };
    let catalogue = Catalogue::new(vec![system, microphone]).unwrap();
    let mut snapshot = AdmissionSnapshot {
        revision: 1,
        peer_instance: PeerInstance::new(11),
        connected: true,
        receive_ready: true,
        send_ready: true,
        quiesced: false,
        remote_run: Some(run),
        session: Some(session),
        sources: vec![
            SourceAdmission {
                entry: system,
                fence: 9,
                receive_ready: true,
            },
            SourceAdmission {
                entry: microphone,
                fence: 12,
                receive_ready: true,
            },
        ],
    };
    let before = remote_render_sources(member, &snapshot).unwrap();
    assert_eq!(before.len(), 2);
    let mut core = core();
    admit(&mut core, before.clone());
    core.push_block(SourceBlock {
        timeline: before[0].timeline,
        permission: before[0].permission,
        format: format(),
        start_frame: 0,
        arrived_at_ms: 4,
        samples: &[0.25; 8],
    })
    .unwrap();

    snapshot.revision += 1;
    snapshot.sources[1].receive_ready = false;
    let after = remote_render_sources(member, &snapshot).unwrap();
    assert_eq!(after, before[..1]);
    assert_eq!(catalogue.as_slice(), &[system, microphone]);
    assert_eq!(snapshot.sources.len(), 2);
    assert_eq!(snapshot.sources[1].entry, microphone);
    admit(&mut core, after);
    assert!(core.source_status(before[0].timeline).unwrap().active);
    assert!(!core.source_status(before[1].timeline).unwrap().active);
    // The surviving permission still admits the next ordinary source span.
    core.push_block(SourceBlock {
        timeline: before[0].timeline,
        permission: before[0].permission,
        format: format(),
        start_frame: 8,
        arrived_at_ms: 5,
        samples: &[0.25; 8],
    })
    .unwrap();
    assert!(
        core.push_block(SourceBlock {
            timeline: before[1].timeline,
            permission: before[1].permission,
            format: format(),
            start_frame: 0,
            arrived_at_ms: 5,
            samples: &[0.25; 8],
        })
        .is_err()
    );
}

#[test]
fn remote_admission_requires_the_matching_source_entry() {
    let (member, run, source_id, session, _) = ids();
    let entry = SourceEntry {
        id: source_id,
        epoch: PublicationEpoch::new(1).unwrap(),
        kind: SourceKind::System,
        state: PublicationState::Active,
        format: None,
    };
    let incomplete = AdmissionSnapshot {
        revision: 1,
        peer_instance: PeerInstance::new(11),
        connected: true,
        receive_ready: true,
        send_ready: true,
        quiesced: false,
        remote_run: Some(run),
        session: Some(session),
        sources: vec![SourceAdmission {
            entry,
            fence: 9,
            receive_ready: true,
        }],
    };
    assert!(remote_render_sources(member, &incomplete).is_err());
    let entry = SourceEntry {
        format: Some(format()),
        ..entry
    };
    let view = AdmissionSnapshot {
        sources: vec![SourceAdmission {
            entry,
            fence: 9,
            receive_ready: true,
        }],
        ..incomplete
    };
    let built = remote_render_sources(member, &view).unwrap();
    let borrowed = block_from_admitted_pcm(AdmittedPcm {
        metadata: AdmittedMetadata {
            peer_instance: PeerInstance::new(11),
            admission_revision: 1,
            member_id: member,
            remote_run: run,
            entry,
            sequence: 1,
            start_frame: 0,
            frames: 1,
            session,
            send_generation: 1,
            receive_generation: 1,
            source_fence: 9,
            arrived_at_ms: 4,
        },
        samples: &[0.1],
    })
    .unwrap();
    assert_eq!(borrowed.samples, &[0.1]);
    assert_eq!(borrowed.permission, permission(session, 9));
    assert_eq!(built.len(), 1);
    assert_eq!(built[0].activity, SourceActivity::Active);
    assert_eq!(built[0].format.channels(), 1);
    let mut core = core();
    core.queue_admission(RenderAdmission {
        revision: 4,
        sources: built,
    })
    .unwrap();
    core.apply_admission().unwrap();
    core.push_block(block(1, session, 9, 0, &tone(8, 0.1)))
        .unwrap();
    core.queue_admission(RenderAdmission {
        revision: 5,
        sources: vec![source(1, session, 12, SourceActivity::Active)],
    })
    .unwrap();
    core.apply_admission().unwrap();
    assert!(
        core.push_block(block(1, session, 9, 8, &tone(8, 0.1)))
            .is_err()
    );
    assert_eq!(
        core.source_status(remote(1, session, 12))
            .unwrap()
            .high_water,
        8
    );
}

#[test]
fn out_of_order_insertion_preserves_the_first_sample_and_the_floor() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active)],
    );
    core.push_block(block(1, session, 1, 4, &tone(4, 0.5)))
        .unwrap();
    let report = core
        .push_block(block(
            1,
            session,
            1,
            0,
            &[0.25, 0.25, 0.25, 0.25, 9.0, 9.0, 9.0, 9.0],
        ))
        .unwrap();
    assert!(report.stored_frames >= 4);
    assert!(report.duplicate_frames >= 4);
    assert_eq!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .high_water,
        8
    );
    assert_eq!(
        core.source_status(remote(1, session, 1))
            .unwrap()
            .closed_before,
        0
    );
    assert!(
        core.push_block(block(1, session, 1, u64::MAX - 2, &tone(8, 0.1)))
            .is_err()
    );
}

fn prepared(core: &mut RenderCore, observation: ServiceObservation) -> QuantumHandle {
    match core.prepare_quantum(&observation).unwrap() {
        PrepareOutcome::Prepared(handle) => handle,
        PrepareOutcome::Waiting { .. } => panic!("selected output service should prepare"),
    }
}

fn accept(core: &mut RenderCore, handle: QuantumHandle) {
    let start = core.submitted_frames();
    core.complete(SubmissionReceipt {
        generation: handle.generation,
        quantum_id: handle.id,
        success: true,
        start_frame: start,
        end_frame: start + u64::from(handle.frames),
    })
    .unwrap();
}

#[test]
fn admission_revisions_preflight_and_epoch_floors_are_atomic() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let original = source(1, session, 1, SourceActivity::Active);
    admit(&mut core, vec![original]);
    core.push_block(block(1, session, 1, 0, &tone(512, 0.2)))
        .unwrap();
    let before = core.source_status(original.timeline).unwrap();
    assert!(
        core.queue_admission(RenderAdmission {
            revision: 0,
            sources: Vec::new(),
        })
        .is_err()
    );
    assert!(
        core.queue_admission(RenderAdmission {
            revision: 1,
            sources: vec![RenderSource {
                activity: SourceActivity::Closed,
                ..original
            }],
        })
        .is_err()
    );
    let mut invalid = source(1, session, 1, SourceActivity::Active);
    if let TimelineId::Remote { source, .. } = &mut invalid.timeline {
        *source = SourceId::from_bytes([9; 16]).unwrap();
    }
    invalid.format = PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F64Le, 64).unwrap();
    core.queue_admission(RenderAdmission {
        revision: 2,
        sources: vec![
            RenderSource {
                activity: SourceActivity::Closed,
                ..original
            },
            invalid,
        ],
    })
    .unwrap();
    assert!(core.apply_admission().is_err());
    assert_eq!(core.applied_revision(), 1);
    let after = core.source_status(original.timeline).unwrap();
    assert!(after.active);
    assert_eq!(after.closed_before, before.closed_before);
    assert_eq!(after.high_water, before.high_water);
    admit(
        &mut core,
        vec![source(2, session, 1, SourceActivity::Active)],
    );
    core.queue_admission(RenderAdmission {
        revision: 3,
        sources: vec![original],
    })
    .unwrap();
    assert!(core.apply_admission().is_err());
    assert!(core.source_status(original.timeline).is_none());
    assert!(core.source_status(remote(2, session, 1)).unwrap().active);
}

#[test]
fn closing_one_source_preserves_the_other_lane_and_bounds_settings() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let first = source(1, session, 1, SourceActivity::Active);
    let mut second = first;
    if let TimelineId::Remote { source, .. } = &mut second.timeline {
        *source = SourceId::from_bytes([6; 16]).unwrap();
    }
    admit(&mut core, vec![first, second]);
    let samples = tone(4_096, 0.2);
    core.push_block(block(1, session, 1, 0, &samples)).unwrap();
    core.push_block(SourceBlock {
        timeline: second.timeline,
        permission: second.permission,
        format: second.format,
        start_frame: 0,
        arrived_at_ms: 10,
        samples: &samples,
    })
    .unwrap();
    let handle = prepared(&mut core, service(0, false));
    accept(&mut core, handle);
    let before = core.source_status(second.timeline).unwrap();
    admit(
        &mut core,
        vec![
            RenderSource {
                activity: SourceActivity::Closed,
                ..first
            },
            second,
        ],
    );
    let after = core.source_status(second.timeline).unwrap();
    assert!(after.active);
    assert_eq!(after.phase, Phase::Running);
    assert_eq!(after.closed_before, before.closed_before);
    assert_eq!(after.high_water, before.high_water);
    assert!(
        core.observe_progress(ProgressObservation {
            timeline: first.timeline,
            permission: first.permission,
            format: first.format,
            end_frame: 50_000,
            discard_through: None,
        })
        .is_err()
    );
    let mut unknown = first.timeline;
    if let TimelineId::Remote { source, .. } = &mut unknown {
        *source = SourceId::from_bytes([10; 16]).unwrap();
    }
    assert!(
        core.set_source_gain(SourceGain {
            timeline: unknown,
            gain: 1.0
        })
        .is_err()
    );
    assert!(
        core.set_route(ChannelRoute {
            timeline: unknown,
            output_for_source: [Some(0), None, None, None, None, None, None, None],
        })
        .is_err()
    );
}

#[test]
fn failed_output_reopens_with_new_format_without_old_kernel_tail_or_floor_reset() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let timeline = remote(1, session, 1);
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active)],
    );
    core.push_block(block(1, session, 1, 0, &tone(4_096, 0.8)))
        .unwrap();
    let first = prepared(&mut core, service(0, false));
    let fixed = core.source_status(timeline).unwrap().closed_before;
    core.complete(SubmissionReceipt {
        generation: first.generation,
        quantum_id: first.id,
        success: false,
        start_frame: 0,
        end_frame: 0,
    })
    .unwrap();
    assert!(matches!(
        core.prepare_quantum(&service(0, false)),
        Err(RenderError::OutputUnavailable)
    ));
    core.set_output(OutputSetup {
        generation: 8,
        sample_rate_hz: 44_100,
        channels: 2,
        layout: ChannelLayout::StereoLeftRight,
        quantum_frames: 64,
    })
    .unwrap();
    let after = core.source_status(timeline).unwrap();
    assert!(after.closed_before >= fixed);
    assert_eq!(after.closed_before, 4_096);
    assert_eq!(after.ratio, 44_100.0 / 48_000.0);
    core.set_route(ChannelRoute {
        timeline,
        output_for_source: [Some(0), None, None, None, None, None, None, None],
    })
    .unwrap();
    core.push_block(block(1, session, 1, 4_096, &tone(4_096, 0.0)))
        .unwrap();
    let mut observation = service(0, false);
    observation.generation = 8;
    let next = prepared(&mut core, observation);
    assert_eq!(core.output_interleaved().len(), 128);
    assert!(
        core.output_interleaved()
            .iter()
            .all(|sample| *sample == 0.0)
    );
    accept(&mut core, next);
    assert_eq!(core.submitted_frames(), u64::from(next.frames));
}

#[test]
fn hard_service_stall_discards_all_known_tail_and_waits_for_later_pcm() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let timeline = remote(1, session, 1);
    admit(
        &mut core,
        vec![source(1, session, 1, SourceActivity::Active)],
    );
    core.push_block(block(1, session, 1, 0, &tone(4_096, 0.7)))
        .unwrap();
    let first = prepared(&mut core, service(0, false));
    accept(&mut core, first);
    let stalled = prepared(&mut core, service(0, true));
    assert!(
        core.output_interleaved()
            .iter()
            .all(|sample| *sample == 0.0)
    );
    assert_eq!(core.source_status(timeline).unwrap().closed_before, 4_096);
    accept(&mut core, stalled);
    let waiting = prepared(&mut core, service(0, false));
    assert!(
        core.output_interleaved()
            .iter()
            .all(|sample| *sample == 0.0)
    );
    assert_eq!(core.source_status(timeline).unwrap().closed_before, 4_096);
    accept(&mut core, waiting);
    core.push_block(block(1, session, 1, 4_096, &tone(512, 0.0)))
        .unwrap();
    let current = prepared(&mut core, service(0, false));
    assert!(
        core.output_interleaved()
            .iter()
            .all(|sample| *sample == 0.0)
    );
    assert!(core.source_status(timeline).unwrap().closed_before > 4_096);
    accept(&mut core, current);
}

#[test]
fn explicit_source_discard_resets_conversion_before_a_new_current_span() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let source = source(1, session, 1, SourceActivity::Active);
    admit(&mut core, vec![source]);
    core.push_block(block(1, session, 1, 0, &tone(4_096, 0.9)))
        .unwrap();
    let first = prepared(&mut core, service(0, false));
    accept(&mut core, first);
    core.observe_progress(ProgressObservation {
        timeline: source.timeline,
        permission: source.permission,
        format: source.format,
        end_frame: 4_096,
        discard_through: Some(4_096),
    })
    .unwrap();
    assert_eq!(
        core.source_status(source.timeline).unwrap().phase,
        Phase::AwaitCurrent
    );
    core.push_block(block(1, session, 1, 4_096, &tone(512, 0.0)))
        .unwrap();
    let current = prepared(&mut core, service(0, false));
    assert!(
        core.output_interleaved()
            .iter()
            .all(|sample| *sample == 0.0)
    );
    accept(&mut core, current);
}

#[test]
fn native_submission_coordinate_is_generation_local_and_contiguous() {
    let mut core = core();
    let handle = prepared(&mut core, service(0, false));
    core.complete(SubmissionReceipt {
        generation: 7,
        quantum_id: handle.id,
        success: true,
        start_frame: 100,
        end_frame: 164,
    })
    .unwrap();
    assert_eq!(core.submitted_frames(), 164);
    let second = prepared(&mut core, service(0, false));
    assert!(
        core.complete(SubmissionReceipt {
            generation: 7,
            quantum_id: second.id,
            success: true,
            start_frame: 0,
            end_frame: 64,
        })
        .is_err()
    );
    core.complete(SubmissionReceipt {
        generation: 7,
        quantum_id: second.id,
        success: true,
        start_frame: 164,
        end_frame: 228,
    })
    .unwrap();
    assert_eq!(core.submitted_frames(), 228);
    core.set_output(OutputSetup {
        generation: 8,
        sample_rate_hz: 48_000,
        channels: 1,
        layout: ChannelLayout::Mono,
        quantum_frames: 64,
    })
    .unwrap();
    assert_eq!(core.submitted_frames(), 0);
}

#[test]
fn source_delivery_guards_are_independent_and_quantum_exhaustion_consumes_nothing() {
    let mut core = core();
    let (_, _, _, session, _) = ids();
    let first = source(1, session, 1, SourceActivity::Active);
    let mut second = first;
    if let TimelineId::Remote { source, .. } = &mut second.timeline {
        *source = SourceId::from_bytes([6; 16]).unwrap();
    }
    admit(&mut core, vec![first, second]);
    core.set_source_delivery_guard(first.timeline, 128).unwrap();
    assert_eq!(
        core.source_status(first.timeline)
            .unwrap()
            .delivery_guard_frames,
        128
    );
    assert_eq!(
        core.source_status(second.timeline)
            .unwrap()
            .delivery_guard_frames,
        64
    );
    assert!(
        core.set_source_delivery_guard(first.timeline, u64::MAX)
            .is_err()
    );
    core.push_block(block(1, session, 1, 0, &tone(512, 0.5)))
        .unwrap();
    core.exhaust_quantum_counter();
    assert!(matches!(
        core.prepare_quantum(&service(0, false)),
        Err(RenderError::CounterExhausted)
    ));
    assert_eq!(core.source_status(first.timeline).unwrap().closed_before, 0);
}

#[test]
fn padding_target_can_exceed_a_bounded_conversion_chunk() {
    let mut core = core();
    let mut observation = service(128, false);
    observation.target_padding_frames = 192;
    let handle = prepared(&mut core, observation);
    assert_eq!(handle.frames, 64);
    accept(&mut core, handle);
    observation.padding_frames = 192;
    assert!(matches!(
        core.prepare_quantum(&observation),
        Ok(PrepareOutcome::Waiting { .. })
    ));
}
