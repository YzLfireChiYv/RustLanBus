//! Stream permission changes use the ordinary current-state control exchange.

use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};

use crate::{
    AdmittedMetadata, CaptureTicket, CaptureWatermark, Catalogue, ChannelLayout, Intent,
    MAX_DATAGRAM_BYTES, PcmEncoding, PcmFormat, Peer, PeerError, PeerOperation, PreparedFragment,
    PreparedStorage, PreparedView, SourceIntent, SourceKind, SourcePolicy, SourcePublisher,
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

fn mono() -> PcmFormat {
    PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap()
}

fn both() -> Intent {
    Intent {
        send: true,
        receive: true,
    }
}

fn microphone_policy(send: bool, receive: bool) -> SourcePolicy {
    SourcePolicy {
        default: both(),
        overrides: vec![SourceIntent {
            kind: SourceKind::Microphone,
            intent: Intent { send, receive },
        }],
    }
}

fn operation(now_ms: u64, allocated_through: u64) -> PeerOperation {
    PeerOperation {
        now_ms,
        watermark: CaptureWatermark { allocated_through },
    }
}

fn bind(publisher: &mut SourcePublisher, id: SourceId, clock: StreamEpoch) {
    let request = publisher
        .begin_binding(id, clock, mono())
        .unwrap()
        .pending
        .unwrap();
    publisher.commit_binding(request, clock, 0).unwrap();
}

struct Pair {
    publisher: SourcePublisher,
    sender: Peer,
    receiver: Peer,
    allocated: u64,
}

impl Pair {
    fn new() -> Self {
        let mut publisher = SourcePublisher::new(run(3));
        for (byte, kind) in [(2, SourceKind::System), (3, SourceKind::Microphone)] {
            publisher.register(source(byte), kind, true).unwrap();
            bind(&mut publisher, source(byte), acquisition(byte));
        }
        let sender = Peer::new(
            member(1),
            member(2),
            run(3),
            both(),
            publisher.catalogue(),
            10,
        )
        .unwrap();
        let receiver = Peer::new(
            member(2),
            member(1),
            run(4),
            both(),
            Catalogue::default(),
            10,
        )
        .unwrap();
        let mut pair = Self {
            publisher,
            sender,
            receiver,
            allocated: 0,
        };
        pair.exchange(10);
        for byte in [2, 3] {
            assert!(pair.sender.source_send_ready(source(byte)));
            assert!(pair.receiver.source_receive_ready(source(byte)));
        }
        pair
    }

    fn exchange(&mut self, now_ms: u64) {
        for _ in 0..16 {
            let mut moved = false;
            while let Some(bytes) = self.sender.take_control() {
                assert_eq!(&bytes[..4], b"RLC5");
                self.receiver
                    .ingest_with(&bytes, now_ms, operation(now_ms, 0))
                    .unwrap();
                moved = true;
            }
            while let Some(bytes) = self.receiver.take_control() {
                assert_eq!(&bytes[..4], b"RLC5");
                self.sender
                    .ingest_with(&bytes, now_ms, operation(now_ms, self.allocated))
                    .unwrap();
                moved = true;
            }
            if !moved {
                return;
            }
        }
        panic!("the normal current-state exchange did not settle");
    }

    fn prepare<'a>(
        &mut self,
        id: SourceId,
        clock: StreamEpoch,
        start_frame: u64,
        now_ms: u64,
        storage: &'a mut PreparedStorage,
    ) -> PreparedView<'a> {
        self.allocated += 1;
        let original = self
            .publisher
            .local_original(id, clock, start_frame, &[0.25, -0.5], now_ms)
            .unwrap()
            .with_ticket(CaptureTicket::new(self.allocated).unwrap());
        self.publisher.prepare_pcm_into(original, storage).unwrap()
    }

    fn write(&self, fragment: &PreparedFragment<'_>) -> Result<Option<Vec<u8>>, PeerError> {
        let mut scratch = [0; MAX_DATAGRAM_BYTES];
        self.sender
            .write_original_into(fragment, &mut scratch)
            .map(|written| {
                written.map(|written| {
                    assert_eq!(&written.bytes[..4], b"RLP5");
                    written.bytes.to_vec()
                })
            })
    }

    fn receive(&mut self, bytes: &[u8], now_ms: u64) -> Option<AdmittedMetadata> {
        let mut pcm = [0.0; 2];
        let admitted = self
            .receiver
            .ingest_into(bytes, now_ms, operation(now_ms, 0), &mut pcm)
            .unwrap();
        admitted.map(|admitted| {
            assert_eq!(admitted.samples, &[0.25, -0.5]);
            admitted.metadata
        })
    }

    fn fence(&self, id: SourceId) -> u64 {
        self.receiver
            .admission_snapshot()
            .sources
            .iter()
            .find(|admission| admission.entry.id == id)
            .unwrap()
            .fence
    }
}

