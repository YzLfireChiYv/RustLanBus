//! Runtime status for the process supervisor.
//!
//! The file names the bound socket, room caption, and run-lifetime instance.
//! It is removed on a normal stop. The password is not a field.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;

use rlb_contact::RoomInfo;

pub fn write(
    path: &Path,
    listen: SocketAddr,
    info: &RoomInfo,
    return_address: Option<SocketAddr>,
) -> Result<(), String> {
    write_runtime(path, listen, info, return_address, &[listen], &[], &[], false, None)
}

pub fn write_runtime(
    path: &Path, listen: SocketAddr, info: &RoomInfo, return_address: Option<SocketAddr>,
    listeners: &[SocketAddr], returns: &[SocketAddr], return_warnings: &[String],
    discovery_active: bool, discovery_error: Option<&str>,
) -> Result<(), String> {
    let process_start_ticks = process_start_ticks()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("The contact status directory could not be created: {error}")
            })?;
        }
    }
    let mut document = serde_json::json!({
        "protocol_version": info.protocol_version,
        "room_name": info.room_name,
        "instance_id": info.instance_id.to_string(),
        "listen": listen.to_string(),
        "port": info.port,
        "pid": process::id(),
        "process_start_ticks": process_start_ticks,
        "listen_addresses": listeners,
        "return_addresses": returns,
        "return_warnings": return_warnings,
        "discovery_active": discovery_active,
        "discovery_error": discovery_error,
    });
    if let Some(address) = return_address {
        document["return_address"] = serde_json::json!(address.to_string());
    }
    let bytes = serde_json::to_vec(&document)
        .map_err(|error| format!("The contact status could not be encoded: {error}"))?;
    let mut temporary_name = path.as_os_str().to_os_string();
    temporary_name.push(".tmp");
    let temporary = PathBuf::from(temporary_name);
    {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("The contact status file could not be created: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("The contact status file could not be written: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("The contact status file could not be written: {error}"))?;
    }
    #[cfg(windows)]
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("The contact status file could not be published: {error}")
    })
}

pub fn remove(path: &Path) {
    let owned = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|value| {
            value.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(process::id()))
                && value
                    .get("process_start_ticks")
                    .and_then(serde_json::Value::as_u64)
                    == process_start_ticks().ok().flatten()
        });
    if owned {
        let _ = fs::remove_file(path);
    }
}

fn process_start_ticks() -> Result<Option<u64>, String> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string("/proc/self/stat")
            .map_err(|error| format!("The contact process identity could not be read: {error}"))?;
        start_ticks_from_stat(&stat)
            .map(Some)
            .ok_or_else(|| "The contact process identity is incomplete.".to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(None)
    }
}

#[cfg(any(target_os = "linux", test))]
fn start_ticks_from_stat(stat: &str) -> Option<u64> {
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_tick_handles_parentheses_in_comm() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "98765";
        assert_eq!(
            start_ticks_from_stat(&format!("42 (name with ) space) {}", fields.join(" "))),
            Some(98765)
        );
        assert_eq!(start_ticks_from_stat("42 (incomplete) S 0"), None);
    }
}
