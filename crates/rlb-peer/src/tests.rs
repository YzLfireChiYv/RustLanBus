use rlb_model::{DirectSessionId, HelloNonce, MemberId, RunId, SourceId, StreamEpoch};

use crate::media::{self, Header};
use crate::publisher::{CaptureTicket, CaptureWatermark, PreparedStorage, SourcePublisher};
use crate::source::{
    Catalogue, ChannelLayout, PcmEncoding, PcmFormat, PublicationEpoch, PublicationState,
    SOURCE_RECORD_BYTES, SourceEntry, SourceError, SourceKind,
};
use crate::wire::{self, Ack, Body, Control, Intent, Snapshot};
use crate::{AdmissionProjection, Peer, PeerError, PeerInstance, PeerOperation};

fn member(byte: u8) -> MemberId {
    MemberId::from_bytes([byte; 16]).unwrap()
}
fn run(byte: u8) -> RunId {
    RunId::from_bytes([byte; 16]).unwrap()
}
fn source(byte: u8) -> SourceId {
    SourceId::from_bytes([byte; 16]).unwrap()
}
fn acquisition(byte: u8) -> StreamEpoch {
    StreamEpoch::from_bytes([byte; 16]).unwrap()
}
fn intent(send: bool, receive: bool) -> Intent {
    Intent { send, receive }
}
fn stereo() -> PcmFormat {
    PcmFormat::new(
        48_000,
        ChannelLayout::StereoLeftRight,
        PcmEncoding::F32Le,
        32,
    )
    .unwrap()
}
fn mono() -> PcmFormat {
    PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap()
}

fn bind(
    publisher: &mut SourcePublisher,
    id: SourceId,
    acquisition: StreamEpoch,
    format: PcmFormat,
) {
    let request = publisher
        .begin_binding(id, acquisition, format)
        .unwrap()
        .pending
        .unwrap();
    publisher.commit_binding(request, acquisition, 0).unwrap();
}

fn exchange_both(first: &mut Peer, second: &mut Peer, now: u64) {
    for _ in 0..8 {
        let mut moved = false;
        while let Some(bytes) = first.take_control() {
            second.ingest(&bytes, now, now).unwrap();
            moved = true;
        }
        while let Some(bytes) = second.take_control() {
            first.ingest(&bytes, now, now).unwrap();
            moved = true;
        }
        if !moved {
            break;
        }
    }
}

fn ready_pair(publisher_sources: &[(u8, SourceKind, PcmFormat)]) -> (SourcePublisher, Peer, Peer) {
    let mut publisher = SourcePublisher::new(run(3));
    for (byte, kind, format) in publisher_sources {
        publisher.register(source(*byte), *kind, true).unwrap();
        bind(&mut publisher, source(*byte), acquisition(*byte), *format);
    }
    let catalogue = publisher.catalogue();
    let mut first = Peer::new(
        member(1),
        member(2),
        run(3),
        intent(true, true),
        catalogue.clone(),
        10,
    )
    .unwrap();
    let mut second = Peer::new(
        member(2),
        member(1),
        run(4),
        intent(true, true),
        Catalogue::new(Vec::new()).unwrap(),
        10,
    )
    .unwrap();
    exchange_both(&mut first, &mut second, 10);
    assert!(first.status().send_ready && second.status().send_ready);
    (publisher, first, second)
}

