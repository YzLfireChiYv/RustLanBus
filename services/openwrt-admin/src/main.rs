//! Stateless, application-owned OpenWrt management. CGI supplies authentication
//! and a cross-process flock; no HTTP listener or gateway dependency is added.
mod config;

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use config::{AppConfig, InputConfig};
use serde::Deserialize;
use serde_json::{Value, json};

const BIN: &str = "/opt/gw-apps/rustlanbus/rustlanbus-contactd";
const INIT: &str = "/etc/init.d/gw-app-rustlanbus";
const STATUS: &str = "/opt/gw-apps/rustlanbus/status.json";
const RUNTIME: &str = "/opt/gw-apps/rustlanbus/runtime.json";
const RC: &str = "/etc/rc.d/S97gw-app-rustlanbus";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Save { config: InputConfig, revision: String },
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
}

struct Failure {
    exit: i32,
    code: &'static str,
    message: String,
    restored: Option<bool>,
}

impl Failure {
    fn invalid(message: impl Into<String>) -> Self {
        Self { exit: 2, code: "invalid_request", message: message.into(), restored: None }
    }
    fn unavailable(message: impl Into<String>) -> Self {
        Self { exit: 4, code: "state_unavailable", message: message.into(), restored: None }
    }
}

fn main() {
    let result = run();
    let (exit, value) = match result {
        Ok(value) => (0, value),
        Err(error) => (error.exit, json!({"ok":false,"error":{"code":error.code,"message":error.message},"restored":error.restored})),
    };
    println!("{value}");
    std::process::exit(exit);
}

fn run() -> Result<Value, Failure> {
    if !cfg!(target_os = "linux") || !Path::new(INIT).is_file() {
        return Err(Failure::unavailable("The native RustLanBus application is not installed."));
    }
    match std::env::args().nth(1).as_deref() {
        Some("status") if std::env::args().len() == 2 => view(),
        Some("request") if std::env::args().len() == 2 => {
            let mut input = Vec::new();
            io::stdin().take(4097).read_to_end(&mut input)
                .map_err(|_| Failure::invalid("The request could not be read."))?;
            if input.is_empty() || input.len() > 4096 {
                return Err(Failure::invalid("The request body is outside the supported bound."));
            }
            let request: Request = serde_json::from_slice(&input)
                .map_err(|_| Failure::invalid("The action or configuration fields are invalid."))?;
            apply(request)
        }
        _ => Err(Failure::invalid("Use status or request; requests are JSON on standard input.")),
    }
}

fn revision(bytes: &[u8]) -> String {
    let mut hash = DefaultHasher::new();
    bytes.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

fn command(program: &str, args: &[&str], seconds: u8, temporary: bool) -> Result<Vec<u8>, String> {
    let mut command = Command::new("/usr/bin/timeout");
    command.args(["-k", "1", &seconds.to_string(), program]).args(args)
        .env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    if temporary { command.env("RUSTLANBUS_TEMPORARY_START", "1"); }
    let output = command.output().map_err(|_| "A native application command could not execute.".to_owned())?;
    if !output.status.success() {
        return Err(format!("The native application command failed (exit {}).", output.status.code().unwrap_or(-1)));
    }
    if output.stdout.len() > 65_536 { return Err("The native application response is too large.".into()); }
    Ok(output.stdout)
}

fn init(action: &str) -> Result<(), String> {
    command(INIT, &[action], 3, matches!(action, "start" | "restart")).map(|_| ())
}

#[derive(Clone, Copy)]
struct Process { active: bool, running: bool, pid: Option<u64> }

fn process() -> Result<Process, String> {
    let bytes = command("/bin/ubus", &["-t", "1", "call", "service", "list", "{\"name\":\"gw-app-rustlanbus\"}"], 2, false)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "The native process state is invalid.".to_owned())?;
    if !value.is_object() { return Err("The native process state is unavailable.".into()); }
    let Some(service) = value.get("gw-app-rustlanbus") else { return Ok(Process { active:false, running:false, pid:None }); };
    let instances = service.get("instances").and_then(Value::as_object)
        .ok_or("The application's native instances are unavailable.")?;
    if instances.is_empty() { return Ok(Process { active:false, running:false, pid:None }); }
    let instance = instances.get("main").ok_or("The application has an unknown native instance.")?;
    let running = instance.get("running").and_then(Value::as_bool)
        .ok_or("The application's native running state is unavailable.")?;
    let pid = instance.get("pid").and_then(Value::as_u64);
    if running && pid.is_none() { return Err("The running application has no process identity.".into()); }
    Ok(Process { active:true, running, pid })
}

fn runtime(pid: u64) -> Result<Value, String> {
    let metadata = fs::metadata(STATUS).map_err(|_| "The running application has not published its status.".to_owned())?;
    if metadata.len() > 65_536 { return Err("The application status exceeds its supported bound.".into()); }
    let bytes = fs::read(STATUS).map_err(|_| "The application status could not be read.".to_owned())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "The application status is invalid.".to_owned())?;
    if value["pid"].as_u64() != Some(pid) { return Err("The status belongs to another process.".into()); }
    let actual = fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| "The process identity could not be read.".to_owned())?;
    let expected = fs::canonicalize(BIN).map_err(|_| "The application executable is unavailable.".to_owned())?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).map_err(|_| "The process arguments could not be read.".to_owned())?;
    let args: Vec<_> = cmdline.split(|byte| *byte == 0).collect();
    let has = |flag: &[u8], path: &[u8]| args.windows(2).any(|pair| pair[0] == flag && pair[1] == path);
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| "The process lifetime could not be read.".to_owned())?;
    let ticks = stat.rfind(')').and_then(|end| stat.get(end + 1..))
        .and_then(|tail| tail.split_whitespace().nth(19)).and_then(|n| n.parse::<u64>().ok());
    if actual != expected || !has(b"--config", RUNTIME.as_bytes()) || !has(b"--status", STATUS.as_bytes())
        || ticks.is_none() || ticks != value["process_start_ticks"].as_u64() {
        return Err("The status does not match this application's process lifetime.".into());
    }
    let listen = value["listen"].as_str().and_then(|s| s.parse::<std::net::SocketAddr>().ok())
        .ok_or("The application has no valid contact binding.")?;
    if listen.port() == 0 || value["port"].as_u64() != Some(u64::from(listen.port())) {
        return Err("The contact service has no usable port.".into());
    }
    let mut public = serde_json::Map::new();
    for key in ["protocol_version","room_name","instance_id","listen","port","listen_addresses",
        "return_address","return_addresses","return_warnings","discovery_active","discovery_error"] {
        if let Some(value) = value.get(key) { public.insert(key.into(), value.clone()); }
    }
    Ok(Value::Object(public))
}

