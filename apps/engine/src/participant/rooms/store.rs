//! Atomic UTF-8 JSON load, create, and save for global rules and room profiles.
//!
//! Writes stage beside the destination with a unique nonce name, then publish
//! with Windows `MoveFileExW`. Failed validation leaves the existing file as it
//! was. Create refuses to replace a path that already exists.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rlb_model::SourceId;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::profile::{GlobalRules, RoomProfile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaveFailureKind {
    ExternalChange,
    WriteFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveFailure {
    pub kind: SaveFailureKind,
    pub detail: String,
}

impl SaveFailure {
    fn write_failed(detail: String) -> Self {
        Self {
            kind: SaveFailureKind::WriteFailed,
            detail,
        }
    }
}

impl std::fmt::Display for SaveFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for SaveFailure {}

impl From<String> for SaveFailure {
    fn from(detail: String) -> Self {
        Self::write_failed(detail)
    }
}

#[derive(Clone, Copy)]
struct Kind {
    lower: &'static str,
    title: &'static str,
}

const GLOBAL: Kind = Kind {
    lower: "global rules",
    title: "Global rules",
};
const ROOM: Kind = Kind {
    lower: "room profile",
    title: "Room profile",
};

pub fn load_global_rules(path: &Path) -> Result<GlobalRules, String> {
    load_validated(path, GLOBAL, GlobalRules::validate)
}

pub fn save_global_rules(path: &Path, rules: &GlobalRules) -> Result<(), String> {
    rules.validate()?;
    save_json(path, rules, GLOBAL)
}

/// Refuse an external replacement observed since the caller loaded these bytes.
pub fn save_global_rules_if_unchanged(
    path: &Path,
    rules: &GlobalRules,
    expected: &[u8],
) -> Result<(), SaveFailure> {
    rules.validate().map_err(SaveFailure::write_failed)?;
    save_json_conditional(path, rules, GLOBAL, Some(expected))
}

pub fn create_global_rules_file(path: &Path, rules: &GlobalRules) -> Result<(), String> {
    rules.validate()?;
    create_json(path, rules, GLOBAL)
}

pub fn load_room_profile(path: &Path) -> Result<RoomProfile, String> {
    load_validated(path, ROOM, RoomProfile::validate)
}

pub fn save_room_profile(path: &Path, profile: &RoomProfile) -> Result<(), String> {
    profile.validate()?;
    save_json(path, profile, ROOM)
}

pub fn save_room_profile_if_unchanged(
    path: &Path,
    profile: &RoomProfile,
    expected: &[u8],
) -> Result<(), SaveFailure> {
    profile.validate().map_err(SaveFailure::write_failed)?;
    save_json_conditional(path, profile, ROOM, Some(expected))
}

pub fn create_room_profile_file(path: &Path, profile: &RoomProfile) -> Result<(), String> {
    profile.validate()?;
    create_json(path, profile, ROOM)
}

fn load_validated<T, F>(path: &Path, kind: Kind, validate: F) -> Result<T, String>
where
    T: DeserializeOwned,
    F: FnOnce(&T) -> Result<(), String>,
{
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "The {} {} could not be read: {error}",
            kind.lower,
            path.display()
        )
    })?;
    let value: T = serde_json::from_str(&text).map_err(|error| {
        format!(
            "The {} {} is invalid and was left unchanged: {error}",
            kind.lower,
            path.display()
        )
    })?;
    validate(&value).map_err(|error| {
        format!(
            "The {} {} is invalid and was left unchanged: {error}",
            kind.lower,
            path.display()
        )
    })?;
    Ok(value)
}

fn create_json<T: Serialize>(path: &Path, value: &T, kind: Kind) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} {} already exists. The existing file was not modified.",
            kind.title,
            path.display()
        ));
    }
    let bytes = encode(value, kind.lower)?;
    let staged = stage_bytes(path, &bytes, kind)?;
    if let Err(error) = publish_staged(&staged, path, false) {
        let _ = fs::remove_file(&staged);
        if path.exists() {
            return Err(format!(
                "{} {} was left unchanged: {error}",
                kind.title,
                path.display()
            ));
        }
        return Err(format!(
            "{} {} could not be created: {error}",
            kind.title,
            path.display()
        ));
    }
    Ok(())
}

