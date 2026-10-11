use rlb_model::{MemberId, RunId};

use crate::{
    Intent, MAX_DATAGRAM_BYTES, Peer, PeerError,
    wire::{self, Body},
};

fn member(value: u8) -> MemberId {
    MemberId::from_bytes([value; 16]).unwrap()
}
fn run(value: u8) -> RunId {
    RunId::from_bytes([value; 16]).unwrap()
}
fn intent(send: bool, receive: bool) -> Intent {
    Intent { send, receive }
}
fn pair(a_intent: Intent, b_intent: Intent) -> (Peer, Peer) {
    (
        Peer::new(member(1), member(2), run(3), a_intent, 0).unwrap(),
        Peer::new(member(2), member(1), run(4), b_intent, 0).unwrap(),
    )
}

fn pump(a: &mut Peer, b: &mut Peer, now_ms: u64) {
    for _ in 0..32 {
        let mut count = 0;
        while let Some(bytes) = a.take_control() {
            b.ingest(&bytes, now_ms).unwrap();
            count += 1;
        }
        while let Some(bytes) = b.take_control() {
            a.ingest(&bytes, now_ms).unwrap();
            count += 1;
        }
        if count == 0 {
            return;
        }
    }
    panic!("control exchange failed to quiesce");
}

#[test]
fn all_sixteen_permission_combinations_are_independently_bilateral() {
    for bits in 0..16 {
        let a_intent = intent(bits & 1 != 0, bits & 2 != 0);
        let b_intent = intent(bits & 4 != 0, bits & 8 != 0);
        let (mut a, mut b) = pair(a_intent, b_intent);
        pump(&mut a, &mut b, 0);
        assert!(a.status().connected && b.status().connected);
        assert_eq!(a.status().send_ready, a_intent.send && b_intent.receive);
        assert_eq!(a.status().receive_ready, a_intent.receive && b_intent.send);
        assert_eq!(b.status().send_ready, b_intent.send && a_intent.receive);
        assert_eq!(b.status().receive_ready, b_intent.receive && a_intent.send);
    }
}

#[test]
fn revoke_and_regrant_reject_old_media_and_preserve_reverse_direction() {
    let (mut a, mut b) = pair(intent(true, true), intent(true, true));
    pump(&mut a, &mut b, 0);
    let session = a.status().session;
    let old_audio = a.encode_original(48_000, 1, 0, &[0.25]).unwrap().remove(0);
    b.set_intent(intent(true, false), 1).unwrap();
    assert!(!b.status().receive_ready);
    assert!(b.status().send_ready);
    assert!(b.ingest(&old_audio, 1).unwrap().is_none());
    pump(&mut a, &mut b, 1);
    assert!(!a.status().send_ready);
    assert!(a.status().receive_ready);
    b.set_intent(intent(true, true), 2).unwrap();
    pump(&mut a, &mut b, 2);
    assert_eq!(a.status().session, session);
    assert!(a.status().send_ready && b.status().send_ready);
    assert!(b.ingest(&old_audio, 2).unwrap().is_none());
    let current = a.encode_original(48_000, 1, 1, &[0.5]).unwrap().remove(0);
    assert_eq!(b.ingest(&current, 2).unwrap().unwrap().samples, vec![0.5]);
    // A sender regrant also requires a fresh acknowledgement. Its receive
    // direction remains usable throughout.
    a.set_intent(intent(false, true), 3).unwrap();
    a.set_intent(intent(true, true), 3).unwrap();
    assert!(!a.status().send_ready);
    assert!(a.status().receive_ready);
    pump(&mut a, &mut b, 3);
    assert!(a.status().send_ready);
    assert!(b.ingest(&current, 3).unwrap().is_none());
}