fn view() -> Result<Value, Failure> {
    let config = config::load().map_err(Failure::unavailable)?;
    let raw = fs::read(config::CONFIG).map_err(|_| Failure::unavailable("The application configuration could not be read."))?;
    let process = process().map_err(Failure::unavailable)?;
    let (runtime, health_error) = if let Some(pid) = process.pid.filter(|_| process.running) {
        match runtime(pid) { Ok(value) => (Some(value), None), Err(error) => (None, Some(error)) }
    } else { (None, process.active.then(|| "The native service is registered, but its process is not running.".to_owned())) };
    Ok(json!({"ok":true,"schema":1,"id":"rustlanbus","revision":revision(&raw),"configured":config.public(),
        "state":{"binary":Path::new(BIN).is_file(),"active":process.active,"running":process.running,"ready":runtime.is_some(),
        "autostart":config.enabled && Path::new(RC).exists(),"autostart_registered":Path::new(RC).exists(),
        "health_error":health_error},"runtime":runtime}))
}

fn wait_running(expected: bool, require_ready: bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = process()?;
        if !expected && !state.active && !state.running { return Ok(()); }
        if expected && state.active && (!require_ready || state.running && state.pid.is_some_and(|pid| runtime(pid).is_ok())) { return Ok(()); }
        if Instant::now() >= deadline { return Err("The application did not reach the requested state. Refresh its actual status before retrying.".into()); }
        thread::sleep(Duration::from_millis(100));
    }
}

fn apply(request: Request) -> Result<Value, Failure> {
    let previous = config::load().map_err(Failure::unavailable)?;
    let before = fs::read(config::CONFIG).map_err(|_| Failure::unavailable("The original configuration could not be retained."))?;
    let was_running = process().map_err(Failure::unavailable)?.active;
    let was_autostart = Path::new(RC).exists();
    let prepared = if let Request::Save { config: input, revision: expected } = &request {
        if *expected != revision(&before) {
            return Err(Failure { exit:3,code:"configuration_changed",message:"The configuration changed. Reload before saving this draft.".into(),restored:None });
        }
        Some(config::from_input(input.clone(), &previous).map_err(Failure::invalid)?)
    } else { None };
    let mut published = None;
    let mut touched = false;
    let result = (|| -> Result<(), String> {
        match request {
            Request::Save { .. } => {
                let next = prepared.expect("validated save configuration");
                if fs::read(config::CONFIG).ok().as_deref() != Some(before.as_slice()) { return Err("The configuration changed before saving.".into()); }
                if next == previous { return Ok(()); }
                config::save(&next)?;
                touched = true;
                published = Some(fs::read(config::CONFIG).map_err(|_| "The saved configuration cannot be confirmed.")?);
                if next.enabled != previous.enabled { init(if next.enabled { "enable" } else { "disable" })?; }
                let changed = next.bind != previous.bind || next.return_bind != previous.return_bind
                    || next.room_name != previous.room_name || next.password != previous.password;
                if was_running && changed { init("restart")?; wait_running(true, true)?; }
            }
            Request::Start => {
                let current = process()?;
                if current.running && current.pid.is_some_and(|pid| runtime(pid).is_ok()) { return Ok(()); }
                touched = true; init(if current.active { "restart" } else { "start" })?; wait_running(true, true)?;
            }
            Request::Restart => { touched = true; init("restart")?; wait_running(true, true)?; }
            Request::Stop => { touched = true; init("stop")?; wait_running(false, false)?; }
            Request::Enable | Request::Disable => {
                let mut next: AppConfig = previous.clone();
                next.enabled = matches!(request, Request::Enable);
                config::save(&next)?;
                touched = true;
                published = Some(fs::read(config::CONFIG).map_err(|_| "The saved preference cannot be confirmed.")?);
                init(if next.enabled { "enable" } else { "disable" })?;
                if !next.enabled { init("stop")?; wait_running(false, false)?; }
            }
        }
        Ok(())
    })();
    if let Err(message) = result {
        if !touched { return Err(Failure::unavailable(message)); }
        let restored = (|| -> Result<(), String> {
            if let Some(bytes) = &published {
                if fs::read(config::CONFIG).ok().as_deref() != Some(bytes.as_slice()) {
                    return Err("An external configuration change was preserved.".into());
                }
                config::restore_bytes(&before)?;
            }
            init(if was_autostart { "enable" } else { "disable" })?;
            init(if was_running { "restart" } else { "stop" })?;
            wait_running(was_running, false)
        })();
        let restored_ok = restored.is_ok();
        return Err(Failure { exit:5,code:if restored_ok{"operation_failed"}else{"restore_failed"},
            message:match restored { Ok(())=>message,Err(error)=>format!("{message} Restoration also failed: {error}") },restored:Some(restored_ok) });
    }
    view()
}