fn save_json<T: Serialize>(path: &Path, value: &T, kind: Kind) -> Result<(), String> {
    save_json_conditional(path, value, kind, None).map_err(|error| error.to_string())
}

fn save_json_conditional<T: Serialize>(
    path: &Path,
    value: &T,
    kind: Kind,
    expected: Option<&[u8]>,
) -> Result<(), SaveFailure> {
    if !path.is_file() {
        return Err(SaveFailure::write_failed(format!(
            "{} {} is not an existing file. Nothing was written.",
            kind.title,
            path.display()
        )));
    }
    let previous = fs::read(path).map_err(|error| {
        SaveFailure::write_failed(format!(
            "{} {} could not be read: {error}",
            kind.title,
            path.display()
        ))
    })?;
    if expected.is_some_and(|expected| previous.as_slice() != expected) {
        return Err(external_change(path, kind));
    }
    let bytes = encode(value, kind.lower).map_err(SaveFailure::write_failed)?;
    let staged = stage_bytes(path, &bytes, kind).map_err(SaveFailure::write_failed)?;
    // Check again after staging, immediately before publication. This catches
    // observed external edits without claiming a filesystem compare-and-swap
    // against arbitrary concurrent editors.
    if let Some(expected) = expected {
        let current = match fs::read(path) {
            Ok(current) => current,
            Err(error) => {
                let _ = fs::remove_file(&staged);
                return Err(SaveFailure::write_failed(format!(
                    "{} {} could not be read: {error}",
                    kind.title,
                    path.display()
                )));
            }
        };
        if current.as_slice() != expected {
            let _ = fs::remove_file(&staged);
            return Err(external_change(path, kind));
        }
    }
    if let Err(error) = publish_staged(&staged, path, true) {
        let _ = fs::remove_file(&staged);
        let current = fs::read(path).ok();
        if current.as_deref() == Some(previous.as_slice()) {
            return Err(SaveFailure::write_failed(format!(
                "{} {} was left unchanged: {error}",
                kind.title,
                path.display()
            )));
        }
        return Err(SaveFailure::write_failed(format!(
            "{} {} could not be updated: {error}",
            kind.title,
            path.display()
        )));
    }
    Ok(())
}

fn external_change(path: &Path, kind: Kind) -> SaveFailure {
    SaveFailure {
        kind: SaveFailureKind::ExternalChange,
        detail: format!(
            "The {} {} changed outside the application. It was left unchanged. Reload it before saving again.",
            kind.lower,
            path.display()
        ),
    }
}

fn encode<T: Serialize>(value: &T, kind: &str) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("The {kind} could not be encoded: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
pub fn staging_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".rlb-room.tmp");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