#[test]
fn control_lengths_and_legacy_magic_are_exact() {
    let mut hello = vec![0_u8; 104];
    hello[..4].copy_from_slice(b"RLC5");
    hello[4] = 1;
    hello[8..24].copy_from_slice(&[1; 16]);
    hello[24..40].copy_from_slice(&[2; 16]);
    hello[40..56].copy_from_slice(&[3; 16]);
    hello[56..72].copy_from_slice(&[4; 16]);
    hello[72..80].copy_from_slice(&1_u64.to_le_bytes());
    hello[80..88].copy_from_slice(&1_u64.to_le_bytes());
    hello[88..96].copy_from_slice(&1_u64.to_le_bytes());
    hello[96] = 0b0011;
    hello[97] = 0b0001;
    let decoded = wire::decode(&hello).unwrap();
    let Body::Hello { contact, state } = &decoded.body else {
        panic!("hello kind");
    };
    assert_eq!(*contact, HelloNonce::from_bytes([4; 16]).unwrap());
    assert_eq!(state.sources.as_slice().len(), 0);
    assert_eq!(wire::encode(&decoded).unwrap(), hello);
    assert!(wire::decode(b"RLC2{}").is_err());
    assert!(wire::decode(b"RLC3{}").is_err());
    assert!(wire::decode(b"RLC4{}").is_err());
    assert!(media::header(b"RLP2").is_err());
    assert!(media::header(b"RLP4").is_err());

    let entries = (1..=25)
        .map(|byte| SourceEntry {
            id: source(byte),
            epoch: PublicationEpoch::new(1).unwrap(),
            kind: SourceKind::System,
            state: PublicationState::Unavailable,
            format: None,
        })
        .collect();
    let state = Snapshot::new(intent(true, true), Catalogue::new(entries).unwrap());
    let ready = Control {
        from: member(1),
        to: member(2),
        run: run(3),
        body: Body::Ready {
            session: DirectSessionId::from_bytes([5; 16]).unwrap(),
            target_run: run(4),
            contact: HelloNonce::from_bytes([6; 16]).unwrap(),
            target_contact: HelloNonce::from_bytes([7; 16]).unwrap(),
            nonce: rlb_model::BindNonce::from_bytes([8; 16]).unwrap(),
            state,
            ack: Ack {
                send_generation: 1,
                receive_generation: 1,
                state_revision: 1,
            },
        },
    };
    let bytes = wire::encode(&ready).unwrap();
    assert_eq!(bytes.len(), 192 + SOURCE_RECORD_BYTES * 25);
    assert_eq!(bytes.len(), 1192);
    assert_eq!(
        wire::decode(&bytes)
            .unwrap()
            .body
            .state()
            .unwrap()
            .sources
            .as_slice()
            .len(),
        25
    );
    let mut oversized = bytes.clone();
    oversized.extend_from_slice(&[0; 40]);
    assert!(matches!(
        wire::decode(&oversized),
        Err(PeerError::DatagramTooLarge)
    ));
    let mut claimed = hello.clone();
    claimed[98] = 26;
    claimed[99] = 0;
    assert!(matches!(
        wire::decode(&claimed),
        Err(PeerError::MalformedControl)
    ));
}

#[test]
fn two_sources_keep_finite_words_and_independent_replacement() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[
        (2, SourceKind::System, stereo()),
        (3, SourceKind::Microphone, mono()),
    ]);
    let system_words = [0x8000_0000, 0x0000_0001, 0x7f7f_ffff, 0xc020_0000];
    let system_samples: Vec<f64> = system_words
        .into_iter()
        .cycle()
        .take(8)
        .map(f32::from_bits)
        .map(f64::from)
        .collect();
    let system = publisher
        .local_original(source(2), acquisition(2), 0, &system_samples, 10)
        .unwrap();
    let system_batch = publisher.prepare_pcm(system).unwrap();
    let mic = publisher
        .local_original(source(3), acquisition(3), 0, &[0.5, -0.25], 10)
        .unwrap();
    let mic_batch = publisher.prepare_pcm(mic).unwrap();
    let mut sent = Vec::new();
    for packet in system_batch.as_slice().iter().chain(mic_batch.as_slice()) {
        sent.push(sender.write_original(packet).unwrap().unwrap());
    }
    let admitted: Vec<_> = sent
        .iter()
        .map(|bytes| receiver.ingest(bytes, 11, 4).unwrap().unwrap())
        .collect();
    assert_eq!(
        admitted[0]
            .samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        system_samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    assert_eq!(admitted[0].source_id, source(2));
    assert_eq!(admitted[1].source_id, source(3));
    assert_eq!(admitted[0].arrived_at_ms, 4);

    publisher.set_publication(source(3), false).unwrap();
    sender
        .install_catalogue(&publisher.catalogue(), 12)
        .unwrap();
    exchange_both(&mut sender, &mut receiver, 12);
    assert!(
        sender
            .write_original(mic_batch.as_slice().first().unwrap())
            .is_err()
    );
    assert!(
        sender
            .write_original(&system_batch.as_slice()[0])
            .unwrap()
            .is_some()
    );
    let next_system = publisher
        .local_original(source(2), acquisition(2), 4, &system_samples[..2], 12)
        .unwrap();
    let next = publisher.prepare_pcm(next_system).unwrap();
    let bytes = sender.write_original(&next.as_slice()[0]).unwrap().unwrap();
    let audio = receiver.ingest(&bytes, 13, 13).unwrap().unwrap();
    assert_eq!(audio.source_id, source(2));
    assert!(receiver.has_seen(source(2), 1));
    assert!(!receiver.has_seen(source(3), 99));

    let mut nan = next.as_slice()[0].payload().to_vec();
    nan[..4].copy_from_slice(&f32::NAN.to_le_bytes());
    let header = Header {
        session: audio.session,
        send_generation: audio.send_generation,
        receive_generation: audio.receive_generation,
        source: source(2),
        epoch: audio.epoch.get(),
        sequence: 9,
        start_frame: 50,
        frames: 1,
        payload_bytes: 8,
    };
    let malformed = media::encode(header, &nan[..8]).unwrap();
    assert_eq!(
        receiver.ingest(&malformed, 14, 14),
        Err(PeerError::NonFiniteSample)
    );
    assert!(!receiver.has_seen(source(2), 9));
    let _ = mic_batch;
}

