use core::str::FromStr;

use rlb_clock::MonotonicInstant;
use rlb_codec::WireCodec;
use rlb_model::{
    CatalogRevision, DirectSessionId, HelloNonce, MemberId, ServerGeneration, SourceId,
    SubscriptionRevision,
};

use super::*;

const MEDIA_HEADER_HEX: &str = "524c42320200017800000438000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f303132333435363738393a3b3c3d3e3f202122232425262728292a2b2c2d2e2f404142434445464748494a4b4c4d4e4f0000000100000000000001e001e000040102000000000f0000000000";
const GAP_HEX: &str = "524c42320200027800000000000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f303132333435363738393a3b3c3d3e3f202122232425262728292a2b2c2d2e2f404142434445464748494a4b4c4d4e4f0000000200000000000003c0000001e0030000000000000000000000";
const NACK_HEX: &str = "524c42320200037800000000000102030405060708090a0b0c0d0e0f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f202122232425262728292a2b2c2d2e2f404142434445464748494a4b4c4d4e4f0000000100000000000001e00000000a000000070000000000000000";
const END_HEX: &str = "524c42320200047800000000000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f303132333435363738393a3b3c3d3e3f202122232425262728292a2b2c2d2e2f404142434445464748494a4b4c4d4e4f0000000300000000000005a002000000000000000000000000000000";

fn id<T: FromStr>(text: &str) -> T
where
    T::Err: core::fmt::Debug,
{
    text.parse().unwrap()
}

fn media_identity() -> MediaIdentity {
    MediaIdentity {
        room_id: id("000102030405060708090a0b0c0d0e0f"),
        sender_member_id: id("101112131415161718191a1b1c1d1e1f"),
        sender_run_id: id("303132333435363738393a3b3c3d3e3f"),
        source_id: id("202122232425262728292a2b2c2d2e2f"),
        stream_epoch: id("404142434445464748494a4b4c4d4e4f"),
    }
}

fn nack_identity() -> MediaIdentity {
    MediaIdentity {
        room_id: media_identity().room_id,
        sender_member_id: id("505152535455565758595a5b5c5d5e5f"),
        sender_run_id: id("606162636465666768696a6b6c6d6e6f"),
        source_id: media_identity().source_id,
        stream_epoch: media_identity().stream_epoch,
    }
}

fn direct_common() -> DirectCommon {
    DirectCommon {
        room_id: media_identity().room_id,
        sender_member_id: media_identity().sender_member_id,
        sender_run_id: media_identity().sender_run_id,
        receiver_member_id: id("505152535455565758595a5b5c5d5e5f"),
        receiver_run_id: id("606162636465666768696a6b6c6d6e6f"),
        direct_session_id: DirectSessionId::from_bytes([0x70; 16]).unwrap(),
    }
}

fn packetizer(codec: WireCodec, channels: u8) -> MediaPacketizer {
    MediaPacketizer::new_nanosecond_domain(media_identity(), codec, channels).unwrap()
}

fn parse_media(bytes: &[u8], codec: WireCodec, channels: u8) -> MediaFragment<'_> {
    match parse_datagram(
        bytes,
        ParseContext {
            expected_channels: Some(channels),
            expected_codec: Some(codec),
            ..ParseContext::default()
        },
    )
    .unwrap()
    {
        ParsedDatagram::Media(fragment) => fragment,
        _ => panic!("expected MEDIA"),
    }
}