#[test]
fn fresh_challenge_blocks_old_run_handshake_and_media_replays() {
    let (mut a, mut b) = pair(intent(true, true), intent(true, true));
    // Save every handshake message in both directions for later replay.
    let mut to_a = Vec::new();
    let mut to_b = Vec::new();
    for _ in 0..8 {
        while let Some(bytes) = a.take_control() {
            b.ingest(&bytes, 0).unwrap();
            to_b.push(bytes);
        }
        while let Some(bytes) = b.take_control() {
            a.ingest(&bytes, 0).unwrap();
            to_a.push(bytes);
        }
    }
    let old_session = a.status().session.unwrap();
    let old_audio = a.encode_original(48_000, 1, 0, &[0.25]).unwrap().remove(0);
    b = Peer::new(member(2), member(1), run(5), intent(false, false), 1).unwrap();
    // A new run's Hello alone preserves A's established state.
    let hello = b.take_control().unwrap();
    a.ingest(&hello, 1).unwrap();
    assert_eq!(a.status().session, Some(old_session));
    // The recorded old messages have old recipient-run/contact/challenge
    // context and cannot promote a candidate on the restarted peer.
    for bytes in &to_b {
        b.ingest(bytes, 1).unwrap();
    }
    assert!(!b.status().connected);
    pump(&mut a, &mut b, 1);
    assert!(a.status().connected && b.status().connected);
    assert_ne!(a.status().session, Some(old_session));
    assert!(!a.status().send_ready);
    let current_session = a.status().session;
    for bytes in &to_a {
        a.ingest(bytes, 1).unwrap();
    }
    for bytes in &to_b {
        b.ingest(bytes, 1).unwrap();
    }
    assert_eq!(a.status().session, current_session);
    assert!(!b.status().receive_ready);
    assert!(b.ingest(&old_audio, 1).unwrap().is_none());
    // Recontact with the same run gets a new contact nonce and media session.
    b.reconnect(2).unwrap();
    pump(&mut a, &mut b, 2);
    assert!(a.status().connected && b.status().connected);
    assert_ne!(a.status().session, current_session);
}

#[test]
fn pcm_is_bit_exact_mtu_bounded_and_accepts_reordering_without_duplicates() {
    let (mut a, mut b) = pair(intent(true, true), intent(true, true));
    pump(&mut a, &mut b, 0);
    let values = [0.0, -0.0, f32::from_bits(1), -0.125, 1.0, f32::MAX];
    let samples: Vec<f32> = (0..1000)
        .map(|index| values[index % values.len()])
        .collect();
    let packets = a.encode_original(48_000, 2, 15, &samples).unwrap();
    assert!(packets.len() > 1);
    assert_eq!(crate::media::header(&packets[0]).unwrap().frames, 141);
    assert!(
        packets
            .iter()
            .all(|bytes| bytes.len() <= MAX_DATAGRAM_BYTES)
    );
    let mut received = Vec::new();
    for bytes in packets.iter().rev() {
        let audio = b.ingest(bytes, 1).unwrap().unwrap();
        assert_eq!((audio.sample_rate, audio.channels), (48_000, 2));
        received.push(audio);
        assert!(b.ingest(bytes, 1).unwrap().is_none());
    }
    received.sort_by_key(|audio| audio.start_frame);
    let actual: Vec<u32> = received
        .iter()
        .flat_map(|audio| audio.samples.iter().map(|sample| sample.to_bits()))
        .collect();
    assert_eq!(
        actual,
        samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        a.encode_original(48_000, 2, 515, &[f32::NAN, 0.0]),
        Err(PeerError::NonFiniteSample)
    );
    assert_eq!(
        a.encode_original(44_100, 2, 515, &[0.0, 0.0]),
        Err(PeerError::SourceFormatChanged)
    );
    assert_eq!(
        a.encode_original(48_000, 2, 514, &[0.0, 0.0]),
        Err(PeerError::SourceTimelineWentBackwards)
    );
}

