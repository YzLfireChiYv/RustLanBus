//! Pure participant grammar.
//!
//! Tokenizing, startup parsing, interactive-line parsing, help text, and JSON
//! record formatting stay in this module. Controller startup, socket binds,
//! and filesystem reads and writes stay outside it.
//!
//! The operational frontend executes accepted requests through the common
//! participant controller. This module keeps parsing and record framing pure.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;

use rlb_model::{RoomId, SourceId};
use serde::Serialize;

use super::command::Command;
use super::config::{SourceKind, validate_name, validate_optional_endpoint};
use super::management::ImportMode;
use super::network::NetworkPolicy;
use super::output_routing::{OutputTarget, validate_output_targets};
use super::rooms::{GlobalRules, JoinMode, MixPatch, RoomProfile, RoomTarget, Rule, RuleMatch};

/// Process exit for a startup the grammar accepts and a later phase completes.
pub const EXIT_SUCCESS: i32 = 0;
/// Process exit for a failure while a participant is running.
pub const EXIT_RUNTIME_FAILURE: i32 = 1;
/// Process exit for startup arguments the grammar rejects.
/// Distinct from [`EXIT_RUNTIME_FAILURE`].
pub const EXIT_STARTUP_USAGE: i32 = 2;

/// Scope field for a recoverable interactive-line error record.
pub const ERROR_SCOPE_LINE: &str = "line";
/// Scope field available for a command-execution error record.
pub const ERROR_SCOPE_COMMAND: &str = "command";
/// Scope name for startup failures, reported on the human channel.
pub const ERROR_SCOPE_STARTUP: &str = "startup";

pub const HELP: &str = "\
RustLanBus participant CLI

The participant prefix is optional. No arguments or --help show this help.
participant init CONFIG --name NAME --bind ADDR
participant inspect CONFIG
participant cards CONFIG
participant login-startup CONFIG on|off|status
participant run CONFIG [--json] [--stdin] [--seconds N] [--join CARD | --create CARD --listen ADDRESS]
participant settings export CONFIG --out FILE
participant settings preview FILE --mode restore|copy [--to CONFIG]
participant settings import FILE --to CONFIG --mode restore|copy --source-revision REV [--destination-revision REV] [--name NAME]
status
cards
card read PATH
card edit PATH --revision REV [--name NAME] [--address IP|--clear-address] [--port N] [--password TEXT] [--strict on|off]
card delete PATH --revision REV
settings export --out FILE
settings pending
devices
rooms
rules
outputs
refresh
reconnect
name NAME
quit
room create --name NAME [--listen ADDR] [--address IP] [--port N] [--password TEXT] [--strict]
room create-saved --card PATH --listen ADDR
room join --card PATH (--manual | --automatic)
room join-unknown --name NAME [--address IP] [--port N] [--password TEXT] [--strict]
room choose GENERATION INSTANCE_ID
room leave
room cancel
room reload
return start
return stop
login-startup on|off|status
network allow|status|remove
network select auto|--interface GUID [--bind ADDR]
device refresh
device source SOURCE_ID (--endpoint ID | --clear)
device output (--endpoint ID | --clear)
source SOURCE_ID publish on|off
source SOURCE_ID monitor on|off
source SOURCE_ID publish on|off monitor on|off
outputs local SOURCE_ID (--main [--endpoint ID ...] | --endpoint ID ... | --none)
outputs room --match all|name NAME|ip IP [--ip IP] [--source all|system|microphone] (--main [--endpoint ID ...] | --endpoint ID ... | --none)
outputs global --match all|name NAME|ip IP [--ip IP] [--source all|system|microphone] (--main [--endpoint ID ...] | --endpoint ID ... | --none)
rule room --match all | --match name NAME | --match ip IP | --match name NAME --ip IP [--source all|system|microphone] [--send on|off] [--listen on|off]
rule global --match all | --match name NAME | --match ip IP | --match name NAME --ip IP [--source all|system|microphone] [--send on|off] [--listen on|off]
rule global-replace JSON
mix room --match all | --match name NAME | --match ip IP | --match name NAME --ip IP --source system|microphone [--gain-db GAIN] [--pan PAN] [--mute on|off] [--solo on|off]
mix local SOURCE_ID [--gain-db GAIN] [--pan PAN] [--mute on|off] [--solo on|off]
master [--gain-db GAIN] [--limiter on|off]

status, cards, devices, rooms, rules, and bare outputs are inquiries.
devices lists endpoint IDs and local source IDs for device/source/outputs commands.
rooms includes discovery and same-name choices. rules includes rule files and save outcomes.
refresh rereads saved cards; live discovery continues.
name changes this computer's display name after leaving the room; quote names containing spaces.
participant inspect and participant cards read saved files without starting audio.
room create without --listen uses the core's default contact listener.
rule global edits one rule. rule global-replace reads a complete global rules document.
Omitted send, listen, publish, and monitor stay omitted.
mix and master change only supplied fields; local mix controls this computer's monitoring.
Output choices do not enable publication, local monitoring, or remote listening.
return start enables sending to and listening to this computer's virtual return member and saves its room rule.
return stop removes the virtual member; its saved room rules remain.
--main follows the selected main output; --endpoint keeps the exact device choice.
Output targets may be combined; --none sends this source to no output devices.
An omitted address and the token null leave the room address empty.
--seconds N is a positive whole number of seconds and may exceed 300.
run remains active until interrupted, quit is requested, or --seconds expires.
--stdin reads commands one per line; reaching EOF shuts down the participant cleanly.
Without --stdin, run does not read standard input.
--json writes newline-delimited snapshot, result, and error records to stdout.
Human-readable output is the default; help and diagnostics use the human channel.
Single or double quotes group one token. Backslashes inside quotes stay literal.
Use --option=value when a value starts with --, for example --password=--abc.
rule global-replace JSON is the raw remainder of the line.
JSON object lines use Command encoding. Unknown JSON fields are rejected.
Only supported user commands are accepted; internal directory and legacy direct-contact commands are rejected.
card read/edit are explicit private results and include the room password; continuous snapshots omit it.
card edit preserves omitted target fields and requires the revision returned by card read.
settings preview supplies source/destination revisions. Import requires the source revision and,
when replacing existing data, the destination revision from that preview. Existing owners must stop first.
copy mode requires --name and resets local identities/audio/network bindings for local setup.
Offline settings export requires an idle configuration; online export includes current effective settings.
CLI quit/EOF/interruption/time limit shuts down without prompting; unsaved settings produce details and exit code 1.
Parsing performs no I/O. Accepted commands execute through the shared controller.
Password values are command input and are never copied into help or error text.
";

/// Process outcome class shared by the parser and operational frontend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessClass {
    Success,
    StartupUsage,
    RuntimeFailure,
}

/// Startup text the grammar rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupError {
    pub message: String,
}

impl StartupError {
    pub const fn process_class(&self) -> ProcessClass {
        ProcessClass::StartupUsage
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StartupError {}

/// One parsed participant startup invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupRequest {
    Init(InitRequest),
    Inspect(InspectRequest),
    Cards(InspectRequest),
    LoginStartup {
        config: PathBuf,
        enabled: Option<bool>,
    },
    Run(RunRequest),
    SettingsExport {
        config: PathBuf,
        output: PathBuf,
    },
    SettingsPreview {
        input: PathBuf,
        destination: Option<PathBuf>,
        mode: ImportMode,
    },
    SettingsImport {
        input: PathBuf,
        destination: PathBuf,
        mode: ImportMode,
        new_name: Option<String>,
        source_revision: String,
        destination_revision: Option<String>,
    },
}

/// Validated `participant init` input. The configuration file stays unwritten.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitRequest {
    pub config: PathBuf,
    pub name: String,
    pub bind: SocketAddr,
}

/// Validated `participant inspect` or `cards` input. Parsing leaves files unread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectRequest {
    pub config: PathBuf,
}

/// Validated `participant run` input. The configuration file stays unread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub config: PathBuf,
    pub json: bool,
    pub stdin: bool,
    /// `None` means the invocation set no duration. `Some` is at least 1.
    /// Values above the historical 300-second run-loop cap stay legal here.
    pub seconds: Option<u64>,
    pub room: RunRoomIntent,
}

/// Room action requested on the run command. Absent flags leave the saved intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunRoomIntent {
    /// No `--join` or `--create`. The caller applies the saved restore intent.
    SavedRestore,
    Join {
        card: PathBuf,
    },
    Create {
        card: PathBuf,
        listen: SocketAddr,
    },
}

/// Result of one interactive line. A bad line stays in the session.
#[derive(Clone, Debug, PartialEq)]
pub enum LineOutcome {
    Blank,
    Parsed(ParsedLine),
    Recoverable(RecoverableLine),
}

/// One interactive line the session can report and then keep reading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoverableLine {
    pub message: String,
}

impl RecoverableLine {
    pub const fn requires_exit(&self) -> bool {
        false
    }

    pub const fn exit_code(&self) -> Option<i32> {
        None
    }
}

/// A line the grammar accepted. Acceptance leaves the room connection untouched.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedLine {
    Command(Box<Command>),
    Inquiry(Inquiry),
    Partial(Box<PartialAction>),
}

/// Read-only questions. They stay out of [`Command`] so they cannot start audio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Inquiry {
    Status,
    Cards,
    Devices,
    Rooms,
    Rules,
    Outputs,
    LoginStartup,
    LanFirewall,
}

/// Input whose common owner supplies a default or performs a partial rule edit.
#[derive(Clone, Debug, PartialEq)]
pub enum PartialAction {
    CardEdit {
        path: PathBuf,
        revision: String,
        name: Option<String>,
        address: Option<Option<IpAddr>>,
        port: Option<u16>,
        password: Option<String>,
        strict: Option<bool>,
    },
    /// `room create` without `--listen` uses the core's default contact listener.
    CreateRoomPendingBind { profile: Box<RoomProfile> },
    /// One global rule edit. Omitted directions stay `None`.
    /// This is a single edit, so it is not [`Command::ReplaceGlobalRules`].
    GlobalRuleEdit { rule: Rule },
    LocalMix {
        source_id: SourceId,
        playback: MixPatch,
    },
    Master {
        gain_db: Option<f32>,
        limiter_enabled: Option<bool>,
    },
}

/// Human text, including help and diagnostics. JSON stdout uses the formatters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsoleFrame {
    JsonStdout(String),
    Human(String),
}

#[must_use]
pub const fn help_text() -> &'static str {
    HELP
}

#[must_use]
pub const fn exit_code(class: ProcessClass) -> i32 {
    match class {
        ProcessClass::Success => EXIT_SUCCESS,
        ProcessClass::StartupUsage => EXIT_STARTUP_USAGE,
        ProcessClass::RuntimeFailure => EXIT_RUNTIME_FAILURE,
    }
}

#[must_use]
pub fn classify_startup(result: &Result<StartupRequest, StartupError>) -> ProcessClass {
    match result {
        Ok(_) => ProcessClass::Success,
        Err(error) => error.process_class(),
    }
}

/// Parses one unsplit startup line with this module's tokenizer.
pub fn parse_startup_line(line: &str) -> Result<StartupRequest, StartupError> {
    let tokens = tokenize(line).map_err(|message| StartupError { message })?;
    parse_startup_tokens(&tokens)
}

/// Parses arguments already split by the process. A leading `participant` word
/// is optional. Tokens are not split again.
pub fn parse_startup_args(args: &[String]) -> Result<StartupRequest, StartupError> {
    parse_startup_tokens(args)
}

#[must_use]
pub fn parse_line(line: &str) -> LineOutcome {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LineOutcome::Blank;
    }
    if trimmed.starts_with('{') {
        return parse_json_command(trimmed);
    }
    if let Some(body) = global_replace_body(trimmed) {
        return parse_global_replace(body);
    }
    match tokenize(trimmed) {
        Ok(tokens) => match parse_human(&tokens) {
            Ok(parsed) => LineOutcome::Parsed(parsed),
            Err(message) => recoverable(message),
        },
        Err(message) => recoverable(message),
    }
}

/// Help stays on the human channel for both values of `json_mode`.
#[must_use]
pub fn present_help(_json_mode: bool) -> ConsoleFrame {
    ConsoleFrame::Human(HELP.to_owned())
}

/// Diagnostics stay on the human channel for both values of `json_mode`.
#[must_use]
pub fn present_diagnostic(_json_mode: bool, message: &str) -> ConsoleFrame {
    ConsoleFrame::Human(message.to_owned())
}

/// Startup failures stay on the human channel for both values of `json_mode`.
#[must_use]
pub fn startup_failure_frame(_json_mode: bool, error: &StartupError) -> ConsoleFrame {
    ConsoleFrame::Human(error.message.clone())
}

pub fn format_snapshot_line(state: &impl Serialize) -> Result<String, String> {
    encode_record("snapshot", "state", state)
}

pub fn format_result_line(result: &impl Serialize) -> Result<String, String> {
    encode_record("result", "result", result)
}

#[must_use]
pub fn format_error_line(scope: &str, message: &str) -> String {
    let mut record = serde_json::Map::new();
    record.insert(
        "kind".to_owned(),
        serde_json::Value::String("error".to_owned()),
    );
    record.insert(
        "message".to_owned(),
        serde_json::Value::String(message.to_owned()),
    );
    record.insert(
        "scope".to_owned(),
        serde_json::Value::String(scope.to_owned()),
    );
    serde_json::to_string(&serde_json::Value::Object(record)).expect("error record")
}