#[test]
fn normative_media_gap_nack_and_end_vectors_match() {
    let mut packetizer = packetizer(WireCodec::F32Le, 2);
    let zeros = vec![0.0; 960];
    packetizer.packetize(0, &zeros).unwrap();
    let unit = packetizer.packetize(480, &zeros).unwrap();
    assert_eq!(
        unit.iter().map(<[u8]>::len).collect::<Vec<_>>(),
        [1_200, 1_200, 1_200, 720]
    );
    assert_eq!(&unit.datagram(0).unwrap()[..120], hex(MEDIA_HEADER_HEX));
    assert_eq!(
        unit.iter().map(sha256_hex).collect::<Vec<_>>(),
        [
            "7d0c9fc3d8024d5c9597fa72b8fee5be117ec95f029df4c6a9b00ebc8ebfd401",
            "0bc93aeea4bb8c367674c1e74e00e9b5c894505e9ca5b72c418a7356d6a2e309",
            "4e9438e8d01113fbce3844ce3dc88592ee4f36702fbc1212656f211bb90fdc12",
            "bfb03e8115848117793b6181794bc93e59d8770fcf9dcb8ef52eb3556409b314",
        ]
    );

    let OutboundRecord::Gap {
        record: gap,
        datagram,
    } = packetizer
        .gap(960, 480, GapReason::SendDeadlineDrop)
        .unwrap()
    else {
        panic!("expected GAP");
    };
    assert_eq!(gap.record_sequence, 2);
    assert_eq!(datagram.as_ref(), hex(GAP_HEX));
    assert_eq!(
        sha256_hex(&datagram),
        "c68a592c7967f1485f0fe3f7937743600ee574b4ec678652b9831740189497e2"
    );

    let nack = NackRecord {
        identity: nack_identity(),
        target_unit_sequence: 1,
        unit_start_sample: 480,
        missing_fragment_mask: 0b1010,
        nack_id: 7,
    };
    let mut nack_bytes = [0_u8; 120];
    encode_nack_into(nack, &mut nack_bytes).unwrap();
    assert_eq!(nack_bytes.as_slice(), hex(NACK_HEX));
    assert_eq!(
        sha256_hex(&nack_bytes),
        "09c377be257f7219fcae3508632fc550080364cbed783cb95e9078b4d52eb2c1"
    );

    let OutboundRecord::EpochEnd {
        record: end,
        datagram,
    } = packetizer
        .epoch_end(1_440, EpochEndReason::EpochReplaced)
        .unwrap()
    else {
        panic!("expected EPOCH_END");
    };
    assert_eq!(end.next_record_sequence, 3);
    assert_eq!(datagram.as_ref(), hex(END_HEX));
    assert_eq!(
        sha256_hex(&datagram),
        "1276a3f890b335392f6c27f3cb4f18043155da9a58fc2a975e11852d53684f78"
    );
}

#[test]
fn all_first_release_formats_use_deterministic_mtu_slices() {
    let formats = [
        (WireCodec::F32Le, 2, vec![1_080, 1_080, 1_080, 600]),
        (WireCodec::S16Le, 2, vec![1_080, 840]),
        (WireCodec::F32Le, 1, vec![1_080, 840]),
        (WireCodec::S16Le, 1, vec![960]),
    ];
    for (codec, channels, expected) in formats {
        let mut packetizer = packetizer(codec, channels);
        let samples = vec![0.25; 480 * usize::from(channels)];
        let set = packetizer.packetize(0, &samples).unwrap();
        let payload_lengths = set
            .iter()
            .map(|datagram| datagram.len() - FIXED_HEADER_BYTES)
            .collect::<Vec<_>>();
        assert_eq!(payload_lengths, expected);
        assert!(set.iter().all(|datagram| datagram.len() <= 1_200));
        assert_eq!(packetizer.metrics().encoded_units_total, 1);
    }
}

