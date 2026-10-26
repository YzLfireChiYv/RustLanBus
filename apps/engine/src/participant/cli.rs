//! Formal headless frontend over the same participant owner used by the GUI.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::cli_grammar::{
    self as grammar, Inquiry, LineOutcome, ParsedLine, PartialAction, RunRequest, RunRoomIntent,
    StartupRequest,
};
use super::config::{self, ParticipantConfig, RoomStartup};
use super::controller::{self, ParticipantHandle};
use super::management::{self, ImportOptions, ManagementResult};
use super::rooms::{RoomLibrary, default_contact_listener, load_room_profile};
use super::{Command, ParticipantSnapshot};

#[cfg(windows)]
#[path = "cli_windows.rs"]
mod platform;

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub exit_code: i32,
}
impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for CliError {}
impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self {
            message,
            exit_code: grammar::EXIT_RUNTIME_FAILURE,
        }
    }
}
impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        error.to_string().into()
    }
}

pub(super) enum InputEvent {
    Line(String),
    BadLine(String),
    Eof,
    Error(String),
}

static CLI_ENTRY: AtomicBool = AtomicBool::new(false);

/// Explicit process entry identity; optional spelling of the CLI prefix cannot
/// change how this process registers its own login-startup action.
pub(crate) fn is_cli_entry() -> bool {
    CLI_ENTRY.load(Ordering::Relaxed)
}

pub fn run(args: &[String]) -> Result<(), CliError> {
    CLI_ENTRY.store(true, Ordering::Relaxed);
    let mut out = std::io::stdout();
    let mut diagnostics = std::io::stderr();
    if args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("help" | "--help" | "-h")
        )
    {
        writeln!(diagnostics, "{}", grammar::help_text())?;
        return Ok(());
    }
    let request = grammar::parse_startup_args(args).map_err(|error| CliError {
        message: error.message,
        exit_code: grammar::EXIT_STARTUP_USAGE,
    })?;
    match request {
        StartupRequest::Init(request) => {
            let config = ParticipantConfig::create(&request.name, request.bind)?;
            config::create_config_file(&request.config, &config)?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?
            )?;
        }
        StartupRequest::Inspect(request) => {
            let config = config::load_config(&request.config)?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?
            )?;
        }
        StartupRequest::Cards(request) => {
            config::load_config(&request.config)?;
            let cards = RoomLibrary::new(&request.config).cards()?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&cards).map_err(|error| error.to_string())?
            )?;
        }
        StartupRequest::Run(request) => run_config(request, &mut out, &mut diagnostics)?,
        StartupRequest::SettingsExport { config, output } => {
            let _instance = controller::claim_config_instance(&config)?;
            let preview = management::export_bundle(&config, &output)?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
            )?;
        }
        StartupRequest::SettingsPreview {
            input,
            destination,
            mode,
        } => {
            let preview = management::preview_bundle(&input, destination.as_deref(), mode)?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
            )?;
        }
        StartupRequest::SettingsImport {
            input,
            destination,
            mode,
            new_name,
            source_revision,
            destination_revision,
        } => {
            // The shared import API claims this destination throughout publication.
            let result = management::import_bundle(
                &input,
                &destination,
                ImportOptions {
                    mode,
                    new_name,
                    expected_source_revision: Some(source_revision),
                    expected_destination_revision: destination_revision,
                },
            )?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?
            )?;
        }
        StartupRequest::LoginStartup {
            config: path,
            enabled,
        } => {
            // The same lock covers offline preference changes and running GUI/CLI owners.
            let _instance = controller::claim_config_instance(&path)?;
            let mut config = config::load_config(&path)?;
            let state = if let Some(enabled) = enabled {
                config.login_startup = enabled;
                let state = super::login_startup::set(&path, &config, enabled);
                config::save_config(&path, &config)?;
                state
            } else {
                super::login_startup::inspect(&path, &config)
            };
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?
            )?;
            if let Some(error) = state.last_error {
                return Err(error.into());
            }
        }
    }
    out.flush()?;
    Ok(())
}

