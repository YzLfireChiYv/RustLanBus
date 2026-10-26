//! Current-user identity, member config, and per-run control descriptors.
//!
//! Files live under `%LOCALAPPDATA%\RustLanBus` unless the composition root
//! supplies another directory. Writes use a sibling temporary file, `flush`,
//! then replace. Windows run descriptors are created with a protected DACL
//! before their IPC token is written. A corrupt `identity.json` is left in
//! place so this process never mints a silent replacement identity.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use rand::{TryRng, rngs::SysRng};
use rlb_model::{IdentityRecord, MemberId, RoomId, RunId};
use rlb_registry::{ClaimSecret, DecimalU64, MemberToken};
use rlb_session::{MEMBER_CONFIG_SCHEMA_V2, MEMBER_CONFIG_SCHEMA_VERSION, MemberConfig};
use serde::{Deserialize, Serialize};

use crate::error::EngineError;

/// On-disk schema for [`identity.json`](PersistStore::identity_path).
pub const IDENTITY_SCHEMA: u16 = 2;
/// On-disk schema for the per-run control descriptor.
pub const RUN_DESCRIPTOR_SCHEMA: u16 = 2;

/// Files owned by one engine data root.
#[derive(Clone, Debug)]
pub struct PersistStore {
    root: PathBuf,
}

/// Result of loading the stable identity file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityLoad {
    /// An existing validated record.
    Existing(IdentityRecord),
    /// First start wrote a newly generated record.
    Created(IdentityRecord),
}

/// Result of loading the member configuration file.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigLoad {
    /// An existing validated configuration.
    Existing(MemberConfig),
    /// First start wrote product-safe defaults.
    Created(MemberConfig),
}

