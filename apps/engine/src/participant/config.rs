//! Versioned participant configuration. Runtime contact state is not stored here.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rlb_model::{MemberId, SourceId};
use serde::{Deserialize, Serialize};

use super::output_routing::{
    OutputTarget, SourceOutputTargets, default_output_targets, validate_output_targets,
};

pub const SCHEMA: u32 = 1;
pub(super) const TRACKED_PEER_BUDGET: usize = 64;
const NAME_LIMIT: usize = 80;
const ENDPOINT_LIMIT: usize = 512;
const GAIN_DB_MIN: f32 = -96.0;
const GAIN_DB_MAX: f32 = 24.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    System,
    Microphone,
}

impl SourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Microphone => "microphone",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixIntent {
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
}

impl MixIntent {
    pub const fn unity() -> Self {
        Self {
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
        }
    }

    pub fn validate(self, label: &str) -> Result<(), String> {
        if !self.gain_db.is_finite() || !(GAIN_DB_MIN..=GAIN_DB_MAX).contains(&self.gain_db) {
            return Err(format!(
                "{label} gain must be a finite value from {GAIN_DB_MIN} to {GAIN_DB_MAX} dB."
            ));
        }
        if !self.pan.is_finite() || !(-1.0..=1.0).contains(&self.pan) {
            return Err(format!("{label} pan must be a finite value from -1 to 1."));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSource {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
    pub endpoint_id: Option<String>,
    pub publish: bool,
    pub monitor: bool,
    pub playback: MixIntent,
    #[serde(default = "default_output_targets")]
    pub output_targets: Vec<OutputTarget>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMixOverride {
    pub source_id: SourceId,
    pub playback: MixIntent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedMember {
    pub id: MemberId,
    pub name: String,
    pub send: bool,
    pub receive: bool,
    pub playback: MixIntent,
    pub source_overrides: Vec<SourceMixOverride>,
    /// Derived room projection, never a persisted per-device preference.
    #[serde(skip)]
    pub source_policy: Option<rlb_peer::SourcePolicy>,
    #[serde(skip)]
    pub source_output_targets: Vec<SourceOutputTargets>,
}

impl SavedMember {
    /// Initial choices for a newly discovered member. Loading saved members
    /// preserves their existing choices without applying these defaults.
    pub fn new(id: MemberId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            send: true,
            receive: false,
            playback: MixIntent::unity(),
            source_overrides: Vec::new(),
            source_policy: None,
            source_output_targets: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedContact {
    pub member_id: MemberId,
    pub address: SocketAddr,
    #[serde(default)]
    pub alternates: Vec<SocketAddr>,
    /// Ephemeral provenance of the room rule/output plan. This is derived
    /// from a proven connected path, never a remembered user permission.
    #[serde(skip)]
    pub resolved_for: Option<ResolvedPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedPath {
    pub address: SocketAddr,
    pub session: rlb_model::DirectSessionId,
    pub remote_run: rlb_model::RunId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedGroup {
    pub name: String,
    pub joined: bool,
}

/// Saved room entry intent. Runtime member addresses and permissions live in
/// the room projection and are recomputed from the user's rule files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoomStartup {
    Join {
        profile_path: PathBuf,
    },
    Create {
        profile_path: PathBuf,
        bind: SocketAddr,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPreferences {
    pub selected: Option<RoomStartup>,
    pub auto_enter: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantConfig {
    pub schema: u32,
    pub member_id: MemberId,
    pub display_name: String,
    pub bind: SocketAddr,
    #[serde(default)]
    pub network: super::network::NetworkPolicy,
    pub auto_reconnect: bool,
    #[serde(default)]
    pub login_startup: bool,
    #[serde(default)]
    pub room: RoomPreferences,
    pub group: SavedGroup,
    pub sources: Vec<LocalSource>,
    pub output_endpoint_id: Option<String>,
    pub master_gain_db: f32,
    pub limiter_enabled: bool,
    pub members: Vec<SavedMember>,
    pub contacts: Vec<SavedContact>,
}

impl ParticipantConfig {
    pub fn create(display_name: &str, bind: SocketAddr) -> Result<Self, String> {
        validate_name("display name", display_name)?;
        let system = SourceId::generate().map_err(|_| {
            "The operating-system random source is unavailable, so a source identity could not be created.".to_owned()
        })?;
        let microphone = SourceId::generate().map_err(|_| {
            "The operating-system random source is unavailable, so a source identity could not be created.".to_owned()
        })?;
        let member_id = MemberId::generate().map_err(|_| {
            "The operating-system random source is unavailable, so a member identity could not be created.".to_owned()
        })?;
        let config = Self {
            schema: SCHEMA,
            member_id,
            display_name: display_name.to_owned(),
            bind,
            network: super::network::NetworkPolicy::default(),
            auto_reconnect: true,
            login_startup: false,
            room: RoomPreferences::default(),
            group: SavedGroup {
                name: String::new(),
                joined: false,
            },
            sources: vec![
                LocalSource {
                    id: system,
                    kind: SourceKind::System,
                    name: "System sound".to_owned(),
                    endpoint_id: None,
                    publish: true,
                    monitor: false,
                    playback: MixIntent::unity(),
                    output_targets: default_output_targets(),
                },
                LocalSource {
                    id: microphone,
                    kind: SourceKind::Microphone,
                    name: "Microphone".to_owned(),
                    endpoint_id: None,
                    publish: false,
                    monitor: false,
                    playback: MixIntent::unity(),
                    output_targets: default_output_targets(),
                },
            ],
            output_endpoint_id: None,
            master_gain_db: 0.0,
            limiter_enabled: false,
            members: Vec::new(),
            contacts: Vec::new(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn source(&self, id: SourceId) -> Option<&LocalSource> {
        self.sources.iter().find(|source| source.id == id)
    }

    pub fn source_mut(&mut self, id: SourceId) -> Option<&mut LocalSource> {
        self.sources.iter_mut().find(|source| source.id == id)
    }

    pub fn member(&self, id: MemberId) -> Option<&SavedMember> {
        self.members.iter().find(|member| member.id == id)
    }

    pub fn member_mut(&mut self, id: MemberId) -> Option<&mut SavedMember> {
        self.members.iter_mut().find(|member| member.id == id)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "Configuration schema {} is unsupported. This controller reads schema {SCHEMA}.",
                self.schema
            ));
        }
        if self.contacts.len() > TRACKED_PEER_BUDGET {
            return Err(format!(
                "The configuration contains more than {TRACKED_PEER_BUDGET} contact members. Reduce the saved contacts before starting this participant."
            ));
        }
        validate_name("display name", &self.display_name)?;
        self.network.validate()?;
        validate_optional_endpoint(self.output_endpoint_id.as_deref())?;
        MixIntent {
            gain_db: self.master_gain_db,
            pan: 0.0,
            mute: false,
            solo: false,
        }
        .validate("master")?;
        if self.group.joined {
            validate_name("group name", &self.group.name)?;
        } else if self.group.name.chars().count() > NAME_LIMIT {
            return Err(format!(
                "Group name must be at most {NAME_LIMIT} characters."
            ));
        }
        if self.sources.len() != 2 {
            return Err(
                "The configuration must contain the system source and the microphone source."
                    .into(),
            );
        }
        let mut saw_system = false;
        let mut saw_microphone = false;
        for source in &self.sources {
            validate_name("source name", &source.name)?;
            validate_optional_endpoint(source.endpoint_id.as_deref())?;
            source.playback.validate("source playback")?;
            validate_output_targets(&source.output_targets)?;
            match source.kind {
                SourceKind::System if saw_system => {
                    return Err("Only one system source may be saved.".into());
                }
                SourceKind::Microphone if saw_microphone => {
                    return Err("Only one microphone source may be saved.".into());
                }
                SourceKind::System => saw_system = true,
                SourceKind::Microphone => saw_microphone = true,
            }
        }
        if !saw_system || !saw_microphone {
            return Err(
                "The configuration must contain distinct system and microphone sources.".into(),
            );
        }
        if self.sources[0].id == self.sources[1].id {
            return Err(
                "The system source and microphone source must use different identities.".into(),
            );
        }
        let mut member_ids = Vec::new();
        for member in &self.members {
            if member.id == self.member_id {
                return Err(
                    "A saved member identity must differ from the local member identity.".into(),
                );
            }
            if member_ids.contains(&member.id) {
                return Err(format!("Member {} is listed more than once.", member.id));
            }
            member_ids.push(member.id);
            validate_name("member name", &member.name)?;
            member.playback.validate("member playback")?;
            let mut override_ids = Vec::new();
            for choice in &member.source_overrides {
                if self.source(choice.source_id).is_some() {
                    return Err(
                        "A remote mix override must use a remote source identity, not a local source identity.".into(),
                    );
                }
                if override_ids.contains(&choice.source_id) {
                    return Err(format!(
                        "Source {} has more than one mix override for member {}.",
                        choice.source_id, member.id
                    ));
                }
                override_ids.push(choice.source_id);
                choice.playback.validate("remote source playback")?;
            }
        }
        let mut contact_members = Vec::new();
        for contact in &self.contacts {
            validate_contact_address(contact.address)?;
            if contact.alternates.len() >= rlb_contact::MAX_MEDIA_CANDIDATES {
                return Err("A peer has too many media candidates.".into());
            }
            for alternate in &contact.alternates { validate_contact_address(*alternate)?; }
            if contact_members.contains(&contact.member_id) {
                return Err(format!(
                    "Member {} has more than one contact address.",
                    contact.member_id
                ));
            }
            contact_members.push(contact.member_id);
            if self.member(contact.member_id).is_none() {
                return Err(format!(
                    "Contact {} has no saved member preferences.",
                    contact.member_id
                ));
            }
        }
        Ok(())
    }
}

pub fn validate_name(label: &str, name: &str) -> Result<(), String> {
    let length = name.chars().count();
    if name.trim().is_empty() || length > NAME_LIMIT || name.chars().any(|ch| ch.is_control()) {
        return Err(format!(
            "The {label} must contain 1 to {NAME_LIMIT} characters and no control characters."
        ));
    }
    Ok(())
}

pub fn validate_contact_address(address: SocketAddr) -> Result<(), String> {
    if address.port() == 0
        || address.ip().is_unspecified()
        || address.ip().is_multicast()
        || matches!(address.ip(), std::net::IpAddr::V4(ip) if ip.is_broadcast())
    {
        return Err(
            "A direct contact needs an individual device address and a nonzero UDP port.".into(),
        );
    }
    Ok(())
}

pub fn validate_optional_endpoint(endpoint_id: Option<&str>) -> Result<(), String> {
    let Some(endpoint_id) = endpoint_id else {
        return Ok(());
    };
    if endpoint_id.is_empty()
        || endpoint_id.chars().count() > ENDPOINT_LIMIT
        || endpoint_id != endpoint_id.trim()
        || endpoint_id.chars().any(|ch| ch.is_control())
    {
        return Err(
            "An endpoint id must be a non-empty opaque value without surrounding whitespace."
                .into(),
        );
    }
    Ok(())
}

pub fn load_config(path: &Path) -> Result<ParticipantConfig, String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "The configuration {} could not be read: {error}",
            path.display()
        )
    })?;
    let config: ParticipantConfig = serde_json::from_str(&text).map_err(|error| {
        format!(
            "The configuration {} is invalid and was left unchanged: {error}",
            path.display()
        )
    })?;
    config.validate().map_err(|error| {
        format!(
            "The configuration {} is invalid and was left unchanged: {error}",
            path.display()
        )
    })?;
    Ok(config)
}

pub fn create_config_file(path: &Path, config: &ParticipantConfig) -> Result<(), String> {
    config.validate()?;
    if path.exists() {
        return Err(format!(
            "Configuration {} already exists. The existing file was not modified.",
            path.display()
        ));
    }
    let bytes = encode(config)?;
    let staged = stage_bytes(path, &bytes)?;
    if let Err(error) = publish_staged(&staged, path, false) {
        let _ = fs::remove_file(&staged);
        if path.exists() {
            return Err(format!(
                "Configuration {} was left unchanged: {error}",
                path.display()
            ));
        }
        return Err(format!(
            "Configuration {} could not be created: {error}",
            path.display()
        ));
    }
    Ok(())
}

pub fn save_config(path: &Path, config: &ParticipantConfig) -> Result<(), String> {
    config.validate()?;
    if !path.is_file() {
        return Err(format!(
            "Configuration {} is not an existing file. Nothing was written.",
            path.display()
        ));
    }
    let previous = fs::read(path).map_err(|error| {
        format!(
            "Configuration {} could not be read: {error}",
            path.display()
        )
    })?;
    let bytes = encode(config)?;
    let staged = stage_bytes(path, &bytes)?;
    if let Err(error) = publish_staged(&staged, path, true) {
        let _ = fs::remove_file(&staged);
        let current = fs::read(path).ok();
        if current.as_deref() == Some(previous.as_slice()) {
            return Err(format!(
                "Configuration {} was left unchanged: {error}",
                path.display()
            ));
        }
        return Err(format!(
            "Configuration {} could not be updated: {error}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
pub fn staging_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".rlb-participant.tmp");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

fn encode(config: &ParticipantConfig) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("The configuration could not be encoded: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn stage_bytes(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    // A previous interrupted save must not block every later save. Each write
    // owns its own stage; any old recovery file remains untouched.
    let nonce = SourceId::generate()
        .map_err(|_| "A unique configuration staging name could not be created.".to_owned())?;
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".rlb-participant-{nonce}.tmp"));
    let staged = path.with_file_name(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|error| {
            format!(
                "Staging file {} could not be created. Configuration {} was not modified: {error}",
                staged.display(),
                path.display()
            )
        })?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "Staging file {} could not be finished. Configuration {} was not modified: {error}",
            staged.display(),
            path.display()
        ));
    }
    Ok(staged)
}

fn publish_staged(from: &Path, to: &Path, replace_existing: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;
    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace_existing {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    let published =
        unsafe { MoveFileExW(PCWSTR(from_wide.as_ptr()), PCWSTR(to_wide.as_ptr()), flags) };
    if published.is_ok() {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rlb-c03-config-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn new_config_uses_distinct_sources_and_safe_defaults() {
        let mut config = ParticipantConfig::create("Ada", "127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(config.auto_reconnect);
        assert!(!config.limiter_enabled);
        assert_eq!(config.master_gain_db, 0.0);
        assert_eq!(config.sources.len(), 2);
        assert_ne!(config.sources[0].id, config.sources[1].id);
        assert!(config.sources.iter().all(|source| !source.monitor));
        assert!(
            config
                .sources
                .iter()
                .any(|source| source.kind == SourceKind::System && source.publish)
        );
        assert!(
            config
                .sources
                .iter()
                .any(|source| source.kind == SourceKind::Microphone && !source.publish)
        );
        let member = SavedMember::new(MemberId::generate().unwrap(), "Bea");
        assert!(member.send);
        assert!(!member.receive);
        config.members.push(member);
        let dir = scratch();
        let path = dir.join("participant.json");
        create_config_file(&path, &config).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded, config);
        let error = create_config_file(&path, &config).unwrap_err();
        assert!(error.contains("was not modified"));
        config.members[0].send = false;
        config.members[0].receive = true;
        save_config(&path, &config).unwrap();
        assert_eq!(load_config(&path).unwrap(), config);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn local_outputs_default_to_main_and_remote_projection_is_never_saved() {
        let mut config = ParticipantConfig::create("Ada", "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut old = serde_json::to_value(&config).unwrap();
        for source in old["sources"].as_array_mut().unwrap() {
            source.as_object_mut().unwrap().remove("output_targets");
        }
        let loaded: ParticipantConfig = serde_json::from_value(old).unwrap();
        assert!(
            loaded
                .sources
                .iter()
                .all(|source| source.output_targets == default_output_targets())
        );
        config.sources[0].output_targets = vec![];
        config.sources[1].output_targets = vec![OutputTarget::Endpoint("headphones".into())];
        let mut member = SavedMember::new(MemberId::generate().unwrap(), "Bea");
        member.source_output_targets.push(SourceOutputTargets {
            kind: SourceKind::System,
            targets: vec![OutputTarget::Endpoint("virtual-microphone".into())],
        });
        config.members.push(member);
        config.validate().unwrap();
        let encoded = serde_json::to_value(&config).unwrap();
        assert!(encoded["members"][0].get("source_output_targets").is_none());
        let loaded: ParticipantConfig = serde_json::from_value(encoded).unwrap();
        assert_eq!(loaded.sources, config.sources);
        assert!(loaded.members[0].source_output_targets.is_empty());
        config.sources[0].output_targets = vec![OutputTarget::Main, OutputTarget::Main];
        assert!(config.validate().is_err());
    }

    #[test]
    fn corrupt_config_is_retained() {
        let dir = scratch();
        let path = dir.join("participant.json");
        fs::write(&path, b"{not-json").unwrap();
        let before = fs::read(&path).unwrap();
        let error = load_config(&path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn blocked_update_preserves_the_previous_file() {
        let dir = scratch();
        let path = dir.join("participant.json");
        let mut config = ParticipantConfig::create("Ada", "127.0.0.1:9".parse().unwrap()).unwrap();
        create_config_file(&path, &config).unwrap();
        let before = fs::read(&path).unwrap();
        use std::os::windows::fs::OpenOptionsExt;
        // Keep reads/writes shared, but deny replacement while the file is open.
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        config.display_name = "Bea".to_owned();
        let error = save_config(&path, &config).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(blocker);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_stage_and_duplicate_contacts_are_handled_explicitly() {
        let dir = scratch();
        let path = dir.join("participant.json");
        let mut config = ParticipantConfig::create("Ada", "127.0.0.1:0".parse().unwrap()).unwrap();
        create_config_file(&path, &config).unwrap();
        fs::write(staging_path(&path), b"old recovery data").unwrap();
        config.display_name = "Bea".into();
        save_config(&path, &config).unwrap();
        assert_eq!(load_config(&path).unwrap().display_name, "Bea");
        assert_eq!(fs::read(staging_path(&path)).unwrap(), b"old recovery data");
        let id = MemberId::generate().unwrap();
        config.members.push(SavedMember {
            id,
            name: "Peer".into(),
            source_policy: None,
            source_output_targets: Vec::new(),
            send: true,
            receive: true,
            playback: MixIntent::unity(),
            source_overrides: Vec::new(),
        });
        config.contacts.push(SavedContact {
            member_id: id,
            address: "127.0.0.1:47110".parse().unwrap(),
            alternates: Vec::new(),
            resolved_for: None,
        });
        config.contacts.push(SavedContact {
            member_id: id,
            address: "127.0.0.1:47111".parse().unwrap(),
            alternates: Vec::new(),
            resolved_for: None,
        });
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("more than one contact")
        );
        assert!(validate_contact_address("0.0.0.0:47110".parse().unwrap()).is_err());
        assert!(validate_contact_address("127.0.0.1:0".parse().unwrap()).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
