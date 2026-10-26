use super::*;
use std::time::Instant;

fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "media owner made no progress");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn media_reacts_and_stops_while_supervisor_persistence_is_blocked() {
    let directory =
        std::env::temp_dir().join(format!("rlb-media-owner-{}", SourceId::generate().unwrap()));
    std::fs::create_dir(&directory).unwrap();
    let mut a_config = ParticipantConfig::create("A", "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut b_config = ParticipantConfig::create("B", "127.0.0.1:0".parse().unwrap()).unwrap();
    // Explicit first connection must work with automatic reconnection disabled.
    a_config.auto_reconnect = false;
    b_config.auto_reconnect = false;
    let a_path = directory.join("a.json");
    let b_path = directory.join("b.json");
    config::create_config_file(&a_path, &a_config).unwrap();
    config::create_config_file(&b_path, &b_config).unwrap();
    let mut a = Worker::boot(&a_path).unwrap();
    let mut b = Worker::boot(&b_path).unwrap();
    a.apply(Command::UpsertContact {
        member_id: b_config.member_id,
        address: b.local_address,
        name: Some("B".into()),
    })
    .unwrap();
    b.apply(Command::UpsertContact {
        member_id: a_config.member_id,
        address: a.local_address,
        name: Some("A".into()),
    })
    .unwrap();
    let a_id = a_config.member_id;
    let b_id = b_config.member_id;
    a.apply(Command::JoinGroup {
        name: "room".into(),
    })
    .unwrap();
    let media = b.media.handle.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    b.persistence_pause = Some(Box::new(move || {
        entered_tx.send(()).unwrap();
        // This is a deadlock bound for the fixture, not a latency requirement.
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    }));
    let supervisor = thread::spawn(move || {
        b.apply(Command::JoinGroup {
            name: "room".into(),
        })
        .unwrap();
        b
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    eventually(|| {
        media
            .snapshot()
            .peers
            .get(&a_id)
            .is_some_and(|peer| peer.status.connected)
    });
    a.apply(Command::SetPeerIntent {
        member_id: b_id,
        send: false,
        receive: true,
    })
    .unwrap();
    eventually(|| {
        media
            .snapshot()
            .peers
            .get(&a_id)
            .is_some_and(|peer| peer.status.remote.is_some_and(|intent| !intent.send))
    });
    // Closing transport does not depend on the busy supervisor's request queue.
    media.shutdown();
    eventually(|| !media.snapshot().running);
    release_tx.send(()).unwrap();
    let mut b = supervisor.join().unwrap();
    b.close_runtime();
    a.close_runtime();
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn full_supervisor_queue_still_closes_media_and_old_configuration_cannot_reopen_it() {
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let remote = MemberId::generate().unwrap();
    let mut config = ParticipantConfig::create("Queue", "127.0.0.1:0".parse().unwrap()).unwrap();
    config.group.name = "room".into();
    config.group.joined = true;
    config.members.push(SavedMember::new(remote, "Remote"));
    config.contacts.push(SavedContact {
        member_id: remote,
        address: receiver.local_addr().unwrap(),
        alternates: Vec::new(),
        resolved_for: None,
    });
    let mut runtime = MediaRuntime::start(config.clone()).unwrap();
    let media = runtime.handle.clone();
    let old_epoch = media.room_fence().epoch;
    let (commands, pending) = sync_channel(COMMAND_CAPACITY);
    for _ in 0..COMMAND_CAPACITY {
        let (reply, _) = sync_channel(1);
        commands
            .try_send(Request {
                command: Command::RefreshDevices,
                room_epoch: old_epoch,
                reply,
            })
            .unwrap();
    }
    let handle = ParticipantHandle {
        commands,
        snapshot: Arc::new(Mutex::new(placeholder_snapshot())),
        media: media.clone(),
    };
    assert!(handle.command(Command::LeaveGroup).is_err());
    eventually(|| media.snapshot().peers[&remote].paused);
    assert!(media.room_fence().closed);
    assert!(
        media
            .configure_in(&config, Resume::All(false), old_epoch)
            .is_err()
    );
    assert!(media.snapshot().peers[&remote].paused);
    let closed_epoch = media.room_fence().epoch;
    assert!(
        handle
            .command(Command::JoinGroup {
                name: "room".into()
            })
            .is_err()
    );
    assert_eq!(media.room_fence().epoch, closed_epoch);
    // A newly accepted explicit entry has a new scope; prior messages do not.
    drop(pending);
    let fresh_epoch = media.submit_scope(RoomTransition::Enter, Ok).unwrap();
    media
        .configure_in(&config, Resume::All(false), fresh_epoch)
        .unwrap();
    assert!(!media.snapshot().peers[&remote].paused);
    runtime.stop();
}
