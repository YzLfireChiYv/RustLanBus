//! Ordered send, listen, and output assignments for trusted LAN members.
//!
//! Evaluation starts at send=true and listen=false, then applies global rules
//! in array order and room rules in array order. Later matching assignments
//! win for each specified field. Omitted fields leave that field
//! unchanged.

use std::fmt;
use std::net::IpAddr;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::super::config::SourceKind;
use super::super::output_routing::{OutputTarget, default_output_targets, validate_output_targets};

const NAME_LIMIT: usize = 80;

/// Selector for the members a directional assignment applies to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuleMatch {
    All,
    Condition {
        ip: Option<IpAddr>,
        name: Option<String>,
    },
}

/// One assignment. At least one of `send`, `listen`, or `outputs` must be set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(rename = "match")]
    pub r#match: RuleMatch,
    /// Absent means every current and future source from the matched members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<SourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<OutputTarget>>,
}

/// Observed member identity used while evaluating rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberFacts {
    pub name: String,
    pub ip: IpAddr,
}

/// Which assignment list produced a resolved direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleListKind {
    Global,
    Room,
}

/// Location of the last matching assignment for one direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AssignmentOrigin {
    pub list: RuleListKind,
    /// Zero-based index in the current rule list.
    pub index: usize,
}

/// Effective send and listen choices after walking global then room rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDirections {
    pub send: bool,
    pub listen: bool,
    pub send_from: Option<AssignmentOrigin>,
    pub listen_from: Option<AssignmentOrigin>,
    pub outputs: Vec<OutputTarget>,
    pub outputs_from: Option<AssignmentOrigin>,
}

#[derive(Serialize)]
struct MatchObjectSer<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<&'a IpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

impl RuleMatch {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::All => Ok(()),
            Self::Condition { ip, name } => {
                if ip.is_none() && name.is_none() {
                    return Err(
                        "A match condition must include an IP address, a member name, or both."
                            .to_owned(),
                    );
                }
                if let Some(name) = name {
                    validate_member_name(name)?;
                }
                Ok(())
            }
        }
    }

    /// Reports whether this selector applies to `member`.
    ///
    /// `All` matches every member. A condition matches when every specified
    /// field matches: names use exact whole-string equality, and IPv4-mapped
    /// IPv6 addresses compare as IPv4. A missing IP or name field is
    /// unconstrained. An empty condition matches nobody.
    pub fn matches(&self, member: &MemberFacts) -> bool {
        match self {
            Self::All => true,
            Self::Condition { ip, name } => {
                if ip.is_none() && name.is_none() {
                    return false;
                }
                if let Some(expected_ip) = ip {
                    if normalize_ip(*expected_ip) != normalize_ip(member.ip) {
                        return false;
                    }
                }
                if let Some(expected_name) = name {
                    if expected_name != &member.name {
                        return false;
                    }
                }
                true
            }
        }
    }
}

impl Serialize for RuleMatch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::All => serializer.serialize_str("all"),
            Self::Condition { ip, name } => MatchObjectSer {
                ip: ip.as_ref(),
                name: name.as_deref(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for RuleMatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RuleMatchVisitor;

        impl<'de> Visitor<'de> for RuleMatchVisitor {
            type Value = RuleMatch;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("the string \"all\" or a match object with ip and/or name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "all" {
                    Ok(RuleMatch::All)
                } else {
                    Err(E::invalid_value(
                        de::Unexpected::Str(value),
                        &"the string \"all\"",
                    ))
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                const FIELDS: &[&str] = &["ip", "name"];
                let mut ip = None;
                let mut name = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ip" => {
                            if ip.is_some() {
                                return Err(de::Error::duplicate_field("ip"));
                            }
                            ip = Some(map.next_value()?);
                        }
                        "name" => {
                            if name.is_some() {
                                return Err(de::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        other => return Err(de::Error::unknown_field(other, FIELDS)),
                    }
                }
                let constructed = RuleMatch::Condition { ip, name };
                constructed.validate().map_err(de::Error::custom)?;
                Ok(constructed)
            }
        }

        deserializer.deserialize_any(RuleMatchVisitor)
    }
}

