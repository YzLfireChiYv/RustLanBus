//! Local room profiles, directional rules, and deterministic room selection.
//!
//! This is a file-and-decision foundation. It does not join rooms, run
//! discovery, or replace the existing participant SavedMember configuration.

mod library;
mod profile;
mod rules;
mod selection;
mod store;

pub use profile::default_contact_listener;

pub use library::{RoomCardView, RoomLibrary};
pub use profile::{
    DEFAULT_CONTACT_PORT, GlobalRules, MixPatch, RoomPlaybackRule, RoomProfile, RoomTarget, SCHEMA,
    apply_playback_edit, evaluate_playback,
};
pub use rules::{
    AssignmentOrigin, MemberFacts, ResolvedDirections, Rule, RuleListKind, RuleMatch,
    apply_room_edit, apply_rule_edit, apply_source_edit, evaluate, evaluate_source,
};
pub use selection::{
    AutoJoinBlockReason, DiscoveredRoom, JoinMode, SelectionError, SelectionPlan, choose_plan,
    deduplicate_instances,
};
pub use store::{SaveFailure, SaveFailureKind};
pub use store::{
    create_global_rules_file, create_room_profile_file, load_global_rules, load_room_profile,
    save_global_rules, save_global_rules_if_unchanged, save_room_profile,
    save_room_profile_if_unchanged,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn global_then_room_last_match_uses_profile_rule_lists() {
        let global = GlobalRules::baseline();
        let mut profile = RoomProfile::create("Headphones").unwrap();
        profile.rules = vec![
            Rule {
                r#match: RuleMatch::All,
                source_kind: None,
                send: Some(false),
                listen: None,
                outputs: None,
            },
            Rule {
                r#match: RuleMatch::Condition {
                    ip: None,
                    name: Some("Ada".into()),
                },
                source_kind: None,
                send: Some(true),
                listen: None,
                outputs: None,
            },
        ];

        let ada = MemberFacts {
            name: "Ada".into(),
            ip: ip("192.0.2.10"),
        };
        let bea = MemberFacts {
            name: "Bea".into(),
            ip: ip("192.0.2.11"),
        };

        let ada_first = evaluate(&global.rules, &profile.rules, &ada);
        assert!(ada_first.send);
        assert!(!ada_first.listen);
        let bea_first = evaluate(&global.rules, &profile.rules, &bea);
        assert!(!bea_first.send);
        assert!(!bea_first.listen);

        apply_room_edit(
            &mut profile.rules,
            &RuleMatch::Condition {
                ip: None,
                name: Some("Ada".into()),
            },
            None,
            Some(true),
        )
        .unwrap();

        let ada_next = evaluate(&global.rules, &profile.rules, &ada);
        assert!(ada_next.send);
        assert!(ada_next.listen);
        let bea_next = evaluate(&global.rules, &profile.rules, &bea);
        assert!(!bea_next.send);
        assert!(!bea_next.listen);
        assert_eq!(profile.room.address, None);
    }
}