#[cfg(windows)]
fn run_config(
    request: RunRequest,
    out: &mut dyn Write,
    diagnostics: &mut dyn Write,
) -> Result<(), CliError> {
    let room = match &request.room {
        RunRoomIntent::SavedRestore => None,
        RunRoomIntent::Join { card } => {
            load_room_profile(card)?;
            Some(RoomStartup::Join {
                profile_path: card.clone(),
            })
        }
        RunRoomIntent::Create { card, listen } => {
            load_room_profile(card)?;
            Some(RoomStartup::Create {
                profile_path: card.clone(),
                bind: *listen,
            })
        }
    };
    let interrupt = platform::Interrupt::install()?;
    let input = request.stdin.then(platform::Input::start).transpose()?;
    let mut participant = controller::start_with_room(&request.config, room)?;
    let handle = participant.handle();
    let result = session_loop(
        &handle,
        &request,
        input.as_ref().map(|input| input.receiver()),
        out,
        diagnostics,
        || interrupt.requested(),
    );
    drop(input);
    // Leave the owner alive until queued rule actions have crossed the shared
    // pending-settings barrier, including quit, EOF, timeout and interruption.
    let pending = report_pending_exit(&handle, request.json, out, diagnostics);
    let stopped = participant.stop().map_err(CliError::from);
    result.and(pending).and(stopped)
}

#[cfg(not(windows))]
fn run_config(_: RunRequest, _: &mut dyn Write, _: &mut dyn Write) -> Result<(), CliError> {
    Err("The formal participant runtime currently targets Windows."
        .to_owned()
        .into())
}

const CONTROL_WAIT: Duration = Duration::from_millis(50);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);

fn remaining_budget(start: Instant, seconds: Option<u64>) -> Option<Duration> {
    let remaining =
        seconds.map(|seconds| Duration::from_secs(seconds).saturating_sub(start.elapsed()));
    if remaining == Some(Duration::ZERO) {
        None
    } else {
        Some(remaining.unwrap_or(CONTROL_WAIT).min(CONTROL_WAIT))
    }
}