impl Rule {
    pub fn validate(&self) -> Result<(), String> {
        self.r#match.validate()?;
        if self.send.is_none() && self.listen.is_none() && self.outputs.is_none() {
            return Err("A rule must set send, listen, or outputs.".to_owned());
        }
        if let Some(outputs) = &self.outputs {
            validate_output_targets(outputs)?;
        }
        Ok(())
    }
}

/// Resolves the member-wide defaults, ignoring source-qualified assignments.
///
/// Starts send=true, listen=false, then applies global rules followed by room
/// rules. Use [`evaluate_source`] for actual source delivery choices.
pub fn evaluate(
    global_rules: &[Rule],
    room_rules: &[Rule],
    member: &MemberFacts,
) -> ResolvedDirections {
    evaluate_matching(global_rules, room_rules, member, None)
}

/// Resolves one source kind using wildcard and exact-kind assignments in the
/// same global-then-room array order. Source-specific assignments have no
/// implicit priority over a later wildcard assignment.
pub fn evaluate_source(
    global_rules: &[Rule],
    room_rules: &[Rule],
    member: &MemberFacts,
    source_kind: SourceKind,
) -> ResolvedDirections {
    evaluate_matching(global_rules, room_rules, member, Some(source_kind))
}

fn evaluate_matching(
    global_rules: &[Rule],
    room_rules: &[Rule],
    member: &MemberFacts,
    source_kind: Option<SourceKind>,
) -> ResolvedDirections {
    let mut resolved = ResolvedDirections {
        send: true,
        listen: false,
        send_from: None,
        listen_from: None,
        outputs: default_output_targets(),
        outputs_from: None,
    };
    apply_list(
        &mut resolved,
        RuleListKind::Global,
        global_rules,
        member,
        source_kind,
    );
    apply_list(
        &mut resolved,
        RuleListKind::Room,
        room_rules,
        member,
        source_kind,
    );
    resolved
}

/// Updates specified direction fields for an exact-same room condition.
///
/// Superseded fields on matching rows are removed. Any untouched field stays
/// as a split rule at its old relative position. The edited field or fields
/// are appended as one new assignment.
pub fn apply_room_edit(
    rules: &mut Vec<Rule>,
    condition: &RuleMatch,
    send: Option<bool>,
    listen: Option<bool>,
) -> Result<(), String> {
    apply_source_edit(rules, condition, None, send, listen)
}

/// Updates an exact member-and-source selector, preserving every untouched
/// direction and the order of other assignments. An absent kind edits only
/// wildcard rows; the appended wildcard still overrides earlier source rows
/// when they are evaluated.
pub fn apply_source_edit(
    rules: &mut Vec<Rule>,
    condition: &RuleMatch,
    source_kind: Option<SourceKind>,
    send: Option<bool>,
    listen: Option<bool>,
) -> Result<(), String> {
    apply_rule_edit(
        rules,
        &Rule {
            r#match: condition.clone(),
            source_kind,
            send,
            listen,
            outputs: None,
        },
    )
}

/// Replaces only the specified fields of an exact member-and-source selector.
/// Untouched assignments retain their original position; edits are appended.
pub fn apply_rule_edit(rules: &mut Vec<Rule>, edit: &Rule) -> Result<(), String> {
    edit.validate()?;

    rules.retain_mut(|rule| {
        if rule.r#match != edit.r#match || rule.source_kind != edit.source_kind {
            return true;
        }
        if edit.send.is_some() {
            rule.send = None;
        }
        if edit.listen.is_some() {
            rule.listen = None;
        }
        if edit.outputs.is_some() {
            rule.outputs = None;
        }
        rule.send.is_some() || rule.listen.is_some() || rule.outputs.is_some()
    });
    rules.push(edit.clone());
    Ok(())
}

fn apply_list(
    resolved: &mut ResolvedDirections,
    list: RuleListKind,
    rules: &[Rule],
    member: &MemberFacts,
    source_kind: Option<SourceKind>,
) {
    for (index, rule) in rules.iter().enumerate() {
        if (rule.source_kind.is_some() && rule.source_kind != source_kind)
            || !rule.r#match.matches(member)
        {
            continue;
        }
        if let Some(send) = rule.send {
            resolved.send = send;
            resolved.send_from = Some(AssignmentOrigin { list, index });
        }
        if let Some(listen) = rule.listen {
            resolved.listen = listen;
            resolved.listen_from = Some(AssignmentOrigin { list, index });
        }
        if let Some(outputs) = &rule.outputs {
            resolved.outputs.clone_from(outputs);
            resolved.outputs_from = Some(AssignmentOrigin { list, index });
        }
    }
}