#[must_use]
pub fn recoverable_error_line(error: &RecoverableLine) -> String {
    format_error_line(ERROR_SCOPE_LINE, &error.message)
}

fn encode_record(kind: &str, field: &str, value: &impl Serialize) -> Result<String, String> {
    let payload = serde_json::to_value(value).map_err(json_error_text)?;
    let mut record = serde_json::Map::new();
    record.insert(
        "kind".to_owned(),
        serde_json::Value::String(kind.to_owned()),
    );
    record.insert(field.to_owned(), payload);
    serde_json::to_string(&serde_json::Value::Object(record)).map_err(json_error_text)
}

fn json_error_text(error: serde_json::Error) -> String {
    error.to_string()
}

fn recoverable(message: impl Into<String>) -> LineOutcome {
    LineOutcome::Recoverable(RecoverableLine {
        message: message.into(),
    })
}

fn parsed_command(command: Command) -> ParsedLine {
    ParsedLine::Command(Box::new(command))
}

fn parsed_partial(action: PartialAction) -> ParsedLine {
    ParsedLine::Partial(Box::new(action))
}

fn parsed_inquiry(inquiry: Inquiry) -> ParsedLine {
    ParsedLine::Inquiry(inquiry)
}

fn parse_startup_tokens(tokens: &[String]) -> Result<StartupRequest, StartupError> {
    let tokens = strip_participant(tokens);
    if tokens.is_empty() {
        return Err(unknown_verb());
    }
    let verb = tokens[0].clone();
    let rest = &tokens[1..];
    let result = match verb.as_str() {
        "init" => parse_init(rest).map(StartupRequest::Init),
        "inspect" => parse_inspect(rest).map(StartupRequest::Inspect),
        "cards" => parse_inspect(rest).map(StartupRequest::Cards),
        "login-startup" => {
            if rest.len() != 2 {
                Err("Use participant login-startup CONFIG on|off|status.".into())
            } else {
                parse_login_startup_value(&rest[1]).map(|enabled| StartupRequest::LoginStartup {
                    config: PathBuf::from(&rest[0]),
                    enabled,
                })
            }
        }
        "run" => parse_run(rest).map(StartupRequest::Run),
        "settings" => parse_startup_settings(rest),
        _ => Err(unknown_verb().message),
    };
    result.map_err(|message| StartupError { message })
}

fn unknown_verb() -> StartupError {
    StartupError {
        message:
            "Unknown participant command. The startup commands are init, inspect, cards, settings, login-startup, and run."
                .to_owned(),
    }
}

fn strip_participant(tokens: &[String]) -> &[String] {
    match tokens.first().map(String::as_str) {
        Some("participant") => &tokens[1..],
        _ => tokens,
    }
}

fn parse_import_mode(value: &str) -> Result<ImportMode, String> {
    match value {
        "restore" => Ok(ImportMode::Restore),
        "copy" | "copy_to_new_device" => Ok(ImportMode::CopyToNewDevice),
        _ => Err("Settings mode must be restore or copy.".into()),
    }
}

fn parse_startup_settings(tokens: &[String]) -> Result<StartupRequest, String> {
    let verb = tokens
        .first()
        .map(String::as_str)
        .ok_or("Use settings export, preview, or import.")?;
    let input = path_token(
        "settings path",
        tokens.get(1).ok_or("The settings path is missing.")?,
    )?;
    if verb == "export" {
        let mut output = None;
        walk_flags(&tokens[2..], |flag, values, index| match flag {
            "--out" => {
                let value = next_value(values, index, flag)?;
                set_once(&mut output, flag, path_token(flag, value)?)
            }
            other => Err(unexpected(other)),
        })?;
        return Ok(StartupRequest::SettingsExport {
            config: input,
            output: output.ok_or_else(|| required("--out"))?,
        });
    }
    if !matches!(verb, "preview" | "import") {
        return Err("Use settings export, preview, or import.".into());
    }
    let mut destination = None;
    let mut mode = None;
    let mut new_name = None;
    let mut source_revision = None;
    let mut destination_revision = None;
    walk_flags(&tokens[2..], |flag, values, index| {
        let value = next_value(values, index, flag)?;
        match flag {
            "--to" => set_once(&mut destination, flag, path_token(flag, value)?),
            "--mode" => set_once(&mut mode, flag, parse_import_mode(value)?),
            "--name" => {
                validate_name("new device name", value)?;
                set_once(&mut new_name, flag, value.to_owned())
            }
            "--source-revision" => set_once(&mut source_revision, flag, value.to_owned()),
            "--destination-revision" => set_once(&mut destination_revision, flag, value.to_owned()),
            other => Err(unexpected(other)),
        }
    })?;
    let mode = mode.ok_or_else(|| required("--mode"))?;
    if verb == "preview" {
        if new_name.is_some() || source_revision.is_some() || destination_revision.is_some() {
            return Err("Preview accepts only --mode and optional --to.".into());
        }
        return Ok(StartupRequest::SettingsPreview {
            input,
            destination,
            mode,
        });
    }
    match mode {
        ImportMode::CopyToNewDevice if new_name.is_none() => return Err(required("--name")),
        ImportMode::Restore if new_name.is_some() => {
            return Err("--name is only used with copy mode.".into());
        }
        _ => {}
    }
    Ok(StartupRequest::SettingsImport {
        input,
        destination: destination.ok_or_else(|| required("--to"))?,
        mode,
        new_name,
        source_revision: source_revision.ok_or_else(|| required("--source-revision"))?,
        destination_revision,
    })
}

fn parse_init(tokens: &[String]) -> Result<InitRequest, String> {
    let mut config = None;
    let mut name = None;
    let mut bind = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        if token.starts_with("--") {
            apply_init_flag(
                &mut name,
                &mut bind,
                option_name(token)?,
                tokens,
                &mut index,
            )?;
        } else {
            set_config(&mut config, token)?;
        }
    }
    let config = config.ok_or_else(|| "The configuration path is missing.".to_owned())?;
    let name = name.ok_or_else(|| required("--name"))?;
    let bind = bind.ok_or_else(|| required("--bind"))?;
    validate_name("display name", &name)?;
    Ok(InitRequest { config, name, bind })
}

fn apply_init_flag(
    name: &mut Option<String>,
    bind: &mut Option<SocketAddr>,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--name" => {
            let value = next_value(tokens, index, "--name")?;
            set_once(name, "--name", value.to_owned())
        }
        "--bind" => {
            let value = next_value(tokens, index, "--bind")?;
            set_once(bind, "--bind", parse_socket("bind address", value)?)
        }
        other => Err(unexpected(other)),
    }
}

fn parse_inspect(tokens: &[String]) -> Result<InspectRequest, String> {
    let Some(path) = tokens.first() else {
        return Err("The configuration path is missing.".to_owned());
    };
    if path.starts_with("--") {
        return Err("The configuration path is missing.".to_owned());
    }
    if tokens.len() != 1 {
        return Err("This command accepts only the configuration path.".to_owned());
    }
    Ok(InspectRequest {
        config: path_token("configuration path", path)?,
    })
}

#[derive(Default)]
struct RunFlags {
    json: Option<bool>,
    stdin: Option<bool>,
    seconds: Option<u64>,
    join: Option<PathBuf>,
    create: Option<PathBuf>,
    listen: Option<SocketAddr>,
}

fn parse_run(tokens: &[String]) -> Result<RunRequest, String> {
    let mut flags = RunFlags::default();
    let mut config = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        if token.starts_with("--") {
            apply_run_flag(&mut flags, option_name(token)?, tokens, &mut index)?;
        } else {
            set_config(&mut config, token)?;
        }
    }
    let config = config.ok_or_else(|| "The configuration path is missing.".to_owned())?;
    let room = run_room(flags.join, flags.create, flags.listen)?;
    Ok(RunRequest {
        config,
        json: flags.json.unwrap_or(false),
        stdin: flags.stdin.unwrap_or(false),
        seconds: flags.seconds,
        room,
    })
}

fn apply_run_flag(
    flags: &mut RunFlags,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--json" => set_once(&mut flags.json, "--json", true),
        "--stdin" => set_once(&mut flags.stdin, "--stdin", true),
        "--seconds" => {
            let value = next_value(tokens, index, "--seconds")?;
            set_once(&mut flags.seconds, "--seconds", parse_seconds(value)?)
        }
        "--join" => {
            let value = next_value(tokens, index, "--join")?;
            set_once(&mut flags.join, "--join", path_token("--join", value)?)
        }
        "--create" => {
            let value = next_value(tokens, index, "--create")?;
            set_once(
                &mut flags.create,
                "--create",
                path_token("--create", value)?,
            )
        }
        "--listen" => {
            let value = next_value(tokens, index, "--listen")?;
            set_once(
                &mut flags.listen,
                "--listen",
                parse_socket("listen address", value)?,
            )
        }
        other => Err(unexpected(other)),
    }
}

fn run_room(
    join: Option<PathBuf>,
    create: Option<PathBuf>,
    listen: Option<SocketAddr>,
) -> Result<RunRoomIntent, String> {
    match (join, create, listen) {
        (Some(_), Some(_), _) => Err("--join and --create are mutually exclusive.".to_owned()),
        (_, None, Some(_)) => Err("--listen requires --create.".to_owned()),
        (None, Some(_), None) => Err("--create requires --listen.".to_owned()),
        (Some(card), None, None) => Ok(RunRoomIntent::Join { card }),
        (None, Some(card), Some(listen)) => Ok(RunRoomIntent::Create { card, listen }),
        (None, None, None) => Ok(RunRoomIntent::SavedRestore),
    }
}

fn parse_human(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(head) = tokens.first() else {
        return Err("The command is missing.".to_owned());
    };
    match head.as_str() {
        "status" => lone(tokens, parsed_inquiry(Inquiry::Status)),
        "cards" => lone(tokens, parsed_inquiry(Inquiry::Cards)),
        "card" => parse_card(&tokens[1..]),
        "settings" => parse_live_settings(&tokens[1..]),
        "devices" => lone(tokens, parsed_inquiry(Inquiry::Devices)),
        "rooms" => lone(tokens, parsed_inquiry(Inquiry::Rooms)),
        "rules" => lone(tokens, parsed_inquiry(Inquiry::Rules)),
        "refresh" => lone(tokens, parsed_command(Command::RefreshRooms)),
        "reconnect" => lone(tokens, parsed_command(Command::Reconnect)),
        "name" => parse_name(&tokens[1..]),
        "quit" => lone(tokens, parsed_command(Command::Quit)),
        "room" => parse_room(&tokens[1..]),
        "return" => parse_return(&tokens[1..]),
        "network" => parse_network(&tokens[1..]),
        "login-startup" => {
            if tokens.len() != 2 {
                return Err("Use login-startup on|off|status.".into());
            }
            Ok(match parse_login_startup_value(&tokens[1])? {
                Some(enabled) => parsed_command(Command::SetLoginStartup { enabled }),
                None => parsed_inquiry(Inquiry::LoginStartup),
            })
        }
        "device" => parse_device(&tokens[1..]),
        "source" => parse_source(tokens),
        "outputs" if tokens.len() == 1 => Ok(parsed_inquiry(Inquiry::Outputs)),
        "outputs" => parse_outputs(&tokens[1..]),
        "rule" => parse_rule(&tokens[1..]),
        "mix" => parse_mix(&tokens[1..]),
        "master" => parse_master(&tokens[1..]),
        other => Err(format!("Unknown command: {other}.")),
    }
}

fn lone(tokens: &[String], parsed: ParsedLine) -> Result<ParsedLine, String> {
    if tokens.len() == 1 {
        Ok(parsed)
    } else {
        Err(unexpected(&tokens[1]))
    }
}

fn parse_name(tokens: &[String]) -> Result<ParsedLine, String> {
    match tokens {
        [name] => {
            validate_name("display name", name)?;
            Ok(parsed_command(Command::SetName { name: name.clone() }))
        }
        [] => Err("The display name is missing. Use name NAME.".to_owned()),
        [_, extra, ..] => Err(unexpected(extra)),
    }
}