#[test]
fn strict_parser_reports_normative_mutations() {
    let mut packetizer = packetizer(WireCodec::F32Le, 2);
    packetizer
        .packetize(0, &vec![0.0; 960])
        .expect("sequence zero");
    let set = packetizer.packetize(480, &vec![0.0; 960]).unwrap();
    let valid = set.datagram(0).unwrap().to_vec();
    let context = ParseContext {
        expected_channels: Some(2),
        expected_codec: Some(WireCodec::F32Le),
        ..ParseContext::default()
    };
    assert!(matches!(
        parse_datagram(&valid, context),
        Ok(ParsedDatagram::Media(_))
    ));

    assert_mutation(
        &valid,
        context,
        |bytes| bytes[3] = b'1',
        PacketError::BadMagic,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[5] = 1,
        PacketError::UnsupportedVersion,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[7] = 119,
        PacketError::BadHeaderLength,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[10..12].copy_from_slice(&1_079_u16.to_be_bytes()),
        PacketError::LengthMismatch,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[106] = 4,
        PacketError::FragmentIndexOutOfRange,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[107] = 0,
        PacketError::FragmentCountOutOfRange,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[108] = 3,
        PacketError::UnsupportedCodec,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[109] = 1,
        PacketError::SourceFormatMismatch,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[104..106].copy_from_slice(&481_u16.to_be_bytes()),
        PacketError::UnitFramesInvalid,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[112..116].copy_from_slice(&3_839_u32.to_be_bytes()),
        PacketError::UnitPayloadMismatch,
    );
    assert_mutation(
        &valid,
        context,
        |bytes| bytes[9] = 1,
        PacketError::ReservedNonzero,
    );
    let mut too_large = valid;
    too_large.resize(1_201, 0);
    assert_eq!(
        parse_datagram(&too_large, context),
        Err(PacketError::DatagramTooLarge)
    );

    let empty_mask = NackRecord {
        missing_fragment_mask: 0,
        ..NackRecord {
            identity: nack_identity(),
            target_unit_sequence: 1,
            unit_start_sample: 480,
            missing_fragment_mask: 1,
            nack_id: 0,
        }
    };
    let mut output = [0_u8; 120];
    assert_eq!(
        encode_nack_into(empty_mask, &mut output),
        Err(EncodeError::InvalidRecord(PacketError::NackMaskEmpty))
    );
    assert_eq!(
        validate_nack_mask(
            NackRecord {
                missing_fragment_mask: 1 << 4,
                ..empty_mask
            },
            4
        ),
        Err(PacketError::NackMaskOutOfRange)
    );
}

#[test]
fn parser_applies_bound_run_subscription_and_epoch_state() {
    let mut packetizer = packetizer(WireCodec::S16Le, 1);
    let set = packetizer.packetize(0, &vec![0.0; 480]).unwrap();
    let valid = set.datagram(0).unwrap();
    let identity = media_identity();
    let context = ParseContext {
        expected_room_id: Some(identity.room_id),
        expected_sender_member_id: Some(identity.sender_member_id),
        expected_sender_run_id: Some(identity.sender_run_id),
        expected_source_id: Some(identity.source_id),
        expected_stream_epoch: Some(identity.stream_epoch),
        subscription_allowed: Some(true),
        expected_channels: Some(1),
        expected_codec: Some(WireCodec::S16Le),
    };
    assert!(parse_datagram(valid, context).is_ok());
    assert_mutation(
        valid,
        context,
        |bytes| bytes[44] ^= 1,
        PacketError::StaleRunPacket,
    );
    assert_mutation(
        valid,
        context,
        |bytes| bytes[76] ^= 1,
        PacketError::MediaEpochUnannounced,
    );
    assert_eq!(
        parse_datagram(
            valid,
            ParseContext {
                subscription_allowed: Some(false),
                ..context
            }
        ),
        Err(PacketError::SubscriptionDenied)
    );
}

#[test]
fn assembler_reorders_deduplicates_nacks_and_preserves_recovery_provenance() {
    let mut packetizer = packetizer(WireCodec::F32Le, 2);
    let samples = (0..960)
        .map(|index| index as f32 / 960.0)
        .collect::<Vec<_>>();
    let set = packetizer.packetize(0, &samples).unwrap();
    let fragments = set
        .iter()
        .map(|bytes| parse_media(bytes, WireCodec::F32Le, 2))
        .collect::<Vec<_>>();
    let mut assembler = FragmentAssembler::new(media_identity(), NackPlannerConfig::default());
    let mut decoded = vec![0.0; 960];

    assert_eq!(
        assembler.push(fragments[2], MonotonicInstant::from_ticks(0), &mut decoded),
        Ok(AssemblyOutcome::Stored)
    );
    assert_eq!(
        assembler.push(
            fragments[0],
            MonotonicInstant::from_ticks(100),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Reordered)
    );
    assert_eq!(
        assembler.push(
            fragments[0],
            MonotonicInstant::from_ticks(200),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Duplicate)
    );
    assert_eq!(
        assembler.push(
            fragments[3],
            MonotonicInstant::from_ticks(300),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Stored)
    );

    let mut candidates = [NackCandidate::default(); 2];
    let count = assembler.poll_nacks(
        MonotonicInstant::from_ticks(1_000_000),
        &mut candidates,
        |_| Some(MonotonicInstant::from_ticks(20_000_000)),
    );
    assert_eq!(count, 1);
    assert_eq!(candidates[0].missing_fragment_mask, 0b0010);
    assert!(!candidates[0].retry);

    let outcome = assembler
        .push(
            fragments[1],
            MonotonicInstant::from_ticks(2_000_000),
            &mut decoded,
        )
        .unwrap();
    let AssemblyOutcome::Complete(complete) = outcome else {
        panic!("repair should complete the unit");
    };
    assert!(complete.recovered_after_nack);
    assert_eq!(decoded, samples);
    let metrics = assembler.metrics();
    assert_eq!(metrics.complete_units_total, 1);
    assert_eq!(metrics.duplicate_fragments_total, 1);
    assert!(metrics.reordered_fragments_total >= 1);
}