fn session_loop(
    handle: &ParticipantHandle,
    request: &RunRequest,
    input: Option<&Receiver<InputEvent>>,
    out: &mut dyn Write,
    diagnostics: &mut dyn Write,
    interrupted: impl Fn() -> bool,
) -> Result<(), CliError> {
    let start = Instant::now();
    let mut last_state = String::new();
    let mut next_snapshot = start;
    loop {
        if interrupted() {
            return Ok(());
        }
        let Some(wait) = remaining_budget(start, request.seconds) else {
            return Ok(());
        };
        if Instant::now() >= next_snapshot {
            let state = handle.snapshot();
            write_snapshot(&state, request.json, out, &mut last_state, false)?;
            if !state.running {
                return Err("The participant owner stopped unexpectedly."
                    .to_owned()
                    .into());
            }
            next_snapshot = Instant::now() + SNAPSHOT_INTERVAL;
        }
        let event = match input {
            Some(input) => match input.recv_timeout(wait) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(InputEvent::Eof),
            },
            None => {
                std::thread::sleep(wait);
                None
            }
        };
        if interrupted() {
            return Ok(());
        }
        match event {
            None => {}
            Some(InputEvent::Eof) => return Ok(()),
            Some(InputEvent::Error(error)) => return Err(error.into()),
            Some(InputEvent::BadLine(error)) => write_error(
                request.json,
                grammar::ERROR_SCOPE_LINE,
                &error,
                out,
                diagnostics,
            )?,
            Some(InputEvent::Line(line)) => {
                if matches!(line.trim(), "help" | "--help" | "-h") {
                    writeln!(diagnostics, "{}", grammar::help_text())?;
                    continue;
                }
                match grammar::parse_line(&line) {
                    LineOutcome::Blank => {}
                    LineOutcome::Recoverable(error) => write_error(
                        request.json,
                        grammar::ERROR_SCOPE_LINE,
                        &error.message,
                        out,
                        diagnostics,
                    )?,
                    LineOutcome::Parsed(parsed) => match parsed {
                        ParsedLine::Inquiry(Inquiry::Status) => write_snapshot(
                            &handle.snapshot(),
                            request.json,
                            out,
                            &mut last_state,
                            true,
                        )?,
                        ParsedLine::Inquiry(Inquiry::Cards) => write_result(
                            request.json,
                            &serde_json::json!({"query":"cards", "cards":handle.snapshot().room.cards}),
                            out,
                        )?,
                        ParsedLine::Inquiry(
                            inquiry @ (Inquiry::Devices
                            | Inquiry::Rooms
                            | Inquiry::Rules
                            | Inquiry::Outputs),
                        ) => {
                            let state = handle.snapshot();
                            let result = match inquiry {
                                Inquiry::Devices => serde_json::json!({
                                    "query": "devices", "endpoints": state.endpoints,
                                    "local_sources": state.local_sources,
                                    "main_output": state.output_endpoint_id,
                                    "network": state.network,
                                    "media_addresses": state.local_addresses,
                                }),
                                Inquiry::Rooms => {
                                    serde_json::json!({ "query": "rooms", "room": state.room })
                                }
                                Inquiry::Rules => serde_json::json!({
                                    "query": "rules", "global": state.room.session.as_ref().and_then(|session| session.global_rules.as_ref()),
                                    "room": state.room.session.as_ref().map(|session| &session.rules),
                                    "playback": state.room.session.as_ref().map(|session| &session.playback_rules),
                                    "files": state.room.session.as_ref().map(|session| &session.rule_files),
                                }),
                                Inquiry::Outputs => serde_json::json!({
                                    "query": "outputs", "main_output": state.output_endpoint_id,
                                    "local_sources": state.local_sources, "members": state.members,
                                    "devices": state.output_devices,
                                    "master_gain_db": state.master_gain_db, "limiter_enabled": state.limiter_enabled,
                                }),
                                _ => unreachable!("the inquiry was matched above"),
                            };
                            write_result(request.json, &result, out)?;
                        }
                        ParsedLine::Inquiry(Inquiry::LoginStartup) => {
                            handle.command(Command::RefreshLoginStartup)?;
                            write_result(request.json, &handle.snapshot().login_startup, out)?;
                        }
                        ParsedLine::Inquiry(Inquiry::LanFirewall) => {
                            handle.command(Command::RefreshLanFirewall)?;
                            let state = handle.snapshot();
                            write_result(
                                request.json,
                                &serde_json::json!({
                                    "network": state.network, "firewall": state.lan_firewall,
                                    "media_addresses": state.local_addresses,
                                }),
                                out,
                            )?;
                        }
                        parsed => match resolve_for_owner(parsed, &handle.snapshot(), handle) {
                            Err(error) => write_error(
                                request.json,
                                grammar::ERROR_SCOPE_COMMAND,
                                &error,
                                out,
                                diagnostics,
                            )?,
                            Ok(command) => {
                                if matches!(command, Command::Quit) {
                                    write_result(
                                        request.json,
                                        &serde_json::json!({
                                            "command": "quit", "requested": true,
                                        }),
                                        out,
                                    )?;
                                    return Ok(());
                                }
                                if command.retires_room_document()
                                    && handle.snapshot().room.session.as_ref().is_some_and(
                                        |session| session.rule_files.room.outcome.unsaved(),
                                    )
                                {
                                    writeln!(
                                        diagnostics,
                                        "Leaving or switching rooms discards this room's unsaved changes. Unsaved global rules remain active."
                                    )?;
                                }
                                match handle.command(command) {
                                    Ok(outcome) => write_result(request.json, &outcome, out)?,
                                    Err(error) => write_error(
                                        request.json,
                                        grammar::ERROR_SCOPE_COMMAND,
                                        &error,
                                        out,
                                        diagnostics,
                                    )?,
                                }
                                next_snapshot = Instant::now();
                            }
                        },
                    },
                }
            }
        }
    }
}