fn parse_card(tokens: &[String]) -> Result<ParsedLine, String> {
    let verb = tokens
        .first()
        .map(String::as_str)
        .ok_or("Use card read, edit, or delete.")?;
    let path = path_token(
        "card path",
        tokens.get(1).ok_or("The card path is missing.")?,
    )?;
    if verb == "read" {
        if tokens.len() != 2 {
            return Err("Use card read PATH.".into());
        }
        return Ok(parsed_command(Command::ReadRoomCard { path }));
    }
    if !matches!(verb, "edit" | "delete") {
        return Err("Use card read, edit, or delete.".into());
    }
    let mut revision = None;
    let mut fields = CreateFields::default();
    walk_flags(&tokens[2..], |flag, values, index| {
        if flag == "--revision" {
            return set_once(
                &mut revision,
                flag,
                next_value(values, index, flag)?.to_owned(),
            );
        }
        if verb == "delete" {
            return Err(unexpected(flag));
        }
        match flag {
            "--name" => {
                let value = next_value(values, index, flag)?;
                set_once(&mut fields.name, flag, value.to_owned())
            }
            "--address" => {
                let value = next_value(values, index, flag)?;
                set_once(&mut fields.address, flag, parse_optional_ip(value)?)
            }
            "--clear-address" => set_once(&mut fields.address, flag, None),
            "--port" => {
                let value = next_value(values, index, flag)?;
                set_once(&mut fields.port, flag, parse_port(value)?)
            }
            "--password" => {
                let value = next_value(values, index, flag)?;
                set_once(&mut fields.password, flag, value.to_owned())
            }
            "--strict" => {
                let value = next_value(values, index, flag)?;
                set_once(&mut fields.strict, flag, parse_on_off(flag, value)?)
            }
            other => Err(unexpected(other)),
        }
    })?;
    let revision = revision.ok_or_else(|| required("--revision"))?;
    if verb == "delete" {
        return Ok(parsed_command(Command::DeleteRoomCard {
            path,
            expected_revision: revision,
        }));
    }
    if fields.name.is_none()
        && fields.address.is_none()
        && fields.port.is_none()
        && fields.password.is_none()
        && fields.strict.is_none()
    {
        return Err("Supply at least one card target field to edit.".into());
    }
    if let Some(name) = &fields.name {
        validate_name("room name", name)?;
    }
    Ok(parsed_partial(PartialAction::CardEdit {
        path,
        revision,
        name: fields.name,
        address: fields.address,
        port: fields.port,
        password: fields.password,
        strict: fields.strict,
    }))
}

fn parse_live_settings(tokens: &[String]) -> Result<ParsedLine, String> {
    match tokens.first().map(String::as_str) {
        Some("pending") => lone(tokens,parsed_command(Command::CheckPendingSettings)),
        Some("export") => {
            let mut path=None;
            walk_flags(&tokens[1..],|flag,values,index| match flag {
                "--out" => { let value=next_value(values,index,flag)?; set_once(&mut path,flag,path_token(flag,value)?) }
                other => Err(unexpected(other)),
            })?;
            Ok(parsed_command(Command::ExportSettings { path:path.ok_or_else(||required("--out"))? }))
        }
        _ => Err("Use settings pending or settings export --out FILE. Preview/import are offline startup commands.".into()),
    }
}

fn parse_network(tokens: &[String]) -> Result<ParsedLine, String> {
    let verb = tokens
        .first()
        .map(String::as_str)
        .ok_or("Use network allow|status|remove or network select.")?;
    match verb {
        "allow" => return lone(tokens,parsed_command(Command::AllowLanFirewall)),
        "status" => return lone(tokens,parsed_inquiry(Inquiry::LanFirewall)),
        "remove" => return lone(tokens,parsed_command(Command::RemoveOwnLanFirewall)),
        "select" => {}
        _ => return Err("Use network allow|status|remove or network select auto|--interface GUID [--bind ADDR].".into()),
    }
    let automatic = tokens.get(1).is_some_and(|value| value == "auto");
    let flags = if automatic {
        &tokens[2..]
    } else {
        &tokens[1..]
    };
    let mut interface = None;
    let mut bind = None;
    walk_flags(flags, |flag, values, index| match flag {
        "--interface" => {
            let value = next_value(values, index, flag)?;
            set_once(&mut interface, flag, value.to_owned())
        }
        "--bind" => {
            let value = next_value(values, index, flag)?;
            set_once(&mut bind, flag, parse_socket("media bind", value)?)
        }
        other => Err(unexpected(other)),
    })?;
    let policy = match (automatic, interface) {
        (true, None) => NetworkPolicy::Auto,
        (false, Some(name)) => NetworkPolicy::Interface { name },
        _ => return Err("Select auto or exactly one --interface GUID.".into()),
    };
    policy.validate()?;
    Ok(parsed_command(Command::SetNetwork { policy, bind }))
}

fn parse_login_startup_value(value: &str) -> Result<Option<bool>, String> {
    match value {
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        "status" => Ok(None),
        _ => Err("Login startup accepts on, off, or status.".into()),
    }
}

fn parse_return(tokens: &[String]) -> Result<ParsedLine, String> {
    match tokens.first().map(String::as_str) {
        Some("start") => lone(tokens, parsed_command(Command::StartVirtualReturn)),
        Some("stop") => lone(tokens, parsed_command(Command::StopVirtualReturn)),
        Some(other) => Err(format!("Unknown return command: {other}.")),
        None => Err("The return command is missing. Use return start or return stop.".into()),
    }
}

fn parse_room(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(head) = tokens.first() else {
        return Err("The room command is missing.".to_owned());
    };
    match head.as_str() {
        "create" => parse_room_create(&tokens[1..]),
        "create-saved" => parse_room_create_saved(&tokens[1..]),
        "join" => parse_room_join(&tokens[1..]),
        "join-unknown" => parse_room_join_unknown(&tokens[1..]),
        "choose" => parse_room_choose(&tokens[1..]),
        "leave" => lone(tokens, parsed_command(Command::LeaveRoom)),
        "cancel" => lone(tokens, parsed_command(Command::CancelRoom)),
        "reload" => lone(tokens, parsed_command(Command::ReloadRoomRules)),
        other => Err(format!("Unknown room command: {other}.")),
    }
}

#[derive(Default)]
struct CreateFields {
    name: Option<String>,
    listen: Option<SocketAddr>,
    address: Option<Option<IpAddr>>,
    port: Option<u16>,
    password: Option<String>,
    strict: Option<bool>,
}

fn parse_room_create(tokens: &[String]) -> Result<ParsedLine, String> {
    let mut fields = CreateFields::default();
    walk_flags(tokens, |token, tokens, index| {
        apply_create_flag(&mut fields, token, tokens, index)
    })?;
    finish_create(fields)
}

fn apply_create_flag(
    fields: &mut CreateFields,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--name" => {
            let value = next_value(tokens, index, "--name")?;
            set_once(&mut fields.name, "--name", value.to_owned())
        }
        "--listen" => {
            let value = next_value(tokens, index, "--listen")?;
            set_once(
                &mut fields.listen,
                "--listen",
                parse_socket("listen address", value)?,
            )
        }
        "--address" => {
            let value = next_value(tokens, index, "--address")?;
            set_once(&mut fields.address, "--address", parse_optional_ip(value)?)
        }
        "--port" => {
            let value = next_value(tokens, index, "--port")?;
            set_once(&mut fields.port, "--port", parse_port(value)?)
        }
        "--password" => {
            let value = next_value(tokens, index, "--password")?;
            set_once(&mut fields.password, "--password", value.to_owned())
        }
        "--strict" => set_once(&mut fields.strict, "--strict", true),
        other => Err(unexpected(other)),
    }
}

fn finish_create(fields: CreateFields) -> Result<ParsedLine, String> {
    let name = fields.name.ok_or_else(|| required("--name"))?;
    let profile = build_profile(
        &name,
        fields.address.unwrap_or(None),
        fields.port,
        fields.password.unwrap_or_default(),
        fields.strict.unwrap_or(false),
    )?;
    match fields.listen {
        Some(bind) => Ok(parsed_command(Command::CreateRoom { profile, bind })),
        None => Ok(parsed_partial(PartialAction::CreateRoomPendingBind {
            profile: Box::new(profile),
        })),
    }
}

#[derive(Default)]
struct SavedCreateFields {
    card: Option<PathBuf>,
    listen: Option<SocketAddr>,
}

fn parse_room_create_saved(tokens: &[String]) -> Result<ParsedLine, String> {
    let mut fields = SavedCreateFields::default();
    walk_flags(tokens, |token, tokens, index| {
        apply_saved_create_flag(&mut fields, token, tokens, index)
    })?;
    let profile_path = fields.card.ok_or_else(|| required("--card"))?;
    let bind = fields.listen.ok_or_else(|| required("--listen"))?;
    Ok(parsed_command(Command::CreateSavedRoom {
        profile_path,
        bind,
    }))
}

fn apply_saved_create_flag(
    fields: &mut SavedCreateFields,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--card" => {
            let value = next_value(tokens, index, "--card")?;
            set_once(&mut fields.card, "--card", path_token("--card", value)?)
        }
        "--listen" => {
            let value = next_value(tokens, index, "--listen")?;
            set_once(
                &mut fields.listen,
                "--listen",
                parse_socket("listen address", value)?,
            )
        }
        other => Err(unexpected(other)),
    }
}

#[derive(Default)]
struct JoinFields {
    card: Option<PathBuf>,
    manual: Option<bool>,
    automatic: Option<bool>,
}

fn parse_room_join(tokens: &[String]) -> Result<ParsedLine, String> {
    let mut fields = JoinFields::default();
    walk_flags(tokens, |token, tokens, index| {
        apply_join_flag(&mut fields, token, tokens, index)
    })?;
    let profile_path = fields.card.ok_or_else(|| required("--card"))?;
    let mode = join_mode(fields.manual, fields.automatic)?;
    Ok(parsed_command(Command::JoinSavedRoom {
        profile_path,
        mode,
    }))
}

fn apply_join_flag(
    fields: &mut JoinFields,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--card" => {
            let value = next_value(tokens, index, "--card")?;
            set_once(&mut fields.card, "--card", path_token("--card", value)?)
        }
        "--manual" => set_once(&mut fields.manual, "--manual", true),
        "--automatic" => set_once(&mut fields.automatic, "--automatic", true),
        other => Err(unexpected(other)),
    }
}

fn join_mode(manual: Option<bool>, automatic: Option<bool>) -> Result<JoinMode, String> {
    match (manual, automatic) {
        (Some(true), None) => Ok(JoinMode::Manual),
        (None, Some(true)) => Ok(JoinMode::Automatic),
        (Some(true), Some(true)) => {
            Err("--manual and --automatic are mutually exclusive.".to_owned())
        }
        _ => Err("Join mode is missing. Supply --manual or --automatic.".to_owned()),
    }
}

fn parse_room_join_unknown(tokens: &[String]) -> Result<ParsedLine, String> {
    let mut fields = CreateFields::default();
    walk_flags(tokens, |token, tokens, index| {
        apply_join_unknown_flag(&mut fields, token, tokens, index)
    })?;
    let name = fields.name.ok_or_else(|| required("--name"))?;
    if fields.listen.is_some() {
        return Err(unexpected("--listen"));
    }
    let target = build_target(
        name,
        fields.address.unwrap_or(None),
        fields.port,
        fields.password.unwrap_or_default(),
        fields.strict.unwrap_or(false),
    )?;
    Ok(parsed_command(Command::JoinUnknownRoom { target }))
}

fn apply_join_unknown_flag(
    fields: &mut CreateFields,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--listen" => Err(unexpected("--listen")),
        other => apply_create_flag(fields, other, tokens, index),
    }
}

fn parse_room_choose(tokens: &[String]) -> Result<ParsedLine, String> {
    match tokens {
        [generation, instance] => {
            let generation = parse_u64_decimal("choice generation", generation)?;
            let instance_id = parse_room_id(instance)?;
            Ok(parsed_command(Command::ChooseRoom {
                generation,
                instance_id,
            }))
        }
        [] => Err("The choice generation is missing.".to_owned()),
        [_] => Err("The room instance id is missing.".to_owned()),
        [_, _, extra, ..] => Err(unexpected(extra)),
    }
}

fn parse_device(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(head) = tokens.first() else {
        return Err("The device command is missing.".to_owned());
    };
    match head.as_str() {
        "refresh" => lone(tokens, parsed_command(Command::RefreshDevices)),
        "source" => parse_device_source(&tokens[1..]),
        "output" => parse_device_output(&tokens[1..]),
        other => Err(format!("Unknown device command: {other}.")),
    }
}

fn parse_device_source(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(id_token) = tokens.first() else {
        return Err("The source id is missing.".to_owned());
    };
    if id_token.starts_with("--") {
        return Err("The source id is missing.".to_owned());
    }
    let source_id = parse_source_id(id_token)?;
    let endpoint_id = parse_endpoint_choice(&tokens[1..])?;
    Ok(parsed_command(Command::SelectSourceEndpoint {
        source_id,
        endpoint_id,
    }))
}

fn parse_device_output(tokens: &[String]) -> Result<ParsedLine, String> {
    let endpoint_id = parse_endpoint_choice(tokens)?;
    Ok(parsed_command(Command::SelectOutput { endpoint_id }))
}

fn parse_endpoint_choice(tokens: &[String]) -> Result<Option<String>, String> {
    let mut endpoint = None;
    let mut clear = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        match option_name(token)? {
            "--endpoint" => {
                let value = next_value(tokens, &mut index, "--endpoint")?;
                set_once(&mut endpoint, "--endpoint", value.to_owned())?;
            }
            "--clear" => set_once(&mut clear, "--clear", true)?,
            other => return Err(unexpected(other)),
        }
    }
    match (endpoint, clear) {
        (Some(_), Some(_)) => Err("--endpoint and --clear are mutually exclusive.".to_owned()),
        (Some(endpoint_id), None) => {
            validate_optional_endpoint(Some(&endpoint_id))?;
            Ok(Some(endpoint_id))
        }
        (None, Some(_)) => Ok(None),
        (None, None) => Err("Supply --endpoint or --clear.".to_owned()),
    }
}

