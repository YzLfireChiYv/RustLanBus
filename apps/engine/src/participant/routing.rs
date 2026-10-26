//! Ephemeral room-to-media projection. User rules are persisted separately.

use rlb_model::MemberId;
use rlb_peer::MemberRun;
use serde::Serialize;
use std::net::SocketAddr;

use super::config::SourceKind;
use super::output_routing::OutputTarget;
use super::room_session::{ProjectedEndpoint, SessionSnapshot};
use super::rooms::{AssignmentOrigin, MemberFacts, evaluate, evaluate_source};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceDirections {
    pub kind: SourceKind,
    pub send: bool,
    pub listen: bool,
    pub send_from: Option<AssignmentOrigin>,
    pub listen_from: Option<AssignmentOrigin>,
    pub outputs: Vec<OutputTarget>,
    pub outputs_from: Option<AssignmentOrigin>,
}

/// Member defaults and resolved source choices from the same ordered rule walk.
/// The transport envelope is derived; it introduces no independent user gate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PeerRoute {
    pub remote: MemberRun,
    pub name: String,
    pub endpoint: ProjectedEndpoint,
    pub alternatives: Vec<ProjectedEndpoint>,
    /// Observed connected path. Candidate preferences remain independent.
    #[serde(skip_serializing)]
    pub resolved_for: Option<super::config::ResolvedPath>,
    pub directory_visible: bool,
    pub send: bool,
    pub listen: bool,
    pub send_from: Option<AssignmentOrigin>,
    pub listen_from: Option<AssignmentOrigin>,
    pub outputs: Vec<OutputTarget>,
    pub outputs_from: Option<AssignmentOrigin>,
    pub sources: Vec<SourceDirections>,
}

impl PeerRoute {
    /// Explanatory origins and directory visibility do not change media intent.
    pub(crate) fn same_media_settings(&self, other: &Self) -> bool {
        self.remote == other.remote
            && self.name == other.name
            && self.endpoint == other.endpoint
            && self.alternatives == other.alternatives
            && self.send == other.send
            && self.listen == other.listen
            && self.outputs == other.outputs
            && self
                .sources
                .iter()
                .map(|source| (source.kind, source.send, source.listen, &source.outputs))
                .eq(other
                    .sources
                    .iter()
                    .map(|source| (source.kind, source.send, source.listen, &source.outputs)))
    }

    pub fn source_policy(&self) -> rlb_peer::SourcePolicy {
        rlb_peer::SourcePolicy {
            default: rlb_peer::Intent {
                send: self.send,
                receive: self.listen,
            },
            overrides: self
                .sources
                .iter()
                .map(|source| rlb_peer::SourceIntent {
                    kind: match source.kind {
                        SourceKind::System => rlb_peer::SourceKind::System,
                        SourceKind::Microphone => rlb_peer::SourceKind::Microphone,
                    },
                    intent: rlb_peer::Intent {
                        send: source.send,
                        receive: source.listen,
                    },
                })
                .collect(),
        }
    }

    pub fn address(&self) -> Option<SocketAddr> {
        if let Some(path) = self.resolved_for { return Some(path.address); }
        match self.endpoint {
            ProjectedEndpoint::Ready(address) => Some(address),
            ProjectedEndpoint::LocalScopeRequired { ip, port } => Some(SocketAddr::new(ip.into(), port)),
        }
    }

    pub fn alternate_addresses(&self) -> Vec<SocketAddr> {
        self.alternatives.iter().map(|endpoint| match *endpoint {
            ProjectedEndpoint::Ready(address) => address,
            ProjectedEndpoint::LocalScopeRequired { ip, port } => SocketAddr::new(ip.into(), port),
        }).collect()
    }

    pub fn preferred_address(&self) -> SocketAddr {
        match self.endpoint {
            ProjectedEndpoint::Ready(address) => address,
            ProjectedEndpoint::LocalScopeRequired { ip, port } => SocketAddr::new(ip.into(), port),
        }
    }
}

