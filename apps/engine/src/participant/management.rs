//! Explicit card editing and portable settings. Cold file work stays outside audio.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rlb_model::{MemberId, SourceId};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::config::{self, ParticipantConfig, RoomStartup};
use super::output_routing::OutputTarget;
use super::rooms::{self, GlobalRules, RoomLibrary, RoomProfile, RoomTarget};

const FORMAT: &str = "rustlanbus-settings";
const BUNDLE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CardDocument {
    pub path: PathBuf,
    pub revision: String,
    pub profile: Option<RoomProfile>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    Restore,
    CopyToNewDevice,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportOptions {
    pub mode: ImportMode,
    pub new_name: Option<String>,
    pub expected_destination_revision: Option<String>,
    #[serde(default)]
    pub expected_source_revision: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CardContent {
    Profile {
        profile: RoomProfile,
    },
    Unreadable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes_base64: Option<String>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleCard {
    file_name: String,
    content: CardContent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum BundleStartup {
    Join {
        card: String,
    },
    Create {
        card: String,
        bind: std::net::SocketAddr,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBundle {
    format: String,
    schema: u32,
    config: ParticipantConfig,
    global: GlobalRules,
    cards: Vec<BundleCard>,
    startup: Option<BundleStartup>,
}

/// Private, explicit query result; never add this to a continuous snapshot.
pub struct ActiveRoomSettings {
    pub(crate) global: Option<GlobalRules>,
    pub(crate) profile: Option<(PathBuf, RoomProfile)>,
    pub(crate) profile_revision: Option<String>,
    pub(crate) has_unsaved: bool,
    pub(crate) rule_files: crate::participant::room_session::RuleFilesState,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UnsavedChange {
    pub kind: &'static str,
    pub path: PathBuf,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CardPreview {
    pub file_name: String,
    pub room_name: Option<String>,
    pub has_password: bool,
    pub rule_count: usize,
    pub unreadable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BundlePreview {
    pub source_name: String,
    pub source_revision: String,
    pub mode: ImportMode,
    pub cards: Vec<CardPreview>,
    pub selected_card: Option<String>,
    pub contains_passwords: bool,
    pub destination: Option<PathBuf>,
    pub destination_revision: Option<String>,
    pub warnings: Vec<String>,
    pub removed_fixed_outputs: Vec<RemovedOutputTarget>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemovedOutputTarget {
    pub scope: String,
    pub endpoint_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportResult {
    pub config_path: PathBuf,
    pub recovery_directory: Option<PathBuf>,
    pub mode: ImportMode,
    pub requires_audio_setup: bool,
    pub installed_revision: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManagementResult {
    PendingChanges {
        changes: Vec<UnsavedChange>,
    },
    Card {
        document: CardDocument,
    },
    CardDeleted {
        path: PathBuf,
    },
    Exported {
        path: PathBuf,
        preview: BundlePreview,
    },
}

pub fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

pub fn read_card(path: &Path) -> Result<CardDocument, String> {
    let bytes = fs::read(path)
        .map_err(|e| format!("Room card {} could not be read: {e}", path.display()))?;
    let parsed = serde_json::from_slice::<RoomProfile>(&bytes)
        .map_err(|_| ())
        .and_then(|profile| profile.validate().map(|()| profile).map_err(|_| ()));
    let (profile, error) = match parsed {
        Ok(profile) => (Some(profile), None),
        Err(()) => (None, Some("The room card is invalid. Its original bytes were preserved. Repair the JSON or delete this card.".to_owned())),
    };
    Ok(CardDocument {
        path: path.to_owned(),
        revision: revision(&bytes),
        profile,
        error,
    })
}

pub fn save_card(
    path: &Path,
    target: RoomTarget,
    expected_revision: &str,
) -> Result<CardDocument, String> {
    target.validate()?;
    let bytes = fs::read(path)
        .map_err(|e| format!("Room card {} could not be read: {e}", path.display()))?;
    require_revision(path, &bytes, expected_revision)?;
    let mut profile: RoomProfile = serde_json::from_slice(&bytes).map_err(|_| {
        "The existing room card is invalid. Repair it before editing its target fields.".to_owned()
    })?;
    profile.validate()?;
    profile.room = target;
    rooms::save_room_profile_if_unchanged(path, &profile, &bytes).map_err(|e| e.to_string())?;
    read_card(path)
}

pub fn delete_card(config_path: &Path, path: &Path, expected_revision: &str) -> Result<(), String> {
    let actual = fs::canonicalize(path)
        .map_err(|e| format!("Room card {} could not be resolved: {e}", path.display()))?;
    let config = fs::canonicalize(config_path).map_err(|e| {
        format!(
            "Configuration {} could not be resolved: {e}",
            config_path.display()
        )
    })?;
    let library = RoomLibrary::new(&config);
    let owned = fs::canonicalize(library.cards_path()).ok();
    let selected = config::load_config(&config)
        .ok()
        .and_then(|c| c.room.selected)
        .map(startup_path);
    let selected = selected.and_then(|p| fs::canonicalize(p).ok());
    if actual == config
        || actual == library.global_rules_path()
        || !(owned.as_deref() == actual.parent() || selected.as_ref() == Some(&actual))
    {
        return Err("This file is not a room card owned or selected by this configuration.".into());
    }
    let bytes = fs::read(&actual)
        .map_err(|e| format!("Room card could not be read before deletion: {e}"))?;
    require_revision(&actual, &bytes, expected_revision)?;
    let retired = sibling(&actual, "deleted-card")?;
    move_new(&actual, &retired).map_err(|e| format!("Room card was not deleted: {e}"))?;
    if fs::read(&retired).ok().as_deref() != Some(bytes.as_slice()) {
        let restored = move_new(&retired, &actual);
        return Err(match restored {
            Ok(()) => {
                "The card changed during deletion and was restored. Reload it before trying again."
                    .into()
            }
            Err(error) => format!(
                "The card changed during deletion. Data is retained at {} and could not be restored automatically: {error}",
                retired.display()
            ),
        });
    }
    if let Err(error) = fs::remove_file(&retired) {
        return Err(match move_new(&retired, &actual) {
            Ok(()) => format!("The room card could not be deleted and was restored: {error}"),
            Err(restore) => format!(
                "The room card could not be deleted. Its data remains at {}. Delete error: {error}; restore error: {restore}",
                retired.display()
            ),
        });
    }
    Ok(())
}

fn startup_path(startup: RoomStartup) -> PathBuf {
    match startup {
        RoomStartup::Join { profile_path } | RoomStartup::Create { profile_path, .. } => {
            profile_path
        }
    }
}

fn require_revision(path: &Path, bytes: &[u8], expected: &str) -> Result<(), String> {
    if revision(bytes) != expected {
        return Err(format!(
            "{} changed after it was opened. Reload it before writing; the external file was preserved.",
            path.display()
        ));
    }
    Ok(())
}

fn card_content(bytes: &[u8]) -> CardContent {
    match serde_json::from_slice::<RoomProfile>(bytes)
        .ok()
        .filter(|p| p.validate().is_ok())
    {
        Some(profile) => CardContent::Profile { profile },
        None => match std::str::from_utf8(bytes) {
            Ok(text) => CardContent::Unreadable {
                text: Some(text.to_owned()),
                bytes_base64: None,
            },
            Err(_) => CardContent::Unreadable {
                text: None,
                bytes_base64: Some(BASE64.encode(bytes)),
            },
        },
    }
}

fn card_bytes(content: &CardContent) -> Result<Vec<u8>, String> {
    match content {
        CardContent::Profile { profile } => encode(profile),
        CardContent::Unreadable {
            text: Some(text),
            bytes_base64: None,
        } => Ok(text.as_bytes().to_vec()),
        CardContent::Unreadable {
            text: None,
            bytes_base64: Some(bytes_base64),
        } => BASE64
            .decode(bytes_base64)
            .map_err(|_| "An unreadable-card recovery record has invalid base64.".to_owned()),
        CardContent::Unreadable { .. } => Err(
            "An unreadable card needs exactly one original-text or original-bytes record.".into(),
        ),
    }
}

pub fn export_bundle(config_path: &Path, output: &Path) -> Result<BundlePreview, String> {
    let config = config::load_config(config_path)?;
    export_effective(config_path, &config, None, output)
}

pub(crate) fn export_effective(
    config_path: &Path,
    config: &ParticipantConfig,
    active: Option<ActiveRoomSettings>,
    output: &Path,
) -> Result<BundlePreview, String> {
    let library = RoomLibrary::new(config_path);
    let mut cards = Vec::new();
    let mut paths = Vec::new();
    for card in library.cards()? {
        let bytes = fs::read(&card.path).map_err(|e| {
            format!(
                "Room card {} could not be included: {e}",
                card.path.display()
            )
        })?;
        let name = card
            .path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or("A card filename is not UTF-8.")?
            .to_owned();
        paths.push(card.path.clone());
        cards.push(BundleCard {
            file_name: name,
            content: card_content(&bytes),
        });
    }
    let global = match active.as_ref().and_then(|v| v.global.clone()) {
        Some(global) => global,
        None => rooms::load_global_rules(&library.global_rules_path())?,
    };
    let mut effective = config.clone();
    let startup_source = config.room.selected.clone();
    if let Some((path, profile)) = active.as_ref().and_then(|v| v.profile.clone()) {
        profile.validate()?;
        if let Some(at) = find_path(&paths, &path) {
            cards[at].content = CardContent::Profile { profile };
        } else {
            paths.push(path.clone());
            cards.push(BundleCard {
                file_name: format!(
                    "active-{}.json",
                    SourceId::generate()
                        .map_err(|_| "An active-card export identity could not be generated.")?
                ),
                content: CardContent::Profile { profile },
            });
        }
    }
    let startup = match &startup_source {
        Some(startup) => {
            let path = startup_path(startup.clone());
            let at = find_path(&paths, &path);
            let name = if let Some(at) = at {
                cards[at].file_name.clone()
            } else {
                let name = format!(
                    "external-{}.json",
                    SourceId::generate()
                        .map_err(|_| "A card export identity could not be generated.")?
                );
                let bytes = fs::read(&path).map_err(|e| {
                    format!(
                        "The selected external card {} could not be included: {e}",
                        path.display()
                    )
                })?;
                paths.push(path.clone());
                cards.push(BundleCard {
                    file_name: name.clone(),
                    content: card_content(&bytes),
                });
                name
            };
            Some(match startup {
                RoomStartup::Join { .. } => BundleStartup::Join { card: name },
                RoomStartup::Create { bind, .. } => BundleStartup::Create {
                    card: name,
                    bind: *bind,
                },
            })
        }
        None => None,
    };
    // The bundle's startup is relative to the included card set. Runtime
    // contacts and live registrations are rebuilt from room rules on restore.
    effective.room.selected = None;
    effective.group.joined = false;
    effective.contacts.clear();
    effective.members.clear();
    let bundle = SettingsBundle {
        format: FORMAT.into(),
        schema: BUNDLE_SCHEMA,
        config: effective,
        global,
        cards,
        startup,
    };
    validate_bundle(&bundle)?;
    let bytes = encode(&bundle)?;
    write_new(output, &bytes)?;
    let mut preview = bundle_preview(&bundle, &bytes, ImportMode::Restore, None)?;
    if preview.contains_passwords {
        preview.warnings.push(
            "Room passwords or unreadable original card data are included in this settings file."
                .into(),
        );
    }
    if active.is_some_and(|v| v.has_unsaved) {
        preview.warnings.push("This export includes current effective settings that have not been saved to their original files. Export did not alter those files.".into());
    }
    Ok(preview)
}

fn find_path(paths: &[PathBuf], wanted: &Path) -> Option<usize> {
    paths.iter().position(|p| same_path(p, wanted))
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

pub fn preview_bundle(
    input: &Path,
    destination: Option<&Path>,
    mode: ImportMode,
) -> Result<BundlePreview, String> {
    let (bundle, bytes) = read_bundle(input)?;
    bundle_preview(&bundle, &bytes, mode, destination)
}

fn read_bundle(path: &Path) -> Result<(SettingsBundle, Vec<u8>), String> {
    let bytes = fs::read(path)
        .map_err(|e| format!("Settings package {} could not be read: {e}", path.display()))?;
    let bundle: SettingsBundle = serde_json::from_slice(&bytes)
        .map_err(|_| "The settings package is invalid JSON or has unsupported fields. No destination was changed.".to_owned())?;
    validate_bundle(&bundle)?;
    Ok((bundle, bytes))
}

fn validate_bundle(bundle: &SettingsBundle) -> Result<(), String> {
    if bundle.format != FORMAT || bundle.schema != BUNDLE_SCHEMA {
        return Err("This settings package format or schema is unsupported.".into());
    }
    bundle.config.validate()?;
    bundle.global.validate()?;
    if bundle.config.room.selected.is_some() {
        return Err("A settings package uses the separate relative startup card reference.".into());
    }
    let mut names = BTreeSet::new();
    for card in &bundle.cards {
        validate_card_file_name(&card.file_name)?;
        if !names.insert(card.file_name.to_ascii_lowercase()) {
            return Err("The settings package contains duplicate room card filenames.".into());
        }
        match &card.content {
            CardContent::Profile { profile } => profile.validate()?,
            CardContent::Unreadable { .. } => {
                card_bytes(&card.content)?;
            }
        }
    }
    if let Some(startup) = &bundle.startup {
        let name = match startup {
            BundleStartup::Join { card } | BundleStartup::Create { card, .. } => card,
        };
        if !bundle.cards.iter().any(|c| &c.file_name == name) {
            return Err("The startup card is missing from the settings package.".into());
        }
    }
    Ok(())
}

fn validate_card_file_name(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    let reserved = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let numbered = reserved
        .strip_prefix("COM")
        .or_else(|| reserved.strip_prefix("LPT"))
        .is_some_and(|number| number.len() == 1 && number.as_bytes()[0].is_ascii_digit());
    if !matches!(path.components().next(), Some(Component::Normal(_)))
        || path.components().count() != 1
        || !name.to_ascii_lowercase().ends_with(".json")
        || name
            .chars()
            .next_back()
            .is_some_and(|c| matches!(c, ' ' | '.'))
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || matches!(reserved.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered
    {
        return Err("A package card filename must be a single ordinary JSON filename.".into());
    }
    Ok(())
}

fn fixed_outputs(bundle: &SettingsBundle) -> Vec<RemovedOutputTarget> {
    let mut result = Vec::new();
    let mut collect = |scope: String, targets: &[OutputTarget]| {
        for target in targets {
            if let OutputTarget::Endpoint(endpoint_id) = target {
                result.push(RemovedOutputTarget {
                    scope: scope.clone(),
                    endpoint_id: endpoint_id.clone(),
                });
            }
        }
    };
    for source in &bundle.config.sources {
        collect(
            format!("local {}", source.kind.as_str()),
            &source.output_targets,
        );
    }
    for (index, rule) in bundle.global.rules.iter().enumerate() {
        if let Some(targets) = &rule.outputs {
            collect(format!("global rule {}", index + 1), targets);
        }
    }
    for card in &bundle.cards {
        if let CardContent::Profile { profile } = &card.content {
            for (index, rule) in profile.rules.iter().enumerate() {
                if let Some(targets) = &rule.outputs {
                    collect(
                        format!("room {} rule {}", profile.room.name, index + 1),
                        targets,
                    );
                }
            }
        }
    }
    result
}

fn clear_fixed_rules(rules: &mut [crate::participant::rooms::Rule]) {
    for rule in rules {
        if let Some(targets) = &mut rule.outputs {
            targets.retain(|target| matches!(target, OutputTarget::Main));
        }
    }
}

fn bundle_preview(
    bundle: &SettingsBundle,
    bytes: &[u8],
    mode: ImportMode,
    destination: Option<&Path>,
) -> Result<BundlePreview, String> {
    let cards = bundle
        .cards
        .iter()
        .map(|c| match &c.content {
            CardContent::Profile { profile } => CardPreview {
                file_name: c.file_name.clone(),
                room_name: Some(profile.room.name.clone()),
                has_password: !profile.room.password.is_empty(),
                rule_count: profile.rules.len() + profile.playback.len(),
                unreadable: false,
            },
            CardContent::Unreadable { .. } => CardPreview {
                file_name: c.file_name.clone(),
                room_name: None,
                has_password: false,
                rule_count: 0,
                unreadable: true,
            },
        })
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();
    if cards.iter().any(|c| c.unreadable) {
        warnings.push("Unreadable room cards are preserved as original bytes. They remain visible and unavailable until repaired.".into());
    }
    if mode == ImportMode::CopyToNewDevice {
        warnings.push("Copying creates a new device identity, clears local audio/network bindings, disables local monitoring and automatic room entry, and requires local setup confirmation.".into());
    }
    if bundle.config.login_startup {
        warnings.push("Windows login-startup registration is machine-local and is not imported. Enable it explicitly from the destination program.".into());
    }
    let selected_card = bundle.startup.as_ref().map(|s| match s {
        BundleStartup::Join { card } | BundleStartup::Create { card, .. } => card.clone(),
    });
    let destination_revision = destination.map(destination_revision).transpose()?.flatten();
    if destination_revision.is_some() {
        warnings.push("Applying replaces this configuration and its room library. Previous bytes are retained in a recovery directory.".into());
    }
    let removed_fixed_outputs = if mode == ImportMode::CopyToNewDevice {
        fixed_outputs(bundle)
    } else {
        Vec::new()
    };
    if !removed_fixed_outputs.is_empty() {
        warnings.push("Fixed output-device targets will be removed for the new device. Main routes remain symbolic; a fixed-only route becomes empty until reconfigured, with no fallback to Main.".into());
    }
    Ok(BundlePreview {
        source_name: bundle.config.display_name.clone(),
        source_revision: revision(bytes),
        mode,
        cards,
        selected_card,
        contains_passwords: bundle.cards.iter().any(|c| match &c.content {
            CardContent::Profile { profile } => !profile.room.password.is_empty(),
            CardContent::Unreadable { .. } => true,
        }),
        destination: destination.map(Path::to_path_buf),
        destination_revision,
        warnings,
        removed_fixed_outputs,
    })
}

/// Caller stops its running participant first. This function claims the same
/// configuration lock for the complete import publication and rollback.
pub fn import_bundle(
    input: &Path,
    destination: &Path,
    options: ImportOptions,
) -> Result<ImportResult, String> {
    validate_import_options(&options)?;
    let (mut bundle, bytes) = read_bundle(input)?;
    if options
        .expected_source_revision
        .as_ref()
        .is_some_and(|expected| *expected != revision(&bytes))
    {
        return Err(
            "The settings package changed after preview. Preview it again before applying.".into(),
        );
    }
    let destination = absolute_destination(destination)?;
    if same_path(input, &destination) {
        return Err(
            "Choose a configuration destination separate from the settings package file.".into(),
        );
    }
    let _instance = claim_destination(&destination)?;
    let original_revision = destination_revision(&destination)?;
    if original_revision != options.expected_destination_revision {
        return Err("Destination settings changed or replacement was not confirmed. Preview them again before importing.".into());
    }
    let parent = destination
        .parent()
        .ok_or("The destination needs a parent directory.")?;
    let transaction = sibling(&destination, "settings-import")?;
    fs::create_dir(&transaction)
        .map_err(|e| format!("Settings staging directory could not be created: {e}"))?;
    let new_config = transaction.join("new-config.json");
    let new_rooms = transaction.join("new-rooms");
    let cards_dir = new_rooms.join("cards");
    fs::create_dir_all(&cards_dir)
        .map_err(|e| format!("Settings staging failed at {}: {e}", transaction.display()))?;
    let result = (|| -> Result<ParticipantConfig, String> {
        let mut config = bundle.config.clone();
        config.group.joined = false;
        config.contacts.clear();
        config.members.clear();
        if options.mode == ImportMode::CopyToNewDevice {
            let name = options
                .new_name
                .as_deref()
                .ok_or("Confirm this new device's display name before copying settings.")?;
            config::validate_name("new device name", name)?;
            config.display_name = name.to_owned();
            config.login_startup = false;
            config.network = Default::default();
            config.member_id = MemberId::generate()
                .map_err(|_| "A new member identity could not be generated.")?;
            config.bind = if config.bind.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            }
            .parse()
            .expect("literal address");
            config.output_endpoint_id = None;
            config.room.auto_enter = false;
            clear_fixed_rules(&mut bundle.global.rules);
            for card in &mut bundle.cards {
                if let CardContent::Profile { profile } = &mut card.content {
                    clear_fixed_rules(&mut profile.rules);
                }
            }
            for source in &mut config.sources {
                source.id = SourceId::generate()
                    .map_err(|_| "A new source identity could not be generated.")?;
                source.endpoint_id = None;
                source.monitor = false;
                source
                    .output_targets
                    .retain(|target| matches!(target, OutputTarget::Main));
            }
        }
        let library = RoomLibrary::new(&destination);
        config.room.selected = bundle.startup.as_ref().map(|s| match s {
            BundleStartup::Join { card } => RoomStartup::Join {
                profile_path: library.cards_path().join(card),
            },
            BundleStartup::Create { card, bind } => {
                if options.mode == ImportMode::CopyToNewDevice {
                    RoomStartup::Join {
                        profile_path: library.cards_path().join(card),
                    }
                } else {
                    RoomStartup::Create {
                        profile_path: library.cards_path().join(card),
                        bind: *bind,
                    }
                }
            }
        });
        config.validate()?;
        write_new(&new_config, &encode(&config)?)?;
        write_new(&new_rooms.join("global.json"), &encode(&bundle.global)?)?;
        for card in &bundle.cards {
            write_new(
                &cards_dir.join(&card.file_name),
                &card_bytes(&card.content)?,
            )?;
        }
        Ok(config)
    })();
    if let Err(error) = result {
        return Err(format!(
            "{error} No destination was changed. Staging data remains at {}.",
            transaction.display()
        ));
    }
    let installed_revision = settings_revision(&new_config, &new_rooms)?
        .ok_or("The staged settings were not complete.")?;
    if destination_revision(&destination)? != original_revision {
        return Err(format!(
            "Destination settings changed during staging. They were not replaced. Staging remains at {}.",
            transaction.display()
        ));
    }
    let rooms = RoomLibrary::new(&destination).data_root().to_path_buf();
    if rooms.parent() != Some(parent) {
        return Err("The derived room library is outside the configuration directory.".into());
    }
    let old_config = transaction.join("previous-config.json");
    let old_rooms = transaction.join("previous-rooms");
    let (mut moved_config, mut moved_rooms, mut installed_config, mut installed_rooms) =
        (false, false, false, false);
    let commit = (|| -> std::io::Result<()> {
        if destination.exists() {
            move_new(&destination, &old_config)?;
            moved_config = true;
        }
        if rooms.exists() {
            move_new(&rooms, &old_rooms)?;
            moved_rooms = true;
        }
        move_new(&new_rooms, &rooms)?;
        installed_rooms = true;
        move_new(&new_config, &destination)?;
        installed_config = true;
        Ok(())
    })();
    if let Err(error) = commit {
        let mut rollback = Vec::new();
        if installed_config {
            if let Err(e) = move_new(&destination, &transaction.join("unpublished-config.json")) {
                rollback.push(e.to_string());
            }
        }
        if installed_rooms {
            if let Err(e) = move_new(&rooms, &transaction.join("unpublished-rooms")) {
                rollback.push(e.to_string());
            }
        }
        if moved_rooms {
            if let Err(e) = move_new(&old_rooms, &rooms) {
                rollback.push(e.to_string());
            }
        }
        if moved_config {
            if let Err(e) = move_new(&old_config, &destination) {
                rollback.push(e.to_string());
            }
        }
        return Err(format!(
            "Settings import could not be committed: {error}. Recovery data remains at {}. Rollback details: {}",
            transaction.display(),
            rollback.join("; ")
        ));
    }
    if !moved_config && !moved_rooms {
        let _ = fs::remove_dir(&transaction);
    }
    Ok(ImportResult {
        config_path: destination,
        recovery_directory: (moved_config || moved_rooms).then_some(transaction),
        mode: options.mode,
        requires_audio_setup: options.mode == ImportMode::CopyToNewDevice,
        installed_revision,
    })
}

pub fn validate_import_options(options: &ImportOptions) -> Result<(), String> {
    if options.mode == ImportMode::CopyToNewDevice {
        let name = options
            .new_name
            .as_deref()
            .ok_or("Confirm this new device's display name before copying settings.")?;
        config::validate_name("new device name", name)?;
    }
    Ok(())
}

/// Restore the previous bytes after activation of an imported configuration
/// fails. Concurrently changed imported data is retained for explicit repair.
pub fn rollback_import(result: &ImportResult) -> Result<(), String> {
    let Some(recovery) = &result.recovery_directory else {
        return Ok(());
    };
    let destination = absolute_destination(&result.config_path)?;
    let _instance = claim_destination(&destination)?;
    if recovery.parent() != destination.parent() {
        return Err("The import recovery directory is outside the configuration directory.".into());
    }
    if destination_revision(&destination)?.as_deref() != Some(result.installed_revision.as_str()) {
        return Err(format!(
            "The imported files changed after publication and were preserved. Previous data remains at {}.",
            recovery.display()
        ));
    }
    let rooms = RoomLibrary::new(&destination).data_root().to_path_buf();
    let previous_config = recovery.join("previous-config.json");
    let previous_rooms = recovery.join("previous-rooms");
    let rejected_config = recovery.join("unstarted-config.json");
    let rejected_rooms = recovery.join("unstarted-rooms");
    let (mut config_retired, mut rooms_retired, mut config_restored, mut rooms_restored) =
        (false, false, false, false);
    let restored = (|| -> std::io::Result<()> {
        if destination.exists() {
            move_new(&destination, &rejected_config)?;
            config_retired = true;
        }
        if rooms.exists() {
            move_new(&rooms, &rejected_rooms)?;
            rooms_retired = true;
        }
        if previous_config.exists() {
            move_new(&previous_config, &destination)?;
            config_restored = true;
        }
        if previous_rooms.exists() {
            move_new(&previous_rooms, &rooms)?;
            rooms_restored = true;
        }
        Ok(())
    })();
    if let Err(error) = restored {
        let mut details = Vec::new();
        if rooms_restored {
            if let Err(e) = move_new(&rooms, &previous_rooms) {
                details.push(e.to_string());
            }
        }
        if config_restored {
            if let Err(e) = move_new(&destination, &previous_config) {
                details.push(e.to_string());
            }
        }
        if rooms_retired {
            if let Err(e) = move_new(&rejected_rooms, &rooms) {
                details.push(e.to_string());
            }
        }
        if config_retired {
            if let Err(e) = move_new(&rejected_config, &destination) {
                details.push(e.to_string());
            }
        }
        return Err(format!(
            "Previous settings could not be restored automatically: {error}. Both versions remain under {}. Recovery details: {}",
            recovery.display(),
            details.join("; ")
        ));
    }
    Ok(())
}

fn absolute_destination(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Import needs an absolute destination configuration path.".into());
    }
    let parent = path
        .parent()
        .ok_or("The destination has no parent directory.")?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("The destination directory could not be prepared: {e}"))?;
    let parent = fs::canonicalize(parent)
        .map_err(|e| format!("The destination directory could not be resolved: {e}"))?;
    let path = parent.join(path.file_name().ok_or("The destination has no filename.")?);
    if path.exists() {
        fs::canonicalize(&path).map_err(|e| format!("Destination could not be resolved: {e}"))
    } else {
        Ok(path)
    }
}

fn claim_destination(path: &Path) -> Result<fs::File, String> {
    let mut name = path.as_os_str().to_owned();
    name.push(".instance-lock");
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options.open(name).map_err(|e| {
        format!(
            "Stop the GUI/CLI using this destination before import. It could not be claimed: {e}"
        )
    })
}

pub fn destination_revision(path: &Path) -> Result<Option<String>, String> {
    let rooms = RoomLibrary::new(path).data_root().to_path_buf();
    settings_revision(path, &rooms)
}

fn settings_revision(path: &Path, rooms: &Path) -> Result<Option<String>, String> {
    if !path.exists() && !rooms.exists() {
        return Ok(None);
    }
    let mut hash = Sha1::new();
    if path.exists() {
        hash.update(b"config\0");
        let bytes = fs::read(path)
            .map_err(|e| format!("Destination configuration could not be read: {e}"))?;
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    if rooms.exists() {
        hash_directory(&rooms, &rooms, &mut hash)?;
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}

fn hash_directory(root: &Path, path: &Path, hash: &mut Sha1) -> Result<(), String> {
    let mut entries = fs::read_dir(path)
        .map_err(|e| format!("Destination room directory could not be read: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        if kind.is_symlink() {
            return Err("Import does not replace a room library containing symbolic links. Choose a new destination.".into());
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy();
        hash.update((relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        if kind.is_dir() {
            hash.update(b"directory\0");
            hash_directory(root, &path, hash)?;
        } else if kind.is_file() {
            hash.update(b"file\0");
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    Ok(())
}

fn sibling(path: &Path, label: &str) -> Result<PathBuf, String> {
    let nonce = SourceId::generate()
        .map_err(|_| "A unique settings operation identity could not be generated.")?;
    Ok(path.with_file_name(format!(
        ".{}.{label}-{nonce}",
        path.file_name().unwrap_or_default().to_string_lossy()
    )))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let staged = sibling(path, "writing")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|e| format!("{} could not be staged: {e}", path.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "{} could not be completed: {error}",
            path.display()
        ));
    }
    drop(file);
    if let Err(error) = move_new(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "{} could not be published; any existing file was preserved: {error}",
            path.display()
        ));
    }
    Ok(())
}

fn move_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    use windows::core::PCWSTR;
    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| std::io::Error::other(error.to_string()))
}