fn parse_source(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(id_token) = tokens.get(1) else {
        return Err("The source id is missing.".to_owned());
    };
    if id_token.starts_with("--") {
        return Err("The source id is missing.".to_owned());
    }
    let source_id = parse_source_id(id_token)?;
    let mut publish = None;
    let mut monitor = None;
    let mut index = 2;
    if index >= tokens.len() {
        return Err("Supply publish or monitor.".to_owned());
    }
    while index < tokens.len() {
        let key = tokens[index].as_str();
        index += 1;
        match key {
            "publish" => {
                let value = next_value(tokens, &mut index, "publish")?;
                set_once(&mut publish, "publish", parse_on_off("publish", value)?)?;
            }
            "monitor" => {
                let value = next_value(tokens, &mut index, "monitor")?;
                set_once(&mut monitor, "monitor", parse_on_off("monitor", value)?)?;
            }
            other => return Err(unexpected(other)),
        }
    }
    Ok(parsed_command(Command::SetLocalSource {
        source_id,
        publish,
        monitor,
    }))
}

fn parse_rule(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(head) = tokens.first() else {
        return Err("The rule command is missing.".to_owned());
    };
    match head.as_str() {
        "room" => parse_directional_rule("room", &tokens[1..]),
        "global" => parse_directional_rule("global", &tokens[1..]),
        "global-replace" => {
            Err("rule global-replace JSON is the raw remainder of the line.".to_owned())
        }
        other => Err(format!("Unknown rule command: {other}.")),
    }
}

#[derive(Default)]
struct OutputChoices {
    targets: Vec<OutputTarget>,
    none: bool,
}

impl OutputChoices {
    fn consume(&mut self, token: &str, tokens: &[String], index: &mut usize) -> Result<(), String> {
        match token {
            "--main" => self.targets.push(OutputTarget::Main),
            "--endpoint" => {
                let endpoint = next_value(tokens, index, "--endpoint")?;
                self.targets
                    .push(OutputTarget::Endpoint(endpoint.to_owned()));
            }
            "--none" => {
                if self.none {
                    return Err(duplicate("--none"));
                }
                self.none = true;
            }
            other => return Err(unexpected(other)),
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<OutputTarget>, String> {
        if self.none && !self.targets.is_empty() {
            return Err("--none cannot be combined with output targets.".to_owned());
        }
        if !self.none && self.targets.is_empty() {
            return Err("Supply --main, --endpoint, or --none.".to_owned());
        }
        validate_output_targets(&self.targets)?;
        Ok(self.targets)
    }
}

fn parse_outputs(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(scope) = tokens.first() else {
        return Err("The output scope is missing.".to_owned());
    };
    if scope == "local" {
        let Some(id) = tokens.get(1) else {
            return Err("The source id is missing.".to_owned());
        };
        let source_id = parse_source_id(id)?;
        let mut outputs = OutputChoices::default();
        walk_flags(&tokens[2..], |token, tokens, index| {
            outputs.consume(token, tokens, index)
        })?;
        return Ok(parsed_command(Command::SetLocalOutputs {
            source_id,
            targets: outputs.finish()?,
        }));
    }
    if !matches!(scope.as_str(), "room" | "global") {
        return Err(format!("Unknown output scope: {scope}."));
    }
    let mut fields = DirectionFields::default();
    let mut outputs = OutputChoices::default();
    walk_flags(&tokens[1..], |token, tokens, index| {
        if matches!(token, "--main" | "--endpoint" | "--none") {
            outputs.consume(token, tokens, index)
        } else if matches!(token, "--send" | "--listen") {
            Err(unexpected(token))
        } else {
            apply_direction_flag(&mut fields, token, tokens, index)
        }
    })?;
    let rule = Rule {
        r#match: fields.condition.finish()?,
        source_kind: fields.source.flatten(),
        send: None,
        listen: None,
        outputs: Some(outputs.finish()?),
    };
    rule.validate()?;
    if scope == "room" {
        Ok(parsed_command(Command::EditRoomRule { rule }))
    } else {
        Ok(parsed_partial(PartialAction::GlobalRuleEdit { rule }))
    }
}

#[derive(Default)]
struct DirectionFields {
    condition: MatchBuilder,
    source: Option<Option<SourceKind>>,
    send: Option<bool>,
    listen: Option<bool>,
}

fn parse_directional_rule(scope: &str, tokens: &[String]) -> Result<ParsedLine, String> {
    let mut fields = DirectionFields::default();
    walk_flags(tokens, |token, tokens, index| {
        apply_direction_flag(&mut fields, token, tokens, index)
    })?;
    finish_direction(scope, fields)
}

fn apply_direction_flag(
    fields: &mut DirectionFields,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    if apply_match_token(&mut fields.condition, token, tokens, index)? {
        return Ok(());
    }
    match token {
        "--source" => {
            let value = next_value(tokens, index, "--source")?;
            set_once(
                &mut fields.source,
                "--source",
                if value == "all" {
                    None
                } else {
                    Some(parse_source_kind(value)?)
                },
            )
        }
        "--send" => {
            let value = next_value(tokens, index, "--send")?;
            set_once(&mut fields.send, "--send", parse_on_off("--send", value)?)
        }
        "--listen" => {
            let value = next_value(tokens, index, "--listen")?;
            set_once(
                &mut fields.listen,
                "--listen",
                parse_on_off("--listen", value)?,
            )
        }
        other => Err(unexpected(other)),
    }
}

fn finish_direction(scope: &str, fields: DirectionFields) -> Result<ParsedLine, String> {
    let rule = Rule {
        outputs: None,
        r#match: fields.condition.finish()?,
        source_kind: fields.source.flatten(),
        send: fields.send,
        listen: fields.listen,
    };
    rule.validate()?;
    match scope {
        "room" => Ok(parsed_command(Command::EditRoomRule { rule })),
        "global" => Ok(parsed_partial(PartialAction::GlobalRuleEdit { rule })),
        other => Err(format!("Unknown rule command: {other}.")),
    }
}

fn parse_mix(tokens: &[String]) -> Result<ParsedLine, String> {
    let Some(head) = tokens.first() else {
        return Err("The mix command is missing.".to_owned());
    };
    match head.as_str() {
        "room" => parse_room_mix(&tokens[1..]),
        "local" => parse_local_mix(&tokens[1..]),
        other => Err(format!("Unknown mix command: {other}.")),
    }
}

#[derive(Default)]
struct MixFields {
    condition: MatchBuilder,
    source: Option<SourceKind>,
    playback: MixPatch,
}

fn parse_room_mix(tokens: &[String]) -> Result<ParsedLine, String> {
    let mut fields = MixFields::default();
    walk_flags(tokens, |token, tokens, index| {
        apply_mix_flag(&mut fields, token, tokens, index)
    })?;
    finish_mix(fields)
}

fn apply_mix_flag(
    fields: &mut MixFields,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    if apply_match_token(&mut fields.condition, token, tokens, index)? {
        return Ok(());
    }
    match token {
        "--source" => {
            let value = next_value(tokens, index, "--source")?;
            set_once(&mut fields.source, "--source", parse_source_kind(value)?)
        }
        other => apply_playback_flag(&mut fields.playback, other, tokens, index),
    }
}

fn apply_playback_flag(
    playback: &mut MixPatch,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<(), String> {
    match token {
        "--gain-db" => {
            let value = next_value(tokens, index, "--gain-db")?;
            set_once(
                &mut playback.gain_db,
                "--gain-db",
                parse_f32("gain", value)?,
            )
        }
        "--pan" => {
            let value = next_value(tokens, index, "--pan")?;
            set_once(&mut playback.pan, "--pan", parse_f32("pan", value)?)
        }
        "--mute" => {
            let value = next_value(tokens, index, "--mute")?;
            set_once(&mut playback.mute, "--mute", parse_on_off("--mute", value)?)
        }
        "--solo" => {
            let value = next_value(tokens, index, "--solo")?;
            set_once(&mut playback.solo, "--solo", parse_on_off("--solo", value)?)
        }
        other => Err(unexpected(other)),
    }
}

fn finish_mix(fields: MixFields) -> Result<ParsedLine, String> {
    let condition = fields.condition.finish()?;
    let source_kind = fields.source.ok_or_else(|| required("--source"))?;
    let playback = fields.playback;
    playback.validate("room source mix")?;
    Ok(parsed_command(Command::SetRoomSourceMix {
        condition,
        source_kind,
        playback,
    }))
}

fn parse_local_mix(tokens: &[String]) -> Result<ParsedLine, String> {
    let source_id = parse_source_id(tokens.first().ok_or("The source id is missing.")?)?;
    let mut playback = MixPatch::default();
    walk_flags(&tokens[1..], |token, tokens, index| {
        apply_playback_flag(&mut playback, token, tokens, index)
    })?;
    playback.validate("local source mix")?;
    Ok(parsed_partial(PartialAction::LocalMix {
        source_id,
        playback,
    }))
}

fn parse_master(tokens: &[String]) -> Result<ParsedLine, String> {
    let mut gain_db = None;
    let mut limiter_enabled = None;
    walk_flags(tokens, |token, tokens, index| match token {
        "--gain-db" => {
            let value = next_value(tokens, index, token)?;
            set_once(&mut gain_db, token, parse_f32("gain", value)?)
        }
        "--limiter" => {
            let value = next_value(tokens, index, token)?;
            set_once(&mut limiter_enabled, token, parse_on_off(token, value)?)
        }
        other => Err(unexpected(other)),
    })?;
    if gain_db.is_none() && limiter_enabled.is_none() {
        return Err("Supply --gain-db or --limiter.".into());
    }
    let mut playback = super::config::MixIntent::unity();
    playback.gain_db = gain_db.unwrap_or(0.0);
    playback.validate("master")?;
    Ok(parsed_partial(PartialAction::Master {
        gain_db,
        limiter_enabled,
    }))
}

#[derive(Default)]
struct MatchBuilder {
    saw_match: bool,
    all: bool,
    name: Option<String>,
    ip: Option<IpAddr>,
    saw_ip: bool,
}

impl MatchBuilder {
    fn consume_match(&mut self, tokens: &[String], index: &mut usize) -> Result<(), String> {
        if self.saw_match {
            return Err(duplicate("--match"));
        }
        self.saw_match = true;
        let kind = next_value(tokens, index, "--match")?;
        match kind {
            "all" => {
                self.all = true;
                Ok(())
            }
            "name" => {
                let name = next_value(tokens, index, "--match name")?;
                self.set_name(name.to_owned())
            }
            "ip" => {
                let ip = next_value(tokens, index, "--match ip")?;
                self.set_ip(parse_optional_ip(ip)?)
            }
            other => Err(format!(
                "The value for --match must be all, name, or ip, not {other}."
            )),
        }
    }

    fn set_name(&mut self, name: String) -> Result<(), String> {
        if self.name.is_some() {
            return Err(duplicate("--name"));
        }
        self.name = Some(name);
        Ok(())
    }

    fn set_ip(&mut self, ip: Option<IpAddr>) -> Result<(), String> {
        if self.saw_ip {
            return Err(duplicate("--ip"));
        }
        self.saw_ip = true;
        self.ip = ip;
        Ok(())
    }

    fn finish(self) -> Result<RuleMatch, String> {
        if !self.saw_match {
            return Err(required("--match"));
        }
        if self.all && (self.name.is_some() || self.saw_ip) {
            return Err("--match all cannot be combined with a name or an IP address.".to_owned());
        }
        let condition = if self.all {
            RuleMatch::All
        } else {
            RuleMatch::Condition {
                ip: self.ip,
                name: self.name,
            }
        };
        condition.validate()?;
        Ok(condition)
    }
}

fn apply_match_token(
    condition: &mut MatchBuilder,
    token: &str,
    tokens: &[String],
    index: &mut usize,
) -> Result<bool, String> {
    match token {
        "--match" => {
            condition.consume_match(tokens, index)?;
            Ok(true)
        }
        "--name" => {
            let value = next_value(tokens, index, "--name")?;
            condition.set_name(value.to_owned())?;
            Ok(true)
        }
        "--ip" => {
            let value = next_value(tokens, index, "--ip")?;
            condition.set_ip(parse_optional_ip(value)?)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn walk_flags(
    tokens: &[String],
    mut visit: impl FnMut(&str, &[String], &mut usize) -> Result<(), String>,
) -> Result<(), String> {
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        if !token.starts_with("--") {
            return Err(unexpected(token));
        }
        visit(option_name(token)?, tokens, &mut index)?;
    }
    Ok(())
}

fn parse_json_command(line: &str) -> LineOutcome {
    let command = match serde_json::from_str::<Command>(line) {
        Ok(command) => command,
        Err(error) => return recoverable(json_rejection("The command JSON", &error)),
    };
    if !command.is_user_command() {
        return recoverable(
            "This command is internal or belongs to a retired control path. Use the formal room/source/rule commands.",
        );
    }
    if let Some(message) = unit_variant_extra_field(&command, line) {
        return recoverable(message);
    }
    LineOutcome::Parsed(parsed_command(command))
}

/// Internally tagged unit variants ignore unknown fields. Struct variants still
/// use `Command`'s `deny_unknown_fields`.
fn unit_variant_extra_field(command: &Command, line: &str) -> Option<String> {
    if !is_unit_command(command) {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Some(unit_field_rejection());
    };
    let Some(object) = value.as_object() else {
        return Some(unit_field_rejection());
    };
    if object.keys().any(|key| key != "type") {
        Some(unit_field_rejection())
    } else {
        None
    }
}

fn is_unit_command(command: &Command) -> bool {
    matches!(
        command.name(),
        "leave_room"
            | "cancel_room"
            | "reload_room_rules"
            | "start_virtual_return"
            | "stop_virtual_return"
            | "refresh_login_startup"
            | "allow_lan_firewall"
            | "refresh_lan_firewall"
            | "remove_own_lan_firewall"
            | "refresh_rooms"
            | "check_pending_settings"
            | "leave_group"
            | "refresh_devices"
            | "reconnect"
            | "quit"
    )
}

fn unit_field_rejection() -> String {
    "The command JSON was rejected (data) at line 1 column 1.".to_owned()
}

fn global_replace_body(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("rule")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("global-replace")?;
    if rest.is_empty() {
        return Some("");
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

fn parse_global_replace(body: &str) -> LineOutcome {
    if body.is_empty() {
        return recoverable("The global rules JSON is missing.");
    }
    match serde_json::from_str::<GlobalRules>(body) {
        Ok(rules) => match rules.validate() {
            Ok(()) => LineOutcome::Parsed(parsed_command(Command::ReplaceGlobalRules { rules })),
            Err(message) => recoverable(message),
        },
        Err(error) => recoverable(json_rejection("The global rules JSON", &error)),
    }
}

fn json_rejection(label: &str, error: &serde_json::Error) -> String {
    format!(
        "{label} was rejected ({}) at line {} column {}.",
        json_category(error),
        error.line(),
        error.column()
    )
}

fn json_category(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    }
}

fn build_profile(
    name: &str,
    address: Option<IpAddr>,
    port: Option<u16>,
    password: String,
    strict: bool,
) -> Result<RoomProfile, String> {
    let mut profile = RoomProfile::create(name)?;
    profile.room.address = address;
    profile.room.port = port;
    profile.room.password = password;
    profile.room.strict_auto_join = strict;
    profile.validate()?;
    Ok(profile)
}

fn build_target(
    name: String,
    address: Option<IpAddr>,
    port: Option<u16>,
    password: String,
    strict: bool,
) -> Result<RoomTarget, String> {
    let target = RoomTarget {
        name,
        address,
        port,
        password,
        strict_auto_join: strict,
    };
    target.validate()?;
    Ok(target)
}

fn tokenize(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        if let Some(opener) = quote {
            if ch == opener {
                quote = None;
                in_token = true;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                quote = Some(ch);
                in_token = true;
            }
            ch if ch.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            _ => {
                current.push(ch);
                in_token = true;
            }
        }
    }
    if quote.is_some() {
        return Err("A quoted token was not closed.".to_owned());
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}

fn next_value<'a>(tokens: &'a [String], index: &mut usize, flag: &str) -> Result<&'a str, String> {
    // The original option stays in the token list. An explicit assignment
    // transports option-like values without confusing them with the next flag.
    if let Some(previous) = index.checked_sub(1).and_then(|at| tokens.get(at)) {
        if let Some((name, value)) = previous.split_once('=') {
            if name == flag {
                return Ok(value);
            }
        }
    }
    let Some(value) = tokens.get(*index) else {
        return Err(missing_value(flag));
    };
    if value.starts_with("--") && !matches!(flag, "--name" | "--password" | "--match name") {
        return Err(missing_value(flag));
    }
    *index += 1;
    Ok(value.as_str())
}

fn option_name(token: &str) -> Result<&str, String> {
    let Some((name, _)) = token.split_once('=') else {
        return Ok(token);
    };
    if matches!(
        name,
        "--json"
            | "--stdin"
            | "--strict"
            | "--manual"
            | "--automatic"
            | "--clear"
            | "--main"
            | "--none"
    ) {
        return Err(format!("The flag {name} does not take a value."));
    }
    Ok(name)
}

fn set_once<T>(slot: &mut Option<T>, flag: &str, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(duplicate(flag));
    }
    *slot = Some(value);
    Ok(())
}

fn set_config(slot: &mut Option<PathBuf>, token: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(unexpected(token));
    }
    *slot = Some(path_token("configuration path", token)?);
    Ok(())
}

fn path_token(flag: &str, token: &str) -> Result<PathBuf, String> {
    if token.is_empty() {
        return Err(format!("The path for {flag} is empty."));
    }
    Ok(PathBuf::from(token))
}

fn parse_seconds(token: &str) -> Result<u64, String> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(
            "The run duration must be a positive whole number of seconds that fits in u64."
                .to_owned(),
        );
    }
    let seconds = token.parse::<u64>().map_err(|_| {
        "The run duration must be a positive whole number of seconds that fits in u64.".to_owned()
    })?;
    if seconds == 0 {
        return Err(
            "The run duration must be a positive whole number of seconds that fits in u64."
                .to_owned(),
        );
    }
    Ok(seconds)
}

