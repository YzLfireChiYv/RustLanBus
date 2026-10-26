//! Local room-card discovery and creation beside a participant configuration.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use rlb_model::SourceId;
use serde::Serialize;

use super::{
    GlobalRules, RoomProfile, create_global_rules_file, create_room_profile_file, load_room_profile,
};

#[derive(Clone, Debug)]
pub struct RoomLibrary {
    root: PathBuf,
}

/// A card's public summary. The password stays exclusively in its profile file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RoomCardView {
    pub path: PathBuf,
    pub name: Option<String>,
    pub entered_address: Option<IpAddr>,
    pub port: Option<u16>,
    pub strict_auto_join: bool,
    pub has_password: bool,
    pub rule_count: usize,
    pub error: Option<String>,
}

impl RoomLibrary {
    pub fn new(participant_config_path: &Path) -> Self {
        Self {
            root: participant_config_path.with_extension("rooms"),
        }
    }

    /// Prepare storage without replacing existing rules or room files.
    pub fn initialize(&self) -> Result<(), String> {
        let cards = self.cards_path();
        fs::create_dir_all(&cards).map_err(|error| {
            format!(
                "Room card directory {} could not be created: {error}",
                cards.display()
            )
        })?;
        let global = self.global_rules_path();
        if !global.try_exists().map_err(|error| {
            format!(
                "Global rules path {} could not be inspected: {error}",
                global.display()
            )
        })? {
            create_global_rules_file(&global, &GlobalRules::baseline())?;
        }
        Ok(())
    }

    pub fn global_rules_path(&self) -> PathBuf {
        self.root.join("global.json")
    }

    /// Allocate a prospective file name; successful room admission can publish it later.
    pub fn allocate_card_path(&self) -> Result<PathBuf, String> {
        for _ in 0..16 {
            let id = SourceId::generate()
                .map_err(|_| "A unique room card name could not be generated.".to_owned())?;
            let path = self.cards_path().join(format!("{id}.json"));
            if !path.try_exists().map_err(|error| {
                format!(
                    "Room card path {} could not be inspected: {error}",
                    path.display()
                )
            })? {
                return Ok(path);
            }
        }
        Err("A free room card file name could not be allocated.".to_owned())
    }

    pub fn create_card(&self, profile: &RoomProfile) -> Result<PathBuf, String> {
        let path = self.allocate_card_path()?;
        create_room_profile_file(&path, profile)?;
        Ok(path)
    }

    pub fn cards(&self) -> Result<Vec<RoomCardView>, String> {
        let cards = self.cards_path();
        let entries = fs::read_dir(&cards).map_err(|error| {
            format!(
                "Room card directory {} could not be read: {error}",
                cards.display()
            )
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "An entry in room card directory {} could not be read: {error}",
                    cards.display()
                )
            })?;
            let path = entry.path();
            if !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                continue;
            }
            let file_type = entry.file_type().map_err(|error| {
                format!(
                    "Room card entry {} could not be inspected: {error}",
                    path.display()
                )
            })?;
            if file_type.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths.into_iter().map(RoomCardView::load).collect())
    }

    pub fn data_root(&self) -> &Path {
        &self.root
    }

    pub fn cards_path(&self) -> PathBuf {
        self.root.join("cards")
    }
}

impl RoomCardView {
    fn load(path: PathBuf) -> Self {
        match load_room_profile(&path) {
            Ok(profile) => Self {
                path,
                name: Some(profile.room.name),
                entered_address: profile.room.address,
                port: profile.room.port,
                strict_auto_join: profile.room.strict_auto_join,
                has_password: !profile.room.password.is_empty(),
                rule_count: profile.rules.len() + profile.playback.len(),
                error: None,
            },
            Err(_) => Self {
                path,
                name: None,
                entered_address: None,
                port: None,
                strict_auto_join: false,
                has_password: false,
                rule_count: 0,
                // Parser diagnostics can quote user-supplied JSON values. Keep
                // the card visible without copying profile contents to the UI.
                error: Some(
                    "The room profile could not be read or validated. Check its JSON and room fields; the file was left unchanged."
                        .to_owned(),
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant::rooms::{Rule, RuleMatch, load_global_rules};

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rlb-room-library-{}",
            SourceId::generate().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn initialize_preserves_existing_rules_and_allocation_creates_no_card() {
        let dir = scratch();
        let library = RoomLibrary::new(&dir.join("participant.json"));
        library.initialize().unwrap();
        assert_eq!(
            library.global_rules_path(),
            dir.join("participant.rooms/global.json")
        );
        assert_eq!(
            load_global_rules(&library.global_rules_path()).unwrap(),
            GlobalRules::baseline()
        );
        let previous = b"external rules awaiting repair";
        fs::write(library.global_rules_path(), previous).unwrap();
        library.initialize().unwrap();
        assert_eq!(fs::read(library.global_rules_path()).unwrap(), previous);
        let allocated = library.allocate_card_path().unwrap();
        assert_eq!(allocated.parent(), Some(library.cards_path().as_path()));
        assert!(!allocated.exists());
        assert!(library.cards().unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cards_are_sorted_shallow_and_keep_invalid_profiles_visible_without_passwords() {
        let dir = scratch();
        let library = RoomLibrary::new(&dir.join("participant.json"));
        library.initialize().unwrap();
        let mut profile = RoomProfile::create("Headphones").unwrap();
        let secret = "private-room-password";
        profile.room.password = secret.to_owned();
        profile.room.address = Some("192.0.2.10".parse().unwrap());
        profile.room.port = Some(47111);
        profile.room.strict_auto_join = true;
        profile.rules.push(Rule {
            r#match: RuleMatch::All,
            source_kind: None,
            send: Some(true),
            listen: Some(false),
            outputs: None,
        });
        let valid = library.create_card(&profile).unwrap();
        let invalid = library.cards_path().join("invalid.json");
        let malformed =
            format!(r#"{{"schema":1,"room":{{"name":"Headphones","address":"{secret}"}}}}"#);
        fs::write(&invalid, &malformed).unwrap();
        fs::write(library.cards_path().join("ignored.txt"), "{}").unwrap();
        let nested = library.cards_path().join("nested.json");
        fs::create_dir(&nested).unwrap();
        create_room_profile_file(&nested.join("child.json"), &profile).unwrap();

        let cards = library.cards().unwrap();
        assert_eq!(cards.len(), 2);
        assert!(cards.windows(2).all(|pair| pair[0].path < pair[1].path));
        let card = cards.iter().find(|card| card.path == valid).unwrap();
        assert_eq!(card.name.as_deref(), Some("Headphones"));
        assert_eq!(card.entered_address, profile.room.address);
        assert_eq!(card.port, Some(47111));
        assert!(card.strict_auto_join);
        assert!(card.has_password);
        assert_eq!(card.rule_count, 1);
        assert!(card.error.is_none());
        let broken = cards.iter().find(|card| card.path == invalid).unwrap();
        assert!(broken.name.is_none());
        assert!(broken.error.is_some());
        assert!(!serde_json::to_string(&cards).unwrap().contains(secret));
        assert!(!format!("{cards:?}").contains(secret));
        assert_eq!(fs::read_to_string(invalid).unwrap(), malformed);
        assert_eq!(load_room_profile(&valid).unwrap(), profile);
        fs::remove_dir_all(dir).unwrap();
    }
}