fn validate_member_name(name: &str) -> Result<(), String> {
    let length = name.chars().count();
    if name.trim().is_empty() || length > NAME_LIMIT || name.chars().any(|ch| ch.is_control()) {
        return Err(format!(
            "The member name must contain 1 to {NAME_LIMIT} characters and no control characters."
        ));
    }
    Ok(())
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        IpAddr::V4(_) => ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn member(name: &str, addr: &str) -> MemberFacts {
        MemberFacts {
            name: name.to_owned(),
            ip: ip(addr),
        }
    }

    fn name_only(name: &str) -> RuleMatch {
        RuleMatch::Condition {
            ip: None,
            name: Some(name.to_owned()),
        }
    }

    fn ip_only(addr: &str) -> RuleMatch {
        RuleMatch::Condition {
            ip: Some(ip(addr)),
            name: None,
        }
    }

    fn ip_and_name(addr: &str, name: &str) -> RuleMatch {
        RuleMatch::Condition {
            ip: Some(ip(addr)),
            name: Some(name.to_owned()),
        }
    }

    fn rule(selector: RuleMatch, send: Option<bool>, listen: Option<bool>) -> Rule {
        Rule {
            r#match: selector,
            source_kind: None,
            send,
            listen,
            outputs: None,
        }
    }

    fn source_rule(
        selector: RuleMatch,
        source_kind: SourceKind,
        send: Option<bool>,
        listen: Option<bool>,
    ) -> Rule {
        Rule {
            source_kind: Some(source_kind),
            ..rule(selector, send, listen)
        }
    }

    #[test]
    fn outputs_follow_the_same_order_without_changing_transport_consent() {
        let ada = member("Ada", "192.0.2.10");
        let headphones = vec![OutputTarget::Endpoint("headphones".into())];
        let virtual_mic = vec![OutputTarget::Endpoint("virtual-microphone".into())];
        let global = [Rule {
            outputs: Some(headphones.clone()),
            ..rule(RuleMatch::All, Some(false), Some(true))
        }];
        let room = [
            Rule {
                outputs: Some(virtual_mic.clone()),
                ..source_rule(name_only("Ada"), SourceKind::Microphone, None, None)
            },
            rule(name_only("Ada"), Some(true), None),
        ];
        let microphone = evaluate_source(&global, &room, &ada, SourceKind::Microphone);
        assert!(microphone.send && microphone.listen);
        assert_eq!(microphone.outputs, virtual_mic);
        assert_eq!(
            microphone.outputs_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 0
            })
        );
        assert_eq!(
            microphone.send_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 1
            })
        );
        assert_eq!(
            microphone.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Global,
                index: 0
            })
        );
        let system = evaluate_source(&global, &room, &ada, SourceKind::System);
        assert_eq!(system.outputs, headphones);
        assert_eq!(system.outputs_from, system.listen_from);
        assert_eq!(evaluate(&[], &[], &ada).outputs, default_output_targets());
        assert_eq!(evaluate(&[], &[], &ada).outputs_from, None);
    }

    #[test]
    fn empty_outputs_disable_rendering_while_listening_stays_enabled() {
        let assignment: Rule = serde_json::from_value(json!({
            "match": {"name": "Ada"}, "source_kind": "system", "outputs": []
        }))
        .unwrap();
        assignment.validate().unwrap();
        assert_eq!(assignment.send, None);
        assert_eq!(assignment.listen, None);
        let resolved = evaluate_source(
            &[rule(RuleMatch::All, None, Some(true))],
            std::slice::from_ref(&assignment),
            &member("Ada", "192.0.2.10"),
            SourceKind::System,
        );
        assert!(resolved.listen);
        assert!(resolved.outputs.is_empty());
        assert_eq!(
            serde_json::to_value(assignment).unwrap(),
            json!({
                "match": {"name": "Ada"}, "source_kind": "system", "outputs": []
            })
        );
    }

    #[test]
    fn output_edit_preserves_direction_fields_and_source_scope() {
        let ada = name_only("Ada");
        let old = Rule {
            outputs: Some(vec![OutputTarget::Endpoint("old".into())]),
            ..source_rule(ada.clone(), SourceKind::System, Some(false), Some(true))
        };
        let untouched = Rule {
            outputs: Some(vec![OutputTarget::Main]),
            ..source_rule(ada.clone(), SourceKind::Microphone, None, None)
        };
        let edit = Rule {
            outputs: Some(vec![]),
            ..source_rule(ada.clone(), SourceKind::System, None, None)
        };
        let mut rules = vec![old, untouched.clone()];
        apply_rule_edit(&mut rules, &edit).unwrap();
        assert_eq!(
            rules,
            vec![
                source_rule(ada.clone(), SourceKind::System, Some(false), Some(true)),
                untouched.clone(),
                edit,
            ]
        );
        apply_source_edit(
            &mut rules,
            &ada,
            Some(SourceKind::System),
            None,
            Some(false),
        )
        .unwrap();
        assert_eq!(rules[1], untouched);
        assert!(rules.iter().any(|rule| rule.outputs == Some(vec![])));
        let resolved = evaluate_source(
            &[],
            &rules,
            &member("Ada", "192.0.2.10"),
            SourceKind::System,
        );
        assert!(!resolved.send && !resolved.listen);
        assert!(resolved.outputs.is_empty());
        let before = rules.clone();
        let invalid = Rule {
            outputs: Some(vec![OutputTarget::Main, OutputTarget::Main]),
            ..source_rule(ada, SourceKind::System, None, None)
        };
        assert!(apply_rule_edit(&mut rules, &invalid).is_err());
        assert_eq!(rules, before);
    }

    #[test]
    fn source_assignments_share_array_order_and_keep_member_defaults_separate() {
        let ada = member("Ada", "192.0.2.10");
        let global = [
            rule(RuleMatch::All, Some(true), Some(true)),
            source_rule(
                RuleMatch::All,
                SourceKind::Microphone,
                Some(false),
                Some(false),
            ),
        ];
        let room = [
            rule(name_only("Ada"), None, Some(false)),
            source_rule(name_only("Ada"), SourceKind::System, None, Some(true)),
        ];

        let microphone = evaluate_source(&global, &room, &ada, SourceKind::Microphone);
        assert!(!microphone.send);
        assert!(!microphone.listen);
        assert_eq!(
            microphone.send_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Global,
                index: 1,
            })
        );
        assert_eq!(
            microphone.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 0,
            })
        );

        let system = evaluate_source(&global, &room, &ada, SourceKind::System);
        assert!(system.send);
        assert!(system.listen);
        assert_eq!(
            system.send_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Global,
                index: 0,
            })
        );
        assert_eq!(
            system.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 1,
            })
        );

        let member_defaults = evaluate(&global, &room, &ada);
        assert!(member_defaults.send);
        assert!(!member_defaults.listen);
        assert_eq!(member_defaults.send_from, system.send_from);
        assert_eq!(member_defaults.listen_from, microphone.listen_from);
    }

    #[test]
    fn later_wildcard_overrides_source_assignment_without_erasing_other_fields() {
        let ada = member("Ada", "192.0.2.10");
        let global = [
            source_rule(
                name_only("Ada"),
                SourceKind::Microphone,
                Some(false),
                Some(false),
            ),
            rule(name_only("Ada"), None, Some(true)),
        ];
        let before_room = evaluate_source(&global, &[], &ada, SourceKind::Microphone);
        assert!(!before_room.send);
        assert!(before_room.listen);
        assert_eq!(
            before_room.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Global,
                index: 1,
            })
        );

        let room = [source_rule(
            name_only("Ada"),
            SourceKind::Microphone,
            None,
            Some(false),
        )];
        let after_room = evaluate_source(&global, &room, &ada, SourceKind::Microphone);
        assert!(!after_room.send);
        assert!(!after_room.listen);
        assert_eq!(after_room.send_from, before_room.send_from);
        assert_eq!(
            after_room.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 0,
            })
        );
    }

    #[test]
    fn source_rules_keep_name_ip_and_combined_member_scopes() {
        let rules = [
            source_rule(name_only("Ada"), SourceKind::Microphone, None, Some(true)),
            source_rule(
                ip_only("192.0.2.10"),
                SourceKind::Microphone,
                Some(false),
                None,
            ),
            source_rule(
                ip_and_name("192.0.2.10", "Ada"),
                SourceKind::System,
                None,
                Some(true),
            ),
        ];
        for address in ["192.0.2.10", "::ffff:192.0.2.10"] {
            let ada = member("Ada", address);
            let microphone = evaluate_source(&[], &rules, &ada, SourceKind::Microphone);
            assert!(!microphone.send);
            assert!(microphone.listen);
            let system = evaluate_source(&[], &rules, &ada, SourceKind::System);
            assert!(system.send);
            assert!(system.listen);
        }
        let other_ada = member("Ada", "192.0.2.11");
        let microphone = evaluate_source(&[], &rules, &other_ada, SourceKind::Microphone);
        assert!(microphone.send);
        assert!(microphone.listen);
        assert!(!evaluate_source(&[], &rules, &other_ada, SourceKind::System).listen);
        let bea = member("Bea", "192.0.2.10");
        let microphone = evaluate_source(&[], &rules, &bea, SourceKind::Microphone);
        assert!(!microphone.send);
        assert!(!microphone.listen);
        assert!(!evaluate_source(&[], &rules, &bea, SourceKind::System).listen);
    }

    #[test]
    fn source_edit_compacts_only_exact_selector_and_preserves_other_directions() {
        let ada = name_only("Ada");
        let wildcard = rule(ada.clone(), Some(true), Some(true));
        let system = source_rule(ada.clone(), SourceKind::System, Some(false), Some(false));
        let address_specific = source_rule(
            ip_and_name("192.0.2.10", "Ada"),
            SourceKind::Microphone,
            Some(true),
            Some(false),
        );
        let mut rules = vec![
            wildcard.clone(),
            source_rule(ada.clone(), SourceKind::Microphone, Some(false), Some(true)),
            system.clone(),
            source_rule(ada.clone(), SourceKind::Microphone, None, Some(true)),
            address_specific.clone(),
        ];
        apply_source_edit(
            &mut rules,
            &ada,
            Some(SourceKind::Microphone),
            None,
            Some(false),
        )
        .unwrap();
        assert_eq!(
            rules,
            vec![
                wildcard,
                source_rule(ada.clone(), SourceKind::Microphone, Some(false), None),
                system,
                address_specific,
                source_rule(ada.clone(), SourceKind::Microphone, None, Some(false)),
            ]
        );
        let before = rules.clone();
        assert!(
            apply_source_edit(&mut rules, &ada, Some(SourceKind::Microphone), None, None).is_err()
        );
        assert_eq!(rules, before);

        apply_room_edit(&mut rules, &ada, None, Some(true)).unwrap();
        assert_eq!(rules.len(), before.len() + 1);
        assert_eq!(rules[0], rule(ada.clone(), Some(true), None));
        assert_eq!(&rules[1..before.len()], &before[1..]);
        assert_eq!(rules.last(), Some(&rule(ada, None, Some(true))));
        let member = member("Ada", "192.0.2.10");
        for source in [SourceKind::System, SourceKind::Microphone] {
            assert!(evaluate_source(&[], &rules, &member, source).listen);
        }
    }

    #[test]
    fn source_selector_json_is_optional_and_uses_stable_kind_names() {
        let wildcard: Rule = serde_json::from_value(json!({
            "match": { "name": "Ada" }, "listen": true
        }))
        .unwrap();
        assert_eq!(wildcard.source_kind, None);
        assert_eq!(
            serde_json::to_value(wildcard).unwrap(),
            json!({"match": {"name": "Ada"}, "listen": true})
        );
        let source = source_rule(name_only("Ada"), SourceKind::Microphone, None, Some(false));
        let encoded = serde_json::to_value(&source).unwrap();
        assert_eq!(
            encoded,
            json!({"match": {"name": "Ada"}, "source_kind": "microphone", "listen": false})
        );
        assert_eq!(serde_json::from_value::<Rule>(encoded).unwrap(), source);
        assert!(
            serde_json::from_value::<Rule>(json!({
                "match": "all", "source_kind": "unrecognized", "listen": true
            }))
            .is_err()
        );
    }

    #[test]
    fn later_matching_assignments_win_per_direction() {
        let ada = member("Ada", "192.0.2.10");
        let bea = member("Bea", "192.0.2.11");
        let global = [
            rule(RuleMatch::All, Some(true), Some(true)),
            rule(name_only("Ada"), Some(false), None),
        ];
        let room = [
            rule(name_only("Ada"), None, Some(false)),
            rule(RuleMatch::All, None, Some(true)),
        ];

        let ada_resolved = evaluate(&global, &room, &ada);
        assert!(!ada_resolved.send);
        assert!(ada_resolved.listen);
        assert_eq!(
            ada_resolved.send_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Global,
                index: 1,
            })
        );
        assert_eq!(
            ada_resolved.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 1,
            })
        );

        let bea_resolved = evaluate(&global, &room, &bea);
        assert!(bea_resolved.send);
        assert!(bea_resolved.listen);
        assert_eq!(
            bea_resolved.send_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Global,
                index: 0,
            })
        );
        assert_eq!(
            bea_resolved.listen_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 1,
            })
        );

        let defaults = evaluate(&[], &[], &ada);
        assert!(defaults.send);
        assert!(!defaults.listen);
        assert_eq!(defaults.send_from, None);
        assert_eq!(defaults.listen_from, None);
    }

    #[test]
    fn match_scopes_cover_all_ip_name_and_combined() {
        let ada_a = member("Ada", "192.0.2.10");
        let ada_b = member("Ada", "192.0.2.11");
        let bea = member("Bea", "192.0.2.10");
        let ada_mapped = member("Ada", "::ffff:192.0.2.10");

        let listen_all = [rule(RuleMatch::All, None, Some(true))];
        assert!(evaluate(&[], &listen_all, &ada_a).listen);
        assert!(evaluate(&[], &listen_all, &bea).listen);

        let listen_ip = [rule(ip_only("192.0.2.10"), None, Some(true))];
        assert!(evaluate(&[], &listen_ip, &ada_a).listen);
        assert!(evaluate(&[], &listen_ip, &bea).listen);
        assert!(!evaluate(&[], &listen_ip, &ada_b).listen);
        assert!(evaluate(&[], &listen_ip, &ada_mapped).listen);

        let listen_name = [rule(name_only("Ada"), None, Some(true))];
        assert!(evaluate(&[], &listen_name, &ada_a).listen);
        assert!(evaluate(&[], &listen_name, &ada_b).listen);
        assert!(!evaluate(&[], &listen_name, &bea).listen);

        let listen_both = [rule(ip_and_name("192.0.2.10", "Ada"), None, Some(true))];
        assert!(evaluate(&[], &listen_both, &ada_a).listen);
        assert!(!evaluate(&[], &listen_both, &ada_b).listen);
        assert!(!evaluate(&[], &listen_both, &bea).listen);
        assert!(evaluate(&[], &listen_both, &ada_mapped).listen);
    }

    #[test]
    fn room_edit_preserves_untouched_fields_and_order() {
        let unrelated_all = rule(RuleMatch::All, Some(false), None);
        let unrelated_ip = rule(ip_only("192.0.2.27"), None, Some(true));
        let ada = name_only("Ada");

        let mut send_only = vec![
            unrelated_all.clone(),
            rule(ada.clone(), Some(true), Some(true)),
            unrelated_ip.clone(),
        ];
        apply_room_edit(&mut send_only, &ada, Some(false), None).unwrap();
        assert_eq!(
            send_only,
            vec![
                unrelated_all.clone(),
                rule(ada.clone(), None, Some(true)),
                unrelated_ip.clone(),
                rule(ada.clone(), Some(false), None),
            ]
        );

        let mut listen_only = vec![
            unrelated_all.clone(),
            rule(ada.clone(), Some(true), Some(true)),
            unrelated_ip.clone(),
        ];
        apply_room_edit(&mut listen_only, &ada, None, Some(false)).unwrap();
        assert_eq!(
            listen_only,
            vec![
                unrelated_all.clone(),
                rule(ada.clone(), Some(true), None),
                unrelated_ip.clone(),
                rule(ada.clone(), None, Some(false)),
            ]
        );

        let mut both = vec![
            unrelated_all.clone(),
            rule(ada.clone(), Some(true), Some(true)),
            unrelated_ip.clone(),
        ];
        apply_room_edit(&mut both, &ada, Some(false), Some(false)).unwrap();
        assert_eq!(
            both,
            vec![
                unrelated_all.clone(),
                unrelated_ip.clone(),
                rule(ada.clone(), Some(false), Some(false)),
            ]
        );

        let mut split_rows = vec![
            rule(ada.clone(), Some(true), None),
            unrelated_all.clone(),
            rule(ada.clone(), None, Some(true)),
            unrelated_ip.clone(),
        ];
        apply_room_edit(&mut split_rows, &ada, Some(false), None).unwrap();
        assert_eq!(
            split_rows,
            vec![
                unrelated_all,
                rule(ada.clone(), None, Some(true)),
                unrelated_ip,
                rule(ada.clone(), Some(false), None),
            ]
        );

        let combined = ip_and_name("192.0.2.10", "Ada");
        let mut distinct = vec![rule(combined.clone(), Some(true), Some(true))];
        apply_room_edit(&mut distinct, &ada, Some(false), None).unwrap();
        assert_eq!(
            distinct,
            vec![
                rule(combined, Some(true), Some(true)),
                rule(ada, Some(false), None),
            ]
        );
    }

    #[test]
    fn invalid_match_and_rule_shapes_are_rejected() {
        assert!(serde_json::from_str::<RuleMatch>("{}").is_err());
        assert!(serde_json::from_str::<RuleMatch>(r#"{"name":"Ada","extra":true}"#).is_err());
        assert!(serde_json::from_str::<RuleMatch>(r#"{"name":"   "}"#).is_err());
        assert!(serde_json::from_str::<RuleMatch>(r#"{"ip":"999.1.2.3"}"#).is_err());
        assert!(serde_json::from_str::<RuleMatch>("\"ALL\"").is_err());
        assert!(serde_json::from_str::<Rule>(r#"{"match":"all","send":true,"extra":1}"#).is_err());

        let empty_rule = Rule {
            r#match: RuleMatch::All,
            source_kind: None,
            send: None,
            listen: None,
            outputs: None,
        };
        assert!(
            empty_rule
                .validate()
                .unwrap_err()
                .contains("send, listen, or outputs")
        );

        let empty_condition = RuleMatch::Condition {
            ip: None,
            name: None,
        };
        assert!(empty_condition.validate().is_err());
        assert!(!empty_condition.matches(&member("Ada", "192.0.2.10")));

        let mut rules = vec![rule(RuleMatch::All, Some(true), None)];
        let before = rules.clone();
        assert!(apply_room_edit(&mut rules, &empty_condition, Some(true), None).is_err());
        assert!(apply_room_edit(&mut rules, &name_only("Ada"), None, None).is_err());
        assert_eq!(rules, before);
    }

    #[test]
    fn match_json_roundtrip_keeps_all_string_and_omits_absent_ip() {
        let all: RuleMatch = serde_json::from_str("\"all\"").unwrap();
        assert_eq!(all, RuleMatch::All);
        assert_eq!(serde_json::to_string(&all).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::to_value(rule(RuleMatch::All, Some(true), Some(false))).unwrap(),
            json!({"match": "all", "send": true, "listen": false})
        );

        let name: RuleMatch = serde_json::from_str(r#"{"name":"Ada"}"#).unwrap();
        let encoded = serde_json::to_value(name).unwrap();
        assert_eq!(encoded, json!({"name": "Ada"}));
        assert!(encoded.get("ip").is_none());

        let ip_only_match: RuleMatch = serde_json::from_str(r#"{"ip":"192.0.2.10"}"#).unwrap();
        let encoded_ip = serde_json::to_value(ip_only_match).unwrap();
        assert_eq!(encoded_ip, json!({"ip": "192.0.2.10"}));
        assert!(encoded_ip.get("name").is_none());
    }
}