fn parse_u64_decimal(label: &str, token: &str) -> Result<u64, String> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "The {label} must be a whole decimal number that fits in u64."
        ));
    }
    token
        .parse::<u64>()
        .map_err(|_| format!("The {label} must be a whole decimal number that fits in u64."))
}

fn parse_port(token: &str) -> Result<u16, String> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("The port must be a whole number from 0 to 65535.".to_owned());
    }
    token
        .parse::<u16>()
        .map_err(|_| "The port must be a whole number from 0 to 65535.".to_owned())
}

fn parse_optional_ip(token: &str) -> Result<Option<IpAddr>, String> {
    if token == "null" {
        return Ok(None);
    }
    let address = token
        .parse::<IpAddr>()
        .map_err(|_| format!("The address {token} is not an IP address."))?;
    Ok(Some(address))
}

fn parse_socket(kind: &str, token: &str) -> Result<SocketAddr, String> {
    token
        .parse::<SocketAddr>()
        .map_err(|_| format!("The {kind} {token} is not a socket address such as 127.0.0.1:47000."))
}

fn parse_room_id(token: &str) -> Result<RoomId, String> {
    RoomId::from_str(token).map_err(|error| format!("The room instance id was rejected: {error}"))
}

fn parse_source_id(token: &str) -> Result<SourceId, String> {
    SourceId::from_str(token).map_err(|error| format!("The source id was rejected: {error}"))
}

fn parse_on_off(flag: &str, token: &str) -> Result<bool, String> {
    match token {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("The value for {flag} must be on or off.")),
    }
}

fn parse_source_kind(token: &str) -> Result<SourceKind, String> {
    match token {
        "system" => Ok(SourceKind::System),
        "microphone" => Ok(SourceKind::Microphone),
        _ => Err("The source kind must be system or microphone.".to_owned()),
    }
}

fn parse_f32(label: &str, token: &str) -> Result<f32, String> {
    token
        .parse::<f32>()
        .map_err(|_| format!("The {label} {token} is not a number."))
}

fn duplicate(flag: &str) -> String {
    format!("The argument {flag} was supplied more than once.")
}

fn required(flag: &str) -> String {
    format!("The argument {flag} is required.")
}

fn missing_value(flag: &str) -> String {
    format!("The value for {flag} is missing.")
}

