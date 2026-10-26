use core::str::FromStr;

use rlb_codec::WireCodec;
use rlb_jitter::{PlayoutState, TargetMode};
use rlb_media::{DatagramSet, MediaIdentity, MediaPacketizer, OutboundRecord};
use rlb_model::{MemberId, RoomId, RunId, SourceId, StreamEpoch};

use super::*;

fn id<T: FromStr>(text: &str) -> T
where
    T::Err: core::fmt::Debug,
{
    text.parse().unwrap()
}

fn identity(source_byte: u8, epoch_byte: u8) -> MediaIdentity {
    MediaIdentity {
        room_id: RoomId::from_bytes([1; 16]).unwrap(),
        sender_member_id: MemberId::from_bytes([2; 16]).unwrap(),
        sender_run_id: RunId::from_bytes([3; 16]).unwrap(),
        source_id: SourceId::from_bytes([source_byte; 16]).unwrap(),
        stream_epoch: StreamEpoch::from_bytes([epoch_byte; 16]).unwrap(),
    }
}

fn config(
    source_byte: u8,
    epoch_byte: u8,
    codec: WireCodec,
    channels: u8,
    target_mode: TargetMode,
) -> SimSourceConfig {
    SimSourceConfig {
        identity: identity(source_byte, epoch_byte),
        codec,
        channels,
        target_mode,
    }
}

fn packetizer(config: SimSourceConfig) -> MediaPacketizer {
    MediaPacketizer::new_nanosecond_domain(config.identity, config.codec, config.channels).unwrap()
}

fn schedule_set(
    simulator: &mut DeterministicSimulator,
    source_id: SourceId,
    set: &DatagramSet,
    first_time_ms: u64,
    order: &[u8],
) {
    for (offset, fragment_index) in order.iter().copied().enumerate() {
        simulator.schedule_arrival(
            SimTime::from_ns(first_time_ms * 1_000_000 + u64::try_from(offset).unwrap() * 100_000),
            source_id,
            set.datagram(fragment_index).unwrap().to_vec(),
        );
    }
}

#[test]
fn arrival_order_is_independent_from_truth_and_equal_time_precedes_render() {
    let source = config(
        0x20,
        0x40,
        WireCodec::F32Le,
        2,
        TargetMode::manual(480).unwrap(),
    );
    let mut packetizer = packetizer(source);
    let samples = (0..960)
        .map(|index| index as f32 / 960.0)
        .collect::<Vec<_>>();
    let set = packetizer.packetize(0, &samples).unwrap();
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();
    simulator.add_truth(TruthRecord::Media {
        source_id: source.identity.source_id,
        sequence: 0,
        start_sample: 0,
        samples: samples.clone().into_boxed_slice(),
    });
    schedule_set(
        &mut simulator,
        source.identity.source_id,
        &set,
        0,
        &[2, 0, 1, 1, 3],
    );
    simulator.schedule_render(SimTime::from_ms(11), 480);
    simulator.run().unwrap();

    assert_eq!(simulator.truth().len(), 1);
    assert_eq!(
        simulator
            .arrival_log()
            .iter()
            .filter(|entry| entry.disposition == ArrivalDisposition::DuplicateFragment)
            .count(),
        1
    );
    assert_eq!(
        simulator.source_output(source.identity.source_id).unwrap(),
        &samples
    );
    assert!(
        simulator
            .assembler_metrics(source.identity.source_id)
            .unwrap()
            .reordered_fragments_total
            >= 1
    );

    let next = packetizer.packetize(480, &vec![0.75; 960]).unwrap();
    for fragment in next.iter() {
        simulator.schedule_arrival(
            SimTime::from_ms(20),
            source.identity.source_id,
            fragment.to_vec(),
        );
    }
    simulator.schedule_render(SimTime::from_ms(20), 480);
    simulator.run().unwrap();
    let output = simulator.source_output(source.identity.source_id).unwrap();
    assert!(output[960..].iter().all(|sample| *sample == 0.75));
}