fn stage_bytes(path: &Path, bytes: &[u8], kind: Kind) -> Result<PathBuf, String> {
    // A previous interrupted save must not block every later save. Each write
    // owns its own stage; any old recovery file remains untouched.
    let nonce = SourceId::generate()
        .map_err(|_| format!("A unique {} staging name could not be created.", kind.lower))?;
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".rlb-room-{nonce}.tmp"));
    let staged = path.with_file_name(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|error| {
            format!(
                "Staging file {} could not be created. {} {} was not modified: {error}",
                staged.display(),
                kind.title,
                path.display()
            )
        })?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "Staging file {} could not be finished. {} {} was not modified: {error}",
            staged.display(),
            kind.title,
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

    const SECRET: &str = "pw-token-7f3a9c2e-not-for-logs";

    const ROOM_CREATE_JSON: &str = concat!(
        "{\n",
        "  \"schema\": 1,\n",
        "  \"room\": {\n",
        "    \"name\": \"Headphones\",\n",
        "    \"address\": null,\n",
        "    \"password\": \"\",\n",
        "    \"strict_auto_join\": false\n",
        "  },\n",
        "  \"rules\": [],\n",
        "  \"playback\": []\n",
        "}\n",
    );

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rlb-gb03-room-store-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assert_secret_absent(error: &str) {
        assert!(!error.contains(SECRET), "{error}");
    }

    #[test]
    fn null_address_roundtrips_as_json_null() {
        let dir = scratch();
        let path = dir.join("room.json");
        let profile = RoomProfile::create("Headphones").unwrap();
        assert_eq!(profile.room.address, None);
        create_room_profile_file(&path, &profile).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, ROOM_CREATE_JSON);
        assert!(text.contains("\"address\": null"), "{text}");
        let loaded = load_room_profile(&path).unwrap();
        assert_eq!(loaded.room.address, None);
        save_room_profile(&path, &loaded).unwrap();
        let loaded = load_room_profile(&path).unwrap();
        assert_eq!(loaded.room.address, None);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, ROOM_CREATE_JSON);
        assert!(text.contains("\"address\": null"), "{text}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn create_refuses_to_overwrite_an_existing_file() {
        let dir = scratch();
        let room_path = dir.join("room.json");
        let profile = RoomProfile::create("Headphones").unwrap();
        create_room_profile_file(&room_path, &profile).unwrap();
        let before = fs::read(&room_path).unwrap();
        let mut other = profile.clone();
        other.room.name = "Other".into();
        let error = create_room_profile_file(&room_path, &other).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert!(error.contains("was not modified"), "{error}");
        assert_eq!(fs::read(&room_path).unwrap(), before);

        let global_path = dir.join("global.json");
        let rules = GlobalRules::baseline();
        create_global_rules_file(&global_path, &rules).unwrap();
        let before = fs::read(&global_path).unwrap();
        let error = create_global_rules_file(&global_path, &rules).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert!(error.contains("was not modified"), "{error}");
        assert_eq!(fs::read(&global_path).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_load_leaves_the_file_unchanged() {
        let dir = scratch();
        let room_path = dir.join("room.json");
        fs::write(&room_path, b"{not-json").unwrap();
        let before = fs::read(&room_path).unwrap();
        let error = load_room_profile(&room_path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&room_path).unwrap(), before);

        let global_path = dir.join("global.json");
        fs::write(&global_path, b"{not-json").unwrap();
        let before = fs::read(&global_path).unwrap();
        let error = load_global_rules(&global_path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&global_path).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn blocked_update_preserves_the_previous_file() {
        let dir = scratch();
        let path = dir.join("room.json");
        let mut profile = RoomProfile::create("Headphones").unwrap();
        create_room_profile_file(&path, &profile).unwrap();
        let before = fs::read(&path).unwrap();
        use std::os::windows::fs::OpenOptionsExt;
        // Keep reads/writes shared, but deny replacement while the file is open.
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        profile.room.name = "Bea".to_owned();
        let error = save_room_profile(&path, &profile).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(blocker);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn conditional_saves_classify_external_bytes_and_preserve_them() {
        let dir = scratch();
        let room_path = dir.join("room.json");
        let profile = RoomProfile::create("Headphones").unwrap();
        create_room_profile_file(&room_path, &profile).unwrap();
        let expected = fs::read(&room_path).unwrap();
        let mut external = expected.clone();
        external.push(b' ');
        fs::write(&room_path, &external).unwrap();
        let error = save_room_profile_if_unchanged(&room_path, &profile, &expected).unwrap_err();
        assert_eq!(error.kind, SaveFailureKind::ExternalChange);
        assert!(
            error
                .to_string()
                .contains("changed outside the application")
        );
        assert_eq!(fs::read(&room_path).unwrap(), external);

        let global_path = dir.join("global.json");
        let rules = GlobalRules::baseline();
        create_global_rules_file(&global_path, &rules).unwrap();
        let expected = fs::read(&global_path).unwrap();
        let mut external = expected.clone();
        external.push(b' ');
        fs::write(&global_path, &external).unwrap();
        let error = save_global_rules_if_unchanged(&global_path, &rules, &expected).unwrap_err();
        assert_eq!(error.kind, SaveFailureKind::ExternalChange);
        assert!(
            error
                .to_string()
                .contains("changed outside the application")
        );
        assert_eq!(fs::read(&global_path).unwrap(), external);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn conditional_saves_classify_missing_paths_as_write_failed() {
        let dir = scratch();
        let room_path = dir.join("missing-room.json");
        let profile = RoomProfile::create("Headphones").unwrap();
        let error = save_room_profile_if_unchanged(&room_path, &profile, b"").unwrap_err();
        assert_eq!(error.kind, SaveFailureKind::WriteFailed);
        assert!(error.to_string().contains("not an existing file"));
        assert!(!room_path.exists());

        let global_path = dir.join("missing-global.json");
        let error = save_global_rules_if_unchanged(&global_path, &GlobalRules::baseline(), b"")
            .unwrap_err();
        assert_eq!(error.kind, SaveFailureKind::WriteFailed);
        assert!(error.to_string().contains("not an existing file"));
        assert!(!global_path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn conditional_save_classifies_blocked_publication_as_write_failed() {
        let dir = scratch();
        let path = dir.join("room.json");
        let mut profile = RoomProfile::create("Headphones").unwrap();
        create_room_profile_file(&path, &profile).unwrap();
        let before = fs::read(&path).unwrap();
        use std::os::windows::fs::OpenOptionsExt;
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        profile.room.name = "Bea".into();
        let error = save_room_profile_if_unchanged(&path, &profile, &before).unwrap_err();
        assert_eq!(error.kind, SaveFailureKind::WriteFailed);
        assert!(error.to_string().contains("left unchanged"));
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(blocker);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn schema_and_shape_mismatches_are_rejected() {
        let dir = scratch();

        let schema_path = dir.join("schema.json");
        let schema_two = "{\n  \"schema\": 2,\n  \"rules\": []\n}\n";
        fs::write(&schema_path, schema_two).unwrap();
        let before = fs::read(&schema_path).unwrap();
        let error = load_global_rules(&schema_path).unwrap_err();
        assert!(error.contains("unsupported"), "{error}");
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&schema_path).unwrap(), before);

        let extra_path = dir.join("extra.json");
        let extra = "{\n  \"schema\": 1,\n  \"rules\": [],\n  \"unexpected\": true\n}\n";
        fs::write(&extra_path, extra).unwrap();
        let before = fs::read(&extra_path).unwrap();
        let error = load_global_rules(&extra_path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&extra_path).unwrap(), before);

        let room_path = dir.join("room.json");
        create_room_profile_file(&room_path, &RoomProfile::create("Headphones").unwrap()).unwrap();
        let before = fs::read(&room_path).unwrap();
        let error = load_global_rules(&room_path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&room_path).unwrap(), before);

        let global_path = dir.join("global.json");
        create_global_rules_file(&global_path, &GlobalRules::baseline()).unwrap();
        let before = fs::read(&global_path).unwrap();
        let error = load_room_profile(&global_path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(fs::read(&global_path).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_stage_file_is_left_untouched() {
        let dir = scratch();
        let path = dir.join("room.json");
        let mut profile = RoomProfile::create("Headphones").unwrap();
        create_room_profile_file(&path, &profile).unwrap();
        fs::write(staging_path(&path), b"old recovery data").unwrap();
        profile.room.name = "Bea".into();
        save_room_profile(&path, &profile).unwrap();
        assert_eq!(load_room_profile(&path).unwrap().room.name, "Bea");
        assert_eq!(fs::read(staging_path(&path)).unwrap(), b"old recovery data");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn password_is_stored_without_appearing_in_errors() {
        let dir = scratch();
        let path = dir.join("room.json");
        let mut profile = RoomProfile::create("Headphones").unwrap();
        profile.room.password = SECRET.into();
        create_room_profile_file(&path, &profile).unwrap();
        let loaded = load_room_profile(&path).unwrap();
        assert_eq!(loaded.room.password, SECRET);
        assert!(fs::read_to_string(&path).unwrap().contains(SECRET));

        let mut invalid = loaded.clone();
        invalid.room.name.clear();
        let error = save_room_profile(&path, &invalid).unwrap_err();
        assert_secret_absent(&error);

        let with_extra = format!(
            "{{\n  \"schema\": 1,\n  \"room\": {{\n    \"name\": \"Headphones\",\n    \"address\": null,\n    \"password\": \"{SECRET}\",\n    \"strict_auto_join\": false\n  }},\n  \"rules\": [],\n  \"unexpected\": true\n}}\n"
        );
        fs::write(&path, &with_extra).unwrap();
        let error = load_room_profile(&path).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_secret_absent(&error);
        assert_eq!(fs::read(&path).unwrap(), with_extra.as_bytes());
        let _ = fs::remove_dir_all(dir);
    }
}
