//! Schema 1 global rules and one editable JSON room profile.
//!
//! Each room is one user-edited file. Missing addresses stay unconstrained and
//! are never inferred from discovered peers. Runtime endpoints are not stored.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use super::super::config::{MixIntent, SourceKind};
use super::rules::{MemberFacts, Rule, RuleMatch};

pub const SCHEMA: u32 = 1;
pub const DEFAULT_CONTACT_PORT: u16 = 47101;

/// A contact listener in the media socket's address family. Explicit local
/// loopback runs stay local; ordinary LAN runs listen on all family interfaces.
pub fn default_contact_listener(media_bind: SocketAddr) -> SocketAddr {
    let ip = if media_bind.ip().is_loopback() {
        media_bind.ip()
    } else if media_bind.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    SocketAddr::new(ip, DEFAULT_CONTACT_PORT)
}

const NAME_LIMIT: usize = 80;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalRules {
    pub schema: u32,
    pub rules: Vec<Rule>,
}

impl GlobalRules {
    pub fn baseline() -> Self {
        Self {
            schema: SCHEMA,
            rules: vec![Rule {
                r#match: RuleMatch::All,
                source_kind: None,
                send: Some(true),
                listen: Some(false),
                outputs: None,
            }],
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "Global rules schema {} is unsupported. This reader expects schema {SCHEMA}.",
                self.schema
            ));
        }
        for rule in &self.rules {
            rule.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomTarget {
    pub name: String,
    /// User-entered. Always serialize, including JSON null. Never infer from peers.
    pub address: Option<IpAddr>,
    /// None or omitted => default 47101 at use time. Serialize as null/omit with serde default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub strict_auto_join: bool,
}

impl RoomTarget {
    pub fn effective_port(&self) -> u16 {
        match self.port {
            Some(0) | None => DEFAULT_CONTACT_PORT,
            Some(port) => port,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_name("room name", &self.name)?;
        if let Some(address) = self.address {
            validate_address(address)?;
        }
        validate_password(&self.password)?;
        Ok(())
    }
}

impl fmt::Debug for RoomTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomTarget")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("port", &self.port)
            .field("password", &"***")
            .field("strict_auto_join", &self.strict_auto_join)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPlaybackRule {
    pub condition: RuleMatch,
    pub source_kind: SourceKind,
    pub playback: MixPatch,
}

/// Omitted fields retain each matching member's independently resolved value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pan: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solo: Option<bool>,
}

impl MixPatch {
    pub fn is_empty(self) -> bool {
        self.gain_db.is_none() && self.pan.is_none() && self.mute.is_none() && self.solo.is_none()
    }

    pub fn validate(self, label: &str) -> Result<(), String> {
        if self.is_empty() {
            return Err(format!("{label} must change gain, pan, mute, or solo."));
        }
        let mut resolved = MixIntent::unity();
        self.apply(&mut resolved);
        resolved.validate(label)
    }

    pub fn apply(self, target: &mut MixIntent) {
        if let Some(value) = self.gain_db {
            target.gain_db = value;
        }
        if let Some(value) = self.pan {
            target.pan = value;
        }
        if let Some(value) = self.mute {
            target.mute = value;
        }
        if let Some(value) = self.solo {
            target.solo = value;
        }
    }

    fn remove_fields(&mut self, changed: Self) {
        if changed.gain_db.is_some() {
            self.gain_db = None;
        }
        if changed.pan.is_some() {
            self.pan = None;
        }
        if changed.mute.is_some() {
            self.mute = None;
        }
        if changed.solo.is_some() {
            self.solo = None;
        }
    }
}

impl From<MixIntent> for MixPatch {
    fn from(value: MixIntent) -> Self {
        Self {
            gain_db: Some(value.gain_db),
            pan: Some(value.pan),
            mute: Some(value.mute),
            solo: Some(value.solo),
        }
    }
}

pub fn evaluate_playback(
    rules: &[RoomPlaybackRule],
    member: &MemberFacts,
    source_kind: SourceKind,
) -> Option<MixIntent> {
    let mut resolved = MixIntent::unity();
    let mut matched = false;
    for rule in rules {
        if rule.source_kind == source_kind && rule.condition.matches(member) {
            rule.playback.apply(&mut resolved);
            matched = true;
        }
    }
    matched.then_some(resolved)
}

/// Compact changed fields for this exact user scope, preserving the others in
/// their original positions before appending the new assignment.
pub fn apply_playback_edit(
    rules: &mut Vec<RoomPlaybackRule>,
    changed: RoomPlaybackRule,
) -> Result<(), String> {
    changed.validate()?;
    rules.retain_mut(|existing| {
        if existing.condition == changed.condition && existing.source_kind == changed.source_kind {
            existing.playback.remove_fields(changed.playback);
        }
        !existing.playback.is_empty()
    });
    rules.push(changed);
    Ok(())
}

impl RoomPlaybackRule {
    pub fn validate(&self) -> Result<(), String> {
        self.condition.validate()?;
        self.playback.validate("room source mix")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomProfile {
    pub schema: u32,
    pub room: RoomTarget,
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub playback: Vec<RoomPlaybackRule>,
}

impl RoomProfile {
    pub fn create(name: &str) -> Result<Self, String> {
        let profile = Self {
            schema: SCHEMA,
            room: RoomTarget {
                name: name.to_owned(),
                address: None,
                port: None,
                password: String::new(),
                strict_auto_join: false,
            },
            rules: Vec::new(),
            playback: Vec::new(),
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "Room profile schema {} is unsupported. This reader expects schema {SCHEMA}.",
                self.schema
            ));
        }
        self.room.validate()?;
        for rule in &self.rules {
            rule.validate()?;
        }
        for rule in &self.playback {
            rule.validate()?;
        }
        Ok(())
    }
}

fn validate_name(label: &str, name: &str) -> Result<(), String> {
    let length = name.chars().count();
    if name.trim().is_empty() || length > NAME_LIMIT || name.chars().any(|ch| ch.is_control()) {
        return Err(format!(
            "The {label} must contain 1 to {NAME_LIMIT} characters and no control characters."
        ));
    }
    Ok(())
}

fn validate_address(address: IpAddr) -> Result<(), String> {
    if address.is_unspecified()
        || address.is_multicast()
        || matches!(address, IpAddr::V4(ip) if ip.is_broadcast())
    {
        return Err(
            "A room address must be an individual device address. Unspecified, multicast, and IPv4 broadcast values are rejected.".into(),
        );
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.chars().count() > rlb_contact::MAX_PASSWORD_CHARS {
        return Err(format!(
            "The room password must contain at most {} characters.",
            rlb_contact::MAX_PASSWORD_CHARS
        ));
    }
    if password.chars().any(|ch| ch.is_control()) {
        return Err("The room password must not contain control characters.".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "pw-token-7f3a9c2e-not-for-logs";

    #[test]
    fn omitted_playback_is_empty_and_invalid_mix_is_rejected() {
        let mut value = serde_json::to_value(RoomProfile::create("Headphones").unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("playback");
        let mut profile: RoomProfile = serde_json::from_value(value).unwrap();
        assert!(profile.playback.is_empty());
        profile.playback.push(RoomPlaybackRule {
            condition: RuleMatch::All,
            source_kind: SourceKind::System,
            playback: MixIntent {
                pan: 2.0,
                ..MixIntent::unity()
            }
            .into(),
        });
        assert!(profile.validate().is_err());
    }

    #[test]
    fn create_starts_with_an_unconstrained_address_and_empty_room_rules() {
        let profile = RoomProfile::create("Headphones").unwrap();
        assert_eq!(profile.schema, SCHEMA);
        assert_eq!(profile.room.name, "Headphones");
        assert_eq!(profile.room.address, None);
        assert_eq!(profile.room.port, None);
        assert_eq!(profile.room.effective_port(), DEFAULT_CONTACT_PORT);
        assert_eq!(profile.room.password, "");
        assert!(!profile.room.strict_auto_join);
        assert!(profile.rules.is_empty());
        let text = serde_json::to_string_pretty(&profile).unwrap();
        assert!(text.contains("\"address\": null"), "{text}");
        assert!(!text.contains("\"port\""), "{text}");
    }

    #[test]
    fn baseline_sends_to_everyone_and_listens_to_nobody() {
        let rules = GlobalRules::baseline();
        assert_eq!(rules.schema, SCHEMA);
        assert_eq!(rules.rules.len(), 1);
        rules.validate().unwrap();
        assert_eq!(rules.rules[0].r#match, RuleMatch::All);
        assert_eq!(rules.rules[0].source_kind, None);
        assert_eq!(rules.rules[0].send, Some(true));
        assert_eq!(rules.rules[0].listen, Some(false));
    }

    #[test]
    fn profile_preserves_optional_source_scope_without_changing_schema() {
        let mut profile = RoomProfile::create("Headphones").unwrap();
        profile.rules.push(Rule {
            r#match: RuleMatch::Condition {
                name: Some("Ada".into()),
                ip: None,
            },
            source_kind: Some(SourceKind::Microphone),
            send: None,
            listen: Some(false),
            outputs: None,
        });
        profile.validate().unwrap();
        let encoded = serde_json::to_value(&profile).unwrap();
        assert_eq!(encoded["schema"], SCHEMA);
        assert_eq!(encoded["rules"][0]["source_kind"], "microphone");
        assert_eq!(
            serde_json::from_value::<RoomProfile>(encoded).unwrap(),
            profile
        );
    }

    #[test]
    fn omitted_or_zero_port_uses_the_default_contact_port() {
        let json = r#"{
            "schema": 1,
            "room": {
                "name": "Headphones",
                "address": null,
                "password": "",
                "strict_auto_join": false
            },
            "rules": []
        }"#;
        let mut profile: RoomProfile = serde_json::from_str(json).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.room.port, None);
        assert_eq!(profile.room.effective_port(), DEFAULT_CONTACT_PORT);
        profile.room.port = Some(0);
        profile.validate().unwrap();
        assert_eq!(profile.room.effective_port(), DEFAULT_CONTACT_PORT);
        profile.room.port = Some(47111);
        assert_eq!(profile.room.effective_port(), 47111);
    }

    #[test]
    fn validation_rejects_schema_names_and_unusable_addresses() {
        let mut rules = GlobalRules::baseline();
        rules.schema = 2;
        assert!(
            rules
                .validate()
                .unwrap_err()
                .contains("Global rules schema 2")
        );

        let mut profile = RoomProfile::create("Headphones").unwrap();
        profile.schema = 2;
        assert!(
            profile
                .validate()
                .unwrap_err()
                .contains("Room profile schema 2")
        );

        profile.schema = SCHEMA;
        profile.room.name.clear();
        assert!(profile.validate().is_err());
        profile.room.name = "n".repeat(81);
        assert!(profile.validate().is_err());
        profile.room.name = "n".repeat(80);
        profile.validate().unwrap();
        profile.room.name = "bad\nname".into();
        assert!(profile.validate().is_err());

        profile.room.name = "Headphones".into();
        profile.room.address = Some("0.0.0.0".parse().unwrap());
        assert!(profile.validate().is_err());
        profile.room.address = Some("::".parse().unwrap());
        assert!(profile.validate().is_err());
        profile.room.address = Some("224.0.0.1".parse().unwrap());
        assert!(profile.validate().is_err());
        profile.room.address = Some("255.255.255.255".parse().unwrap());
        assert!(profile.validate().is_err());
        profile.room.address = Some("192.0.2.27".parse().unwrap());
        profile.validate().unwrap();
    }

    #[test]
    fn extra_fields_and_wrong_shapes_are_rejected_by_serde() {
        let extra_global = r#"{"schema":1,"rules":[],"unexpected":true}"#;
        assert!(serde_json::from_str::<GlobalRules>(extra_global).is_err());
        let extra_room = r#"{
            "schema": 1,
            "room": {
                "name": "Headphones",
                "address": null,
                "password": "",
                "strict_auto_join": false
            },
            "rules": [],
            "unexpected": true
        }"#;
        assert!(serde_json::from_str::<RoomProfile>(extra_room).is_err());

        let global = serde_json::to_string(&GlobalRules::baseline()).unwrap();
        assert!(serde_json::from_str::<RoomProfile>(&global).is_err());
        let room = serde_json::to_string(&RoomProfile::create("Headphones").unwrap()).unwrap();
        assert!(serde_json::from_str::<GlobalRules>(&room).is_err());
    }

    #[test]
    fn password_stays_out_of_debug_and_validation_errors() {
        let mut profile = RoomProfile::create("Headphones").unwrap();
        profile.room.password = SECRET.into();
        profile.validate().unwrap();
        let debug = format!("{profile:?}");
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(debug.contains("***"), "{debug}");

        profile.room.password = format!("{SECRET}\n");
        let error = profile.validate().unwrap_err();
        assert!(error.contains("control characters"), "{error}");
        assert!(!error.contains(SECRET), "{error}");

        profile.room.password = "音".repeat(rlb_contact::MAX_PASSWORD_CHARS);
        profile.validate().unwrap();
        profile.room.password.push('音');
        assert!(
            profile
                .validate()
                .unwrap_err()
                .contains("at most 128 characters")
        );
    }

    #[test]
    fn default_listener_retains_media_family_and_explicit_loopback_scope() {
        for (media, expected) in [
            ("0.0.0.0:5004", "0.0.0.0:47101"),
            ("192.0.2.1:5004", "0.0.0.0:47101"),
            ("[::]:5004", "[::]:47101"),
            ("[2001:db8::1]:5004", "[::]:47101"),
            ("127.0.0.1:5004", "127.0.0.1:47101"),
            ("[::1]:5004", "[::1]:47101"),
        ] {
            assert_eq!(
                default_contact_listener(media.parse().unwrap()),
                expected.parse::<SocketAddr>().unwrap()
            );
        }
    }
}