#[test]
fn assembler_rejects_late_repairs_and_conflicting_payload() {
    let mut packetizer = packetizer(WireCodec::S16Le, 1);
    let set = packetizer.packetize(0, &vec![0.5; 480]).unwrap();
    let original = set.datagram(0).unwrap().to_vec();
    let fragment = parse_media(&original, WireCodec::S16Le, 1);
    let mut assembler = FragmentAssembler::new(media_identity(), NackPlannerConfig::default());
    let mut decoded = [0.0; 480];
    assert!(matches!(
        assembler.push(fragment, MonotonicInstant::from_ticks(0), &mut decoded),
        Ok(AssemblyOutcome::Complete(_))
    ));

    let mut conflicting = original.clone();
    conflicting[FIXED_HEADER_BYTES] ^= 1;
    let conflicting = parse_media(&conflicting, WireCodec::S16Le, 1);
    assert_eq!(
        assembler.push(conflicting, MonotonicInstant::from_ticks(1), &mut decoded),
        Err(AssemblyError::FragmentPayloadConflict)
    );

    let mut late = FragmentAssembler::new(media_identity(), NackPlannerConfig::default());
    late.set_read_cursor(480);
    assert_eq!(
        late.push(fragment, MonotonicInstant::from_ticks(10), &mut decoded),
        Ok(AssemblyOutcome::Late)
    );
}

#[test]
fn preroll_anchor_rejects_and_discards_older_incomplete_units() {
    let mut packetizer = packetizer(WireCodec::F32Le, 1);
    let old = packetizer.packetize(0, &vec![0.25; 480]).unwrap();
    let anchor = packetizer.packetize(4_800, &vec![0.5; 480]).unwrap();
    let old_fragments = old
        .iter()
        .map(|bytes| parse_media(bytes, WireCodec::F32Le, 1))
        .collect::<Vec<_>>();
    let anchor_fragments = anchor
        .iter()
        .map(|bytes| parse_media(bytes, WireCodec::F32Le, 1))
        .collect::<Vec<_>>();
    let mut assembler = FragmentAssembler::new(media_identity(), NackPlannerConfig::default());
    let mut decoded = [0.0; 480];

    assert_eq!(
        assembler.push(
            old_fragments[0],
            MonotonicInstant::from_ticks(0),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Stored)
    );
    assert_eq!(
        assembler.push(
            anchor_fragments[0],
            MonotonicInstant::from_ticks(1),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Stored)
    );
    assert!(matches!(
        assembler.push(
            anchor_fragments[1],
            MonotonicInstant::from_ticks(2),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Complete(_))
    ));

    assembler.set_preroll_anchor(4_800);
    assert_eq!(assembler.metrics().assembler_units, 1);
    assert_eq!(
        assembler.push(
            old_fragments[1],
            MonotonicInstant::from_ticks(3),
            &mut decoded
        ),
        Ok(AssemblyOutcome::Late)
    );
    assert_eq!(assembler.metrics().late_fragments_total, 1);
    assert_eq!(assembler.metrics().complete_units_total, 1);
}