fn unexpected(arg: &str) -> String {
    format!("Unknown or unexpected argument: {arg}.")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::net::IpAddr;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use rlb_model::{RoomId, SourceId};
    use serde_json::Value;

    use super::super::command::Command;
    use super::super::config::SourceKind;
    use super::super::output_routing::OutputTarget;
    use super::super::rooms::{GlobalRules, JoinMode, MixPatch, Rule, RuleMatch, SCHEMA};
    use super::{
        ConsoleFrame, ERROR_SCOPE_COMMAND, ERROR_SCOPE_LINE, EXIT_RUNTIME_FAILURE,
        EXIT_STARTUP_USAGE, EXIT_SUCCESS, HELP, Inquiry, LineOutcome, PartialAction, ProcessClass,
        RunRoomIntent, StartupRequest,
    };
    use super::{
        classify_startup, exit_code, format_error_line, format_result_line, format_snapshot_line,
        help_text, parse_line, parse_startup_args, parse_startup_line, present_diagnostic,
        present_help, recoverable_error_line, startup_failure_frame,
    };

    const ID: &str = "000102030405060708090a0b0c0d0e0f";
    const SECRET: &str = "pw-token-7f3a9c2e-not-for-logs";
    const MISSING_DIR: &str = r"C:\no\such\rustlanbus-c21a";

    #[test]
    fn output_commands_keep_local_monitor_and_remote_consent_independent() {
        assert_eq!(
            expect_command(&format!(
                "outputs local {ID} --main --endpoint 'headphones' --endpoint 'virtual cable'"
            )),
            Command::SetLocalOutputs {
                source_id: SourceId::from_str(ID).unwrap(),
                targets: vec![
                    OutputTarget::Main,
                    OutputTarget::Endpoint("headphones".into()),
                    OutputTarget::Endpoint("virtual cable".into()),
                ],
            }
        );
        assert_eq!(
            expect_command(&format!("outputs local {ID} --none")),
            Command::SetLocalOutputs {
                source_id: SourceId::from_str(ID).unwrap(),
                targets: vec![],
            }
        );
        let Command::EditRoomRule { rule } = expect_command(
            "outputs room --match name Ada --ip 192.0.2.31 --source microphone --endpoint=--cable",
        ) else {
            panic!("expected a room assignment");
        };
        assert_eq!(
            rule.r#match,
            RuleMatch::Condition {
                name: Some("Ada".into()),
                ip: Some("192.0.2.31".parse().unwrap()),
            }
        );
        assert_eq!(rule.source_kind, Some(SourceKind::Microphone));
        assert_eq!(rule.send, None);
        assert_eq!(rule.listen, None);
        assert_eq!(
            rule.outputs,
            Some(vec![OutputTarget::Endpoint("--cable".into())])
        );
        let PartialAction::GlobalRuleEdit { rule } =
            expect_partial("outputs global --match all --source all --none")
        else {
            panic!("expected one global assignment");
        };
        assert_eq!(rule.r#match, RuleMatch::All);
        assert_eq!(rule.source_kind, None);
        assert_eq!(rule.send, None);
        assert_eq!(rule.listen, None);
        assert_eq!(rule.outputs, Some(vec![]));
    }

    #[test]
    fn output_commands_require_explicit_unique_valid_destinations() {
        for line in [
            format!("outputs local {ID}"),
            format!("outputs local {ID} --main --main"),
            format!("outputs local {ID} --endpoint x --endpoint x"),
            format!("outputs local {ID} --none --main"),
            format!("outputs local {ID} --endpoint x --none"),
            format!("outputs local {ID} --none --none"),
            format!("outputs local {ID} --endpoint ''"),
            format!("outputs local {ID} --main=true"),
            format!("outputs local {ID} --none=false"),
            "outputs room --match all".into(),
            "outputs global --source system --main".into(),
            "outputs room --match all --source unknown --main".into(),
            "outputs room --match all --send on --main".into(),
            "outputs global --match all --listen on --none".into(),
        ] {
            expect_recoverable(&line);
        }
    }

    #[test]
    fn output_json_preserves_symbolic_main_and_empty_override() {
        assert_eq!(
            expect_command(&format!(
                r#"{{"type":"set_local_outputs","source_id":"{ID}","targets":["main",{{"endpoint":"headphones"}}]}}"#
            )),
            expect_command(&format!("outputs local {ID} --main --endpoint headphones"))
        );
        let Command::EditRoomRule { rule } = expect_command(
            r#"{"type":"edit_room_rule","rule":{"match":"all","source_kind":"system","outputs":[]}}"#,
        ) else {
            panic!("expected an output-only room assignment");
        };
        rule.validate().unwrap();
        assert_eq!(rule.outputs, Some(vec![]));
        assert_eq!(rule.send, None);
        assert_eq!(rule.listen, None);
    }

    #[track_caller]
    fn expect_command(line: &str) -> Command {
        match parse_line(line) {
            LineOutcome::Parsed(super::ParsedLine::Command(command)) => *command,
            LineOutcome::Parsed(other) => panic!("parsed as {other:?}"),
            LineOutcome::Blank => panic!("line was blank"),
            LineOutcome::Recoverable(error) => {
                let message = error.message;
                panic!("recoverable: {message}");
            }
        }
    }

    #[track_caller]
    fn expect_partial(line: &str) -> PartialAction {
        match parse_line(line) {
            LineOutcome::Parsed(super::ParsedLine::Partial(action)) => *action,
            LineOutcome::Parsed(other) => panic!("parsed as {other:?}"),
            LineOutcome::Blank => panic!("line was blank"),
            LineOutcome::Recoverable(error) => {
                let message = error.message;
                panic!("recoverable: {message}");
            }
        }
    }

    #[track_caller]
    fn expect_inquiry(line: &str) -> Inquiry {
        match parse_line(line) {
            LineOutcome::Parsed(super::ParsedLine::Inquiry(inquiry)) => inquiry,
            LineOutcome::Parsed(other) => panic!("parsed as {other:?}"),
            LineOutcome::Blank => panic!("line was blank"),
            LineOutcome::Recoverable(error) => {
                let message = error.message;
                panic!("recoverable: {message}");
            }
        }
    }

    #[track_caller]
    fn expect_recoverable(line: &str) -> super::RecoverableLine {
        match parse_line(line) {
            LineOutcome::Recoverable(error) => error,
            other => panic!("expected a recoverable line, got {other:?}"),
        }
    }

    #[track_caller]
    fn expect_run(line: &str) -> super::RunRequest {
        match parse_startup_line(line).unwrap() {
            StartupRequest::Run(request) => request,
            other => panic!("expected run, got {other:?}"),
        }
    }

    fn missing(name: &str) -> String {
        format!(r"{MISSING_DIR}\{name}")
    }

    fn source_id() -> SourceId {
        SourceId::from_str(ID).unwrap()
    }

    fn room_id() -> RoomId {
        RoomId::from_str(ID).unwrap()
    }

    fn object_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn help_is_human_text_on_the_human_channel() {
        assert!(serde_json::from_str::<Value>(help_text()).is_err());
        assert!(!help_text().contains("\"kind\""));
        assert!(!help_text().contains(SECRET));
        assert_eq!(present_help(true), ConsoleFrame::Human(HELP.to_owned()));
        assert_eq!(present_help(false), present_help(true));
        match present_help(true) {
            ConsoleFrame::Human(text) => assert_eq!(text, help_text()),
            ConsoleFrame::JsonStdout(text) => panic!("help entered JSON stdout: {text}"),
        }
    }

    #[test]
    fn startup_usage_exit_differs_from_runtime_failure() {
        let missing = parse_startup_line("participant");
        let startup_code = exit_code(classify_startup(&missing));
        let runtime_code = exit_code(ProcessClass::RuntimeFailure);
        assert_eq!(classify_startup(&missing), ProcessClass::StartupUsage);
        assert_eq!(startup_code, EXIT_STARTUP_USAGE);
        assert_eq!(runtime_code, EXIT_RUNTIME_FAILURE);
        assert_eq!(startup_code, 2);
        assert_eq!(runtime_code, 1);
        assert_ne!(startup_code, runtime_code);
        let frame = startup_failure_frame(true, missing.as_ref().unwrap_err());
        assert_eq!(
            frame,
            startup_failure_frame(false, missing.as_ref().unwrap_err())
        );
        match frame {
            ConsoleFrame::Human(text) => {
                assert!(serde_json::from_str::<Value>(&text).is_err());
                assert!(!text.contains(SECRET));
            }
            ConsoleFrame::JsonStdout(text) => panic!("startup entered JSON stdout: {text}"),
        }

        let legal = parse_startup_line("participant inspect C:\\cfg.json");
        let success_code = exit_code(classify_startup(&legal));
        assert_eq!(classify_startup(&legal), ProcessClass::Success);
        assert_eq!(success_code, EXIT_SUCCESS);
        assert_eq!(success_code, 0);
    }

    #[test]
    fn startup_surface_covers_restore_join_create_and_independent_flags() {
        let config = missing("cfg.json");
        assert!(!Path::new(&config).exists());
        let saved = expect_run(&format!(
            "participant run \"{config}\" --seconds 301 --json"
        ));
        assert_eq!(saved.config.as_os_str(), OsStr::new(&config));
        assert_eq!(saved.seconds, Some(301));
        assert!(saved.json);
        assert!(!saved.stdin);
        assert_eq!(saved.room, RunRoomIntent::SavedRestore);
        assert!(!Path::new(&config).exists());

        let joined = expect_run(&format!(
            "run \"{config}\" --stdin --join \"{}\\card.json\"",
            MISSING_DIR
        ));
        assert!(!joined.json);
        assert!(joined.stdin);
        assert_eq!(joined.seconds, None);
        match joined.room {
            RunRoomIntent::Join { card } => {
                assert_eq!(
                    card.as_os_str(),
                    OsStr::new(&format!(r"{MISSING_DIR}\card.json"))
                );
            }
            other => panic!("{other:?}"),
        }

        let created = expect_run(&format!(
            "participant run \"{config}\" --create \"{}\\card.json\" --listen 0.0.0.0:47000 --json --stdin",
            MISSING_DIR
        ));
        assert!(created.json);
        assert!(created.stdin);
        match created.room {
            RunRoomIntent::Create { card, listen } => {
                assert_eq!(
                    card.as_os_str(),
                    OsStr::new(&format!(r"{MISSING_DIR}\card.json"))
                );
                assert_eq!(listen, "0.0.0.0:47000".parse().unwrap());
            }
            other => panic!("{other:?}"),
        }
        assert!(!Path::new(&config).exists());
    }

    #[test]
    fn json_and_stdin_flags_are_independent() {
        let config = missing("flags.json");
        let cases = [
            (format!("participant run \"{config}\""), false, false),
            (format!("participant run \"{config}\" --json"), true, false),
            (format!("participant run \"{config}\" --stdin"), false, true),
            (
                format!("participant run \"{config}\" --stdin --json"),
                true,
                true,
            ),
        ];
        for (line, json, stdin) in cases {
            let run = expect_run(&line);
            assert_eq!(run.json, json);
            assert_eq!(run.stdin, stdin);
            assert_eq!(run.room, RunRoomIntent::SavedRestore);
        }
    }

    #[test]
    fn seconds_301_is_legal_and_bad_durations_are_rejected() {
        let config = missing("seconds.json");
        assert_eq!(
            expect_run(&format!("participant run \"{config}\" --seconds 1")).seconds,
            Some(1)
        );
        assert_eq!(
            expect_run(&format!("participant run \"{config}\" --seconds 300")).seconds,
            Some(300)
        );
        assert_eq!(
            expect_run(&format!("participant run \"{config}\" --seconds 301")).seconds,
            Some(301)
        );
        assert_eq!(
            expect_run(&format!(
                "participant run \"{config}\" --seconds {}",
                u64::MAX
            ))
            .seconds,
            Some(u64::MAX)
        );

        for bad in [
            "0",
            "-5",
            "+8",
            "10s",
            "1.5",
            "18446744073709551616",
            "999999999999999999999",
        ] {
            let error =
                parse_startup_line(&format!("participant run \"{config}\" --seconds {bad}"))
                    .unwrap_err();
            assert_eq!(error.process_class(), ProcessClass::StartupUsage);
            let message = error.message.as_str();
            assert!(message.contains("positive whole number"), "{message}");
        }
        let missing_value =
            parse_startup_line(&format!("participant run \"{config}\" --seconds")).unwrap_err();
        assert!(
            missing_value
                .message
                .contains("The value for --seconds is missing.")
        );
    }

    #[test]
    fn startup_flags_reject_unknown_duplicate_missing_and_mutex_forms() {
        let config = missing("bad-flags.json");
        let unknown =
            parse_startup_line(&format!("participant run \"{config}\" --nope")).unwrap_err();
        assert!(
            unknown
                .message
                .contains("Unknown or unexpected argument: --nope.")
        );
        assert_eq!(exit_code(unknown.process_class()), EXIT_STARTUP_USAGE);

        let duplicate =
            parse_startup_line(&format!("participant run \"{config}\" --json --json")).unwrap_err();
        assert!(duplicate.message.contains("more than once"));

        let mutex = parse_startup_line(&format!(
            "participant run \"{config}\" --join a.json --create b.json --listen 127.0.0.1:1"
        ))
        .unwrap_err();
        assert!(mutex.message.contains("mutually exclusive"));

        let missing_listen =
            parse_startup_line(&format!("participant run \"{config}\" --create a.json"))
                .unwrap_err();
        assert!(
            missing_listen
                .message
                .contains("--create requires --listen.")
        );

        let listen_alone = parse_startup_line(&format!(
            "participant run \"{config}\" --listen 127.0.0.1:1"
        ))
        .unwrap_err();
        assert!(listen_alone.message.contains("--listen requires --create."));

        let unclosed = parse_startup_line("participant run \"C:\\nope --json").unwrap_err();
        assert!(unclosed.message.contains("quoted"));
        assert_eq!(classify_startup(&Err(unclosed)), ProcessClass::StartupUsage);
    }

    #[test]
    fn init_and_inspect_validate_explicit_input_without_touching_files() {
        let config = missing("participant.json");
        assert!(!Path::new(&config).exists());
        let line = format!("participant init \"{config}\" --name Ada --bind 127.0.0.1:47000");
        match parse_startup_line(&line).unwrap() {
            StartupRequest::Init(init) => {
                assert_eq!(init.config.as_os_str(), OsStr::new(&config));
                assert_eq!(init.name, "Ada");
                assert_eq!(init.bind, "127.0.0.1:47000".parse().unwrap());
            }
            other => panic!("{other:?}"),
        }
        assert!(!Path::new(&config).exists());

        let args = vec![
            "init".to_owned(),
            config.clone(),
            "--bind".to_owned(),
            "127.0.0.1:47000".to_owned(),
            "--name".to_owned(),
            "Ada".to_owned(),
        ];
        assert!(matches!(
            parse_startup_args(&args).unwrap(),
            StartupRequest::Init(_)
        ));

        let missing_bind =
            parse_startup_line(&format!("participant init \"{config}\" --name Ada")).unwrap_err();
        assert!(missing_bind.message.contains("--bind"));
        let bad_bind = parse_startup_line(&format!(
            "participant init \"{config}\" --name Ada --bind 127.0.0.1"
        ))
        .unwrap_err();
        assert!(bad_bind.message.contains("socket address"));
        let bad_name = parse_startup_line(&format!(
            "participant init \"{config}\" --name \"   \" --bind 127.0.0.1:1"
        ))
        .unwrap_err();
        assert!(bad_name.message.contains("display name"));
        assert!(!Path::new(&config).exists());

        match parse_startup_line(&format!("inspect \"{config}\"")).unwrap() {
            StartupRequest::Inspect(inspect) => {
                assert_eq!(inspect.config.as_os_str(), OsStr::new(&config));
            }
            other => panic!("{other:?}"),
        }
        let extra =
            parse_startup_line(&format!("participant inspect \"{config}\" --json")).unwrap_err();
        assert!(
            extra
                .message
                .contains("This command accepts only the configuration path.")
        );
        let absent = parse_startup_line("participant inspect").unwrap_err();
        assert!(absent.message.contains("configuration path is missing"));
    }

    #[test]
    fn offline_cards_accepts_only_a_path_without_touching_files() {
        let config = missing("saved rooms.json");
        assert!(!Path::new(&config).exists());
        match parse_startup_line(&format!("participant cards \"{config}\"")).unwrap() {
            StartupRequest::Cards(request) => {
                assert_eq!(request.config.as_os_str(), OsStr::new(&config));
            }
            other => panic!("{other:?}"),
        }
        let args = vec!["cards".to_owned(), config.clone()];
        assert!(matches!(
            parse_startup_args(&args),
            Ok(StartupRequest::Cards(request)) if request.config == Path::new(&config)
        ));
        assert!(
            parse_startup_line("participant cards")
                .unwrap_err()
                .message
                .contains("configuration path is missing")
        );
        for extra in ["--json", "extra.json"] {
            assert!(
                parse_startup_line(&format!("cards \"{config}\" {extra}"))
                    .unwrap_err()
                    .message
                    .contains("accepts only the configuration path")
            );
        }
        assert!(!Path::new(&config).exists());
    }

    #[test]
    fn quoted_unicode_and_literal_backslashes_stay_intact() {
        let line =
            r#"participant run "C:\配置\引擎.json" --join "C:\卡片\my card.json" --seconds 301"#;
        let run = expect_run(line);
        assert_eq!(run.config.as_os_str(), OsStr::new(r"C:\配置\引擎.json"));
        match run.room {
            RunRoomIntent::Join { card } => {
                assert_eq!(card.as_os_str(), OsStr::new(r"C:\卡片\my card.json"));
            }
            other => panic!("{other:?}"),
        }

        let joined = expect_command(r#"room join --card "C:\new\tab\\share\card.json" --manual"#);
        match joined {
            Command::JoinSavedRoom { profile_path, mode } => {
                assert_eq!(
                    profile_path.as_os_str(),
                    OsStr::new(r"C:\new\tab\\share\card.json")
                );
                assert_eq!(mode, JoinMode::Manual);
            }
            other => panic!("{other:?}"),
        }

        let quoted = expect_command(r#"room join --card 'C:\My "Card".json' --automatic"#);
        match quoted {
            Command::JoinSavedRoom { profile_path, mode } => {
                assert_eq!(profile_path.as_os_str(), OsStr::new("C:\\My \"Card\".json"));
                assert_eq!(mode, JoinMode::Automatic);
            }
            other => panic!("{other:?}"),
        }

        let args = vec![
            "participant".to_owned(),
            "run".to_owned(),
            r"C:\配置\a b.json".to_owned(),
            "--join".to_owned(),
            r"C:\卡片\x y.json".to_owned(),
        ];
        match parse_startup_args(&args).unwrap() {
            StartupRequest::Run(request) => {
                assert_eq!(request.config.as_os_str(), OsStr::new(r"C:\配置\a b.json"));
                match request.room {
                    RunRoomIntent::Join { card } => {
                        assert_eq!(card.as_os_str(), OsStr::new(r"C:\卡片\x y.json"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inquiries_and_simple_commands_map_without_starting_work() {
        assert_eq!(expect_inquiry("status"), Inquiry::Status);
        assert_eq!(expect_inquiry("  cards  "), Inquiry::Cards);
        assert!(matches!(expect_command("refresh"), Command::RefreshRooms));
        assert!(matches!(expect_command("reconnect"), Command::Reconnect));
        assert!(matches!(expect_command("quit"), Command::Quit));
        assert!(matches!(expect_command("room leave"), Command::LeaveRoom));
        assert!(matches!(expect_command("room cancel"), Command::CancelRoom));
        assert!(matches!(
            expect_command("room reload"),
            Command::ReloadRoomRules
        ));
        assert!(matches!(
            expect_command("device refresh"),
            Command::RefreshDevices
        ));
        assert_eq!(parse_line(" \t"), LineOutcome::Blank);
        assert!(matches!(
            parse_line("status"),
            LineOutcome::Parsed(super::ParsedLine::Inquiry(Inquiry::Status))
        ));
        let startup_line = expect_recoverable("participant run cfg.json");
        assert!(startup_line.message.contains("Unknown command"));
        assert!(!startup_line.requires_exit());
    }

    #[test]
    fn room_create_with_listen_maps_to_create_room() {
        let password = format!("{SECRET} tail");
        let line = format!(
            "room create --name 耳机房 --listen 0.0.0.0:47000 --address 192.0.2.10 --port 47111 --password \"{password}\" --strict"
        );
        match expect_command(&line) {
            Command::CreateRoom { profile, bind } => {
                assert_eq!(bind, "0.0.0.0:47000".parse().unwrap());
                assert_eq!(profile.schema, SCHEMA);
                assert_eq!(profile.room.name, "耳机房");
                assert_eq!(profile.room.address, Some(ip("192.0.2.10")));
                assert_eq!(profile.room.port, Some(47111));
                assert_eq!(profile.room.password, password);
                assert!(profile.room.strict_auto_join);
                assert!(profile.rules.is_empty());
                assert!(profile.playback.is_empty());
                let debug = format!("{profile:?}");
                assert!(debug.contains("***"));
                assert!(!debug.contains(SECRET));
            }
            other => panic!("{other:?}"),
        }

        match expect_command("room create --name Ada --listen 127.0.0.1:9 --address null") {
            Command::CreateRoom { profile, .. } => {
                assert_eq!(profile.room.address, None);
                assert_eq!(profile.room.port, None);
                assert_eq!(profile.room.password, "");
                assert!(!profile.room.strict_auto_join);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn room_create_without_listen_stays_pending_and_does_not_invent_a_bind() {
        match expect_partial("room create --name Ada --port 0") {
            PartialAction::CreateRoomPendingBind { profile } => {
                assert_eq!(profile.room.name, "Ada");
                assert_eq!(profile.room.address, None);
                assert_eq!(profile.room.port, Some(0));
                let debug = format!("{profile:?}");
                assert!(!debug.contains("0.0.0.0:0"));
            }
            other => panic!("{other:?}"),
        }
        let error = expect_recoverable(&format!(
            "room create --name Ada --address 0.0.0.0 --password \"{SECRET}\""
        ));
        assert!(error.message.contains("individual device address"));
        assert!(!error.message.contains(SECRET));
        assert!(!error.requires_exit());
    }

    #[test]
    fn saved_cards_parse_without_reading_them() {
        let card = missing("missing-卡片.json");
        assert!(!Path::new(&card).exists());
        let join = expect_command(&format!("room join --card \"{card}\" --manual"));
        match join {
            Command::JoinSavedRoom { profile_path, mode } => {
                assert_eq!(profile_path.as_os_str(), OsStr::new(&card));
                assert_eq!(mode, JoinMode::Manual);
                assert!(!format!("{profile_path:?}").contains("NotFound"));
            }
            other => panic!("{other:?}"),
        }
        let created = expect_command(&format!(
            "room create-saved --card \"{card}\" --listen 192.0.2.8:47101"
        ));
        match created {
            Command::CreateSavedRoom { profile_path, bind } => {
                assert_eq!(profile_path.as_os_str(), OsStr::new(&card));
                assert_eq!(bind, "192.0.2.8:47101".parse().unwrap());
            }
            other => panic!("{other:?}"),
        }
        assert!(!Path::new(&card).exists());
        let parsed = parse_line(&format!("room join --card \"{card}\" --automatic"));
        match parsed {
            LineOutcome::Parsed(super::ParsedLine::Command(command)) => assert!(matches!(
                *command,
                Command::JoinSavedRoom {
                    mode: JoinMode::Automatic,
                    ..
                }
            )),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn join_unknown_keeps_an_omitted_address_and_is_not_create_room() {
        match expect_command("room join-unknown --name Ada") {
            Command::JoinUnknownRoom { target } => {
                assert_eq!(target.name, "Ada");
                assert_eq!(target.address, None);
                assert_eq!(target.port, None);
                assert_eq!(target.password, "");
                assert!(!target.strict_auto_join);
            }
            other => panic!("{other:?}"),
        }
        match expect_command(
            "room join-unknown --name Ada --address null --port 47111 --strict --password secret-value",
        ) {
            Command::JoinUnknownRoom { target } => {
                assert_eq!(target.address, None);
                assert_eq!(target.port, Some(47111));
                assert!(target.strict_auto_join);
                assert_eq!(target.password, "secret-value");
                let debug = format!("{target:?}");
                assert!(debug.contains("***"));
                assert!(!debug.contains("secret-value"));
            }
            other => panic!("{other:?}"),
        }
        match expect_command("room join-unknown --name Ada --address 2001:db8::10") {
            Command::JoinUnknownRoom { target } => {
                assert_eq!(target.address, Some(ip("2001:db8::10")));
            }
            other => panic!("{other:?}"),
        }
        let error = expect_recoverable(&format!(
            "room join-unknown --name Ada --password \"{SECRET}\u{0001}\""
        ));
        assert!(error.message.contains("control characters"));
        assert!(!error.message.contains(SECRET));
        let record: Value = serde_json::from_str(&recoverable_error_line(&error)).unwrap();
        assert_eq!(record["kind"], "error");
        assert_eq!(record["scope"], ERROR_SCOPE_LINE);
        assert!(!record["message"].as_str().unwrap().contains(SECRET));
        assert!(!error.requires_exit());
        assert_eq!(error.exit_code(), None);
    }

    #[test]
    fn choose_keeps_the_exact_u64_generation() {
        let line = format!("room choose 9007199254740993 {ID}");
        match expect_command(&line) {
            Command::ChooseRoom {
                generation,
                instance_id,
            } => {
                assert_eq!(generation, 9_007_199_254_740_993);
                assert_ne!(generation, 9_007_199_254_740_992);
                assert_eq!(instance_id, room_id());
            }
            other => panic!("{other:?}"),
        }
        match expect_command(&format!("room choose 0 {ID}")) {
            Command::ChooseRoom { generation, .. } => assert_eq!(generation, 0),
            other => panic!("{other:?}"),
        }
        assert!(
            expect_recoverable("room choose 18446744073709551616 000102030405060708090a0b0c0d0e0f")
                .message
                .contains("choice generation")
        );
        assert!(
            expect_recoverable("room choose -1 000102030405060708090a0b0c0d0e0f")
                .message
                .contains("choice generation")
        );
    }

    #[test]
    fn device_selection_accepts_clear_and_rejects_bad_endpoints() {
        match expect_command(&format!("device source {ID} --clear")) {
            Command::SelectSourceEndpoint {
                source_id,
                endpoint_id,
            } => {
                assert_eq!(source_id, source_id_value());
                assert_eq!(endpoint_id, None);
            }
            other => panic!("{other:?}"),
        }
        let endpoint = r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{guid}";
        match expect_command(&format!("device output --endpoint \"{endpoint}\"")) {
            Command::SelectOutput { endpoint_id } => {
                assert_eq!(endpoint_id.as_deref(), Some(endpoint));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            expect_recoverable(r#"device output --endpoint " bad""#)
                .message
                .contains("endpoint id")
        );
        assert!(
            expect_recoverable(r#"device output --endpoint """#)
                .message
                .contains("endpoint id")
        );
        let overlong = "a".repeat(513);
        assert!(
            expect_recoverable(&format!("device output --endpoint {overlong}"))
                .message
                .contains("endpoint id")
        );
        let exact = "b".repeat(512);
        match expect_command(&format!("device source {ID} --endpoint {exact}")) {
            Command::SelectSourceEndpoint { endpoint_id, .. } => {
                assert_eq!(endpoint_id.as_deref(), Some(exact.as_str()));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            expect_recoverable("device output --endpoint speakers --clear")
                .message
                .contains("mutually exclusive")
        );
        assert!(
            expect_recoverable("device output")
                .message
                .contains("Supply --endpoint or --clear.")
        );
    }

    fn source_id_value() -> SourceId {
        source_id()
    }

    #[test]
    fn one_source_side_leaves_the_other_side_unset() {
        match expect_command(&format!("source {ID} monitor on")) {
            Command::SetLocalSource {
                source_id,
                publish,
                monitor,
            } => {
                assert_eq!(source_id, source_id_value());
                assert_eq!(publish, None);
                assert_ne!(publish, Some(false));
                assert_eq!(monitor, Some(true));
            }
            other => panic!("{other:?}"),
        }
        match expect_command(&format!("source {ID} publish off")) {
            Command::SetLocalSource {
                publish, monitor, ..
            } => {
                assert_eq!(publish, Some(false));
                assert_eq!(monitor, None);
                assert_ne!(monitor, Some(false));
            }
            other => panic!("{other:?}"),
        }
        match expect_command(&format!("source {ID} monitor off publish on")) {
            Command::SetLocalSource {
                source_id,
                publish,
                monitor,
            } => {
                assert_eq!(source_id, source_id_value());
                assert_eq!(publish, Some(true));
                assert_eq!(monitor, Some(false));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            expect_recoverable(&format!("source {ID} publish ON"))
                .message
                .contains("on or off")
        );
        assert!(
            expect_recoverable(&format!("source {ID} publish on publish off"))
                .message
                .contains("more than once")
        );
        assert!(
            expect_recoverable(&format!("source {ID}"))
                .message
                .contains("Supply publish or monitor")
        );
        match expect_command(&format!(
            r#"{{"type":"set_local_source","source_id":"{ID}","monitor":false}}"#
        )) {
            Command::SetLocalSource {
                publish, monitor, ..
            } => {
                assert_eq!(publish, None);
                assert_eq!(monitor, Some(false));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn room_rules_keep_omitted_directions_and_global_edits_stay_partial() {
        match expect_command("rule room --listen on --match all") {
            Command::EditRoomRule { rule } => {
                assert_eq!(rule.r#match, RuleMatch::All);
                assert_eq!(rule.send, None);
                assert_eq!(rule.listen, Some(true));
            }
            other => panic!("{other:?}"),
        }
        match expect_command("rule room --match ip 192.0.2.10 --send off") {
            Command::EditRoomRule { rule } => {
                assert_eq!(
                    rule.r#match,
                    RuleMatch::Condition {
                        ip: Some(ip("192.0.2.10")),
                        name: None,
                    }
                );
                assert_eq!(rule.send, Some(false));
                assert_eq!(rule.listen, None);
            }
            other => panic!("{other:?}"),
        }
        match expect_command(
            "rule room --match name \"Ada Lovelace\" --ip null --send on --listen off",
        ) {
            Command::EditRoomRule { rule } => {
                assert_eq!(
                    rule.r#match,
                    RuleMatch::Condition {
                        ip: None,
                        name: Some("Ada Lovelace".to_owned()),
                    }
                );
                assert_eq!(rule.send, Some(true));
                assert_eq!(rule.listen, Some(false));
            }
            other => panic!("{other:?}"),
        }
        match expect_partial("rule global --match name Ada --listen on") {
            PartialAction::GlobalRuleEdit { rule } => {
                assert_eq!(
                    rule.r#match,
                    RuleMatch::Condition {
                        ip: None,
                        name: Some("Ada".to_owned()),
                    }
                );
                assert_eq!(rule.send, None);
                assert_eq!(rule.listen, Some(true));
            }
            other => panic!("{other:?}"),
        }
        match expect_partial("rule global --match all --send on --listen off") {
            PartialAction::GlobalRuleEdit { rule } => {
                assert_eq!(rule.r#match, RuleMatch::All);
                assert_eq!(rule.send, Some(true));
                assert_eq!(rule.listen, Some(false));
            }
            other => panic!("global edit became {other:?}"),
        }
        assert!(
            expect_recoverable("rule room --match all")
                .message
                .contains("send, listen, or outputs")
        );
        assert!(
            expect_recoverable("rule room --match ip 999.1.2.3 --send on")
                .message
                .contains("not an IP address")
        );
    }

    #[test]
    fn directional_source_scope_reaches_room_and_global_commands() {
        let Command::EditRoomRule { rule } =
            expect_command("rule room --match name Music --source microphone --listen off")
        else {
            panic!("room source rule");
        };
        assert_eq!(rule.source_kind, Some(SourceKind::Microphone));
        assert_eq!(rule.send, None);
        assert_eq!(rule.listen, Some(false));
        let PartialAction::GlobalRuleEdit { rule } =
            expect_partial("rule global --match all --source system --send off")
        else {
            panic!("global source rule");
        };
        assert_eq!(rule.source_kind, Some(SourceKind::System));
        assert_eq!(rule.listen, None);
        let Command::EditRoomRule { rule } =
            expect_command("rule room --match all --source all --listen on")
        else {
            panic!("wildcard rule");
        };
        assert_eq!(rule.source_kind, None);
        assert!(
            expect_recoverable("rule room --match all --source all --source system --listen on")
                .message
                .contains("--source")
        );
        let _ = expect_recoverable("rule room --match all --source unknown --listen on");
    }

    #[test]
    fn global_replace_requires_a_complete_document() {
        let line = r#"rule global-replace {"schema":1,"rules":[{"match":"all","send":true,"listen":false}]}"#;
        match expect_command(line) {
            Command::ReplaceGlobalRules { rules } => {
                assert_eq!(
                    rules,
                    GlobalRules {
                        schema: SCHEMA,
                        rules: vec![Rule {
                            outputs: None,
                            r#match: RuleMatch::All,
                            source_kind: None,
                            send: Some(true),
                            listen: Some(false),
                        }],
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        let bare = expect_recoverable(r#"rule global-replace {"match":"all","send":true}"#);
        assert!(bare.message.contains("rejected"));
        assert!(!bare.requires_exit());
        let extra = expect_recoverable(&format!(
            r#"rule global-replace {{"schema":1,"rules":[],"note":"{SECRET}"}}"#
        ));
        assert!(extra.message.contains("data"));
        assert!(!extra.message.contains(SECRET));
        assert!(
            expect_recoverable(r#"rule global-replace {"schema":2,"rules":[]}"#)
                .message
                .contains("schema")
        );
        assert!(
            expect_recoverable("rule global-replace")
                .message
                .contains("missing")
        );
    }

    #[test]
    fn room_mix_accepts_selected_mix_fields_and_maps_to_room_playback() {
        match expect_command(
            "mix room --source microphone --match all --gain-db -12.5 --pan -0.25 --mute on --solo off",
        ) {
            Command::SetRoomSourceMix {
                condition,
                source_kind,
                playback,
            } => {
                assert_eq!(condition, RuleMatch::All);
                assert_eq!(source_kind, SourceKind::Microphone);
                assert_eq!(
                    playback,
                    MixPatch {
                        gain_db: Some(-12.5),
                        pan: Some(-0.25),
                        mute: Some(true),
                        solo: Some(false),
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            expect_command("mix room --match all --source system --mute off"),
            Command::SetRoomSourceMix {
                playback: MixPatch {
                    gain_db: None,
                    pan: None,
                    mute: Some(false),
                    solo: None
                },
                ..
            }
        ));
        assert!(
            expect_recoverable("mix room --match all --source system")
                .message
                .contains("must change")
        );
        assert!(
            expect_recoverable(
                "mix room --match all --source system --gain-db 24.5 --pan 0 --mute off --solo off"
            )
            .message
            .contains("gain must be a finite value")
        );
        assert!(
            expect_recoverable(
                "mix room --match all --source system --gain-db 0 --pan 2 --mute off --solo off"
            )
            .message
            .contains("pan must be a finite value")
        );
        assert!(
            expect_recoverable("room join-unknown --name Ada --port 70000")
                .message
                .contains("65535")
        );
        assert!(
            expect_recoverable("room join-unknown --name Ada --address 255.255.255.255")
                .message
                .contains("individual device address")
        );
    }

    #[test]
    fn human_lines_reject_unknown_duplicate_missing_and_broken_quotes() {
        assert!(
            expect_recoverable("room leave --now")
                .message
                .contains("Unknown or unexpected argument: --now.")
        );
        assert!(
            expect_recoverable("room join --card a.json --card b.json --manual")
                .message
                .contains("more than once")
        );
        assert!(
            expect_recoverable("room join --manual")
                .message
                .contains("--card")
        );
        assert!(
            expect_recoverable("room join --card a.json --manual --automatic")
                .message
                .contains("mutually exclusive")
        );
        assert!(
            expect_recoverable("room create-saved --card a.json")
                .message
                .contains("--listen")
        );
        let broken = expect_recoverable(r#"room join --card "C:\missing.json --manual"#);
        assert!(broken.message.contains("quoted"));
        assert!(!broken.message.contains("missing.json --manual"));
        assert!(!broken.requires_exit());
        assert_eq!(broken.exit_code(), None);
        let secret_quote = expect_recoverable(&format!(
            "room join-unknown --name Ada --password \"{SECRET}"
        ));
        assert!(secret_quote.message.contains("quoted"));
        assert!(!secret_quote.message.contains(SECRET));
    }

    #[test]
    fn illegal_identities_are_rejected_by_from_str() {
        assert!(
            expect_recoverable("room choose 1 NOTHEX")
                .message
                .contains("room instance id")
        );
        assert!(
            expect_recoverable("room choose 1 00000000000000000000000000000000")
                .message
                .contains("room instance id")
        );
        assert!(
            expect_recoverable("room choose 1 000102030405060708090A0B0C0D0E0F")
                .message
                .contains("room instance id")
        );
        assert!(
            expect_recoverable("room choose 1 00010203-0405-0607-0809-0a0b0c0d0e0f")
                .message
                .contains("room instance id")
        );
        assert!(
            expect_recoverable("device source GGG --clear")
                .message
                .contains("source id")
        );
    }

    #[test]
    fn explicit_option_values_preserve_leading_dashes_and_equals() {
        assert!(
            matches!(expect_command(r#"room join-unknown --name Room --password "--abc""#),
            Command::JoinUnknownRoom { target } if target.password == "--abc")
        );
        let command = expect_command(r#"room join-unknown --name=--Studio --password="--abc=xyz""#);
        match command {
            Command::JoinUnknownRoom { target } => {
                assert_eq!(target.name, "--Studio");
                assert_eq!(target.password, "--abc=xyz");
                assert!(target.address.is_none());
            }
            other => panic!("expected join, got {other:?}"),
        }
        let args = [
            "init",
            r"C:\Room files\config.json",
            "--name=--Device",
            "--bind=0.0.0.0:0",
        ]
        .map(str::to_owned);
        assert!(
            matches!(parse_startup_args(&args), Ok(StartupRequest::Init(request)) if request.name == "--Device")
        );
        assert!(
            matches!(expect_command("device output --endpoint=--render=value"),
            Command::SelectOutput { endpoint_id: Some(value) } if value == "--render=value")
        );
        assert!(
            matches!(expect_command("rule room --match=name --Ada --send=off"),
            Command::EditRoomRule { rule } if rule.r#match == (RuleMatch::Condition { ip: None, name: Some("--Ada".into()) })
                && rule.send == Some(false) && rule.listen.is_none())
        );
        expect_recoverable("room join-unknown --name=A --password=x --password=y");
        expect_recoverable("room join-unknown --name=A --strict=false");
    }

    #[test]
    fn json_lines_accept_commands_and_reject_unknown_fields() {
        assert!(matches!(
            expect_command(r#"{"type":"refresh_rooms"}"#),
            Command::RefreshRooms
        ));
        assert!(matches!(
            expect_command("  { \"type\" : \"quit\" }"),
            Command::Quit
        ));
        assert!(matches!(
            expect_command(r#"{"type":"leave_group"}"#),
            Command::LeaveGroup
        ));
        let choose = format!(
            r#"{{"type":"choose_room","generation":9007199254740993,"instance_id":"{ID}"}}"#
        );
        match expect_command(&choose) {
            Command::ChooseRoom {
                generation,
                instance_id,
            } => {
                assert_eq!(generation, 9_007_199_254_740_993);
                assert_eq!(instance_id, room_id());
            }
            other => panic!("{other:?}"),
        }
        match expect_command(
            r#"{"type":"join_unknown_room","target":{"name":"Ada","address":null}}"#,
        ) {
            Command::JoinUnknownRoom { target } => {
                assert_eq!(target.address, None);
                assert_eq!(target.password, "");
                assert!(!target.strict_auto_join);
            }
            other => panic!("{other:?}"),
        }
        let unknown = expect_recoverable(&format!(r#"{{"type":"quit","note":"{SECRET}"}}"#));
        assert!(unknown.message.contains("rejected"));
        assert!(unknown.message.contains("data"));
        assert!(!unknown.message.contains(SECRET));
        assert!(!unknown.requires_exit());
        assert_eq!(unknown.exit_code(), None);
        for command_type in [
            "leave_room",
            "cancel_room",
            "reload_room_rules",
            "refresh_rooms",
            "leave_group",
            "refresh_devices",
            "reconnect",
            "quit",
        ] {
            expect_command(&format!(r#"{{"type":"{command_type}"}}"#));
            let error =
                expect_recoverable(&format!(r#"{{"type":"{command_type}","note":"{SECRET}"}}"#));
            let message = error.message.as_str();
            assert!(message.contains("rejected"), "{message}");
            assert!(message.contains("data"));
            assert!(!message.contains(SECRET));
            assert!(!message.contains("note"));
            assert!(!error.requires_exit());
            assert_eq!(error.exit_code(), None);
        }
        let struct_extra = expect_recoverable(&format!(
            r#"{{"type":"choose_room","generation":1,"instance_id":"{ID}","note":"{SECRET}"}}"#
        ));
        assert!(struct_extra.message.contains("rejected"));
        assert!(struct_extra.message.contains("data"));
        assert!(!struct_extra.message.contains(SECRET));
        assert!(!struct_extra.requires_exit());
        let missing_monitor = expect_command(&format!(
            r#"{{"type":"set_local_source","source_id":"{ID}","publish":true}}"#
        ));
        assert!(matches!(
            missing_monitor,
            Command::SetLocalSource {
                publish: Some(true),
                monitor: None,
                ..
            }
        ));
        let source_extra = expect_recoverable(&format!(
            r#"{{"type":"set_local_source","source_id":"{ID}","publish":true,"note":"{SECRET}"}}"#
        ));
        assert!(source_extra.message.contains("rejected"));
        assert!(!source_extra.message.contains(SECRET));
        let status = expect_recoverable(r#"{"type":"status"}"#);
        assert!(status.message.contains("rejected"));
        assert!(!status.requires_exit());
        match expect_command(r#"{"type":"join_unknown_room","target":{"name":"Ada"}}"#) {
            Command::JoinUnknownRoom { target } => assert_eq!(target.address, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn json_records_have_exact_shapes_and_diagnostics_stay_human() {
        let state = serde_json::json!({"running": false, "room_phase": "idle"});
        let snapshot = format_snapshot_line(&state).unwrap();
        let snapshot_value: Value = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(snapshot_value["kind"], "snapshot");
        assert_eq!(snapshot_value["state"], state);
        assert_eq!(
            object_keys(&snapshot_value),
            vec!["kind".to_owned(), "state".to_owned()]
        );

        let result = serde_json::json!({"command": "quit", "applied": true});
        let result_line = format_result_line(&result).unwrap();
        let result_value: Value = serde_json::from_str(&result_line).unwrap();
        assert_eq!(result_value["kind"], "result");
        assert_eq!(result_value["result"], result);
        assert_eq!(
            object_keys(&result_value),
            vec!["kind".to_owned(), "result".to_owned()]
        );

        let error_line = format_error_line(ERROR_SCOPE_COMMAND, "say \"hi\"");
        let error_value: Value = serde_json::from_str(&error_line).unwrap();
        assert_eq!(error_value["kind"], "error");
        assert_eq!(error_value["scope"], ERROR_SCOPE_COMMAND);
        assert_eq!(error_value["message"], "say \"hi\"");
        assert_eq!(
            object_keys(&error_value),
            vec!["kind".to_owned(), "message".to_owned(), "scope".to_owned()]
        );
        assert!(!error_line.contains(SECRET));
        assert_ne!(help_text(), snapshot);
        assert_ne!(help_text(), result_line);
        assert_ne!(help_text(), error_line);

        let diagnostic = "usage: see help";
        assert_eq!(
            present_diagnostic(true, diagnostic),
            ConsoleFrame::Human(diagnostic.to_owned())
        );
        assert_eq!(
            present_diagnostic(false, diagnostic),
            present_diagnostic(true, diagnostic)
        );
        assert!(serde_json::from_str::<Value>(diagnostic).is_err());
    }

    #[test]
    fn bad_lines_are_recoverable_and_later_commands_still_parse() {
        for line in [
            "room leave --now",
            r#"{"type":"quit","extra":true}"#,
            "nope",
            "",
        ] {
            if line.is_empty() {
                assert_eq!(parse_line(line), LineOutcome::Blank);
            } else {
                assert!(matches!(parse_line(line), LineOutcome::Recoverable(_)));
            }
        }
        let joined = parse_line(&format!(
            "room join --card \"{}\" --manual",
            missing("never-created.json")
        ));
        match joined {
            LineOutcome::Parsed(parsed) => match parsed {
                super::ParsedLine::Command(command) => {
                    assert!(matches!(*command, Command::JoinSavedRoom { .. }));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn path_values_compare_as_the_original_token_text() {
        let path = missing("unused.json");
        let command = expect_command(&format!("room join --card \"{path}\" --manual"));
        match command {
            Command::JoinSavedRoom { profile_path, .. } => {
                assert_eq!(profile_path, PathBuf::from(&path));
                assert_eq!(profile_path.as_os_str(), OsStr::new(&path));
            }
            other => panic!("{other:?}"),
        }
    }
}
