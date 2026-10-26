//! The application's single UCI configuration and masked public view.

use std::fs;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

pub const CONFIG: &str = "/etc/config/gw_app_rustlanbus";
pub const DEFAULT_PORT: u16 = 47101;
const NAME_BYTES_LIMIT: usize = 250;

// Do not derive Debug: this value includes the saved room password.
#[derive(Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub enabled: bool,
    pub bind: SocketAddr,
    pub return_bind: Option<SocketAddr>,
    pub room_name: String,
    pub password: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: ([0, 0, 0, 0], DEFAULT_PORT).into(),
            // An omitted option must not enable the optional return host.
            return_bind: None,
            room_name: "RustLanBus".into(),
            password: String::new(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), String> {
        validate_socket(self.bind, "TCP bind")?;
        if let Some(bind) = self.return_bind {
            validate_socket(bind, "UDP return bind")?;
        }
        if self.room_name.trim().is_empty()
            || self.room_name.len() > NAME_BYTES_LIMIT
            || self.room_name.chars().count() > 80
            || self.room_name.chars().any(char::is_control)
        {
            return Err(
                "The room name must be nonblank, contain 1 to 80 characters and at most 250 bytes, and contain no control characters."
                    .into(),
            );
        }
        if self.password.chars().count() > 128 || self.password.chars().any(char::is_control) {
            return Err(
                "The password must contain at most 128 characters and no control characters."
                    .into(),
            );
        }
        Ok(())
    }

    pub fn public(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "bind": self.bind.to_string(),
            "return_bind": self.return_bind.map(|bind| bind.to_string()).unwrap_or_default(),
            "room_name": self.room_name,
            "password_set": !self.password.is_empty(),
        })
    }
}

fn validate_socket(address: SocketAddr, label: &str) -> Result<(), String> {
    let ip = address.ip();
    let v4 = match ip {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(ip) => ip.to_ipv4_mapped(),
    };
    if address.port() == 0
        || ip.is_multicast()
        || v4.is_some_and(|ip| ip.is_broadcast() || ip.is_multicast())
    {
        return Err(format!(
            "{label} must be a unicast or wildcard address with a port from 1 to 65535."
        ));
    }
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    pub enabled: bool,
    pub bind: String,
    #[serde(default)]
    pub return_bind: String,
    pub room_name: String,
    pub password_action: PasswordAction,
    #[serde(default)]
    pub password: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasswordAction {
    Keep,
    Replace,
    Clear,
}

pub fn from_input(input: InputConfig, previous: &AppConfig) -> Result<AppConfig, String> {
    let bind = input
        .bind
        .parse()
        .map_err(|_| "TCP bind must be IP:port; bracket IPv6 addresses.")?;
    let return_bind = if input.return_bind.is_empty() {
        None
    } else {
        Some(
            input
                .return_bind
                .parse()
                .map_err(|_| "UDP return bind must be IP:port; bracket IPv6 addresses.")?,
        )
    };
    let password = match input.password_action {
        PasswordAction::Keep if input.password.is_empty() => previous.password.clone(),
        PasswordAction::Replace if !input.password.is_empty() => input.password,
        PasswordAction::Clear if input.password.is_empty() => String::new(),
        _ => return Err("The password action does not match its input.".into()),
    };
    let config = AppConfig {
        enabled: input.enabled,
        bind,
        return_bind,
        room_name: input.room_name,
        password,
    };
    config.validate()?;
    Ok(config)
}

pub fn load() -> Result<AppConfig, String> {
    let text =
        fs::read_to_string(CONFIG).map_err(|error| format!("Could not read {CONFIG}: {error}"))?;
    parse_uci(&text)
}

fn parse_uci(text: &str) -> Result<AppConfig, String> {
    let mut config = AppConfig::default();
    let mut found_main = false;
    for (line_no, line) in text.lines().enumerate() {
        let fields =
            uci_words(line).map_err(|_| format!("Invalid UCI quoting at line {}.", line_no + 1))?;
        if fields.is_empty() {
            continue;
        }
        if fields[0] == "config" {
            if fields.len() != 3 || fields[1] != "rustlanbus" || fields[2] != "main" {
                return Err(format!(
                    "Unsupported configuration section at line {}; the original file was preserved.",
                    line_no + 1
                ));
            }
            if found_main {
                return Err(
                    "The configuration contains more than one rustlanbus main section.".into(),
                );
            }
            found_main = true;
            continue;
        }
        if !found_main || fields.len() != 3 || fields[0] != "option" {
            return Err(format!(
                "Unsupported configuration entry at line {}.",
                line_no + 1
            ));
        }
        match fields[1].as_str() {
            "enabled" => {
                config.enabled = match fields[2].as_str() {
                    "0" => false,
                    "1" => true,
                    _ => return Err("The enabled option must be 0 or 1.".into()),
                };
            }
            "bind" => {
                config.bind = fields[2]
                    .parse()
                    .map_err(|_| "The TCP bind option is invalid.")?
            }
            "return_bind" => {
                config.return_bind = if fields[2].is_empty() {
                    None
                } else {
                    Some(
                        fields[2]
                            .parse()
                            .map_err(|_| "The UDP return bind option is invalid.")?,
                    )
                };
            }
            "room_name" => config.room_name = fields[2].clone(),
            "password" => config.password = fields[2].clone(),
            _ => {
                return Err(format!(
                    "Unsupported configuration option at line {}.",
                    line_no + 1
                ));
            }
        }
    }
    if !found_main {
        return Err("The configuration is missing config rustlanbus 'main'.".into());
    }
    config.validate()?;
    Ok(config)
}

// Accept UCI quoted fragments and escaped characters without evaluating shell text.
fn uci_words(line: &str) -> Result<Vec<String>, ()> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in line.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                word.push(ch);
            }
            continue;
        }
        if ch == '#' {
            break;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    if quote.is_some() || escaped {
        return Err(());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn render(config: &AppConfig) -> Result<String, String> {
    config.validate()?;
    let mut result = "config rustlanbus 'main'\n".to_owned();
    for (key, value) in [
        (
            "enabled",
            if config.enabled {
                "1".into()
            } else {
                "0".into()
            },
        ),
        ("bind", config.bind.to_string()),
        (
            "return_bind",
            config
                .return_bind
                .map(|bind| bind.to_string())
                .unwrap_or_default(),
        ),
        ("room_name", config.room_name.clone()),
        ("password", config.password.clone()),
    ] {
        result.push_str(&format!("\toption {key} {}\n", quoted(&value)));
    }
    Ok(result)
}

pub fn save(config: &AppConfig) -> Result<(), String> {
    atomic_write(render(config)?.as_bytes())
}

/// Restore validated original bytes, including comments and their line endings.
pub fn restore_bytes(bytes: &[u8]) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "The original UCI configuration is not UTF-8.")?;
    parse_uci(text)?;
    atomic_write(bytes)
}

fn atomic_write(bytes: &[u8]) -> Result<(), String> {
    let path = Path::new(CONFIG);
    let parent = path
        .parent()
        .ok_or("The configuration has no parent directory.")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Could not obtain a temporary configuration identity.")?
        .as_nanos();
    let temporary = parent.join(format!(
        ".gw_app_rustlanbus.{}.{stamp}.tmp",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Opening failure never removes a path owned by another writer.
    let mut file = options.open(&temporary).map_err(|error| {
        format!("Could not create a temporary application configuration: {error}")
    })?;
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    format!("Could not protect the temporary configuration: {error}")
                })?;
        }
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("Could not write the temporary configuration: {error}"))?;
        drop(file);
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not replace the application configuration: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