/// Loopback control discovery record for one `run_id`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunDescriptor {
    pub schema: u16,
    pub member_id: MemberId,
    pub run_id: RunId,
    pub pid: u32,
    pub process_start_marker: String,
    pub endpoint: ControlEndpointDescriptor,
    pub ipc_token: String,
    pub created_at: String,
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
enum ProcessMarkerState {
    Missing,
    Running(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RunEntryFileType {
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    is_reparse_point: bool,
}

struct ValidatedRunDirectory {
    canonical_path: PathBuf,
    #[cfg(windows)]
    _handle: fs::File,
}

/// Loopback HTTP bind published to the current-user UI host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlEndpointDescriptor {
    pub host: String,
    pub port: u16,
    pub api_root: String,
}

/// Schema for [`registry-credentials.json`](PersistStore::registry_credentials_path).
pub const REGISTRY_CREDENTIAL_SCHEMA: u16 = 1;

/// Durable claim secret and optional member token for one room.
#[derive(Clone, Debug)]
pub struct RegistryCredentials {
    pub room_id: RoomId,
    pub claim_secret: ClaimSecret,
    pub member_token: Option<MemberToken>,
    pub token_generation: Option<DecimalU64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryCredentialFile {
    schema: u16,
    room_id: RoomId,
    claim_secret: ClaimSecret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    member_token: Option<MemberToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_generation: Option<DecimalU64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IdentityFile {
    schema: u16,
    identity: IdentityRecord,
}

impl PersistStore {
    /// Uses `%LOCALAPPDATA%\RustLanBus`, creating the directory if needed.
    pub fn user_default() -> Result<Self, EngineError> {
        let local = std::env::var_os("LOCALAPPDATA").ok_or(EngineError::Persist(
            "LOCALAPPDATA is unset; cannot locate the member data directory",
        ))?;
        Self::open(PathBuf::from(local).join("RustLanBus"))
    }

    /// Uses an explicit data root. Tests pass a temporary directory.
    pub fn open(root: PathBuf) -> Result<Self, EngineError> {
        fs::create_dir_all(root.join("run")).map_err(EngineError::from)?;
        fs::create_dir_all(root.join("logs")).map_err(EngineError::from)?;
        let store = Self { root };
        cleanup_run_directory_with(&store.root.join("run"), process_marker_state)?;
        Ok(store)
    }

    /// Returns the data root.
    #[cfg(test)]
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns `identity.json`.
    #[must_use]
    pub fn identity_path(&self) -> PathBuf {
        self.root.join("identity.json")
    }

    /// Returns `config.json`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Returns `registry-credentials.json`.
    #[must_use]
    pub fn registry_credentials_path(&self) -> PathBuf {
        self.root.join("registry-credentials.json")
    }

    /// Returns the run-descriptor path for one process run.
    #[must_use]
    pub fn run_descriptor_path(&self, member_id: MemberId, run_id: RunId) -> PathBuf {
        self.root
            .join("run")
            .join(format!("{member_id}-{run_id}.json"))
    }

    /// Loads a persisted identity or writes the first record.
    ///
    /// A file that exists but fails schema or identity validation is left
    /// untouched. The caller must keep room and media down.
    pub fn load_or_create_identity(&self) -> Result<IdentityLoad, EngineError> {
        let path = self.identity_path();
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<IdentityFile>(&bytes) {
                Ok(file) if file.schema == IDENTITY_SCHEMA => {
                    Ok(IdentityLoad::Existing(file.identity))
                }
                Ok(_) => Err(EngineError::IdentityCorrupt),
                Err(_) => Err(EngineError::IdentityCorrupt),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let identity = IdentityRecord::new(
                    MemberId::generate()?,
                    rlb_model::SourceId::generate()?,
                    rlb_model::SourceId::generate()?,
                )?;
                self.write_identity(identity)?;
                Ok(IdentityLoad::Created(identity))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Loads member config or writes an unjoined first-start candidate.
    pub fn load_or_create_config(
        &self,
        identity: IdentityRecord,
    ) -> Result<ConfigLoad, EngineError> {
        let path = self.config_path();
        match fs::read(&path) {
            Ok(bytes) => {
                let (config, migrated) = migrate_member_config(&bytes, identity)?;
                if migrated {
                    self.write_config(&config)?;
                }
                Ok(ConfigLoad::Existing(config))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let config = MemberConfig::unjoined(identity);
                self.write_config(&config)?;
                Ok(ConfigLoad::Created(config))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Atomically replaces `identity.json`.
    pub fn write_identity(&self, identity: IdentityRecord) -> Result<(), EngineError> {
        let file = IdentityFile {
            schema: IDENTITY_SCHEMA,
            identity,
        };
        atomic_write(&self.identity_path(), &serde_json::to_vec_pretty(&file)?)
    }

    /// Atomically replaces `config.json`.
    pub fn write_config(&self, config: &MemberConfig) -> Result<(), EngineError> {
        atomic_write(&self.config_path(), &serde_json::to_vec_pretty(config)?)
    }

    /// Loads claim material for `room_id`, or writes a new secret for that room.
    pub fn load_or_create_registry_credentials(
        &self,
        room_id: RoomId,
    ) -> Result<RegistryCredentials, EngineError> {
        let path = self.registry_credentials_path();
        match fs::read(&path) {
            Ok(bytes) => {
                let file: RegistryCredentialFile =
                    serde_json::from_slice(&bytes).map_err(|_| EngineError::ConfigCorrupt)?;
                if file.schema != REGISTRY_CREDENTIAL_SCHEMA || file.room_id != room_id {
                    return Err(EngineError::ConfigCorrupt);
                }
                Ok(RegistryCredentials {
                    room_id: file.room_id,
                    claim_secret: file.claim_secret,
                    member_token: file.member_token,
                    token_generation: file.token_generation,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let credentials = RegistryCredentials {
                    room_id,
                    claim_secret: ClaimSecret::generate().map_err(|_| {
                        EngineError::Persist("operating-system random source is unavailable")
                    })?,
                    member_token: None,
                    token_generation: None,
                };
                self.write_registry_credentials(&credentials)?;
                Ok(credentials)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Atomically replaces `registry-credentials.json`.
    pub fn write_registry_credentials(
        &self,
        credentials: &RegistryCredentials,
    ) -> Result<(), EngineError> {
        let file = RegistryCredentialFile {
            schema: REGISTRY_CREDENTIAL_SCHEMA,
            room_id: credentials.room_id,
            claim_secret: credentials.claim_secret.clone(),
            member_token: credentials.member_token.clone(),
            token_generation: credentials.token_generation,
        };
        atomic_write(
            &self.registry_credentials_path(),
            &serde_json::to_vec_pretty(&file)?,
        )
    }

    /// Atomically writes the current-run control descriptor.
    pub fn write_run_descriptor(&self, descriptor: &RunDescriptor) -> Result<PathBuf, EngineError> {
        if descriptor.schema != RUN_DESCRIPTOR_SCHEMA {
            return Err(EngineError::Persist(
                "run descriptor schema must be 2 before it is written",
            ));
        }
        if descriptor.endpoint.host != "127.0.0.1" {
            return Err(EngineError::Persist(
                "run descriptor host must be exactly 127.0.0.1",
            ));
        }
        let path = self.run_descriptor_path(descriptor.member_id, descriptor.run_id);
        atomic_write_run_descriptor(&path, &serde_json::to_vec_pretty(descriptor)?)?;
        Ok(path)
    }

    /// Removes this process's run descriptor during a normal shutdown.
    pub fn remove_run_descriptor(
        &self,
        member_id: MemberId,
        run_id: RunId,
    ) -> Result<(), EngineError> {
        let path = self.run_descriptor_path(member_id, run_id);
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn cleanup_run_directory_with<F>(
    run_directory: &Path,
    mut process_state: F,
) -> Result<(), EngineError>
where
    F: FnMut(u32) -> ProcessMarkerState,
{
    let validated_directory = validate_run_directory(run_directory)?;
    for entry in fs::read_dir(run_directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let parent_matches = direct_parent_matches(&validated_directory.canonical_path, &path)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !should_process_run_entry(run_entry_file_type(&metadata), parent_matches) {
            continue;
        }
        let Some(bytes) = read_regular_run_entry(&path)? else {
            continue;
        };
        let descriptor = match serde_json::from_slice::<RunDescriptor>(&bytes) {
            Ok(descriptor) => descriptor,
            Err(_) => {
                remove_file_if_present(&path)?;
                continue;
            }
        };
        if run_descriptor_is_stale(
            &descriptor.process_start_marker,
            &process_state(descriptor.pid),
        ) {
            remove_file_if_present(&path)?;
        }
    }
    Ok(())
}

fn validate_run_directory(path: &Path) -> Result<ValidatedRunDirectory, EngineError> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = run_entry_file_type(&metadata);
    if !file_type.is_directory || file_type.is_symlink || file_type.is_reparse_point {
        return Err(EngineError::Persist(
            "run directory must be a direct non-reparse directory",
        ));
    }
    #[cfg(windows)]
    let handle = windows_open_run_directory_no_reparse(path)?;
    let canonical_path = fs::canonicalize(path)?;
    Ok(ValidatedRunDirectory {
        canonical_path,
        #[cfg(windows)]
        _handle: handle,
    })
}

fn direct_parent_matches(canonical_directory: &Path, path: &Path) -> Result<bool, EngineError> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    match fs::canonicalize(parent) {
        Ok(canonical_parent) => Ok(canonical_parent == canonical_directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn run_entry_file_type(metadata: &fs::Metadata) -> RunEntryFileType {
    let file_type = metadata.file_type();
    RunEntryFileType {
        is_file: file_type.is_file(),
        is_directory: file_type.is_dir(),
        is_symlink: file_type.is_symlink(),
        is_reparse_point: {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

                metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            }
            #[cfg(not(windows))]
            {
                false
            }
        },
    }
}

fn should_process_run_entry(file_type: RunEntryFileType, parent_matches: bool) -> bool {
    parent_matches
        && file_type.is_file
        && !file_type.is_directory
        && !file_type.is_symlink
        && !file_type.is_reparse_point
}

#[cfg(not(windows))]
fn read_regular_run_entry(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn read_regular_run_entry(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    use std::io::Read as _;
    use windows::Win32::Foundation::GENERIC_READ;

    let Some(mut opened) = windows_open_path_no_reparse(path, GENERIC_READ.0, true)? else {
        return Ok(None);
    };
    if opened.is_directory() || opened.is_reparse_point() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    opened.file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn run_descriptor_is_stale(expected_marker: &str, process_state: &ProcessMarkerState) -> bool {
    match process_state {
        ProcessMarkerState::Missing => true,
        ProcessMarkerState::Running(actual_marker) => actual_marker != expected_marker,
        ProcessMarkerState::Unknown => false,
    }
}

fn remove_file_if_present(path: &Path) -> Result<(), EngineError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(windows))]
fn process_marker_state(pid: u32) -> ProcessMarkerState {
    if pid == std::process::id() {
        ProcessMarkerState::Running(process_start_marker())
    } else {
        ProcessMarkerState::Unknown
    }
}

#[cfg(windows)]
fn process_marker_state(pid: u32) -> ProcessMarkerState {
    use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => OwnedWindowsHandle(handle),
        Err(error) if windows_error_is(&error, ERROR_INVALID_PARAMETER) => {
            return ProcessMarkerState::Missing;
        }
        Err(_) => return ProcessMarkerState::Unknown,
    };
    let mut exit_code = 0_u32;
    if unsafe { GetExitCodeProcess(handle.0, &mut exit_code) }.is_err() {
        return ProcessMarkerState::Unknown;
    }
    if exit_code != windows::Win32::Foundation::STILL_ACTIVE.0 as u32 {
        return ProcessMarkerState::Missing;
    }
    match windows_process_marker(handle.0) {
        Some(marker) => ProcessMarkerState::Running(marker),
        None => ProcessMarkerState::Unknown,
    }
}

fn migrate_member_config(
    bytes: &[u8],
    identity: IdentityRecord,
) -> Result<(MemberConfig, bool), EngineError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| EngineError::ConfigCorrupt)?;
    let schema = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(EngineError::ConfigCorrupt)?;
    let migrated = match schema {
        v if v == u64::from(MEMBER_CONFIG_SCHEMA_VERSION) => false,
        v if v == u64::from(MEMBER_CONFIG_SCHEMA_V2) => {
            value["schema_version"] = serde_json::json!(MEMBER_CONFIG_SCHEMA_VERSION);
            if let Some(room) = value
                .get_mut("room")
                .and_then(serde_json::Value::as_object_mut)
            {
                let joined = room
                    .get("join")
                    .is_some_and(|join| !join.is_null() && join.get("authority_url").is_some());
                if !joined {
                    room.insert("room_id".to_owned(), serde_json::Value::Null);
                    room.entry("join".to_owned())
                        .or_insert(serde_json::Value::Null);
                }
            }
            true
        }
        _ => return Err(EngineError::ConfigCorrupt),
    };
    let config: MemberConfig =
        serde_json::from_value(value).map_err(|_| EngineError::ConfigCorrupt)?;
    config
        .validate(identity)
        .map_err(|_| EngineError::ConfigCorrupt)?;
    Ok((config, migrated))
}

/// 32 random bytes, unpadded base64url, matching CONTROL §2.2.
pub fn generate_ipc_token() -> Result<String, EngineError> {
    let mut bytes = [0_u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| EngineError::Persist("operating-system CSPRNG was unavailable"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Windows process creation FILETIME ticks, used to reject PID reuse.
pub fn process_start_marker() -> String {
    #[cfg(windows)]
    {
        windows_process_start_marker().unwrap_or_else(|| format!("pid-{}", std::process::id()))
    }
    #[cfg(not(windows))]
    {
        format!("pid-{}", std::process::id())
    }
}

#[cfg(windows)]
fn windows_process_start_marker() -> Option<String> {
    use windows::Win32::System::Threading::GetCurrentProcess;

    windows_process_marker(unsafe { GetCurrentProcess() })
}

#[cfg(windows)]
fn windows_process_marker(handle: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;
    }
    let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    Some(ticks.to_string())
}

/// RFC 3339 UTC timestamp used by descriptors and A snapshots.
pub fn rfc3339_utc_now() -> String {
    rfc3339_utc(std::time::SystemTime::now())
}

pub(crate) fn rfc3339_utc(system: std::time::SystemTime) -> String {
    let duration = system
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day, hour, minute, second) = unix_civil(duration.as_secs());
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    )
}

fn unix_civil(unix_seconds: u64) -> (i32, u32, u32, u32, u32, u32) {
    let second = (unix_seconds % 60) as u32;
    let minute = ((unix_seconds / 60) % 60) as u32;
    let hour = ((unix_seconds / 3_600) % 24) as u32;
    let mut days = (unix_seconds / 86_400) as i64;
    let mut year = 1970_i32;
    loop {
        let length = i64::from(if is_leap(year) { 366 } else { 365 });
        if days >= length {
            days -= length;
            year += 1;
        } else {
            break;
        }
    }
    let month_lengths = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1_u32;
    for length in month_lengths {
        if days >= i64::from(length) {
            days -= i64::from(length);
            month += 1;
        } else {
            break;
        }
    }
    (year, month, days as u32 + 1, hour, minute, second)
}

const fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).map_err(EngineError::from)
}

fn atomic_write_run_descriptor(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    #[cfg(windows)]
    {
        windows_atomic_write_run_descriptor(path, bytes)
    }
    #[cfg(not(windows))]
    {
        atomic_write(path, bytes)
    }
}

#[cfg(windows)]
fn windows_atomic_write_run_descriptor(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let run_directory = path.parent().ok_or(EngineError::Persist(
        "run descriptor path has no parent directory",
    ))?;
    let validated_directory = validate_run_directory(run_directory)?;
    if !direct_parent_matches(&validated_directory.canonical_path, path)? {
        return Err(EngineError::Persist(
            "run descriptor path escaped the run directory",
        ));
    }
    let security = windows_run_descriptor_security()?;
    let temporary_path = windows_run_descriptor_temporary_path(path);
    let result = (|| -> Result<(), EngineError> {
        let mut temporary_file = windows_create_restricted_file(&temporary_path, &security)?;
        temporary_file.write_all(bytes)?;
        temporary_file.flush()?;
        temporary_file.sync_all()?;
        let temporary_identity = windows_file_identity(&temporary_file)?;

        let existing_target = windows_open_and_secure_descriptor(path, &security, false)?;
        drop(existing_target);
        if !direct_parent_matches(&validated_directory.canonical_path, path)? {
            return Err(EngineError::Persist(
                "run descriptor parent changed before replacement",
            ));
        }
        let temporary_wide = windows_path_wide(&temporary_path);
        let destination_wide = windows_path_wide(path);
        unsafe {
            MoveFileExW(
                PCWSTR(temporary_wide.as_ptr()),
                PCWSTR(destination_wide.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(windows_io_error)?;
        let final_target = windows_open_and_secure_descriptor(path, &security, true)?.ok_or(
            EngineError::Persist("run descriptor disappeared after replacement"),
        )?;
        if windows_file_identity(&final_target)? != temporary_identity {
            return Err(EngineError::Persist(
                "run descriptor target changed during replacement",
            ));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary_path);
    }
    result
}

#[cfg(windows)]
fn windows_file_identity(file: &fs::File) -> io::Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(windows_io_error)?;
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, file_index))
}

#[cfg(windows)]
fn windows_run_descriptor_temporary_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .map_or_else(|| "descriptor".into(), |name| name.to_string_lossy());
    path.with_file_name(format!(
        ".{file_name}.{}-{timestamp}-{serial}.tmp",
        std::process::id()
    ))
}

#[cfg(windows)]
fn windows_create_restricted_file(
    path: &Path,
    security: &WindowsSecurityDescriptor,
) -> io::Result<fs::File> {
    use std::mem::size_of;
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    use windows::core::PCWSTR;

    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.descriptor.0,
        bInheritHandle: false.into(),
    };
    let wide = windows_path_wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            Some(&attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(windows_io_error)?;
    Ok(unsafe { fs::File::from_raw_handle(handle.0) })
}

#[cfg(windows)]
fn windows_open_and_secure_descriptor(
    path: &Path,
    security: &WindowsSecurityDescriptor,
    must_exist: bool,
) -> Result<Option<fs::File>, EngineError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetKernelObjectSecurity,
    };
    use windows::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC};

    let desired_access = FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0 | WRITE_DAC.0;
    let Some(opened) = windows_open_path_no_reparse(path, desired_access, true)? else {
        if must_exist {
            return Err(EngineError::Persist("run descriptor target does not exist"));
        }
        return Ok(None);
    };
    if opened.is_directory() || opened.is_reparse_point() {
        return Err(EngineError::Persist(
            "run descriptor target must be a non-reparse regular file",
        ));
    }
    unsafe {
        SetKernelObjectSecurity(
            HANDLE(opened.file.as_raw_handle()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            security.descriptor,
        )
    }
    .map_err(windows_io_error)?;
    Ok(Some(opened.file))
}

#[cfg(windows)]
fn windows_open_run_directory_no_reparse(path: &Path) -> Result<fs::File, EngineError> {
    use windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

    let opened = windows_open_path_no_reparse(path, FILE_READ_ATTRIBUTES.0, false)?
        .ok_or(EngineError::Persist("run directory does not exist"))?;
    if !opened.is_directory() || opened.is_reparse_point() {
        return Err(EngineError::Persist(
            "run directory must be a non-reparse directory",
        ));
    }
    Ok(opened.file)
}

#[cfg(windows)]
fn windows_open_path_no_reparse(
    path: &Path,
    desired_access: u32,
    share_delete: bool,
) -> io::Result<Option<WindowsOpenedPath>> {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileAttributeTagInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    let mut share_mode = FILE_SHARE_READ | FILE_SHARE_WRITE;
    if share_delete {
        share_mode |= FILE_SHARE_DELETE;
    }
    let wide = windows_path_wide(path);
    let handle = match unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            desired_access,
            share_mode,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    } {
        Ok(handle) => handle,
        Err(error)
            if windows_error_is(&error, ERROR_FILE_NOT_FOUND)
                || windows_error_is(&error, ERROR_PATH_NOT_FOUND) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(windows_io_error(error)),
    };
    let file = unsafe { fs::File::from_raw_handle(handle.0) };
    let mut information = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileAttributeTagInfo,
            std::ptr::addr_of_mut!(information).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(windows_io_error)?;
    Ok(Some(WindowsOpenedPath {
        file,
        attributes: information.FileAttributes,
    }))
}

#[cfg(windows)]
struct WindowsOpenedPath {
    file: fs::File,
    attributes: u32,
}

#[cfg(windows)]
impl WindowsOpenedPath {
    fn is_directory(&self) -> bool {
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

        self.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    }

    fn is_reparse_point(&self) -> bool {
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }
}

#[cfg(windows)]
fn windows_run_descriptor_security() -> io::Result<WindowsSecurityDescriptor> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::core::PCWSTR;

    let user_sid = windows_current_user_sid_string()?;
    let sddl = if user_sid == "S-1-5-18" {
        format!("O:{user_sid}D:P(A;;FA;;;SY)")
    } else {
        format!("O:{user_sid}D:P(A;;FA;;;SY)(A;;FA;;;{user_sid})")
    };
    let wide: Vec<u16> = OsStr::new(&sddl)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(windows_io_error)?;
    Ok(WindowsSecurityDescriptor {
        descriptor,
        _allocation: WindowsLocalAllocation(windows::Win32::Foundation::HLOCAL(descriptor.0)),
    })
}

#[cfg(windows)]
fn windows_current_user_sid_string() -> io::Result<String> {
    use std::mem::size_of;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(windows_io_error)?;
    let token = OwnedWindowsHandle(token);
    let mut byte_length = 0_u32;
    if let Err(error) =
        unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut byte_length) }
        && byte_length == 0
    {
        return Err(windows_io_error(error));
    }
    let word_count = (byte_length as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; word_count];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            byte_length,
            &mut byte_length,
        )
    }
    .map_err(windows_io_error)?;
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    windows_sid_string(token_user.User.Sid)
}

#[cfg(windows)]
fn windows_sid_string(sid: windows::Win32::Security::PSID) -> io::Result<String> {
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::core::PWSTR;

    let mut string = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut string) }.map_err(windows_io_error)?;
    let _allocation = WindowsLocalAllocation(HLOCAL(string.0.cast()));
    unsafe { string.to_string() }.map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an invalid SID",
        )
    })
}

