//! Bounded checks of the real headless loop over the shared participant owner.

use super::*;
use std::cell::Cell;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc;

use rlb_model::SourceId;
use serde_json::Value;

struct Fixture {
    root: PathBuf,
    path: PathBuf,
    media_address: SocketAddr,
    participant: controller::RunningParticipant,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rlb-cli-{}",
            SourceId::generate().expect("test identity")
        ));
        std::fs::create_dir(&root).expect("test directory");
        let path = root.join("participant.json");
        let config = ParticipantConfig::create("CLI test", "127.0.0.1:0".parse().unwrap())
            .expect("test configuration");
        config::create_config_file(&path, &config).expect("write test configuration");
        let participant = controller::start(&path).expect("start shared owner");
        let media_address = participant
            .handle()
            .snapshot()
            .local_address
            .expect("bound media socket");
        Self {
            root,
            path,
            media_address,
            participant,
        }
    }

    fn request(&self, stdin: bool) -> RunRequest {
        RunRequest {
            config: self.path.clone(),
            json: true,
            stdin,
            seconds: Some(5),
            room: RunRoomIntent::SavedRestore,
        }
    }

    fn stop(&mut self) {
        let handle = self.participant.handle();
        self.participant
            .stop()
            .expect("join all participant owners");
        assert!(!handle.snapshot().running);
        assert!(handle.command(Command::RefreshRooms).is_err());
        let rebound =
            UdpSocket::bind(self.media_address).expect("media socket released after owner join");
        drop(rebound);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.participant.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn send_line(sender: &mpsc::Sender<InputEvent>, line: impl Into<String>) -> Result<(), String> {
    sender
        .send(InputEvent::Line(line.into()))
        .map_err(|_| "The CLI stopped before its scripted command.".to_owned())
}

fn wait_for(
    handle: &ParticipantHandle,
    predicate: impl Fn(&ParticipantSnapshot) -> bool,
) -> Result<ParticipantSnapshot, String> {
    let start = Instant::now();
    loop {
        let state = handle.snapshot();
        if predicate(&state) {
            return Ok(state);
        }
        if start.elapsed() >= Duration::from_secs(2) {
            return Err(format!(
                "Shared owner did not reach the scripted state; room phase: {}.",
                state.room_phase
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn scripted_commands_use_one_owner_and_wait_for_actual_room_completion() {
    let mut fixture = Fixture::new();
    let handle = fixture.participant.handle();
    let initial = handle.snapshot();
    let run = serde_json::to_value(initial.run_id).unwrap();
    let source = initial.local_sources[0].id;
    assert!(initial.local_sources[0].publish);
    assert!(!initial.local_sources[0].monitor);
    let (sender, receiver) = mpsc::channel();
    let observer = handle.clone();
    let script = std::thread::spawn(move || -> Result<(), String> {
        send_line(&sender, "{")?;
        send_line(&sender, "status")?;
        send_line(&sender, "help")?;
        send_line(&sender, format!("source {source} monitor on"))?;
        send_line(
            &sender,
            format!(r#"{{"type":"set_local_source","source_id":"{source}","publish":false}}"#),
        )?;
        send_line(&sender, "rule global --match name Remote --listen on")?;
        let edited = wait_for(&observer, |state| {
            state
                .room
                .session
                .as_ref()
                .and_then(|session| session.global_rules.as_ref())
                .is_some_and(|global| {
                    global.rules.iter().any(|rule| {
                        rule.listen == Some(true)
                            && rule.r#match
                                == (super::super::rooms::RuleMatch::Condition {
                                    name: Some("Remote".to_owned()),
                                    ip: None,
                                })
                    })
                })
        })?;
        let current = edited
            .local_sources
            .iter()
            .find(|entry| entry.id == source)
            .ok_or("The local source disappeared.")?;
        if current.publish || !current.monitor {
            return Err("A partial local-source command overwrote its other field.".to_owned());
        }
        send_line(
            &sender,
            "room create --name \"CLI room\" --listen 127.0.0.1:0",
        )?;
        wait_for(&observer, |state| {
            state.room_phase == "joined" && state.joined
        })?;
        send_line(&sender, "cards")?;
        send_line(&sender, "quit")
    });
    let request = fixture.request(true);
    let mut out = Vec::new();
    let mut diagnostics = Vec::new();
    let result = session_loop(
        &handle,
        &request,
        Some(&receiver),
        &mut out,
        &mut diagnostics,
        || false,
    );
    let script_result = script.join().expect("script thread joined");
    fixture.stop();
    result.expect("scripted loop");
    script_result.expect("script observed completed shared state");
    assert_eq!(
        String::from_utf8(diagnostics).unwrap(),
        format!("{}\n", grammar::help_text())
    );
    let records = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON record per line"))
        .collect::<Vec<_>>();
    assert!(records.iter().all(|record| matches!(
        record["kind"].as_str(),
        Some("snapshot" | "result" | "error")
    )));
    assert_eq!(
        records
            .iter()
            .filter(|record| record["kind"] == "error")
            .count(),
        1
    );
    assert!(records.iter().any(|record| {
        record["kind"] == "error" && record["scope"] == grammar::ERROR_SCOPE_LINE
    }));
    let snapshots = records
        .iter()
        .filter_map(|record| {
            record
                .get("state")
                .or_else(|| record.get("result")?.get("snapshot"))
        })
        .collect::<Vec<_>>();
    assert!(snapshots.len() >= 2, "initial state and status result");
    for state in snapshots {
        if state["running"] == true {
            assert_eq!(state["run_id"], run);
        } else {
            assert!(state["run_id"].is_null(), "a stopped owner has no live run");
        }
        assert_eq!(
            state["local_member_id"],
            serde_json::to_value(initial.local_member_id).unwrap()
        );
    }
    assert!(records.iter().any(|record| {
        record["kind"] == "result"
            && record["result"]["query"] == "cards"
            && record["result"]["cards"]
                .as_array()
                .is_some_and(|cards| cards.iter().any(|card| card["name"] == "CLI room"))
    }));
    assert!(records.iter().any(|record| {
        record["result"]["command"] == "quit" && record["result"]["snapshot"]["running"] == false
    }));
    let saved = config::load_config(&fixture.path).unwrap();
    assert!(!saved.source(source).unwrap().publish);
    assert!(saved.source(source).unwrap().monitor);
}

#[test]
fn eof_is_orderly_and_no_stdin_mode_does_not_consume_a_closed_receiver() {
    let mut fixture = Fixture::new();
    let handle = fixture.participant.handle();
    let (sender, receiver) = mpsc::channel();
    sender.send(InputEvent::Eof).unwrap();
    drop(sender);
    let mut out = Vec::new();
    let mut diagnostics = Vec::new();
    session_loop(
        &handle,
        &fixture.request(true),
        Some(&receiver),
        &mut out,
        &mut diagnostics,
        || false,
    )
    .expect("EOF completes the frontend loop");
    assert!(handle.snapshot().running);
    let calls = Cell::new(0);
    let request = fixture.request(false);
    session_loop(&handle, &request, None, &mut out, &mut diagnostics, || {
        calls.set(calls.get() + 1);
        calls.get() >= 4
    })
    .expect("no-stdin run continues until interruption");
    assert!(calls.get() >= 4);
    assert!(diagnostics.is_empty());
    fixture.stop();
}

#[test]
fn duration_budget_and_default_listener_keep_full_range_and_ipv6_family() {
    let now = Instant::now();
    assert_eq!(remaining_budget(now, None), Some(CONTROL_WAIT));
    assert_eq!(remaining_budget(now, Some(u64::MAX)), Some(CONTROL_WAIT));
    assert_eq!(
        remaining_budget(now - Duration::from_secs(2), Some(1)),
        None
    );
    let mut fixture = Fixture::new();
    let mut state = fixture.participant.handle().snapshot();
    state.bind = "[::]:0".parse().unwrap();
    state.local_address = Some("[::]:52000".parse().unwrap());
    let LineOutcome::Parsed(parsed) = grammar::parse_line("room create --name IPv6 --port 48123")
    else {
        panic!("valid implicit listener command");
    };
    match resolve(parsed, &state).unwrap() {
        Command::CreateRoom { bind, .. } => assert_eq!(bind, "[::]:48123".parse().unwrap()),
        other => panic!("unexpected command: {other:?}"),
    }
    state.local_address = Some("[::1]:52000".parse().unwrap());
    let LineOutcome::Parsed(parsed) = grammar::parse_line("room create --name Local") else {
        panic!("valid local implicit listener command");
    };
    match resolve(parsed, &state).unwrap() {
        Command::CreateRoom { bind, .. } => {
            assert_eq!(bind, "[::1]:47101".parse().unwrap());
        }
        other => panic!("unexpected command: {other:?}"),
    }
    fixture.stop();
}

struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "scripted closed stdout",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn broken_stdout_and_input_errors_return_to_the_same_owner_cleanup() {
    let mut fixture = Fixture::new();
    let handle = fixture.participant.handle();
    let mut diagnostics = Vec::new();
    let error = session_loop(
        &handle,
        &fixture.request(false),
        None,
        &mut BrokenWriter,
        &mut diagnostics,
        || false,
    )
    .expect_err("closed stdout terminates the frontend loop");
    assert_eq!(error.exit_code, grammar::EXIT_RUNTIME_FAILURE);
    assert!(error.message.contains("scripted closed stdout"));
    assert!(handle.snapshot().running);
    let (sender, receiver) = mpsc::channel();
    sender
        .send(InputEvent::Error("scripted input failure".to_owned()))
        .unwrap();
    drop(sender);
    let error = session_loop(
        &handle,
        &fixture.request(true),
        Some(&receiver),
        &mut Vec::new(),
        &mut diagnostics,
        || false,
    )
    .expect_err("input failure terminates the frontend loop");
    assert_eq!(error.message, "scripted input failure");
    fixture.stop();
}

#[test]
fn human_updates_report_asynchronous_room_and_save_errors_without_a_status_command() {
    let mut fixture = Fixture::new();
    let mut state = fixture.participant.handle().snapshot();
    state.room.session = Some(super::super::room_session::SessionSnapshot::idle());
    let mut out = Vec::new();
    let mut last = String::new();
    write_snapshot(&state, false, &mut out, &mut last, false).unwrap();
    let baseline_len = out.len();
    state.room.error = Some("scripted room owner failure".to_owned());
    write_snapshot(&state, false, &mut out, &mut last, false).unwrap();
    assert!(out.len() > baseline_len);
    assert!(last.contains("scripted room owner failure"));

    let room_failure_len = out.len();
    state.room.session.as_mut().unwrap().rules_save_error =
        Some("scripted room rule save failed".to_owned());
    write_snapshot(&state, false, &mut out, &mut last, false).unwrap();
    assert!(out.len() > room_failure_len);
    assert!(last.contains("scripted room rule save failed"));

    let rule_failure_len = out.len();
    state.persistence_error = Some("scripted participant save failed".to_owned());
    write_snapshot(&state, false, &mut out, &mut last, false).unwrap();
    assert!(out.len() > rule_failure_len);
    assert!(last.contains("scripted participant save failed"));
    let unchanged_len = out.len();
    write_snapshot(&state, false, &mut out, &mut last, false).unwrap();
    assert_eq!(out.len(), unchanged_len);
    fixture.stop();
}