fn report_pending_exit(
    handle: &ParticipantHandle,
    json: bool,
    out: &mut dyn Write,
    diagnostics: &mut dyn Write,
) -> Result<(), CliError> {
    let outcome = handle
        .command(Command::CheckPendingSettings)
        .map_err(|error| {
            CliError::from(format!(
                "Pending settings could not be confirmed before shutdown: {error}"
            ))
        })?;
    let Some(pending @ ManagementResult::PendingChanges { .. }) = outcome.management else {
        return Err(
            "The pending-settings query did not return its result before shutdown."
                .to_owned()
                .into(),
        );
    };
    let ManagementResult::PendingChanges { changes } = &pending else {
        unreachable!()
    };
    if changes.is_empty() {
        return Ok(());
    }
    for change in changes {
        writeln!(
            diagnostics,
            "Unsaved {} at {}{}",
            change.kind,
            change.path.display(),
            change
                .detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        )?;
    }
    if json {
        write_result(true, &pending, out)?;
        write_error(
            true,
            "exit",
            "Stopping with unsaved settings; no interactive confirmation is requested by the CLI.",
            out,
            diagnostics,
        )?;
    }
    Err("The participant stopped with unsaved settings. Inspect the reported paths before reopening.".to_owned().into())
}

fn resolve_for_owner(
    parsed: ParsedLine,
    state: &ParticipantSnapshot,
    handle: &ParticipantHandle,
) -> Result<Command, String> {
    let partial = match parsed {
        ParsedLine::Partial(partial) => partial,
        other => return resolve(other, state),
    };
    if !matches!(partial.as_ref(), PartialAction::CardEdit { .. }) {
        return resolve(ParsedLine::Partial(partial), state);
    }
    let PartialAction::CardEdit {
        path,
        revision,
        name,
        address,
        port,
        password,
        strict,
    } = *partial
    else {
        unreachable!("the partial action was identified as a card edit");
    };
    let outcome = handle.command(Command::ReadRoomCard { path: path.clone() })?;
    let Some(ManagementResult::Card { document }) = outcome.management else {
        return Err("The card query did not return a card document.".into());
    };
    if document.revision != revision {
        return Err(
            "The room card changed after it was read. Read it again before editing.".into(),
        );
    }
    let mut target = document
        .profile
        .ok_or("This card is invalid. Repair its JSON before editing target fields.")?
        .room;
    if let Some(name) = name {
        target.name = name;
    }
    if let Some(address) = address {
        target.address = address;
    }
    if let Some(port) = port {
        target.port = Some(port);
    }
    if let Some(password) = password {
        target.password = password;
    }
    if let Some(strict) = strict {
        target.strict_auto_join = strict;
    }
    target.validate()?;
    Ok(Command::SaveRoomCard {
        path,
        target,
        expected_revision: revision,
    })
}