#[cfg(windows)]
fn windows_path_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn windows_io_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(windows)]
fn windows_error_is(
    error: &windows::core::Error,
    code: windows::Win32::Foundation::WIN32_ERROR,
) -> bool {
    error.code() == windows::core::HRESULT::from_win32(code.0)
}

#[cfg(windows)]
struct OwnedWindowsHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
struct WindowsLocalAllocation(windows::Win32::Foundation::HLOCAL);

#[cfg(windows)]
impl Drop for WindowsLocalAllocation {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Foundation::LocalFree(Some(self.0));
        }
    }
}

#[cfg(windows)]
struct WindowsSecurityDescriptor {
    descriptor: windows::Win32::Security::PSECURITY_DESCRIPTOR,
    _allocation: WindowsLocalAllocation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::SourceKind;

    fn temp_store() -> (PersistStore, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rlb-persist-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        (PersistStore::open(root.clone()).expect("store"), root)
    }

    fn test_run_descriptor(pid: u32, process_start_marker: &str) -> RunDescriptor {
        RunDescriptor {
            schema: RUN_DESCRIPTOR_SCHEMA,
            member_id: MemberId::generate().expect("member"),
            run_id: RunId::generate().expect("run"),
            pid,
            process_start_marker: process_start_marker.to_owned(),
            endpoint: ControlEndpointDescriptor {
                host: "127.0.0.1".to_owned(),
                port: 53_142,
                api_root: "/rlb/local/v2".to_owned(),
            },
            ipc_token: generate_ipc_token().expect("token"),
            created_at: rfc3339_utc_now(),
        }
    }