#[test]
fn first_complete_unit_sets_reported_preroll_anchor_and_closes_older_assembly() {
    let source = config(
        0x29,
        0x49,
        WireCodec::F32Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let mut packetizer = packetizer(source);
    let old = packetizer.packetize(0, &vec![0.25; 480]).unwrap();
    let anchor = packetizer.packetize(4_800, &vec![0.5; 480]).unwrap();
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();
    simulator.schedule_arrival(
        SimTime::from_ms(0),
        source.identity.source_id,
        old.datagram(0).unwrap().to_vec(),
    );
    schedule_set(
        &mut simulator,
        source.identity.source_id,
        &anchor,
        1,
        &[0, 1],
    );
    simulator.schedule_arrival(
        SimTime::from_ms(2),
        source.identity.source_id,
        old.datagram(1).unwrap().to_vec(),
    );
    simulator.schedule_render(SimTime::from_ms(12), 480);
    simulator.run().unwrap();

    let metrics = simulator.jitter_metrics(source.identity.source_id).unwrap();
    assert_eq!(metrics.preroll_anchor_sample, Some(4_800));
    assert_eq!(metrics.complete_units_total, 1);
    assert_eq!(
        simulator
            .assembler_metrics(source.identity.source_id)
            .unwrap()
            .late_fragments_total,
        1
    );
    assert_eq!(
        simulator.arrival_log().last().unwrap().disposition,
        ArrivalDisposition::LateFragment
    );
    assert!(
        simulator
            .source_output(source.identity.source_id)
            .unwrap()
            .iter()
            .all(|sample| (*sample - 0.5).abs() < f32::EPSILON)
    );
}

#[test]
fn simulator_rejects_repair_completed_after_fixed_callback_deadline() {
    let source = config(
        0x2a,
        0x4a,
        WireCodec::F32Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let mut packetizer = packetizer(source);
    let first = packetizer.packetize(0, &vec![1.0; 480]).unwrap();
    let repair = packetizer.packetize(480, &vec![0.5; 480]).unwrap();
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();
    schedule_set(
        &mut simulator,
        source.identity.source_id,
        &first,
        0,
        &[0, 1],
    );
    simulator.schedule_render(SimTime::from_ms(11), 480);
    simulator.schedule_arrival(
        SimTime::from_ms(12),
        source.identity.source_id,
        repair.datagram(0).unwrap().to_vec(),
    );
    simulator.schedule_recovery_poll(
        SimTime::from_ms(14),
        source.identity.source_id,
        SimTime::from_ms(40),
    );
    simulator.run().unwrap();
    assert_eq!(simulator.recovery_log().len(), 1);

    simulator
        .source_mut(source.identity.source_id)
        .unwrap()
        .jitter
        .note_render_callback_entry(SimTime::from_ms(21).instant(), 480);
    simulator.schedule_arrival(
        SimTime::from_ms(22),
        source.identity.source_id,
        repair.datagram(1).unwrap().to_vec(),
    );
    simulator.run().unwrap();

    assert_eq!(
        simulator.arrival_log().last().unwrap().disposition,
        ArrivalDisposition::LateFragment
    );
    let metrics = simulator.jitter_metrics(source.identity.source_id).unwrap();
    assert_eq!(metrics.late_frames_total, 480);
    assert_eq!(metrics.complete_units_total, 1);
    assert_eq!(metrics.recovered_before_deadline_units_total, 0);
    assert_eq!(metrics.recovered_before_deadline_frames_total, 0);
}

#[test]
fn missing_fragment_recovers_before_deadline_and_misses_after_callback() {
    let source = config(
        0x21,
        0x41,
        WireCodec::F32Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let mut packetizer = packetizer(source);
    let first = packetizer.packetize(0, &vec![0.25; 480]).unwrap();
    let second = packetizer.packetize(480, &vec![0.5; 480]).unwrap();
    let third = packetizer.packetize(960, &vec![0.75; 480]).unwrap();
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();

    schedule_set(&mut simulator, source.identity.source_id, &first, 0, &[1]);
    simulator.schedule_recovery_poll(
        SimTime::from_ms(1),
        source.identity.source_id,
        SimTime::from_ms(20),
    );
    simulator.schedule_arrival(
        SimTime::from_ms(5),
        source.identity.source_id,
        first.datagram(0).unwrap().to_vec(),
    );
    schedule_set(
        &mut simulator,
        source.identity.source_id,
        &second,
        6,
        &[0, 1],
    );
    simulator.schedule_render(SimTime::from_ms(15), 480);

    simulator.schedule_arrival(
        SimTime::from_ms(16),
        source.identity.source_id,
        third.datagram(1).unwrap().to_vec(),
    );
    simulator.schedule_recovery_poll(
        SimTime::from_ms(17),
        source.identity.source_id,
        SimTime::from_ms(25),
    );
    simulator.schedule_render(SimTime::from_ms(25), 480);
    simulator.schedule_render(SimTime::from_ms(35), 480);
    simulator.schedule_arrival(
        SimTime::from_ms(36),
        source.identity.source_id,
        third.datagram(0).unwrap().to_vec(),
    );
    simulator.run().unwrap();

    assert_eq!(
        simulator.recovery_log()[0].candidate.missing_fragment_mask,
        1
    );
    assert_eq!(
        simulator
            .jitter_metrics(source.identity.source_id)
            .unwrap()
            .recovered_before_deadline_units_total,
        1
    );
    assert!(simulator.arrival_log().iter().any(|entry| {
        entry.time == SimTime::from_ms(36) && entry.disposition == ArrivalDisposition::LateFragment
    }));
    let metrics = simulator.jitter_metrics(source.identity.source_id).unwrap();
    assert_eq!(metrics.exact_frames_total, 960);
    assert_eq!(metrics.concealed_frames_total, 480);
}

#[test]
fn publish_pause_keeps_epoch_and_creates_new_playout_generation() {
    let source = config(
        0x22,
        0x42,
        WireCodec::S16Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let mut packetizer = packetizer(source);
    let initial = packetizer.packetize(0, &vec![0.25; 480]).unwrap();
    let resumed = packetizer.packetize(4_800, &vec![0.75; 480]).unwrap();
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();
    let first_generation = simulator
        .source_generation(source.identity.source_id)
        .unwrap();
    schedule_set(&mut simulator, source.identity.source_id, &initial, 0, &[0]);
    simulator.schedule_render(SimTime::from_ms(10), 480);
    simulator.schedule_disable(SimTime::from_ms(20), source.identity.source_id);
    simulator.schedule_render(SimTime::from_ms(20), 480);
    simulator.schedule_enable(SimTime::from_ms(100), source.identity.source_id);
    schedule_set(
        &mut simulator,
        source.identity.source_id,
        &resumed,
        101,
        &[0],
    );
    simulator.schedule_render(SimTime::from_ms(110), 480);
    simulator.schedule_render(SimTime::from_ms(120), 480);
    simulator.run().unwrap();

    let second_generation = simulator
        .source_generation(source.identity.source_id)
        .unwrap();
    assert_ne!(first_generation, second_generation);
    assert_eq!(
        source.identity.stream_epoch,
        resumed.identity().stream_epoch
    );
    let metrics = simulator.jitter_metrics(source.identity.source_id).unwrap();
    assert_eq!(metrics.concealed_frames_total, 0);
    assert_eq!(metrics.exact_frames_total, 960);
    let output = simulator.source_output(source.identity.source_id).unwrap();
    assert!(
        output[..480]
            .iter()
            .all(|sample| (*sample - 0.25).abs() < 0.000_1)
    );
    assert!(output[480..1_440].iter().all(|sample| *sample == 0.0));
    assert!(
        output[1_440..]
            .iter()
            .all(|sample| (*sample - 0.75).abs() < 0.000_1)
    );
}

#[test]
fn stalled_source_never_blocks_healthy_source() {
    let healthy = config(
        0x23,
        0x43,
        WireCodec::S16Le,
        1,
        TargetMode::manual(2_400).unwrap(),
    );
    let stalled = config(
        0x24,
        0x44,
        WireCodec::S16Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let mut healthy_packetizer = packetizer(healthy);
    let mut stalled_packetizer = packetizer(stalled);
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(healthy).unwrap();
    simulator.add_source(stalled).unwrap();

    for unit_index in 0..5 {
        let value = 0.1 * (unit_index + 1) as f32;
        let set = healthy_packetizer
            .packetize(unit_index * 480, &vec![value; 480])
            .unwrap();
        schedule_set(
            &mut simulator,
            healthy.identity.source_id,
            &set,
            unit_index,
            &[0],
        );
    }
    let stalled_first = stalled_packetizer.packetize(0, &vec![0.5; 480]).unwrap();
    schedule_set(
        &mut simulator,
        stalled.identity.source_id,
        &stalled_first,
        0,
        &[0],
    );
    for callback in 5..=8 {
        simulator.schedule_render(SimTime::from_ms(callback * 10), 480);
    }
    simulator.run().unwrap();

    let healthy_metrics = simulator
        .jitter_metrics(healthy.identity.source_id)
        .unwrap();
    let stalled_metrics = simulator
        .jitter_metrics(stalled.identity.source_id)
        .unwrap();
    assert_eq!(healthy_metrics.exact_frames_total, 1_920);
    assert_eq!(healthy_metrics.concealed_frames_total, 0);
    assert_eq!(stalled_metrics.exact_frames_total, 480);
    assert_eq!(stalled_metrics.concealed_frames_total, 1_440);
    assert_eq!(stalled_metrics.reprime_count_total, 1);
    assert_eq!(
        simulator.source_state(stalled.identity.source_id),
        Some(PlayoutState::ReprimeSeek)
    );
    let healthy_output = simulator.source_output(healthy.identity.source_id).unwrap();
    for (unit_index, chunk) in healthy_output.chunks_exact(480).enumerate() {
        let expected = 0.1 * (unit_index + 1) as f32;
        assert!(
            chunk
                .iter()
                .all(|sample| (*sample - expected).abs() < 0.000_1)
        );
    }
}

#[test]
fn source_drift_drives_bounded_commands_and_sample_slip_direction() {
    for (source_byte, drift_frames, expect_advance) in
        [(0x25, 48_010_u64, true), (0x26, 47_990_u64, false)]
    {
        let source = config(
            source_byte,
            source_byte + 0x20,
            WireCodec::S16Le,
            1,
            TargetMode::manual(2_400).unwrap(),
        );
        let mut packetizer = packetizer(source);
        let sets = (0..18)
            .map(|unit_index| {
                packetizer
                    .packetize(unit_index * 480, &vec![0.2; 480])
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut simulator = DeterministicSimulator::new();
        simulator.add_source(source).unwrap();
        for set in &sets[..5] {
            schedule_set(&mut simulator, source.identity.source_id, set, 0, &[0]);
        }
        simulator.schedule_drift_observation(
            SimTime::from_ms(1),
            source.identity.source_id,
            drift_frames,
            48_000,
        );
        for callback_index in 0..13 {
            let callback_ms = 50 + u64::try_from(callback_index).unwrap() * 10;
            if let Some(set) = sets.get(5 + callback_index) {
                schedule_set(
                    &mut simulator,
                    source.identity.source_id,
                    set,
                    callback_ms + 9,
                    &[0],
                );
            }
            simulator.schedule_render(SimTime::from_ms(callback_ms), 480);
        }
        simulator.run().unwrap();
        let metrics = simulator.jitter_metrics(source.identity.source_id).unwrap();
        assert!(metrics.drift_command_ppm.unsigned_abs() <= 500);
        if expect_advance {
            assert!(metrics.sample_slip_advance_frames_total > 0);
            assert_eq!(metrics.sample_slip_retard_frames_total, 0);
        } else {
            assert!(metrics.sample_slip_retard_frames_total > 0);
            assert_eq!(metrics.sample_slip_advance_frames_total, 0);
        }
    }

    let two_hours = 48_000_u64 * 60 * 60 * 2;
    assert_eq!(
        DeterministicSimulator::ideal_drift_slips(two_hours, 200),
        69_120
    );
    assert_eq!(
        DeterministicSimulator::ideal_drift_slips(two_hours, -200),
        -69_120
    );
}

#[test]
fn gap_and_epoch_end_preserve_declared_duration() {
    let source = config(
        0x27,
        0x47,
        WireCodec::S16Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let mut packetizer = packetizer(source);
    let first = packetizer.packetize(0, &vec![0.5; 480]).unwrap();
    let OutboundRecord::Gap { datagram: gap, .. } = packetizer
        .gap(480, 480, GapReason::KnownAdapterGap)
        .unwrap()
    else {
        panic!("expected gap");
    };
    let last = packetizer.packetize(960, &vec![0.25; 480]).unwrap();
    let OutboundRecord::EpochEnd { datagram: end, .. } = packetizer
        .epoch_end(1_440, rlb_media::EpochEndReason::SourceStopped)
        .unwrap()
    else {
        panic!("expected end");
    };
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();
    schedule_set(&mut simulator, source.identity.source_id, &first, 0, &[0]);
    simulator.schedule_arrival(SimTime::from_ms(1), source.identity.source_id, gap);
    schedule_set(&mut simulator, source.identity.source_id, &last, 2, &[0]);
    simulator.schedule_arrival(SimTime::from_ms(3), source.identity.source_id, end);
    for callback in 1..=4 {
        simulator.schedule_render(SimTime::from_ms(callback * 10), 480);
    }
    simulator.run().unwrap();
    let metrics = simulator.jitter_metrics(source.identity.source_id).unwrap();
    assert_eq!(metrics.exact_frames_total, 960);
    assert_eq!(metrics.concealed_frames_total, 480);
    assert_eq!(metrics.sender_gap_frames_total[3], 480);
    assert_eq!(
        simulator.source_state(source.identity.source_id),
        Some(PlayoutState::Ended)
    );
    assert_eq!(
        simulator
            .source_output(source.identity.source_id)
            .unwrap()
            .len(),
        1_920
    );
}

#[test]
fn parser_context_used_by_simulator_rejects_wrong_format() {
    let source = config(
        0x28,
        0x48,
        WireCodec::S16Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let wrong = config(
        0x28,
        0x48,
        WireCodec::F32Le,
        1,
        TargetMode::manual(480).unwrap(),
    );
    let set = packetizer(wrong).packetize(0, &vec![0.0; 480]).unwrap();
    let mut simulator = DeterministicSimulator::new();
    simulator.add_source(source).unwrap();
    simulator.schedule_arrival(
        SimTime::from_ms(0),
        source.identity.source_id,
        set.datagram(0).unwrap().to_vec(),
    );
    assert!(matches!(
        simulator.run(),
        Err(SimError::Packet(PacketError::SourceFormatMismatch))
    ));
}

#[test]
fn unknown_source_and_time_regression_are_closed_errors() {
    let source_id: SourceId = id("202122232425262728292a2b2c2d2e2f");
    let mut simulator = DeterministicSimulator::new();
    simulator.schedule_render(SimTime::from_ms(10), 1);
    simulator.run().unwrap();
    simulator.schedule_disable(SimTime::from_ms(9), source_id);
    assert!(matches!(
        simulator.run(),
        Err(SimError::TimeRegression { .. })
    ));
}