#[test]
fn receive_stream_reopen_keeps_its_neighbour_and_rejects_old_grant_audio() {
    let mut pair = Pair::new();
    let mut microphone_storage = PreparedStorage::for_format(mono()).unwrap();
    let microphone = pair.prepare(source(3), acquisition(3), 0, 20, &mut microphone_storage);
    let old_wire = pair.write(&microphone.get(0).unwrap()).unwrap().unwrap();
    let mut system_storage = PreparedStorage::for_format(mono()).unwrap();
    let system = pair.prepare(source(2), acquisition(2), 0, 20, &mut system_storage);
    let system_fence = pair.fence(source(2));
    let microphone_fence = pair.fence(source(3));

    pair.receiver
        .set_source_policy_with(microphone_policy(true, false), operation(21, 0))
        .unwrap();
    assert!(pair.receiver.status().receive_ready);
    assert!(!pair.receiver.source_receive_ready(source(3)));
    pair.exchange(21);
    assert!(!pair.sender.source_send_ready(source(3)));
    assert!(pair.sender.source_send_ready(source(2)));
    assert_eq!(pair.fence(source(2)), system_fence);
    assert_ne!(pair.fence(source(3)), microphone_fence);
    assert!(pair.receive(&old_wire, 22).is_none());
    let system_wire = pair.write(&system.get(0).unwrap()).unwrap().unwrap();
    assert_eq!(pair.receive(&system_wire, 22).unwrap().entry.id, source(2));

    pair.receiver
        .set_source_policy_with(microphone_policy(true, true), operation(23, 0))
        .unwrap();
    pair.exchange(23);
    assert!(pair.sender.source_send_ready(source(3)));
    assert_eq!(pair.fence(source(2)), system_fence);
    assert!(pair.write(&microphone.get(0).unwrap()).unwrap().is_none());
    assert!(pair.write(&system.get(0).unwrap()).unwrap().is_some());
    assert_eq!(
        pair.receiver.remote_catalogue().unwrap().as_slice().len(),
        2
    );

    let mut fresh_storage = PreparedStorage::for_format(mono()).unwrap();
    let fresh = pair.prepare(source(3), acquisition(3), 2, 24, &mut fresh_storage);
    let bytes = pair.write(&fresh.get(0).unwrap()).unwrap().unwrap();
    let admitted = pair.receive(&bytes, 24).unwrap();
    assert_eq!(admitted.entry.id, source(3));
    assert_ne!(admitted.source_fence, microphone_fence);
    assert!(pair.receive(&old_wire, 25).is_none());
}

#[test]
fn skipped_sender_refusal_still_requires_a_fresh_exact_stream_grant() {
    let mut pair = Pair::new();
    let mut microphone_storage = PreparedStorage::for_format(mono()).unwrap();
    let microphone = pair.prepare(source(3), acquisition(3), 0, 20, &mut microphone_storage);
    let old_wire = pair.write(&microphone.get(0).unwrap()).unwrap().unwrap();
    let mut system_storage = PreparedStorage::for_format(mono()).unwrap();
    let system = pair.prepare(source(2), acquisition(2), 0, 20, &mut system_storage);
    let system_fence = pair.fence(source(2));
    let microphone_fence = pair.fence(source(3));

    pair.sender
        .set_source_policy_with(
            microphone_policy(false, true),
            operation(21, pair.allocated),
        )
        .unwrap();
    assert!(!pair.sender.source_send_ready(source(3)));
    assert!(pair.sender.source_send_ready(source(2)));
    // Lose the intermediate refusal. The next current state must be sufficient.
    while pair.sender.take_control().is_some() {}
    pair.sender
        .set_source_policy_with(microphone_policy(true, true), operation(22, pair.allocated))
        .unwrap();
    assert!(!pair.sender.source_send_ready(source(3)));
    assert!(pair.sender.source_send_ready(source(2)));
    assert!(pair.write(&microphone.get(0).unwrap()).unwrap().is_none());
    pair.exchange(22);

    assert!(pair.sender.source_send_ready(source(3)));
    assert_eq!(pair.fence(source(2)), system_fence);
    assert_ne!(pair.fence(source(3)), microphone_fence);
    assert!(pair.receive(&old_wire, 23).is_none());
    assert!(pair.write(&microphone.get(0).unwrap()).unwrap().is_none());
    let system_wire = pair.write(&system.get(0).unwrap()).unwrap().unwrap();
    assert_eq!(pair.receive(&system_wire, 23).unwrap().entry.id, source(2));

    let mut fresh_storage = PreparedStorage::for_format(mono()).unwrap();
    let fresh = pair.prepare(source(3), acquisition(3), 2, 24, &mut fresh_storage);
    let bytes = pair.write(&fresh.get(0).unwrap()).unwrap().unwrap();
    assert_eq!(pair.receive(&bytes, 24).unwrap().entry.id, source(3));
    assert_eq!(
        pair.receiver.remote_catalogue().unwrap().as_slice().len(),
        2
    );
}