#[test]
fn source_reset_changes_format_and_timeline_preserving_reverse_and_rejecting_late_audio() {
    let (mut a, mut b) = pair(intent(true, true), intent(true, true));
    pump(&mut a, &mut b, 0);
    let a_status = a.status();
    let b_status = b.status();
    let first = a
        .encode_original(48_000, 2, 100, &[0.25, -0.25])
        .unwrap()
        .remove(0);
    assert_eq!(b.ingest(&first, 1).unwrap().unwrap().source_epoch, 1);
    let late_old = a
        .encode_original(48_000, 2, 101, &[0.5, -0.5])
        .unwrap()
        .remove(0);

    assert_eq!(a.reset_source().unwrap(), 2);
    assert_eq!(a.status(), a_status);
    assert_eq!(b.status(), b_status);
    let samples = [-0.0, f32::from_bits(1), f32::MAX];
    let current = a.encode_original(44_100, 1, 0, &samples).unwrap().remove(0);
    let received = b.ingest(&current, 2).unwrap().unwrap();
    assert_eq!(received.source_epoch, 2);
    assert_eq!(
        (
            received.sample_rate,
            received.channels,
            received.start_frame
        ),
        (44_100, 1, 0)
    );
    assert_eq!(
        received
            .samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    assert!(b.ingest(&late_old, 2).unwrap().is_none());
    let reverse = b.encode_original(48_000, 1, 50, &[0.75]).unwrap().remove(0);
    assert_eq!(a.ingest(&reverse, 2).unwrap().unwrap().source_epoch, 1);
    assert_eq!(a.status(), a_status);

    // A malformed higher epoch cannot retire the most recent valid source.
    let next = a.encode_original(44_100, 1, 3, &[0.125]).unwrap().remove(0);
    let mut corrupt_future = next.clone();
    corrupt_future[60..68].copy_from_slice(&3_u64.to_le_bytes());
    corrupt_future[68..72].copy_from_slice(&f32::NAN.to_le_bytes());
    assert_eq!(
        b.ingest(&corrupt_future, 3),
        Err(PeerError::NonFiniteSample)
    );
    assert_eq!(b.ingest(&next, 3).unwrap().unwrap().source_epoch, 2);

    // A restart of the same format also permits frame zero in a fresh epoch.
    assert_eq!(a.reset_source().unwrap(), 3);
    let restarted = a.encode_original(44_100, 1, 0, &[0.5]).unwrap().remove(0);
    assert_eq!(b.ingest(&restarted, 3).unwrap().unwrap().source_epoch, 3);
    assert!(b.ingest(&current, 3).unwrap().is_none());
}

#[test]
fn receive_source_high_water_survives_regrant_and_recontact_until_remote_run_changes() {
    let (mut a, mut b) = pair(intent(true, true), intent(true, true));
    pump(&mut a, &mut b, 0);
    a.reset_source().unwrap();
    a.reset_source().unwrap();
    let first = a.encode_original(48_000, 1, 0, &[0.25]).unwrap().remove(0);
    assert_eq!(b.ingest(&first, 0).unwrap().unwrap().source_epoch, 3);
    b.set_intent(intent(true, false), 1).unwrap();
    pump(&mut a, &mut b, 1);
    b.set_intent(intent(true, true), 2).unwrap();
    pump(&mut a, &mut b, 2);
    let granted = a.encode_original(48_000, 1, 1, &[0.5]).unwrap().remove(0);
    let mut lower_epoch = granted.clone();
    lower_epoch[60..68].copy_from_slice(&2_u64.to_le_bytes());
    assert!(b.ingest(&lower_epoch, 2).unwrap().is_none());
    assert_eq!(b.ingest(&granted, 2).unwrap().unwrap().source_epoch, 3);

    let session = a.status().session;
    a.reconnect(3).unwrap();
    pump(&mut a, &mut b, 3);
    assert_ne!(a.status().session, session);
    let recontacted = a.encode_original(48_000, 1, 2, &[0.75]).unwrap().remove(0);
    let mut lower_epoch = recontacted.clone();
    lower_epoch[60..68].copy_from_slice(&1_u64.to_le_bytes());
    assert!(b.ingest(&lower_epoch, 3).unwrap().is_none());
    assert_eq!(b.ingest(&recontacted, 3).unwrap().unwrap().source_epoch, 3);
    let reverse = b.encode_original(48_000, 1, 0, &[0.125]).unwrap().remove(0);
    assert_eq!(a.ingest(&reverse, 3).unwrap().unwrap().samples, vec![0.125]);

    a = Peer::new(member(1), member(2), run(6), intent(true, true), 4).unwrap();
    pump(&mut a, &mut b, 4);
    assert_eq!(b.status().remote_run, Some(run(6)));
    let replacement = a.encode_original(44_100, 1, 0, &[-0.0]).unwrap().remove(0);
    let audio = b.ingest(&replacement, 4).unwrap().unwrap();
    assert_eq!(audio.source_epoch, 1);
    assert_eq!(audio.samples[0].to_bits(), (-0.0_f32).to_bits());
}

#[test]
fn lost_handshake_retries_and_timeout_recontacts_with_bounded_control() {
    let (mut a, mut b) = pair(intent(true, true), intent(true, true));
    while a.take_control().is_some() {}
    let hello = b.take_control().unwrap();
    a.ingest(&hello, 0).unwrap();
    // Lose the first Offer, then retry while Hello is suppressed by pending.
    while a.take_control().is_some() {}
    a.tick(250).unwrap();
    assert_eq!(a.next_deadline_ms(), 500);
    b.tick(250).unwrap();
    pump(&mut a, &mut b, 250);
    assert!(a.status().send_ready && b.status().send_ready);
    let old = a.status().session;
    a.tick(3_250).unwrap();
    b.tick(3_250).unwrap();
    assert!(!a.status().connected && !b.status().connected);
    pump(&mut a, &mut b, 3_250);
    assert_ne!(a.status().session, old);
    let hello = {
        b.reconnect(3_251).unwrap();
        b.take_control().unwrap()
    };
    assert!(matches!(
        wire::decode(&hello).unwrap().body,
        Body::Hello { .. }
    ));
    for _ in 0..1000 {
        a.ingest(&hello, 3_251).unwrap();
    }
    let mut queued = 0;
    while a.take_control().is_some() {
        queued += 1;
    }
    assert!(queued <= 8);
    assert_eq!(a.tick(3_250), Err(PeerError::TimeWentBackwards));
}