pub fn project_peers(local: MemberId, session: &SessionSnapshot) -> Vec<PeerRoute> {
    project_peers_on_paths(local, session, |_| None)
}

pub fn project_peers_on_paths(local: MemberId, session: &SessionSnapshot, selected: impl Fn(MemberRun) -> Option<super::config::ResolvedPath>) -> Vec<PeerRoute> {
    let global = session
        .global_rules
        .as_ref()
        .map(|value| value.rules.as_slice())
        .unwrap_or(&[]);
    session
        .members
        .iter()
        .filter(|member| member.member_id != local)
        .map(|member| {
            let remote = MemberRun { member: member.member_id, run: member.run_id };
            let resolved_for = selected(remote);
            let ip = resolved_for.map(|path| path.address.ip()).unwrap_or_else(|| match member.endpoint {
                ProjectedEndpoint::Ready(address) => address.ip(),
                ProjectedEndpoint::LocalScopeRequired { ip, .. } => ip.into(),
            });
            let facts = MemberFacts {
                name: member.display_name.clone(),
                ip,
            };
            let intent = evaluate(global, &session.rules, &facts);
            PeerRoute {
                remote: MemberRun {
                    member: member.member_id,
                    run: member.run_id,
                },
                name: member.display_name.clone(),
                endpoint: member.endpoint,
                alternatives: member.alternatives.clone(),
                resolved_for,
                directory_visible: member.visible,
                send: intent.send,
                listen: intent.listen,
                send_from: intent.send_from,
                listen_from: intent.listen_from,
                outputs: intent.outputs,
                outputs_from: intent.outputs_from,
                sources: [SourceKind::System, SourceKind::Microphone]
                    .into_iter()
                    .map(|kind| {
                        let choice = evaluate_source(global, &session.rules, &facts, kind);
                        SourceDirections {
                            kind,
                            send: choice.send,
                            listen: choice.listen,
                            send_from: choice.send_from,
                            listen_from: choice.listen_from,
                            outputs: choice.outputs,
                            outputs_from: choice.outputs_from,
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_model::RunId;
    use serde_json::json;

    use crate::participant::room_session::MemberView;
    use crate::participant::rooms::{GlobalRules, Rule, RuleListKind, RuleMatch, apply_room_edit};

    fn member(id: u8, address: &str) -> MemberView {
        MemberView {
            member_id: MemberId::from_bytes([id; 16]).unwrap(),
            run_id: RunId::from_bytes([id; 16]).unwrap(),
            display_name: "Ada".to_owned(),
            endpoint: ProjectedEndpoint::Ready(address.parse().unwrap()),
            visible: true,
            alternatives: Vec::new(),
        }
    }

    fn rule(condition: RuleMatch, send: Option<bool>, listen: Option<bool>) -> Rule {
        Rule {
            source_kind: None,
            r#match: condition,
            send,
            listen,
            outputs: None,
        }
    }

    #[test]
    fn output_assignments_are_independent_of_consent_and_origins_do_not_change_media() {
        let local = MemberId::from_bytes([1; 16]).unwrap();
        let mut session = SessionSnapshot::idle();
        session.members.push(member(2, "192.0.2.10:47001"));
        let initial = project_peers(local, &session).remove(0);
        session.rules = vec![Rule {
            r#match: RuleMatch::All,
            source_kind: Some(SourceKind::Microphone),
            send: None,
            listen: None,
            outputs: Some(vec![OutputTarget::Endpoint("headphones".into())]),
        }];
        let routed = project_peers(local, &session).remove(0);
        assert_eq!(routed.source_policy(), initial.source_policy());
        assert!(routed.send && !routed.listen);
        assert_eq!(routed.outputs, vec![OutputTarget::Main]);
        assert_eq!(routed.sources[0].outputs, vec![OutputTarget::Main]);
        assert_eq!(
            routed.sources[1].outputs,
            vec![OutputTarget::Endpoint("headphones".into())]
        );
        assert_eq!(
            routed.sources[1].outputs_from,
            Some(AssignmentOrigin {
                list: RuleListKind::Room,
                index: 0
            })
        );
        assert!(!routed.same_media_settings(&initial));
        session
            .rules
            .insert(0, rule(RuleMatch::All, Some(true), Some(false)));
        let reindexed = project_peers(local, &session).remove(0);
        assert!(routed.same_media_settings(&reindexed));
        assert_ne!(
            routed.sources[1].outputs_from,
            reindexed.sources[1].outputs_from
        );
    }

    #[test]
    fn projected_origins_follow_each_current_assignment_after_edits_and_replacement() {
        let local = MemberId::from_bytes([1; 16]).unwrap();
        let mut session = SessionSnapshot::idle();
        session.members = vec![
            member(1, "192.0.2.1:47001"),
            member(2, "192.0.2.10:47001"),
            member(3, "192.0.2.11:47001"),
        ];

        // Implicit defaults have no file assignment to cite, and the local
        // member never becomes a projected remote route.
        let initial = project_peers(local, &session);
        assert_eq!(initial.len(), 2);
        for route in &initial {
            assert!(route.send);
            assert!(!route.listen);
            assert_eq!(route.send_from, None);
            assert_eq!(route.listen_from, None);
        }
        let encoded = serde_json::to_value(&initial[0]).unwrap();
        assert_eq!(encoded["send_from"], json!(null));
        assert_eq!(encoded["listen_from"], json!(null));

        let by_name = RuleMatch::Condition {
            ip: None,
            name: Some("Ada".to_owned()),
        };
        let by_name_and_ip = RuleMatch::Condition {
            ip: Some("192.0.2.10".parse().unwrap()),
            name: Some("Ada".to_owned()),
        };
        session.global_rules = Some(GlobalRules {
            rules: vec![
                rule(RuleMatch::All, Some(false), Some(false)),
                rule(by_name.clone(), Some(true), None),
            ],
            ..GlobalRules::baseline()
        });
        session.rules = vec![
            rule(by_name.clone(), None, Some(true)),
            rule(by_name_and_ip, None, Some(false)),
        ];

        let assigned = project_peers(local, &session);
        for route in &assigned {
            assert!(route.send);
            assert_eq!(
                route.send_from,
                Some(AssignmentOrigin {
                    list: RuleListKind::Global,
                    index: 1,
                })
            );
        }
        assert!(!assigned[0].listen);
        assert!(assigned[1].listen);
        assert_eq!(
            serde_json::to_value(&assigned[0]).unwrap()["listen_from"],
            json!({"list": "room", "index": 1})
        );
        assert_eq!(
            serde_json::to_value(&assigned[1]).unwrap()["listen_from"],
            json!({"list": "room", "index": 0})
        );
        assert_eq!(
            serde_json::to_value(&assigned[0]).unwrap()["send_from"],
            json!({"list": "global", "index": 1})
        );

        // The existing edit operation changes just send. Listen keeps its
        // original independent winner, including the address-specific rule.
        apply_room_edit(&mut session.rules, &by_name, Some(false), None).unwrap();
        let edited = project_peers(local, &session);
        for (route, previous) in edited.iter().zip(&assigned) {
            assert!(!route.send);
            assert_eq!(
                route.send_from,
                Some(AssignmentOrigin {
                    list: RuleListKind::Room,
                    index: 2,
                })
            );
            assert_eq!(route.listen, previous.listen);
            assert_eq!(route.listen_from, previous.listen_from);
        }

        // A fresh loaded list replaces the snapshot's rules. Provenance is
        // derived again from that list; it is never cached by member identity.
        session.rules.clear();
        let replaced = project_peers(local, &session);
        for route in replaced {
            assert!(route.send);
            assert!(!route.listen);
            assert_eq!(route.send_from, assigned[0].send_from);
            assert_eq!(
                route.listen_from,
                Some(AssignmentOrigin {
                    list: RuleListKind::Global,
                    index: 0,
                })
            );
        }
    }
}