fn resolve(parsed: ParsedLine, state: &ParticipantSnapshot) -> Result<Command, String> {
    match parsed {
        ParsedLine::Command(command) => Ok(*command),
        ParsedLine::Partial(partial) => match *partial {
            PartialAction::CardEdit { .. } => Err(
                "A card edit requires the live shared card query before applying its patch.".into(),
            ),
            PartialAction::GlobalRuleEdit { rule } => Ok(Command::EditGlobalRule { rule }),
            PartialAction::LocalMix {
                source_id,
                playback,
            } => {
                let mut mix = state
                    .local_sources
                    .iter()
                    .find(|source| source.id == source_id)
                    .ok_or_else(|| {
                        format!("Local source {source_id} is not part of this configuration.")
                    })?
                    .playback;
                playback.apply(&mut mix);
                Ok(Command::SetSourceMix {
                    member_id: None,
                    source_id,
                    gain_db: mix.gain_db,
                    pan: mix.pan,
                    mute: mix.mute,
                    solo: mix.solo,
                })
            }
            PartialAction::Master {
                gain_db,
                limiter_enabled,
            } => Ok(Command::SetMaster {
                gain_db: gain_db.unwrap_or(state.master_gain_db),
                limiter_enabled: limiter_enabled.unwrap_or(state.limiter_enabled),
            }),
            PartialAction::CreateRoomPendingBind { profile } => {
                let mut bind = default_contact_listener(state.local_address.unwrap_or(state.bind));
                bind.set_port(profile.room.effective_port());
                Ok(Command::CreateRoom {
                    profile: *profile,
                    bind,
                })
            }
        },
        ParsedLine::Inquiry(_) => Err("An inquiry does not mutate the participant.".into()),
    }
}

fn write_snapshot(
    state: &ParticipantSnapshot,
    json: bool,
    out: &mut dyn Write,
    last: &mut String,
    force: bool,
) -> Result<(), CliError> {
    let text = if json {
        grammar::format_snapshot_line(state)?
    } else if force {
        serde_json::to_string_pretty(state).map_err(|error| error.to_string())?
    } else {
        let session = state.room.session.as_ref();
        let mut errors = Vec::new();
        for error in [
            session.and_then(|value| value.failure.as_deref()),
            state.room.error.as_deref(),
            session.and_then(|value| value.rules_save_error.as_deref()),
            session.and_then(|value| value.virtual_return.as_ref()?.error.as_deref()),
            state.persistence_error.as_deref(),
            state.socket_error.as_deref(),
            state.network.error.as_deref(),
            state.capture_error.as_deref(),
            state.output.last_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !errors.contains(&error) {
                errors.push(error);
            }
        }
        if let Some(notice) = session.and_then(|value| value.operation_notice.as_deref()) {
            errors.push(notice);
        }
        for device in &state.output_devices {
            for error in [
                device.state.last_error.as_deref(),
                device.state.clock_error.as_deref(),
                device.state.scheduling_error.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !errors.contains(&error) {
                    errors.push(error);
                }
            }
            for error in &device.state.source_errors {
                if !errors.contains(&error.detail.as_str()) {
                    errors.push(&error.detail);
                }
            }
            for source in &device.state.sources {
                if let Some(error) = source.last_error {
                    if !errors.contains(&error) {
                        errors.push(error);
                    }
                }
            }
        }
        let errors = if errors.is_empty() {
            String::new()
        } else {
            format!(" | {}", errors.join(" | "))
        };
        format!(
            "{} | {} | {} members | {}{}",
            state.local_name,
            state.room_phase,
            state.members.len(),
            if state.local_addresses.is_empty() {
                "no media socket".into()
            } else {
                state
                    .local_addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            errors
        )
    };
    if force || text != *last {
        writeln!(out, "{text}")?;
        out.flush()?;
        if !force {
            *last = text;
        }
    }
    Ok(())
}

fn write_result(
    json: bool,
    result: &impl serde::Serialize,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let text = if json {
        grammar::format_result_line(result)?
    } else {
        serde_json::to_string_pretty(result).map_err(|error| error.to_string())?
    };
    writeln!(out, "{text}")?;
    out.flush()?;
    Ok(())
}

fn write_error(
    json: bool,
    scope: &str,
    message: &str,
    out: &mut dyn Write,
    diagnostics: &mut dyn Write,
) -> Result<(), CliError> {
    if json {
        writeln!(out, "{}", grammar::format_error_line(scope, message))?;
        out.flush()?;
    } else {
        writeln!(diagnostics, "{scope}: {message}")?;
    }
    Ok(())
}