#[test]
fn publication_epoch_refreshes_its_grant_and_preserves_the_other_source_policy() {
    let mut pair = Pair::new();
    pair.receiver
        .set_source_policy_with(microphone_policy(true, false), operation(20, 0))
        .unwrap();
    pair.exchange(20);
    let mut old_storage = PreparedStorage::for_format(mono()).unwrap();
    let old = pair.prepare(source(2), acquisition(2), 0, 21, &mut old_storage);
    let old_fragment = old.get(0).unwrap();
    let old_wire = pair.write(&old_fragment).unwrap().unwrap();
    let system_fence = pair.fence(source(2));
    let microphone_fence = pair.fence(source(3));

    bind(&mut pair.publisher, source(2), acquisition(9));
    pair.sender
        .install_catalogue(&pair.publisher.catalogue(), 22)
        .unwrap();
    assert!(!pair.sender.source_send_ready(source(2)));
    assert_eq!(pair.write(&old_fragment), Err(PeerError::StalePublication));
    pair.exchange(22);
    assert!(pair.sender.source_send_ready(source(2)));
    assert!(!pair.sender.source_send_ready(source(3)));
    assert!(!pair.receiver.source_receive_ready(source(3)));
    assert_eq!(pair.fence(source(3)), microphone_fence);
    assert_ne!(pair.fence(source(2)), system_fence);
    assert!(pair.receive(&old_wire, 23).is_none());

    let mut fresh_storage = PreparedStorage::for_format(mono()).unwrap();
    let fresh = pair.prepare(source(2), acquisition(9), 0, 23, &mut fresh_storage);
    let bytes = pair.write(&fresh.get(0).unwrap()).unwrap().unwrap();
    let admitted = pair.receive(&bytes, 23).unwrap();
    assert!(admitted.entry.epoch > old_fragment.epoch());
    let catalogue = pair.receiver.remote_catalogue().unwrap();
    assert_eq!(catalogue.as_slice().len(), 2);
    assert_eq!(
        catalogue.source(source(3)).unwrap().kind,
        SourceKind::Microphone
    );
}

#[test]
fn newly_published_sources_follow_current_all_source_and_kind_specific_choices() {
    for receive_microphones in [true, false] {
        let mut pair = Pair::new();
        pair.receiver
            .set_source_policy_with(
                microphone_policy(true, receive_microphones),
                operation(20, 0),
            )
            .unwrap();
        pair.exchange(20);
        let system_fence = pair.fence(source(2));
        pair.publisher
            .register(source(7), SourceKind::Microphone, true)
            .unwrap();
        bind(&mut pair.publisher, source(7), acquisition(7));
        pair.sender
            .install_catalogue(&pair.publisher.catalogue(), 21)
            .unwrap();
        assert!(!pair.sender.source_send_ready(source(7)));
        pair.exchange(21);
        assert!(pair.sender.source_send_ready(source(2)));
        assert_eq!(pair.fence(source(2)), system_fence);
        assert_eq!(
            pair.sender.source_send_ready(source(7)),
            receive_microphones
        );
        assert_eq!(
            pair.receiver.source_receive_ready(source(7)),
            receive_microphones
        );
        let snapshot = pair.receiver.admission_snapshot();
        assert_eq!(snapshot.sources.len(), 3);
        assert_eq!(
            snapshot
                .sources
                .iter()
                .find(|entry| entry.entry.id == source(7))
                .unwrap()
                .receive_ready,
            receive_microphones
        );

        let mut storage = PreparedStorage::for_format(mono()).unwrap();
        let prepared = pair.prepare(source(7), acquisition(7), 0, 22, &mut storage);
        let written = pair.write(&prepared.get(0).unwrap()).unwrap();
        assert_eq!(written.is_some(), receive_microphones);
        if let Some(bytes) = written {
            assert_eq!(pair.receive(&bytes, 22).unwrap().entry.id, source(7));
        }
    }
}