#[test]
fn assembler_slot_exhaustion_is_counted_drop_without_faulting_source() {
    let mut packetizer = packetizer(WireCodec::F32Le, 2);
    let set = packetizer.packetize(0, &vec![0.25; 960]).unwrap();
    let fragments = set
        .iter()
        .map(|bytes| parse_media(bytes, WireCodec::F32Le, 2))
        .collect::<Vec<_>>();
    let mut assembler = FragmentAssembler::new(media_identity(), NackPlannerConfig::default());
    let mut decoded = [0.0; 960];

    for slot in 0..26_u32 {
        let mut fragment = fragments[0];
        fragment.header.unit_sequence = slot;
        fragment.header.start_sample = u64::from(slot);
        assert_eq!(
            assembler.push(
                fragment,
                MonotonicInstant::from_ticks(u64::from(slot)),
                &mut decoded
            ),
            Ok(AssemblyOutcome::Stored)
        );
    }
    let mut overflow = fragments[0];
    overflow.header.unit_sequence = 26;
    overflow.header.start_sample = 26;
    assert_eq!(
        assembler.push(overflow, MonotonicInstant::from_ticks(26), &mut decoded),
        Ok(AssemblyOutcome::CapacityDropped)
    );
    assert_eq!(assembler.metrics().capacity_dropped_fragments_total, 1);

    for fragment in &fragments[1..] {
        assert!(matches!(
            assembler.push(*fragment, MonotonicInstant::from_ticks(30), &mut decoded),
            Ok(AssemblyOutcome::Stored | AssemblyOutcome::Reordered | AssemblyOutcome::Complete(_))
        ));
    }
    assert_eq!(assembler.metrics().complete_units_total, 1);
}

#[test]
fn encode_once_fanout_and_history_reuse_identical_bytes() {
    let mut packetizer = packetizer(WireCodec::F32Le, 2);
    let set = packetizer.packetize(0, &vec![0.25; 960]).unwrap();
    let originals = set.iter().map(<[u8]>::to_vec).collect::<Vec<_>>();
    let recipients: [MemberId; 3] = [
        id("505152535455565758595a5b5c5d5e5f"),
        MemberId::from_bytes([0x71; 16]).unwrap(),
        MemberId::from_bytes([0x72; 16]).unwrap(),
    ];
    let fanout = set.fanout(&recipients).collect::<Vec<_>>();
    assert_eq!(fanout.len(), 12);
    for (recipient_index, recipient) in recipients.iter().enumerate() {
        for (fragment_index, original) in originals.iter().enumerate() {
            let item = fanout[recipient_index * 4 + fragment_index];
            assert_eq!(item.recipient, *recipient);
            assert_eq!(item.bytes, original);
            assert_eq!(
                item.bytes.as_ptr(),
                set.datagram(fragment_index as u8).unwrap().as_ptr()
            );
        }
    }
    assert_eq!(packetizer.metrics().encoded_units_total, 1);

    let mut history = ResendHistory::new_nanosecond_domain();
    let key = history.insert(set, MonotonicInstant::from_ticks(0));
    assert_eq!(history.get(key).unwrap().iter().count(), 4);
    let nack = NackRecord {
        identity: nack_identity(),
        target_unit_sequence: key.unit_sequence,
        unit_start_sample: key.start_sample,
        missing_fragment_mask: 0b1010,
        nack_id: 1,
    };
    let mut resent = Vec::new();
    let decision = history.process_nack(
        recipients[0],
        true,
        nack,
        MonotonicInstant::from_ticks(1_000_000),
        |bytes| {
            resent.push(bytes.to_vec());
            true
        },
    );
    assert_eq!(decision.sent_fragments, 2);
    assert_eq!(resent, [originals[1].clone(), originals[3].clone()]);

    let limited = history.process_nack(
        recipients[0],
        true,
        nack,
        MonotonicInstant::from_ticks(1_500_000),
        |_| true,
    );
    assert_eq!(limited.rate_limited_fragments, 2);
    let second = history.process_nack(
        recipients[0],
        true,
        nack,
        MonotonicInstant::from_ticks(3_000_000),
        |_| true,
    );
    assert_eq!(second.sent_fragments, 2);
    let exhausted = history.process_nack(
        recipients[0],
        true,
        nack,
        MonotonicInstant::from_ticks(6_000_000),
        |_| true,
    );
    assert_eq!(exhausted.rate_limited_fragments, 2);
}

