//! Command line and room-file configuration.
//!
//! Errors name the missing or invalid field. They do not repeat the password or
//! the raw configuration text.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use rlb_contact::ContactConfig;
use serde::Deserialize;

pub const USAGE: &str = "\
usage: rustlanbus-contactd --bind ADDRESS:PORT --room-name NAME [--password PASSWORD] [--return-bind ADDRESS:PORT] [--status PATH]
       rustlanbus-contactd --config PATH [--bind ADDRESS:PORT] [--room-name NAME] [--password PASSWORD] [--return-bind ADDRESS:PORT] [--status PATH]

ADDRESS:PORT is the socket to bind. Port 0 selects an ephemeral port.
The ordinary contact port is 47101. A password that begins with -- belongs in the configuration file.
--return-bind enables virtual audio return participants on the given UDP socket. It is disabled when omitted.
The JSON configuration accepts an optional return_bind string with the same meaning.
--status writes a runtime JSON file with the bound socket, room name, instance id, and optional return_address. It omits the password and is removed on a normal stop.";

const MAX_CONFIG_BYTES: u64 = 65_536;

#[derive(Debug)]
pub enum Command {
    Help,
    Run {
        config: ContactConfig,
        return_bind: Option<SocketAddr>,
        status: Option<PathBuf>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bind: String,
    room_name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    return_bind: Option<String>,
}

struct Selected {
    bind: Option<SocketAddr>,
    return_bind: Option<SocketAddr>,
    room_name: Option<String>,
    password: Option<String>,
    help: bool,
    config: Option<PathBuf>,
    status: Option<PathBuf>,
}

pub fn load(args: &[String]) -> Result<Command, String> {
    let selected = parse_args(args)?;
    if selected.help {
        return Ok(Command::Help);
    }
    let mut bind = None;
    let mut room_name = None;
    let mut password = String::new();
    let mut return_bind = None;
    if let Some(path) = &selected.config {
        let file = read_file(path)?;
        bind = Some(parse_bind(&file.bind)?);
        room_name = Some(file.room_name);
        password = file.password;
        return_bind = file
            .return_bind
            .as_deref()
            .map(|value| parse_socket(value, "return_bind in the room configuration"))
            .transpose()?;
    }
    if let Some(value) = selected.bind {
        bind = Some(value);
    }
    if let Some(value) = selected.room_name {
        room_name = Some(value);
    }
    if let Some(value) = selected.password {
        password = value;
    }
    if let Some(value) = selected.return_bind {
        return_bind = Some(value);
    }
    let bind = bind.ok_or_else(|| {
        "A bind address is required. Pass --bind ADDRESS:PORT or set bind in the configuration file."
            .to_owned()
    })?;
    let room_name = room_name.ok_or_else(|| {
        "A room name is required. Pass --room-name NAME or set room_name in the configuration file."
            .to_owned()
    })?;
    ContactConfig::try_new(bind, room_name, password)
        .map(|config| Command::Run {
            config,
            return_bind,
            status: selected.status,
        })
        .map_err(|error| error.to_string())
}

fn parse_args(args: &[String]) -> Result<Selected, String> {
    let mut selected = Selected {
        bind: None,
        return_bind: None,
        room_name: None,
        password: None,
        help: false,
        config: None,
        status: None,
    };
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "--help" | "-h" => selected.help = true,
            "--bind" => {
                let value = required_value(args, &mut index, "--bind")?;
                let address = value.parse::<SocketAddr>().map_err(|_| {
                    "The value of --bind must be an ADDRESS:PORT socket.".to_owned()
                })?;
                if selected.bind.replace(address).is_some() {
                    return Err("--bind was provided more than once.".to_owned());
                }
            }
            "--room-name" => {
                let value = required_value(args, &mut index, "--room-name")?;
                if selected.room_name.replace(value).is_some() {
                    return Err("--room-name was provided more than once.".to_owned());
                }
            }
            "--return-bind" => {
                let value = required_value(args, &mut index, "--return-bind")?;
                let address = parse_socket(&value, "--return-bind")?;
                if selected.return_bind.replace(address).is_some() {
                    return Err("--return-bind was provided more than once.".to_owned());
                }
            }
            "--password" => {
                let value = required_value(args, &mut index, "--password")?;
                if selected.password.replace(value).is_some() {
                    return Err("--password was provided more than once.".to_owned());
                }
            }
            "--config" => {
                let value = required_value(args, &mut index, "--config")?;
                if selected.config.replace(PathBuf::from(value)).is_some() {
                    return Err("--config was provided more than once.".to_owned());
                }
            }
            "--status" => {
                let value = required_value(args, &mut index, "--status")?;
                if selected.status.replace(PathBuf::from(value)).is_some() {
                    return Err("--status was provided more than once.".to_owned());
                }
            }
            other => {
                return Err(format!(
                    "Unknown argument {other}. Run with --help for usage."
                ));
            }
        }
        index += 1;
    }
    Ok(selected)
}

fn required_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    let Some(value) = args.get(*index + 1) else {
        return Err(format!("{flag} requires a value."));
    };
    if value.starts_with("--") {
        return Err(format!(
            "{flag} requires a value. A value that begins with -- belongs in the configuration file."
        ));
    }
    *index += 1;
    Ok(value.clone())
}