#[test]
fn refusal_regrant_reconnect_and_fresh_run_keep_their_windows() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let original = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25], 20)
        .unwrap();
    let batch = publisher.prepare_pcm(original).unwrap();
    let first = sender
        .write_original(&batch.as_slice()[0])
        .unwrap()
        .unwrap();
    let admitted = receiver.ingest(&first, 20, 20).unwrap().unwrap();
    let old_token = admitted.admission_token;
    receiver.set_intent(intent(true, false), 21).unwrap();
    exchange_both(&mut sender, &mut receiver, 21);
    assert!(!sender.status().send_ready);
    assert_ne!(receiver.admission().direction_fence, old_token);
    assert!(receiver.ingest(&first, 22, 22).unwrap().is_none());
    assert!(receiver.has_seen(source(2), 1));
    receiver.set_intent(intent(true, true), 23).unwrap();
    exchange_both(&mut sender, &mut receiver, 23);
    let again = publisher
        .local_original(source(2), acquisition(2), 1, &[0.5], 24)
        .unwrap();
    let next = publisher.prepare_pcm(again).unwrap();
    let bytes = sender.write_original(&next.as_slice()[0]).unwrap().unwrap();
    let admitted = receiver.ingest(&bytes, 24, 7).unwrap().unwrap();
    assert_ne!(admitted.admission_token, old_token);
    assert_eq!(admitted.arrived_at_ms, 7);

    let revision = sender.local_revision();
    sender.reconnect(30).unwrap();
    assert_eq!(sender.local_revision(), revision);
    exchange_both(&mut sender, &mut receiver, 30);
    assert!(receiver.ingest(&bytes, 31, 31).unwrap().is_none());
    assert!(receiver.has_seen(source(2), next.as_slice()[0].sequence()));

    let mut fresh = SourcePublisher::new(run(9));
    fresh.register(source(2), SourceKind::System, true).unwrap();
    bind(&mut fresh, source(2), acquisition(2), mono());
    let mut replacement = Peer::new(
        member(1),
        member(2),
        run(9),
        intent(true, true),
        fresh.catalogue(),
        40,
    )
    .unwrap();
    exchange_both(&mut replacement, &mut receiver, 40);
    let sample = fresh
        .local_original(source(2), acquisition(2), 0, &[-0.0], 40)
        .unwrap();
    let packet = fresh.prepare_pcm(sample).unwrap();
    let bytes = replacement
        .write_original(&packet.as_slice()[0])
        .unwrap()
        .unwrap();
    let admitted = receiver.ingest(&bytes, 41, 41).unwrap().unwrap();
    assert_eq!(admitted.remote_run, run(9));
    assert_eq!(admitted.samples[0].to_bits(), (-0.0_f64).to_bits());
}

#[test]
fn skipped_refusal_snapshots_still_retire_both_permission_intervals() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let original = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25], 20)
        .unwrap();
    let batch = publisher.prepare_pcm(original).unwrap();
    let bytes = sender
        .write_original(&batch.as_slice()[0])
        .unwrap()
        .unwrap();
    let old = receiver.ingest(&bytes, 20, 20).unwrap().unwrap();

    sender.set_intent(intent(false, true), 21).unwrap();
    while sender.take_control().is_some() {}
    sender.set_intent(intent(true, true), 22).unwrap();
    exchange_both(&mut sender, &mut receiver, 22);
    assert!(receiver.status().receive_ready);
    assert_ne!(receiver.admission().sources[0].fence, old.admission_token);

    let original = publisher
        .local_original(source(2), acquisition(2), 1, &[0.5], 23)
        .unwrap();
    let queued = publisher.prepare_pcm(original).unwrap();
    assert!(
        sender
            .write_original(&queued.as_slice()[0])
            .unwrap()
            .is_some()
    );
    receiver.set_intent(intent(true, false), 24).unwrap();
    while receiver.take_control().is_some() {}
    receiver.set_intent(intent(true, true), 25).unwrap();
    exchange_both(&mut sender, &mut receiver, 25);
    assert!(sender.status().send_ready);
    assert!(
        sender
            .write_original(&queued.as_slice()[0])
            .unwrap()
            .is_none()
    );
    let original = publisher
        .local_original(source(2), acquisition(2), 2, &[0.75], 26)
        .unwrap();
    let current = publisher.prepare_pcm(original).unwrap();
    let bytes = sender
        .write_original(&current.as_slice()[0])
        .unwrap()
        .unwrap();
    assert!(receiver.ingest(&bytes, 26, 26).unwrap().is_some());
}