#[test]
fn deadline_drop_keeps_record_sequence_and_source_duration() {
    let mut packetizer = packetizer(WireCodec::S16Le, 1);
    let record = packetizer
        .packetize_at_deadline(
            MonotonicInstant::from_ticks(1_000),
            MonotonicInstant::from_ticks(10_001_000),
            0,
            &vec![0.0; 480],
        )
        .unwrap();
    let OutboundRecord::Gap { record, .. } = record else {
        panic!("deadline must produce GAP");
    };
    assert_eq!(record.record_sequence, 0);
    assert_eq!(record.start_sample, 0);
    assert_eq!(record.frame_count, 480);
    let media = packetizer.packetize(480, &vec![0.0; 480]).unwrap();
    assert_eq!(media.unit_sequence(), 1);
    assert!(matches!(
        packetizer.packetize(480, &vec![0.0; 480]),
        Err(PacketizeError::TimelineConflict)
    ));
    assert_eq!(packetizer.metrics().encoded_units_total, 1);
    assert_eq!(
        packetizer.metrics().gap_frames_total_by_reason,
        [0, 0, 480, 0]
    );
}

#[test]
fn direct_control_roundtrips_all_exact_lengths() {
    let common = direct_common();
    let hello = DirectDatagram::Hello(DirectHello {
        common,
        server_generation: ServerGeneration::from_bytes([0x80; 16]).unwrap(),
        hello_nonce: HelloNonce::from_bytes([0x81; 16]).unwrap(),
        echo_nonce: None,
    });
    assert_direct_roundtrip(hello, 156);

    let sources = [
        SourceId::from_bytes([0x20; 16]).unwrap(),
        SourceId::from_bytes([0x21; 16]).unwrap(),
    ];
    for count in 0..=2 {
        let request = SubscribeSet::new(
            common,
            SubscriptionRevision::new(7),
            CatalogRevision::new(9),
            BoundedSourceIds::new(&sources[..count]).unwrap(),
        )
        .unwrap();
        assert_direct_roundtrip(DirectDatagram::SubscribeSet(request), 128 + count * 16);
    }

    let accepted = BoundedSourceIds::new(&sources).unwrap();
    let ack = SubscribeAck::new(
        common,
        SubscriptionRevision::new(7),
        CatalogRevision::new(9),
        AckStatus::Ok,
        accepted,
        BoundedRejections::empty(),
    )
    .unwrap();
    assert_direct_roundtrip(DirectDatagram::SubscribeAck(ack), 160);

    let rejections = BoundedRejections::new(&[
        Rejection {
            source_id: sources[0],
            code: RejectionCode::SourceHidden,
        },
        Rejection {
            source_id: sources[1],
            code: RejectionCode::RoomSendMuted,
        },
    ])
    .unwrap();
    let max_ack = SubscribeAck::new(
        common,
        SubscriptionRevision::new(8),
        CatalogRevision::new(10),
        AckStatus::Ok,
        BoundedSourceIds::empty(),
        rejections,
    )
    .unwrap();
    assert_direct_roundtrip(DirectDatagram::SubscribeAck(max_ack), 168);
    for status in [AckStatus::RevisionConflict, AckStatus::CatalogStale] {
        let ack = SubscribeAck::new(
            common,
            SubscriptionRevision::new(8),
            CatalogRevision::new(10),
            status,
            BoundedSourceIds::empty(),
            BoundedRejections::empty(),
        )
        .unwrap();
        assert_direct_roundtrip(DirectDatagram::SubscribeAck(ack), 128);
    }
}

