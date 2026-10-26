//! A returned publication uses the same pair admission as any original source.

use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};

use crate::{
    CaptureTicket, CaptureWatermark, ChannelLayout, Intent, MAX_DATAGRAM_BYTES, MemberRun,
    PcmEncoding, PcmFormat, Peer, PeerOperation, PreparedStorage, PublicationRef, ReceivedBinding,
    SourceKind, SourcePublisher,
};

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

fn ticket(value: u64) -> CaptureTicket {
    CaptureTicket::new(value).unwrap()
}

fn operation(now_ms: u64, allocated_through: u64) -> PeerOperation {
    PeerOperation {
        now_ms,
        watermark: CaptureWatermark { allocated_through },
    }
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

fn finite_words(frames: usize) -> Vec<f64> {
    [0x8000_0000, 0x0000_0001, 0x7f7f_ffff, 0xc020_0000]
        .into_iter()
        .cycle()
        .take(frames * 2)
        .map(f32::from_bits)
        .map(f64::from)
        .collect()
}

fn bits(samples: &[f64]) -> Vec<u64> {
    samples.iter().map(|sample| sample.to_bits()).collect()
}

struct ReturnPair {
    original: SourcePublisher,
    returned: SourcePublisher,
    computer: Peer,
    router: Peer,
    binding: ReceivedBinding,
    acquisition: StreamEpoch,
}

impl ReturnPair {
    fn new() -> Self {
        let mut original = SourcePublisher::new(run(3));
        original
            .register(source(5), SourceKind::System, true)
            .unwrap();
        let request = original
            .begin_binding(source(5), acquisition(6), stereo())
            .unwrap()
            .pending
            .unwrap();
        let original_binding = original
            .commit_binding(request, acquisition(6), 0)
            .unwrap()
            .binding
            .unwrap();
        let input = PublicationRef {
            owner: MemberRun {
                member: member(1),
                run: run(3),
            },
            source: source(5),
            epoch: original_binding.epoch(),
        };
        let mut returned = SourcePublisher::new(run(4));
        returned
            .register(source(7), SourceKind::System, true)
            .unwrap();
        let binding = returned
            .bind_received(source(7), input, stereo())
            .unwrap()
            .received
            .unwrap();
        let both = Intent {
            send: true,
            receive: true,
        };
        let computer =
            Peer::new(member(1), member(2), run(3), both, original.catalogue(), 10).unwrap();
        let router =
            Peer::new(member(2), member(1), run(4), both, returned.catalogue(), 10).unwrap();
        let mut pair = Self {
            original,
            returned,
            computer,
            router,
            binding,
            acquisition: acquisition(6),
        };
        pair.exchange(10, 0, 0);
        assert!(pair.computer.status().send_ready);
        assert!(pair.router.status().send_ready);
        pair
    }

    fn exchange(&mut self, now_ms: u64, computer_watermark: u64, router_watermark: u64) {
        for _ in 0..16 {
            let mut moved = false;
            while let Some(bytes) = self.computer.take_control() {
                self.router
                    .ingest_with(&bytes, now_ms, operation(now_ms, router_watermark))
                    .unwrap();
                moved = true;
            }
            while let Some(bytes) = self.router.take_control() {
                self.computer
                    .ingest_with(&bytes, now_ms, operation(now_ms, computer_watermark))
                    .unwrap();
                moved = true;
            }
            if !moved {
                return;
            }
        }
        panic!("the pair control exchange did not settle");
    }

    fn original_datagrams(
        &mut self,
        start_frame: u64,
        samples: &[f64],
        allocated_ticket: u64,
        now_ms: u64,
    ) -> Vec<Vec<u8>> {
        let original = self
            .original
            .local_original(source(5), self.acquisition, start_frame, samples, now_ms)
            .unwrap()
            .with_ticket(ticket(allocated_ticket));
        let mut storage = PreparedStorage::for_format(stereo()).unwrap();
        let prepared = self
            .original
            .prepare_pcm_into(original, &mut storage)
            .unwrap();
        let mut scratch = [0; MAX_DATAGRAM_BYTES];
        (0..prepared.len())
            .map(|index| {
                self.computer
                    .write_original_into(&prepared.get(index).unwrap(), &mut scratch)
                    .unwrap()
                    .unwrap()
                    .bytes
                    .to_vec()
            })
            .collect()
    }
}

#[test]
fn return_publication_preserves_words_and_reordered_frames_through_normal_peers() {
    let mut pair = ReturnPair::new();
    let samples = finite_words(280);
    let datagrams = pair.original_datagrams(0, &samples, 1, 20);
    assert_eq!(datagrams.len(), 2);
    assert_eq!(pair.binding.input().owner.member, member(1));
    assert_eq!(pair.binding.source(), source(7));
    assert_eq!(pair.binding.format(), stereo());
    assert_eq!(
        pair.returned.received_binding(source(7)),
        Some(pair.binding)
    );
    assert_eq!(
        pair.original.input(source(5)),
        Some(crate::PublicationInput::Capture {
            source: source(5),
            acquisition: acquisition(6),
        })
    );
    assert_eq!(
        pair.returned.input(source(7)),
        Some(crate::PublicationInput::Received {
            publication: pair.binding.input(),
        })
    );
    let mapping = pair.binding.upstream_mapping();
    assert_eq!(mapping.input, pair.binding.input());
    assert_eq!(mapping.input_frame(0), Some(0));
    assert_eq!(mapping.input_frame(140), Some(140));

    let mut input_pcm = [0.0; 280];
    let mut output_pcm = [0.0; 280];
    let mut storage = PreparedStorage::for_format(stereo()).unwrap();
    let mut wire = [0; MAX_DATAGRAM_BYTES];
    for (arrival, index) in [1_usize, 0].into_iter().enumerate() {
        let now_ms = 21 + arrival as u64 * 3;
        let admitted = pair
            .router
            .ingest_into(
                &datagrams[index],
                now_ms,
                operation(now_ms, arrival as u64),
                &mut input_pcm,
            )
            .unwrap()
            .unwrap();
        assert_eq!(admitted.metadata.start_frame, (index * 140) as u64);
        let returned = pair
            .returned
            .prepare_received_into(
                pair.binding,
                admitted,
                ticket(arrival as u64 + 1),
                &mut storage,
            )
            .unwrap();
        assert_eq!(returned.len(), 1);
        let fragment = returned.get(0).unwrap();
        assert_eq!(fragment.sequence(), arrival as u64 + 1);
        assert_eq!(fragment.start_frame(), admitted.metadata.start_frame);
        assert_eq!(fragment.enqueued_at_ms(), now_ms);
        let bytes = pair
            .router
            .write_original_into(&fragment, &mut wire)
            .unwrap()
            .unwrap();
        let heard = pair
            .computer
            .ingest_into(
                bytes.bytes,
                now_ms + 1,
                operation(now_ms + 1, 1),
                &mut output_pcm,
            )
            .unwrap()
            .unwrap();
        assert_eq!(heard.metadata.member_id, member(2));
        assert_eq!(heard.metadata.remote_run, run(4));
        assert_eq!(heard.metadata.entry.id, source(7));
        assert_eq!(heard.metadata.entry.epoch, pair.binding.epoch());
        assert_eq!(heard.metadata.start_frame, admitted.metadata.start_frame);
        assert_eq!(heard.metadata.entry.format, Some(stereo()));
        assert_eq!(bits(heard.samples), bits(admitted.samples));
        let start = index * 280;
        assert_eq!(bits(heard.samples), bits(&samples[start..start + 280]));
    }
}

#[test]
fn received_binding_replacement_rejects_old_binding_and_old_upstream_epoch() {
    let mut pair = ReturnPair::new();
    let samples = finite_words(2);
    let datagrams = pair.original_datagrams(0, &samples, 1, 20);
    let mut pcm = [0.0; 4];
    let admitted = pair
        .router
        .ingest_into(&datagrams[0], 21, operation(21, 0), &mut pcm)
        .unwrap()
        .unwrap();
    let old = pair.binding;
    pair.acquisition = acquisition(8);
    let request = pair
        .original
        .begin_binding(source(5), pair.acquisition, stereo())
        .unwrap()
        .pending
        .unwrap();
    let original_binding = pair
        .original
        .commit_binding(request, pair.acquisition, 0)
        .unwrap()
        .binding
        .unwrap();
    let mut input = old.input();
    input.epoch = original_binding.epoch();
    let replacement = pair
        .returned
        .bind_received(source(7), input, stereo())
        .unwrap()
        .received
        .unwrap();
    assert_ne!(old, replacement);
    assert!(replacement.epoch() > old.epoch());
    assert_eq!(replacement.input(), input);
    assert_eq!(pair.returned.received_binding(source(7)), Some(replacement));
    let mut storage = PreparedStorage::for_format(stereo()).unwrap();
    assert!(
        pair.returned
            .prepare_received_into(old, admitted, ticket(1), &mut storage)
            .is_err()
    );
    assert!(
        pair.returned
            .prepare_received_into(replacement, admitted, ticket(1), &mut storage)
            .is_err()
    );

    pair.computer
        .install_catalogue(&pair.original.catalogue(), 22)
        .unwrap();
    pair.router
        .install_catalogue(&pair.returned.catalogue(), 22)
        .unwrap();
    pair.exchange(23, 1, 0);
    let fresh = pair.original_datagrams(0, &samples, 2, 24);
    let matching_epoch = pair
        .router
        .ingest_into(&fresh[0], 25, operation(25, 0), &mut pcm)
        .unwrap()
        .unwrap();
    assert_eq!(matching_epoch.metadata.entry.epoch, input.epoch);
    let prepared = pair
        .returned
        .prepare_received_into(replacement, matching_epoch, ticket(1), &mut storage)
        .unwrap();
    assert_eq!(prepared.get(0).unwrap().sequence(), 1);
    assert_eq!(prepared.get(0).unwrap().epoch(), replacement.epoch());
}

#[test]
fn return_audio_obeys_receiver_consent_and_reopened_ticket_floor() {
    let mut pair = ReturnPair::new();
    let samples = finite_words(2);
    let datagrams = pair.original_datagrams(0, &samples, 1, 20);
    let mut pcm = [0.0; 4];
    let admitted = pair
        .router
        .ingest_into(&datagrams[0], 21, operation(21, 0), &mut pcm)
        .unwrap()
        .unwrap();
    let mut storage = PreparedStorage::for_format(stereo()).unwrap();
    let prepared = pair
        .returned
        .prepare_received_into(pair.binding, admitted, ticket(1), &mut storage)
        .unwrap();
    let fragment = prepared.get(0).unwrap();
    let mut wire = [0; MAX_DATAGRAM_BYTES];
    let pending = pair
        .router
        .write_original_into(&fragment, &mut wire)
        .unwrap()
        .unwrap()
        .bytes
        .to_vec();
    pair.computer
        .set_intent_with(
            Intent {
                send: true,
                receive: false,
            },
            operation(22, 1),
        )
        .unwrap();
    let mut output_pcm = [0.0; 4];
    assert!(
        pair.computer
            .ingest_into(&pending, 22, operation(22, 1), &mut output_pcm)
            .unwrap()
            .is_none()
    );
    pair.exchange(23, 1, 1);
    assert!(
        pair.router
            .write_original_into(&fragment, &mut wire)
            .unwrap()
            .is_none()
    );
    pair.computer
        .set_intent_with(
            Intent {
                send: true,
                receive: true,
            },
            operation(24, 1),
        )
        .unwrap();
    pair.exchange(25, 1, 1);
    assert!(pair.router.status().send_ready);
    assert_eq!(pair.router.outgoing_interval().ticket_floor, 1);
    assert!(
        pair.router
            .write_original_into(&fragment, &mut wire)
            .unwrap()
            .is_none()
    );

    let fresh = pair.original_datagrams(2, &samples, 2, 26);
    let admitted = pair
        .router
        .ingest_into(&fresh[0], 27, operation(27, 1), &mut pcm)
        .unwrap()
        .unwrap();
    let mut fresh_storage = PreparedStorage::for_format(stereo()).unwrap();
    let prepared = pair
        .returned
        .prepare_received_into(pair.binding, admitted, ticket(2), &mut fresh_storage)
        .unwrap();
    let bytes = pair
        .router
        .write_original_into(&prepared.get(0).unwrap(), &mut wire)
        .unwrap()
        .unwrap();
    let heard = pair
        .computer
        .ingest_into(bytes.bytes, 28, operation(28, 2), &mut output_pcm)
        .unwrap()
        .unwrap();
    assert_eq!(heard.metadata.start_frame, 2);
    assert_eq!(bits(heard.samples), bits(&samples));
}