#[test]
fn stale_completion_cannot_replace_a_remembered_refusal() {
    let (_, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    receiver.set_intent(intent(false, true), 50).unwrap();
    exchange_both(&mut sender, &mut receiver, 50);
    assert!(sender.status().connected);
    assert!(!sender.status().receive_ready);
    while sender.take_control().is_some() {}
    while receiver.take_control().is_some() {}

    let old = Snapshot::new(intent(true, true), Catalogue::new(Vec::new()).unwrap());
    let contact = HelloNonce::from_bytes([9; 16]).unwrap();
    let hello = wire::encode(&Control {
        from: member(2),
        to: member(1),
        run: run(4),
        body: Body::Hello {
            contact,
            state: old.clone(),
        },
    })
    .unwrap();
    sender.ingest(&hello, 51, 51).unwrap();
    let offer = sender.take_control().unwrap();
    assert_eq!(offer[4], 2);
    let session = DirectSessionId::from_bytes(offer[56..72].try_into().unwrap()).unwrap();
    let target_contact = HelloNonce::from_bytes(offer[88..104].try_into().unwrap()).unwrap();
    let confirm = wire::encode(&Control {
        from: member(2),
        to: member(1),
        run: run(4),
        body: Body::Confirm {
            session,
            target_run: run(3),
            contact,
            target_contact,
            nonce: rlb_model::BindNonce::from_bytes([8; 16]).unwrap(),
            state: old,
        },
    })
    .unwrap();
    sender.ingest(&confirm, 52, 52).unwrap();
    assert!(sender.status().connected);
    assert!(!sender.status().receive_ready);

    receiver.set_intent(intent(true, true), 53).unwrap();
    exchange_both(&mut sender, &mut receiver, 53);
    assert!(sender.status().connected);
    assert!(sender.status().receive_ready);
}

#[test]
fn pause_retains_revision_and_drops_outgoing_contact() {
    let (_, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let revision = sender.local_revision();
    sender.set_intent(intent(false, true), 60).unwrap();
    let changed = sender.local_revision();
    assert!(changed > revision);
    sender.pause();
    assert!(sender.is_paused());
    assert!(sender.take_control().is_none());
    sender.pause();
    sender.set_intent(intent(true, true), 61).unwrap();
    assert!(sender.take_control().is_none());
    assert!(sender.local_revision() > changed);
    sender.resume(62).unwrap();
    assert!(!sender.is_paused());
    let resumed = sender.local_revision();
    exchange_both(&mut sender, &mut receiver, 62);
    assert!(
        sender.status().send_ready,
        "sender {:?} receiver {:?}",
        sender.status(),
        receiver.status()
    );
    assert_eq!(sender.local_revision(), resumed);
    assert!(resumed > revision);
}

fn ticket(value: u64) -> CaptureTicket {
    CaptureTicket::new(value).expect("nonzero ticket")
}

fn operation(now_ms: u64, allocated_through: u64) -> PeerOperation {
    PeerOperation {
        now_ms,
        watermark: CaptureWatermark { allocated_through },
    }
}

#[test]
fn equal_enqueue_time_still_refuses_the_ticket_from_the_closed_interval() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let mut old_storage = PreparedStorage::for_format(mono()).unwrap();
    let old = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25], 50)
        .unwrap()
        .with_ticket(ticket(1));
    let old_view = publisher.prepare_pcm_into(old, &mut old_storage).unwrap();
    assert_eq!(old_view.get(0).unwrap().enqueued_at_ms(), 50);
    sender
        .set_intent_with(intent(false, true), operation(60, 1))
        .unwrap();
    let closed = sender.outgoing_interval().revision;
    sender
        .set_intent_with(intent(true, true), operation(61, 1))
        .unwrap();
    assert!(sender.outgoing_interval().revision > closed);
    assert_eq!(sender.outgoing_interval().ticket_floor, 1);
    exchange_both(&mut sender, &mut receiver, 61);
    let mut scratch = [0_u8; 1200];
    assert!(
        sender
            .write_original_into(&old_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_none()
    );
    let mut new_storage = PreparedStorage::for_format(mono()).unwrap();
    let new = publisher
        .local_original(source(2), acquisition(2), 1, &[0.5], 50)
        .unwrap()
        .with_ticket(ticket(2));
    let new_view = publisher.prepare_pcm_into(new, &mut new_storage).unwrap();
    assert_eq!(new_view.get(0).unwrap().enqueued_at_ms(), 50);
    assert!(
        sender
            .write_original_into(&new_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_some()
    );
}

#[test]
fn consecutive_local_reopens_each_sample_the_current_watermark_in_one_millisecond() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let initial = sender.outgoing_interval().revision;
    sender
        .set_intent_with(intent(false, true), operation(50, 1))
        .unwrap();
    sender
        .set_intent_with(intent(true, true), operation(50, 1))
        .unwrap();
    let first = sender.outgoing_interval();
    assert_eq!(first.revision, initial + 1);
    assert_eq!(first.ticket_floor, 1);
    exchange_both(&mut sender, &mut receiver, 50);

    let mut old_storage = PreparedStorage::for_format(mono()).unwrap();
    let old = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25], 50)
        .unwrap()
        .with_ticket(ticket(2));
    let old_view = publisher.prepare_pcm_into(old, &mut old_storage).unwrap();
    sender
        .set_intent_with(intent(false, true), operation(50, 2))
        .unwrap();
    sender
        .set_intent_with(intent(true, true), operation(50, 2))
        .unwrap();
    let second = sender.outgoing_interval();
    assert_eq!(second.revision, first.revision + 1);
    assert_eq!(second.ticket_floor, 2);
    exchange_both(&mut sender, &mut receiver, 50);
    assert!(sender.status().send_ready);
    let mut scratch = [0_u8; 1200];
    assert!(
        sender
            .write_original_into(&old_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_none()
    );

    let mut fresh_storage = PreparedStorage::for_format(mono()).unwrap();
    let fresh = publisher
        .local_original(source(2), acquisition(2), 1, &[0.5], 50)
        .unwrap()
        .with_ticket(ticket(3));
    let fresh_view = publisher
        .prepare_pcm_into(fresh, &mut fresh_storage)
        .unwrap();
    assert!(
        sender
            .write_original_into(&fresh_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_some()
    );
}

#[test]
fn session_replacement_samples_a_floor_while_send_stays_ready() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    assert!(sender.status().send_ready);
    let mut old_storage = PreparedStorage::for_format(mono()).unwrap();
    let old = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25], 70)
        .unwrap()
        .with_ticket(ticket(4));
    let old_view = publisher.prepare_pcm_into(old, &mut old_storage).unwrap();
    let before = sender.outgoing_interval();
    assert!(
        sender
            .reopen_session_for_test(CaptureWatermark::through(ticket(4)))
            .unwrap()
    );
    assert!(sender.status().send_ready);
    assert!(sender.outgoing_interval().revision > before.revision);
    assert_eq!(sender.outgoing_interval().ticket_floor, 4);
    let mut scratch = [0_u8; 1200];
    assert!(
        sender
            .write_original_into(&old_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_none()
    );
    let mut new_storage = PreparedStorage::for_format(mono()).unwrap();
    let new = publisher
        .local_original(source(2), acquisition(2), 1, &[0.5], 70)
        .unwrap()
        .with_ticket(ticket(5));
    let new_view = publisher.prepare_pcm_into(new, &mut new_storage).unwrap();
    // Pair-level readiness survives this synthetic activation, while the new
    // session still needs the receiver's explicit source grant.
    assert!(
        sender
            .write_original_into(&new_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_none()
    );
    sender.reconnect(70).unwrap();
    for _ in 0..16 {
        let mut moved = false;
        while let Some(bytes) = sender.take_control() {
            receiver.ingest_with(&bytes, 70, operation(70, 0)).unwrap();
            moved = true;
        }
        while let Some(bytes) = receiver.take_control() {
            sender.ingest_with(&bytes, 70, operation(70, 5)).unwrap();
            moved = true;
        }
        if !moved {
            break;
        }
    }
    assert!(sender.source_send_ready(source(2)));
    assert!(
        sender
            .write_original_into(&new_view.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_none()
    );
    let fresh = publisher
        .local_original(source(2), acquisition(2), 2, &[0.75], 71)
        .unwrap()
        .with_ticket(ticket(6));
    let fresh = publisher.prepare_pcm_into(fresh, &mut new_storage).unwrap();
    assert!(
        sender
            .write_original_into(&fresh.get(0).unwrap(), &mut scratch)
            .unwrap()
            .is_some()
    );
}

#[test]
fn ordered_reopen_and_skipped_remote_receive_replace_once() {
    let (_publisher, mut sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let initial = sender.outgoing_interval().revision;
    sender
        .set_intent_with(intent(false, true), operation(80, 3))
        .unwrap();
    assert_eq!(sender.outgoing_interval().revision, initial);
    sender
        .set_intent_with(intent(true, true), operation(81, 3))
        .unwrap();
    let reopened = sender.outgoing_interval().revision;
    assert!(reopened > initial);
    sender
        .set_intent_with(intent(true, true), operation(82, 3))
        .unwrap();
    assert_eq!(sender.outgoing_interval().revision, reopened);
    exchange_both(&mut sender, &mut receiver, 82);
    assert!(sender.status().send_ready);

    let before_remote = sender.outgoing_interval().revision;
    sender
        .advance_remote_receive_for_test(CaptureWatermark::through(ticket(6)))
        .unwrap();
    assert!(sender.status().send_ready);
    assert!(sender.outgoing_interval().revision > before_remote);
    let after_remote = sender.outgoing_interval().revision;
    assert_eq!(
        sender
            .repeat_remote_snapshot_for_test(CaptureWatermark::through(ticket(6)))
            .unwrap(),
        after_remote
    );
}

#[test]
fn admission_snapshot_keeps_full_entries_and_outer_instance() {
    let (mut publisher, mut sender, mut receiver) = ready_pair(&[
        (2, SourceKind::System, stereo()),
        (3, SourceKind::Microphone, mono()),
    ]);
    let instance = PeerInstance::new(7).unwrap();
    receiver.attach_instance(instance).unwrap();
    assert_eq!(
        receiver.attach_instance(PeerInstance::new(8).unwrap()),
        Err(PeerError::InstanceConflict)
    );
    let snapshot = receiver.admission_snapshot();
    assert_eq!(snapshot.peer_instance, Some(instance));
    assert_eq!(snapshot.sources.len(), 2);
    assert_eq!(snapshot.sources[0].source_id(), source(2));
    assert_eq!(snapshot.sources[0].state(), PublicationState::Active);
    assert_eq!(snapshot.sources[0].format(), Some(stereo()));
    let other_fence = snapshot.sources[1].fence;
    let revision = snapshot.revision;

    publisher.set_publication(source(2), false).unwrap();
    sender
        .install_catalogue(&publisher.catalogue(), 90)
        .unwrap();
    exchange_both(&mut sender, &mut receiver, 90);
    let changed = receiver.admission_snapshot();
    assert!(changed.revision > revision);
    assert_eq!(changed.sources[0].state(), PublicationState::Disabled);
    assert!(changed.sources[0].format().is_none());
    assert_ne!(changed.sources[0].fence, snapshot.sources[0].fence);
    assert_eq!(changed.sources[1].fence, other_fence);
    assert_eq!(changed.sources[1].format(), Some(mono()));

    let before_error = receiver.admission_snapshot().revision;
    assert!(receiver.ingest(b"RLP3", 91, 91).is_err());
    let mut projection = AdmissionProjection::default();
    receiver.project_admission(&mut projection);
    assert_eq!(projection.revision, before_error);
    assert_eq!(projection.peer_instance, Some(instance));
    assert!(!projection.quiesced);

    receiver
        .set_intent_with(intent(false, true), operation(92, 1))
        .unwrap();
    receiver.exhaust_outgoing_for_test();
    assert_eq!(
        receiver.set_intent_with(intent(true, true), operation(93, 1)),
        Err(PeerError::CounterExhausted)
    );
    assert!(receiver.admission_snapshot().quiesced);
}

#[test]
fn admission_revision_reserves_max_for_a_stable_closed_snapshot() {
    let (_publisher, mut sender, _receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    sender.exhaust_admission_on_next_change_for_test();
    let before = sender.admission_snapshot();
    assert_eq!(before.revision, u64::MAX - 1);
    assert!(before.connected && before.send_ready && before.receive_ready);
    assert!(!before.quiesced);

    let instance = PeerInstance::new(7).unwrap();
    sender.attach_instance(instance).unwrap();
    let terminal = sender.admission_snapshot();
    assert_eq!(terminal.revision, u64::MAX);
    assert!(terminal.quiesced);
    assert!(!terminal.connected && !terminal.send_ready && !terminal.receive_ready);
    assert!(terminal.session.is_none());
    assert!(sender.is_paused());
    assert!(sender.take_control().is_none());

    sender.attach_instance(instance).unwrap();
    assert_eq!(
        sender.set_intent_with(intent(false, true), operation(11, 0)),
        Err(PeerError::Quiesced)
    );
    assert_eq!(sender.admission_snapshot(), terminal);
}

#[test]
fn terminal_admission_during_received_source_state_keeps_contact_closed() {
    let (_publisher, mut sender, mut receiver) = ready_pair(&[
        (2, SourceKind::System, mono()),
        (3, SourceKind::Microphone, mono()),
    ]);
    receiver.exhaust_admission_on_next_change_for_test();
    sender
        .set_source_policy_with(
            crate::SourcePolicy {
                default: intent(true, true),
                overrides: vec![crate::SourceIntent {
                    kind: SourceKind::Microphone,
                    intent: intent(false, true),
                }],
            },
            operation(20, 0),
        )
        .unwrap();
    while let Some(bytes) = sender.take_control() {
        receiver.ingest_with(&bytes, 20, operation(20, 0)).unwrap();
    }
    let closed = receiver.admission_snapshot();
    assert_eq!(closed.revision, u64::MAX);
    assert!(closed.quiesced && !closed.connected && closed.session.is_none());
    assert!(receiver.take_control().is_none());
}

#[test]
fn short_storage_does_not_commit_receive_or_preparation() {
    let (mut publisher, sender, mut receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    let mut tiny = PreparedStorage::with_limits(0, 0);
    let original = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25, -0.5], 10)
        .unwrap()
        .with_ticket(ticket(1));
    assert!(matches!(
        publisher.prepare_pcm_into(original, &mut tiny),
        Err(SourceError::StorageTooSmall)
    ));
    let mut storage = PreparedStorage::for_format(mono()).unwrap();
    let original = publisher
        .local_original(source(2), acquisition(2), 0, &[0.25, -0.5], 10)
        .unwrap()
        .with_ticket(ticket(1));
    let view = publisher.prepare_pcm_into(original, &mut storage).unwrap();
    assert_eq!(view.get(0).unwrap().sequence(), 1);
    let mut datagram = [0_u8; 1200];
    assert_eq!(
        sender.write_original_into(&view.get(0).unwrap(), &mut datagram[..8]),
        Err(PeerError::StorageTooSmall)
    );
    let written = sender
        .write_original_into(&view.get(0).unwrap(), &mut datagram)
        .unwrap()
        .unwrap();
    let bytes = written.bytes.to_vec();
    assert!(matches!(
        receiver.ingest_into(&bytes, 11, operation(11, 1), &mut []),
        Err(PeerError::StorageTooSmall)
    ));
    assert!(!receiver.has_seen(source(2), 1));
    let mut pcm = [0.0_f64; 8];
    let admitted = receiver
        .ingest_into(&bytes, 11, operation(12, 1), &mut pcm)
        .unwrap()
        .unwrap();
    assert_eq!(admitted.metadata.arrived_at_ms, 11);
    assert_eq!(admitted.samples[0].to_bits(), 0.25_f64.to_bits());
    assert_eq!(admitted.samples[1].to_bits(), (-0.5_f64).to_bits());
    assert!(receiver.has_seen(source(2), 1));
    let legacy = media::decode_pcm(&bytes[80..], mono(), admitted.metadata.frames).unwrap();
    assert_eq!(legacy[1].to_bits(), (-0.5_f64).to_bits());
}

#[test]
fn one_preparation_fans_out_with_independent_intervals() {
    let (mut publisher, mut first, mut first_remote) =
        ready_pair(&[(2, SourceKind::System, mono())]);
    let mut second = Peer::new(
        member(1),
        member(9),
        run(3),
        intent(true, true),
        publisher.catalogue(),
        10,
    )
    .unwrap();
    let mut second_remote = Peer::new(
        member(9),
        member(1),
        run(8),
        intent(true, true),
        Catalogue::new(Vec::new()).unwrap(),
        10,
    )
    .unwrap();
    exchange_both(&mut second, &mut second_remote, 10);
    assert!(first.status().send_ready && second.status().send_ready);
    let mut storage = PreparedStorage::for_format(mono()).unwrap();
    let original = publisher
        .local_original(source(2), acquisition(2), 0, &[0.125, 1.5], 15)
        .unwrap()
        .with_ticket(ticket(5));
    let view = publisher.prepare_pcm_into(original, &mut storage).unwrap();
    let fragment = view.get(0).unwrap();
    let mut first_bytes = [0_u8; 1200];
    let mut second_bytes = [0_u8; 1200];
    let first_written = first
        .write_original_into(&fragment, &mut first_bytes)
        .unwrap()
        .unwrap();
    let first_copy = first_written.bytes.to_vec();
    let second_written = second
        .write_original_into(&fragment, &mut second_bytes)
        .unwrap()
        .unwrap();
    assert_eq!(first_copy[80..], second_written.bytes[80..]);
    assert_ne!(first_copy[4..20], second_written.bytes[4..20]);
    assert_eq!(fragment.sequence(), 1);
    let _ = second_written;
    first
        .set_intent_with(intent(false, true), operation(16, 5))
        .unwrap();
    first
        .set_intent_with(intent(true, true), operation(17, 5))
        .unwrap();
    exchange_both(&mut first, &mut first_remote, 17);
    assert!(
        first
            .write_original_into(&view.get(0).unwrap(), &mut first_bytes)
            .unwrap()
            .is_none()
    );
    assert!(
        second
            .write_original_into(&view.get(0).unwrap(), &mut second_bytes)
            .unwrap()
            .is_some()
    );
}

#[test]
fn regressing_watermark_and_unticketed_write_are_refused() {
    let (_publisher, mut sender, _receiver) = ready_pair(&[(2, SourceKind::System, mono())]);
    sender
        .set_intent_with(intent(false, true), operation(30, 4))
        .unwrap();
    assert_eq!(
        sender.set_intent_with(intent(true, true), operation(31, 3)),
        Err(PeerError::RegressingWatermark)
    );
    let (mut legacy_publisher, legacy_sender, _) = ready_pair(&[(2, SourceKind::System, mono())]);
    let original = legacy_publisher
        .local_original(source(2), acquisition(2), 0, &[0.25], 10)
        .unwrap();
    let batch = legacy_publisher.prepare_pcm(original).unwrap();
    let mut legacy_scratch = [0_u8; 1200];
    assert_eq!(
        legacy_sender.write_original_into(&batch.as_slice()[0].fragment(), &mut legacy_scratch),
        Err(PeerError::MissingCaptureTicket)
    );
    sender.exhaust_outgoing_for_test();
    assert_eq!(
        sender.set_intent_with(intent(true, true), operation(32, 4)),
        Err(PeerError::CounterExhausted)
    );
    assert!(sender.is_quiesced());
}

#[test]
fn native_pcm_precision_layout_and_mtu_survive_normal_peers() {
    for (encoding, valid_bits, layout) in [
        (PcmEncoding::F32Le, 32, ChannelLayout::Surround51),
        (PcmEncoding::F32Le, 32, ChannelLayout::Surround51Side),
        (PcmEncoding::Signed32Le, 32, ChannelLayout::Surround71),
        (PcmEncoding::Signed32Le, 24, ChannelLayout::Surround51Side),
        (PcmEncoding::F64Le, 64, ChannelLayout::Surround71),
        (PcmEncoding::F64Le, 64, ChannelLayout::Surround71Wide),
    ] {
        let format = PcmFormat::new(48_000, layout, encoding, valid_bits).unwrap();
        let capacity = crate::max_packet_frames(format);
        let frames = capacity * 2 + 3;
        let channels = usize::from(format.channels());
        let samples: Vec<f64> = (0..frames * channels).map(|index| match encoding {
            PcmEncoding::F32Le => f64::from(match index {
                0 => -0.0_f32,
                1 => f32::from_bits(1),
                _ => f32::from_bits(0x3e80_0000 + index as u32),
            }),
            PcmEncoding::Signed32Le => {
                let unused = 32 - u32::from(valid_bits);
                let integer = match index {
                    0 => i32::MIN,
                    1 => (i32::MAX >> unused) << unused,
                    _ => ((index as i32 * 17) - 1_024) << unused,
                };
                f64::from(integer) / 2_147_483_648.0
            }
            PcmEncoding::F64Le => f64::from_bits(0x3fe0_0000_0000_0001 + index as u64),
        }).collect();
        if encoding == PcmEncoding::F64Le {
            assert_ne!(samples[0], f64::from(samples[0] as f32));
        }
        // Each interleaved position carries an independent value. Comparing
        // every canonical word catches channel swaps as well as precision loss.
        let (mut publisher, sender, mut receiver) =
            ready_pair(&[(2, SourceKind::System, format)]);
        let original = publisher.local_original(source(2), acquisition(2), 0, &samples, 10)
            .unwrap().with_ticket(ticket(1));
        let mut storage = PreparedStorage::for_format(format).unwrap();
        let prepared = publisher.prepare_pcm_into(original, &mut storage).unwrap();
        assert_eq!(prepared.len(), 3);
        let mut datagram = [0_u8; crate::MAX_DATAGRAM_BYTES];
        let mut decoded = [0.0_f64; crate::MAX_DATAGRAM_BYTES / 4];
        let mut next_frame = 0;
        for index in 0..prepared.len() {
            let fragment = prepared.get(index).unwrap();
            let written = sender.write_original_into(&fragment, &mut datagram).unwrap().unwrap();
            assert_eq!(&written.bytes[..4], b"RLP5");
            assert!(written.bytes.len() <= crate::MAX_DATAGRAM_BYTES);
            assert_eq!(written.bytes.len(), crate::MEDIA_HEADER_BYTES
                + usize::from(fragment.frames()) * format.frame_bytes());
            if index < 2 {
                assert_eq!(usize::from(fragment.frames()), capacity);
                assert!(written.bytes.len() + format.frame_bytes() > crate::MAX_DATAGRAM_BYTES);
            }
            let admitted = receiver.ingest_into(written.bytes, 11, operation(11, 0), &mut decoded)
                .unwrap().unwrap();
            assert_eq!(admitted.metadata.entry.format, Some(format));
            assert_eq!(admitted.metadata.entry.format.unwrap().layout().channel_mask(), layout.channel_mask());
            assert_eq!(admitted.metadata.start_frame, next_frame);
            let offset = next_frame as usize * channels;
            for (actual, expected) in admitted.samples.iter().zip(&samples[offset..]) {
                assert_eq!(actual.to_bits(), expected.to_bits(), "{encoding:?} {layout:?}");
            }
            next_frame += u64::from(admitted.metadata.frames);
        }
        assert_eq!(next_frame, frames as u64);
    }
}