    fn write_descriptor_unchecked(store: &PersistStore, descriptor: &RunDescriptor) -> PathBuf {
        let path = store.run_descriptor_path(descriptor.member_id, descriptor.run_id);
        fs::write(
            &path,
            serde_json::to_vec_pretty(descriptor).expect("descriptor JSON"),
        )
        .expect("write descriptor");
        path
    }

    #[test]
    fn first_start_creates_stable_identity_and_config() {
        let (store, root) = temp_store();
        let IdentityLoad::Created(identity) = store.load_or_create_identity().expect("identity")
        else {
            panic!("expected a newly created identity");
        };
        let IdentityLoad::Existing(again) = store.load_or_create_identity().expect("reload") else {
            panic!("expected the same identity on reload");
        };
        assert_eq!(identity, again);
        let ConfigLoad::Created(config) = store.load_or_create_config(identity).expect("config")
        else {
            panic!("expected newly created config");
        };
        assert!(!config.sources[0].config.master_capture_enabled);
        assert_eq!(
            config.sources[0].source_id,
            identity.source_id(SourceKind::SystemAudio)
        );
        let ConfigLoad::Existing(reloaded) = store.load_or_create_config(identity).expect("reload")
        else {
            panic!("expected existing config");
        };
        assert_eq!(config, reloaded);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_identity_is_left_in_place() {
        let (store, root) = temp_store();
        fs::write(store.identity_path(), b"{not-json").expect("corrupt");
        assert!(matches!(
            store.load_or_create_identity(),
            Err(EngineError::IdentityCorrupt)
        ));
        assert_eq!(
            fs::read(store.identity_path()).expect("left in place"),
            b"{not-json"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_descriptor_roundtrip_keeps_loopback_host() {
        let (store, root) = temp_store();
        let identity = IdentityRecord::new(
            MemberId::generate().expect("member"),
            rlb_model::SourceId::generate().expect("sys"),
            rlb_model::SourceId::generate().expect("mic"),
        )
        .expect("identity");
        let run_id = RunId::generate().expect("run");
        let token = generate_ipc_token().expect("token");
        let path = store
            .write_run_descriptor(&RunDescriptor {
                schema: RUN_DESCRIPTOR_SCHEMA,
                member_id: identity.member_id(),
                run_id,
                pid: std::process::id(),
                process_start_marker: process_start_marker(),
                endpoint: ControlEndpointDescriptor {
                    host: "127.0.0.1".to_owned(),
                    port: 53_142,
                    api_root: "/rlb/local/v2".to_owned(),
                },
                ipc_token: token.clone(),
                created_at: rfc3339_utc_now(),
            })
            .expect("write");
        let loaded: RunDescriptor =
            serde_json::from_slice(&fs::read(path).expect("read")).expect("json");
        assert_eq!(loaded.ipc_token, token);
        assert_eq!(loaded.endpoint.host, "127.0.0.1");
        store
            .remove_run_descriptor(identity.member_id(), run_id)
            .expect("remove");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_decision_keeps_matching_and_unknown_processes() {
        assert!(!run_descriptor_is_stale(
            "creation-marker",
            &ProcessMarkerState::Running("creation-marker".to_owned())
        ));
        assert!(run_descriptor_is_stale(
            "old-creation-marker",
            &ProcessMarkerState::Running("reused-pid-marker".to_owned())
        ));
        assert!(run_descriptor_is_stale(
            "creation-marker",
            &ProcessMarkerState::Missing
        ));
        assert!(!run_descriptor_is_stale(
            "creation-marker",
            &ProcessMarkerState::Unknown
        ));
    }

    #[test]
    fn run_entry_filter_accepts_only_direct_regular_files() {
        let regular_file = RunEntryFileType {
            is_file: true,
            is_directory: false,
            is_symlink: false,
            is_reparse_point: false,
        };
        assert!(should_process_run_entry(regular_file, true));
        assert!(!should_process_run_entry(regular_file, false));
        assert!(!should_process_run_entry(
            RunEntryFileType {
                is_symlink: true,
                ..regular_file
            },
            true
        ));
        assert!(!should_process_run_entry(
            RunEntryFileType {
                is_reparse_point: true,
                ..regular_file
            },
            true
        ));
        assert!(!should_process_run_entry(
            RunEntryFileType {
                is_file: false,
                is_directory: true,
                is_symlink: false,
                is_reparse_point: false,
            },
            true
        ));
    }

    #[test]
    fn cleanup_removes_corrupt_missing_and_reused_pid_descriptors() {
        let (store, root) = temp_store();
        let missing = test_run_descriptor(10, "missing");
        let reused = test_run_descriptor(20, "old-marker");
        let matching = test_run_descriptor(30, "matching-marker");
        let unknown = test_run_descriptor(40, "unknown-marker");
        let missing_path = write_descriptor_unchecked(&store, &missing);
        let reused_path = write_descriptor_unchecked(&store, &reused);
        let matching_path = write_descriptor_unchecked(&store, &matching);
        let unknown_path = write_descriptor_unchecked(&store, &unknown);
        let corrupt_path = root.join("run").join("corrupt.json");
        fs::write(&corrupt_path, b"{broken-json").expect("corrupt descriptor");

        cleanup_run_directory_with(&root.join("run"), |pid| match pid {
            10 => ProcessMarkerState::Missing,
            20 => ProcessMarkerState::Running("new-marker".to_owned()),
            30 => ProcessMarkerState::Running("matching-marker".to_owned()),
            _ => ProcessMarkerState::Unknown,
        })
        .expect("cleanup");

        assert!(!missing_path.exists());
        assert!(!reused_path.exists());
        assert!(!corrupt_path.exists());
        assert!(matching_path.exists());
        assert!(unknown_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_startup_cleanup_checks_pid_and_creation_marker() {
        struct ChildGuard(std::process::Child);

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let (store, root) = temp_store();
        let marker = process_start_marker();
        let matching = test_run_descriptor(std::process::id(), &marker);
        let reused = test_run_descriptor(std::process::id(), &format!("{marker}-reused"));
        let missing = test_run_descriptor(0, "no-process");
        let ping = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("ping.exe");
        let child = std::process::Command::new(ping)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("descriptor child");
        let child = ChildGuard(child);
        let child_marker = match process_marker_state(child.0.id()) {
            ProcessMarkerState::Running(marker) => marker,
            state => panic!("child process marker unavailable: {state:?}"),
        };
        let other_instance = test_run_descriptor(child.0.id(), &child_marker);
        let matching_path = write_descriptor_unchecked(&store, &matching);
        let reused_path = write_descriptor_unchecked(&store, &reused);
        let missing_path = write_descriptor_unchecked(&store, &missing);
        let other_instance_path = write_descriptor_unchecked(&store, &other_instance);
        let corrupt_path = root.join("run").join("damaged.json");
        fs::write(&corrupt_path, b"[]").expect("damaged descriptor");
        drop(store);

        let reopened = PersistStore::open(root.clone()).expect("startup cleanup");

        assert!(matching_path.exists());
        assert!(other_instance_path.exists());
        assert!(!reused_path.exists());
        assert!(!missing_path.exists());
        assert!(!corrupt_path.exists());
        drop(reopened);
        drop(child);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_run_descriptor_dacl_has_only_user_and_local_system() {
        let (store, root) = temp_store();
        let mut descriptor = test_run_descriptor(std::process::id(), &process_start_marker());
        let path = store
            .write_run_descriptor(&descriptor)
            .expect("secure create");
        assert_windows_restricted_dacl(&path);

        fs::remove_file(&path).expect("remove initial descriptor");
        fs::write(&path, b"preexisting descriptor with inherited permissions")
            .expect("broad preexisting file");
        descriptor.ipc_token = "replacement-secret".to_owned();
        store
            .write_run_descriptor(&descriptor)
            .expect("secure atomic replacement");
        assert_windows_restricted_dacl(&path);

        let loaded: RunDescriptor =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("descriptor JSON");
        assert_eq!(loaded.ipc_token, "replacement-secret");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_symlink_is_never_followed() {
        use std::os::windows::fs::symlink_file;

        let (store, root) = temp_store();
        let outside = root.with_extension("outside-file");
        fs::write(&outside, b"outside sentinel").expect("outside target");
        let descriptor = test_run_descriptor(std::process::id(), &process_start_marker());
        let link = store.run_descriptor_path(descriptor.member_id, descriptor.run_id);
        if let Err(error) = symlink_file(&outside, &link) {
            if windows_link_permission_unavailable(&error) {
                eprintln!("skipped file symlink test: {error}");
                let _ = fs::remove_file(outside);
                let _ = fs::remove_dir_all(root);
                return;
            }
            panic!("create file symlink: {error}");
        }

        assert!(store.write_run_descriptor(&descriptor).is_err());
        assert_eq!(
            fs::read(&outside).expect("outside target unchanged"),
            b"outside sentinel"
        );
        drop(store);
        let reopened = PersistStore::open(root.clone()).expect("cleanup skips symlink");
        assert!(
            fs::symlink_metadata(&link)
                .expect("link remains")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(&outside).expect("cleanup did not follow link"),
            b"outside sentinel"
        );
        drop(reopened);
        fs::remove_file(link).expect("remove link itself");
        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_is_never_followed() {
        let (store, root) = temp_store();
        let outside = root.with_extension("outside-directory");
        fs::create_dir(&outside).expect("outside directory");
        let sentinel = outside.join("sentinel.txt");
        fs::write(&sentinel, b"outside sentinel").expect("outside sentinel");
        let descriptor = test_run_descriptor(std::process::id(), &process_start_marker());
        let junction = store.run_descriptor_path(descriptor.member_id, descriptor.run_id);
        if !windows_create_junction(&junction, &outside) {
            let _ = fs::remove_dir_all(outside);
            let _ = fs::remove_dir_all(root);
            return;
        }

        assert!(store.write_run_descriptor(&descriptor).is_err());
        assert_eq!(
            fs::read(&sentinel).expect("outside sentinel unchanged"),
            b"outside sentinel"
        );
        drop(store);
        let reopened = PersistStore::open(root.clone()).expect("cleanup skips junction");
        let metadata = fs::symlink_metadata(&junction).expect("junction remains");
        assert!(run_entry_file_type(&metadata).is_reparse_point);
        assert_eq!(
            fs::read(&sentinel).expect("cleanup did not follow junction"),
            b"outside sentinel"
        );
        drop(reopened);
        fs::remove_dir(&junction).expect("remove junction itself");
        let _ = fs::remove_dir_all(outside);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    fn windows_link_permission_unavailable(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(1_314)
    }

    #[cfg(windows)]
    fn windows_create_junction(junction: &Path, target: &Path) -> bool {
        let command = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("cmd.exe");
        let output = std::process::Command::new(command)
            .args(["/D", "/C", "mklink", "/J"])
            .arg(junction)
            .arg(target)
            .output()
            .expect("run mklink");
        if output.status.success() {
            true
        } else {
            eprintln!(
                "skipped junction test: mklink failed with {}; stdout={}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
    }

    #[cfg(windows)]
    fn assert_windows_restricted_dacl(path: &Path) {
        use std::collections::BTreeSet;

        let aces = windows_allowed_aces(path);
        let actual_sids: BTreeSet<_> = aces.iter().map(|(sid, _)| sid.clone()).collect();
        let expected_sids = BTreeSet::from([
            windows_current_user_sid_string().expect("current user SID"),
            "S-1-5-18".to_owned(),
        ]);
        assert_eq!(actual_sids, expected_sids);
        assert_eq!(aces.len(), expected_sids.len());
        assert!(
            aces.iter()
                .all(|(_, mask)| mask & 0x0000_0001 != 0 && mask & 0x0000_0002 != 0)
        );
        assert!(!actual_sids.contains("S-1-1-0"));
        assert!(!actual_sids.contains("S-1-5-32-545"));
    }

    #[cfg(windows)]
    fn windows_allowed_aces(path: &Path) -> Vec<(String, u32)> {
        use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL};
        use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        };
        use windows::core::PCWSTR;

        let wide = windows_path_wide(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut security_descriptor = PSECURITY_DESCRIPTOR::default();
        let status = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut security_descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS, "read descriptor DACL");
        let _allocation =
            WindowsLocalAllocation(HLOCAL(security_descriptor.0.cast::<core::ffi::c_void>()));
        assert!(!dacl.is_null(), "descriptor DACL must be present");

        let ace_count = unsafe { (*dacl).AceCount };
        let mut result = Vec::with_capacity(usize::from(ace_count));
        for index in 0..u32::from(ace_count) {
            let mut raw_ace = std::ptr::null_mut();
            unsafe { windows::Win32::Security::GetAce(dacl, index, &mut raw_ace) }
                .expect("read DACL entry");
            let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(unsafe { (*ace).Header.AceType }, 0, "allow ACE");
            let sid = PSID(
                unsafe { std::ptr::addr_of_mut!((*ace).SidStart) }.cast::<core::ffi::c_void>(),
            );
            result.push((windows_sid_string(sid).expect("ACE SID"), unsafe {
                (*ace).Mask
            }));
        }
        result
    }

    #[test]
    fn config_without_binding_fields_loads_as_follow_default() {
        let (store, root) = temp_store();
        let IdentityLoad::Created(identity) = store.load_or_create_identity().expect("identity")
        else {
            panic!("expected a newly created identity");
        };
        let config = MemberConfig::defaults(identity, rlb_model::RoomId::generate().expect("room"));
        let mut value = serde_json::to_value(&config).expect("json");
        let object = value.as_object_mut().expect("object");
        object.remove("system_audio_binding");
        object.remove("microphone_binding");
        object.remove("playback_binding");
        fs::write(
            store.config_path(),
            serde_json::to_vec_pretty(&value).expect("write json"),
        )
        .expect("write");
        let ConfigLoad::Existing(loaded) = store.load_or_create_config(identity).expect("load")
        else {
            panic!("expected existing config");
        };
        assert_eq!(
            loaded.system_audio_binding.endpoint_mode,
            rlb_model::EndpointMode::FollowDefault
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn first_start_writes_unjoined_config() {
        let (store, root) = temp_store();
        let IdentityLoad::Created(identity) = store.load_or_create_identity().expect("identity")
        else {
            panic!("expected a newly created identity");
        };
        let ConfigLoad::Created(config) = store.load_or_create_config(identity).expect("create")
        else {
            panic!("expected created config");
        };
        assert_eq!(config.attached_room_id(), None);
        assert!(config.join_target().is_none());
        assert_eq!(config.schema_version, MEMBER_CONFIG_SCHEMA_VERSION);
        assert!(store.root().join("config.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn schema_v2_config_migrates_without_inventing_join() {
        let (store, root) = temp_store();
        let IdentityLoad::Created(identity) = store.load_or_create_identity().expect("identity")
        else {
            panic!("expected a newly created identity");
        };
        let mut config =
            MemberConfig::defaults(identity, rlb_model::RoomId::generate().expect("room"));
        config.schema_version = 2;
        let mut value = serde_json::to_value(&config).expect("json");
        value["schema_version"] = serde_json::json!(2);
        let object = value.as_object_mut().expect("object");
        object.remove("startup");
        object.remove("logging");
        object.remove("driver");
        object.remove("audit_seq");
        if let Some(room) = object.get_mut("room").and_then(|room| room.as_object_mut()) {
            room.remove("join");
        }
        fs::write(
            store.config_path(),
            serde_json::to_vec_pretty(&value).expect("write json"),
        )
        .expect("write");
        let ConfigLoad::Existing(loaded) = store.load_or_create_config(identity).expect("load")
        else {
            panic!("expected existing config");
        };
        assert_eq!(loaded.schema_version, MEMBER_CONFIG_SCHEMA_VERSION);
        assert_eq!(loaded.attached_room_id(), None);
        assert!(loaded.join_target().is_none());
        assert!(loaded.logging.directory.is_none());
        let disk: serde_json::Value =
            serde_json::from_slice(&fs::read(store.config_path()).expect("read")).expect("json");
        assert_eq!(disk["schema_version"], MEMBER_CONFIG_SCHEMA_VERSION);
        let _ = fs::remove_dir_all(root);
    }
}