#[test]
fn direct_parser_rejects_closed_invalid_forms_and_ack_coverage() {
    let sources = [
        SourceId::from_bytes([0x20; 16]).unwrap(),
        SourceId::from_bytes([0x21; 16]).unwrap(),
    ];
    let request = SubscribeSet::new(
        direct_common(),
        SubscriptionRevision::new(1),
        CatalogRevision::new(2),
        BoundedSourceIds::new(&sources).unwrap(),
    )
    .unwrap();
    let mut bytes = [0_u8; 168];
    let length = encode_direct_into(DirectDatagram::SubscribeSet(request), &mut bytes).unwrap();
    let valid = &bytes[..length];
    assert_direct_mutation(valid, |bytes| bytes[3] = b'2', DirectError::BadMagic);
    assert_direct_mutation(valid, |bytes| bytes[5] = 1, DirectError::UnsupportedVersion);
    assert_direct_mutation(valid, |bytes| bytes[6] = 9, DirectError::UnknownKind);
    assert_direct_mutation(valid, |bytes| bytes[7] = 107, DirectError::BadHeaderLength);
    assert_direct_mutation(valid, |bytes| bytes[9] ^= 1, DirectError::LengthMismatch);
    assert_direct_mutation(valid, |bytes| bytes[11] = 1, DirectError::ReservedNonzero);
    assert_direct_mutation(
        valid,
        |bytes| bytes[12..28].fill(0),
        DirectError::ZeroIdentity,
    );
    assert_direct_mutation(
        valid,
        |bytes| bytes[124] = 3,
        DirectError::SourceCountOutOfRange,
    );
    assert_direct_mutation(
        valid,
        |bytes| {
            let second = bytes[128..144].to_vec();
            bytes[144..160].copy_from_slice(&second);
        },
        DirectError::SourceOrderInvalid,
    );
    assert_direct_mutation(
        valid,
        |bytes| bytes[108..116].fill(0),
        DirectError::SubscriptionRevisionZero,
    );

    let incomplete_ack = SubscribeAck::new(
        direct_common(),
        SubscriptionRevision::new(1),
        CatalogRevision::new(2),
        AckStatus::Ok,
        BoundedSourceIds::new(&sources[..1]).unwrap(),
        BoundedRejections::empty(),
    )
    .unwrap();
    assert_eq!(
        incomplete_ack.validate_against(&request),
        Err(DirectError::AckCoverageInvalid)
    );
}

#[test]
fn parsers_are_total_for_every_permitted_input_length() {
    let mut state = 0x9e37_79b9_u32;
    for length in 0..=MAX_UDP_APPLICATION_BYTES {
        let mut bytes = vec![0_u8; length];
        for byte in &mut bytes {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        let _ = parse_datagram(&bytes, ParseContext::default());
    }
    for length in 0..=168 {
        let mut bytes = vec![0_u8; length];
        for byte in &mut bytes {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        let _ = parse_direct(&bytes);
    }
}

fn assert_mutation(
    original: &[u8],
    context: ParseContext,
    mutate: impl FnOnce(&mut [u8]),
    expected: PacketError,
) {
    let mut bytes = original.to_vec();
    mutate(&mut bytes);
    assert_eq!(parse_datagram(&bytes, context), Err(expected));
}

fn assert_direct_roundtrip(datagram: DirectDatagram, expected_length: usize) {
    let mut bytes = [0_u8; 168];
    let length = encode_direct_into(datagram, &mut bytes).unwrap();
    assert_eq!(length, expected_length);
    assert_eq!(parse_direct(&bytes[..length]), Ok(datagram));
}

fn assert_direct_mutation(original: &[u8], mutate: impl FnOnce(&mut [u8]), expected: DirectError) {
    let mut bytes = original.to_vec();
    mutate(&mut bytes);
    assert_eq!(parse_direct(&bytes), Err(expected));
}

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0]);
            let low = hex_nibble(pair[1]);
            (high << 4) | low
        })
        .collect()
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("invalid fixture hex"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha256(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut hash = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in words[..16].iter_mut().enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes(chunk[offset..offset + 4].try_into().unwrap());
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for index in 0..64 {
            let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let first = h
                .wrapping_add(sigma1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = sigma0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        hash[0] = hash[0].wrapping_add(a);
        hash[1] = hash[1].wrapping_add(b);
        hash[2] = hash[2].wrapping_add(c);
        hash[3] = hash[3].wrapping_add(d);
        hash[4] = hash[4].wrapping_add(e);
        hash[5] = hash[5].wrapping_add(f);
        hash[6] = hash[6].wrapping_add(g);
        hash[7] = hash[7].wrapping_add(h);
    }
    let mut output = [0_u8; 32];
    for (word, destination) in hash.into_iter().zip(output.chunks_exact_mut(4)) {
        destination.copy_from_slice(&word.to_be_bytes());
    }
    output
}