fn read_file(path: &PathBuf) -> Result<FileConfig, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("The room configuration file could not be read: {error}"))?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err("The room configuration file is larger than 65536 bytes.".to_owned());
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("The room configuration file could not be read: {error}"))?;
    if std::str::from_utf8(&bytes).is_err() {
        return Err("The room configuration file must be UTF-8.".to_owned());
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        "The room configuration file must contain bind, room_name, and optional password and return_bind strings."
            .to_owned()
    })
}

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    value.parse::<SocketAddr>().map_err(|_| {
        "The bind address in the room configuration is not a valid socket address.".to_owned()
    })
}

fn parse_socket(value: &str, field: &str) -> Result<SocketAddr, String> {
    value
        .parse::<SocketAddr>()
        .map_err(|_| format!("The value of {field} must be an ADDRESS:PORT socket."))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rlb_contact::{ContactClient, ContactService, DEFAULT_IO_TIMEOUT, JoinParams};
    use rlb_model::{MemberId, RunId};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(body: &str) -> io::Result<Self> {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let name = format!(
                "rlb-contactd-{}-{}-{}.json",
                std::process::id(),
                nanos,
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            let mut file = fs::File::create(&path)?;
            file.write_all(body.as_bytes())?;
            Ok(Self(path))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn configuration_errors_omit_the_password() {
        let secret = "contact-daemon-secret";
        let file = TempFile::new(&format!(
            "{{\"bind\":\"127.0.0.1:47101\",\"room_name\":\"Headphones\",\"password\":\"{secret}\",\"extra\":true}}"
        ))
        .expect("temp");
        let error =
            load(&["--config".to_owned(), file.0.display().to_string()]).expect_err("invalid file");
        assert!(!error.contains(secret));
        assert!(error.contains("room configuration file"));
    }

    #[test]
    fn omitted_password_is_empty_and_debug_redacts_it() {
        let file =
            TempFile::new("{\"bind\":\"127.0.0.1:9\",\"room_name\":\"Headphones\"}").expect("temp");
        let command = load(&[
            "--config".to_owned(),
            file.0.display().to_string(),
            "--bind".to_owned(),
            "127.0.0.1:0".to_owned(),
        ])
        .expect("load");
        let Command::Run { config, .. } = command else {
            panic!("expected a run configuration");
        };
        let text = format!("{config:?}");
        assert!(text.contains("redacted"));
        assert!(text.contains("Headphones"));
        let service = ContactService::start(config).expect("start");
        let address = service.local_addr();
        assert_ne!(address.port(), 0);
        let mut client = ContactClient::connect(address, DEFAULT_IO_TIMEOUT).expect("connect");
        client
            .join(JoinParams {
                room_name: "Headphones".to_owned(),
                password: String::new(),
                member_id: member(1),
                run_id: run(1),
                display_name: "甲".to_owned(),
                media_port: 9_100,
                media_ip: None,
                media_scope_id: 0,
                media_alternates: Vec::new(),
            })
            .expect("empty password joins");
        service.stop().expect("stop");
    }

    #[test]
    fn help_and_missing_bind_are_explicit() {
        assert!(matches!(
            load(&["--help".to_owned()]).expect("help"),
            Command::Help
        ));
        let error = load(&["--room-name".to_owned(), "Headphones".to_owned()]).expect_err("bind");
        assert!(error.contains("bind"));
        let error = load(&["--password".to_owned(), "--room-name".to_owned()]).expect_err("value");
        assert!(!error.contains("Headphones"));
        let _ = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    }

    #[test]
    fn status_file_records_the_running_instance_and_releases_the_port() {
        let secret = "contact-status-secret";
        let directory = std::env::temp_dir().join(format!(
            "rlb-contact-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("directory");
        let status_path = directory.join("status.json");
        let config = rlb_contact::ContactConfig::try_new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            "Headphones",
            secret,
        )
        .expect("config");
        let service = ContactService::start(config).expect("start");
        let listen = service.local_addr();
        let info = service.room_info();
        crate::status::write(&status_path, listen, &info, None).expect("status");
        let text = fs::read_to_string(&status_path).expect("read");
        assert!(!text.contains(secret));
        assert!(text.contains(&info.instance_id.to_string()));
        assert!(text.contains(&format!("\"port\":{}", info.port)));
        assert!(text.contains(&listen.to_string()));
        service.stop().expect("stop");
        crate::status::remove(&status_path);
        assert!(!status_path.exists());
        let again = ContactService::start(
            rlb_contact::ContactConfig::try_new(listen, "Headphones", secret).expect("config"),
        )
        .expect("the same port can be bound again");
        assert_eq!(again.local_addr(), listen);
        assert_ne!(again.room_info().instance_id, info.instance_id);
        again.stop().expect("stop");
        let _ = fs::remove_dir_all(directory);
    }

    fn member(number: u8) -> MemberId {
        let mut bytes = [0_u8; 16];
        bytes[15] = number;
        MemberId::from_bytes(bytes).expect("member")
    }

    fn run(number: u8) -> RunId {
        let mut bytes = [0_u8; 16];
        bytes[0] = number;
        RunId::from_bytes(bytes).expect("run")
    }
}
