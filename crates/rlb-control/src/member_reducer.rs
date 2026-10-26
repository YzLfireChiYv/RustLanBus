//! Pure member-side reducer for the five delegated B mutations.

use rlb_model::{
    Capability, CapabilityGrants, CatalogRevision, ErrorCode, GrantRevision, LocalSourceConfig,
    MemberConfigRevision, MemberId, MemberMix, QualityProfile, RoomId, RoomRevision, SourceId,
    SourceMix,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::{DelegatedMutation, ValidatedDelegatedCommand};
use crate::envelope::{CanonicalRevisions, CommandStatus, WarningCode};

/// One local source's persistent room configuration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberSourceConfig {
    pub source_id: SourceId,
    pub config: LocalSourceConfig,
}

/// One remote member's persistent mix entry.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMemberMix {
    pub remote_member_id: MemberId,
    pub mix: MemberMix,
}

/// One remote source's persistent mix entry.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSourceMix {
    pub remote_member_id: MemberId,
    pub source_id: SourceId,
    pub mix: SourceMix,
}

/// Canonical member-owned state relevant to remote room control.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedMemberState {
    pub room_id: RoomId,
    pub member_id: MemberId,
    pub room_revision: RoomRevision,
    pub member_config_revision: MemberConfigRevision,
    pub catalog_revision: CatalogRevision,
    pub grant_revision: GrantRevision,
    pub grants: CapabilityGrants,
    pub quality_cap: QualityProfile,
    pub auto_reconnect: bool,
    pub sources: Vec<MemberSourceConfig>,
    pub member_mix: Vec<RemoteMemberMix>,
    pub source_mix: Vec<RemoteSourceMix>,
}

impl DelegatedMemberState {
    /// Validates uniqueness in a candidate persistent state.
    pub fn validate(&self) -> Result<(), DelegatedStateError> {
        if self.sources.len() != 2 {
            return Err(DelegatedStateError::SourceCount);
        }
        if self.member_mix.len() > 31 || self.source_mix.len() > 62 {
            return Err(DelegatedStateError::MixBounds);
        }
        if has_duplicate_by(&self.sources, |entry| entry.source_id) {
            return Err(DelegatedStateError::DuplicateSource);
        }
        if has_duplicate_by(&self.member_mix, |entry| entry.remote_member_id) {
            return Err(DelegatedStateError::DuplicateMemberMix);
        }
        let mut source_keys: Vec<_> = self
            .source_mix
            .iter()
            .map(|entry| (entry.remote_member_id, entry.source_id))
            .collect();
        source_keys.sort_unstable();
        if source_keys.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(DelegatedStateError::DuplicateSourceMix);
        }
        Ok(())
    }

    /// Returns a source configuration without exposing unrelated local state.
    #[must_use]
    pub fn source(&self, source_id: SourceId) -> Option<&LocalSourceConfig> {
        self.sources
            .iter()
            .find(|entry| entry.source_id == source_id)
            .map(|entry| &entry.config)
    }

    fn revisions(&self) -> CanonicalRevisions {
        CanonicalRevisions {
            room_revision: Some(self.room_revision),
            catalog_revision: Some(self.catalog_revision),
            member_config_revision: Some(self.member_config_revision),
            grant_revision: Some(self.grant_revision),
            ..CanonicalRevisions::default()
        }
    }
}

fn has_duplicate_by<T, K: Ord>(values: &[T], key: impl Fn(&T) -> K) -> bool {
    let mut keys: Vec<_> = values.iter().map(key).collect();
    keys.sort_unstable();
    keys.windows(2).any(|pair| pair[0] == pair[1])
}

/// Invalid candidate member state.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DelegatedStateError {
    #[error("member state must contain exactly two local sources")]
    SourceCount,
    #[error("receive-mix collection exceeds room bounds")]
    MixBounds,
    #[error("duplicate local source identity")]
    DuplicateSource,
    #[error("duplicate remote member mix identity")]
    DuplicateMemberMix,
    #[error("duplicate remote source mix identity")]
    DuplicateSourceMix,
}

/// Command-specific effective value returned by the member reducer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DelegatedEffective {
    SourcePublication {
        publish_requested: bool,
        master_capture_enabled: bool,
    },
    SendQuality {
        selected_profile: QualityProfile,
        effective_profile: QualityProfile,
    },
    MemberMix {
        remote_member_id: MemberId,
        mix: MemberMix,
    },
    SourceMix {
        remote_member_id: MemberId,
        source_id: SourceId,
        mix: SourceMix,
    },
    AutoReconnect {
        enabled: bool,
    },
}

/// Successful pure reduction.
#[derive(Clone, Debug, PartialEq)]
pub struct DelegatedTransition {
    pub state: DelegatedMemberState,
    pub status: CommandStatus,
    pub revisions: CanonicalRevisions,
    pub effective: DelegatedEffective,
    pub warnings: Vec<WarningCode>,
}

/// Stable rejected reduction. The original state remains canonical.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("delegated command rejected with {code:?}")]
pub struct DelegatedApplyError {
    pub code: ErrorCode,
    pub current_revisions: CanonicalRevisions,
    pub capability: Capability,
}

/// Applies one fully validated delegated command to a cloned candidate state.
///
/// This function performs no I/O. A caller persists `transition.state`
/// atomically before publishing it as canonical.
pub fn reduce_delegated_command(
    current: &DelegatedMemberState,
    command: &ValidatedDelegatedCommand,
) -> Result<DelegatedTransition, DelegatedApplyError> {
    let capability = command.capability();
    let reject = |code| DelegatedApplyError {
        code,
        current_revisions: current.revisions(),
        capability,
    };

    if current.validate().is_err() {
        return Err(reject(ErrorCode::InvalidCatalog));
    }

    if command.room_id != current.room_id {
        return Err(reject(ErrorCode::RoomScopeMismatch));
    }
    if command.member_id != current.member_id {
        return Err(reject(ErrorCode::MemberScopeMismatch));
    }
    if command.expected.room_revision != current.room_revision {
        return Err(reject(ErrorCode::StaleRoomRevision));
    }

    let granted = current.grants.get(capability);
    if command.expected.grant_revision != current.grant_revision {
        if !granted && command.expected.grant_revision < current.grant_revision {
            return Err(reject(ErrorCode::CapabilityRevoked));
        }
        return Err(reject(ErrorCode::StaleGrantRevision));
    }
    if !granted {
        return Err(reject(ErrorCode::CapabilityDenied));
    }
    if command.expected.member_config_revision != current.member_config_revision {
        return Err(reject(ErrorCode::StaleMemberConfigRevision));
    }

    let source_command = matches!(
        command.mutation,
        DelegatedMutation::SetSourcePublication { .. } | DelegatedMutation::SetSendQuality { .. }
    );
    if source_command && command.expected.catalog_revision != Some(current.catalog_revision) {
        return Err(reject(ErrorCode::StaleCatalogRevision));
    }
    if !source_command && command.expected.catalog_revision.is_some() {
        return Err(reject(ErrorCode::FieldNotAllowed));
    }

    let mut next = current.clone();
    let (changed, effective, warnings, catalog_changed) = match command.mutation {
        DelegatedMutation::SetSourcePublication {
            source_id,
            publish_requested,
        } => {
            let Some(source) = next
                .sources
                .iter_mut()
                .find(|entry| entry.source_id == source_id)
            else {
                return Err(reject(ErrorCode::SourceNotFound));
            };
            let changed = source.config.publish_requested != publish_requested;
            source.config.publish_requested = publish_requested;
            (
                changed,
                DelegatedEffective::SourcePublication {
                    publish_requested,
                    master_capture_enabled: source.config.master_capture_enabled,
                },
                Vec::new(),
                changed,
            )
        }
        DelegatedMutation::SetSendQuality {
            source_id,
            selected_profile,
        } => {
            let Some(source) = next
                .sources
                .iter_mut()
                .find(|entry| entry.source_id == source_id)
            else {
                return Err(reject(ErrorCode::SourceNotFound));
            };
            let changed = source.config.selected_profile != selected_profile;
            source.config.selected_profile = selected_profile;
            let effective_profile = selected_profile.capped_by(next.quality_cap);
            let warnings = if effective_profile != selected_profile {
                vec![WarningCode::QualityCapped]
            } else {
                Vec::new()
            };
            (
                changed,
                DelegatedEffective::SendQuality {
                    selected_profile,
                    effective_profile,
                },
                warnings,
                changed,
            )
        }
        DelegatedMutation::SetMemberMix {
            remote_member_id,
            gain_db,
            mute,
            solo,
        } => {
            let desired = MemberMix {
                gain_db,
                mute,
                solo,
            };
            let changed = if let Some(entry) = next
                .member_mix
                .iter_mut()
                .find(|entry| entry.remote_member_id == remote_member_id)
            {
                let changed = entry.mix != desired;
                entry.mix = desired;
                changed
            } else {
                next.member_mix.push(RemoteMemberMix {
                    remote_member_id,
                    mix: desired,
                });
                true
            };
            (
                changed,
                DelegatedEffective::MemberMix {
                    remote_member_id,
                    mix: desired,
                },
                Vec::new(),
                false,
            )
        }
        DelegatedMutation::SetSourceMix {
            remote_member_id,
            source_id,
            gain_db,
            pan,
            mute,
        } => {
            let desired = SourceMix { gain_db, pan, mute };
            let changed = if let Some(entry) = next.source_mix.iter_mut().find(|entry| {
                entry.remote_member_id == remote_member_id && entry.source_id == source_id
            }) {
                let changed = entry.mix != desired;
                entry.mix = desired;
                changed
            } else {
                next.source_mix.push(RemoteSourceMix {
                    remote_member_id,
                    source_id,
                    mix: desired,
                });
                true
            };
            (
                changed,
                DelegatedEffective::SourceMix {
                    remote_member_id,
                    source_id,
                    mix: desired,
                },
                Vec::new(),
                false,
            )
        }
        DelegatedMutation::SetAutoReconnect { enabled } => {
            let changed = next.auto_reconnect != enabled;
            next.auto_reconnect = enabled;
            (
                changed,
                DelegatedEffective::AutoReconnect { enabled },
                Vec::new(),
                false,
            )
        }
    };

    if changed {
        next.member_config_revision = next
            .member_config_revision
            .checked_next()
            .ok_or_else(|| reject(ErrorCode::UnexpectedError))?;
        if catalog_changed {
            next.catalog_revision = next
                .catalog_revision
                .checked_next()
                .ok_or_else(|| reject(ErrorCode::UnexpectedError))?;
        }
    }
    if next.validate().is_err() {
        return Err(reject(ErrorCode::InvalidValue));
    }
    let status = if changed {
        CommandStatus::Applied
    } else {
        CommandStatus::Unchanged
    };
    let revisions = next.revisions();
    Ok(DelegatedTransition {
        state: next,
        status,
        revisions,
        effective,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rlb_model::{GainDb, Pan, RequestId};

    use super::*;
    use crate::command::{DelegatedExpected, DelegatedMutation, ValidatedDelegatedCommand};

    fn id<T: FromStr>(value: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        value.parse().unwrap()
    }

    fn state(grants: CapabilityGrants) -> DelegatedMemberState {
        DelegatedMemberState {
            room_id: id("01010101010101010101010101010101"),
            member_id: id("02020202020202020202020202020202"),
            room_revision: RoomRevision::new(4),
            member_config_revision: MemberConfigRevision::new(7),
            catalog_revision: CatalogRevision::new(3),
            grant_revision: GrantRevision::new(2),
            grants,
            quality_cap: QualityProfile::PcmS16V1,
            auto_reconnect: true,
            sources: vec![
                MemberSourceConfig {
                    source_id: id("03030303030303030303030303030303"),
                    config: LocalSourceConfig {
                        master_capture_enabled: false,
                        publish_requested: false,
                        selected_profile: QualityProfile::PcmF32V1,
                    },
                },
                MemberSourceConfig {
                    source_id: id("06060606060606060606060606060606"),
                    config: LocalSourceConfig::default(),
                },
            ],
            member_mix: Vec::new(),
            source_mix: Vec::new(),
        }
    }

    fn command(mutation: DelegatedMutation) -> ValidatedDelegatedCommand {
        ValidatedDelegatedCommand {
            request_id: id::<RequestId>("09090909090909090909090909090909"),
            room_id: id("01010101010101010101010101010101"),
            member_id: id("02020202020202020202020202020202"),
            expected: DelegatedExpected {
                room_revision: RoomRevision::new(4),
                member_config_revision: MemberConfigRevision::new(7),
                catalog_revision: if matches!(
                    mutation,
                    DelegatedMutation::SetSourcePublication { .. }
                        | DelegatedMutation::SetSendQuality { .. }
                ) {
                    Some(CatalogRevision::new(3))
                } else {
                    None
                },
                grant_revision: GrantRevision::new(2),
            },
            mutation,
        }
    }

    fn mutations() -> Vec<DelegatedMutation> {
        vec![
            DelegatedMutation::SetSourcePublication {
                source_id: id("03030303030303030303030303030303"),
                publish_requested: true,
            },
            DelegatedMutation::SetSendQuality {
                source_id: id("03030303030303030303030303030303"),
                selected_profile: QualityProfile::PcmS16V1,
            },
            DelegatedMutation::SetMemberMix {
                remote_member_id: id("04040404040404040404040404040404"),
                gain_db: GainDb::new(-3.0).unwrap(),
                mute: false,
                solo: false,
            },
            DelegatedMutation::SetSourceMix {
                remote_member_id: id("04040404040404040404040404040404"),
                source_id: id("05050505050505050505050505050505"),
                gain_db: GainDb::new(-6.0).unwrap(),
                pan: Pan::CENTER,
                mute: false,
            },
            DelegatedMutation::SetAutoReconnect { enabled: false },
        ]
    }

    #[test]
    fn four_capabilities_form_a_field_level_whitelist() {
        let capabilities = [
            Capability::SourcePublication,
            Capability::SendQuality,
            Capability::ReceiveMix,
            Capability::AutoReconnect,
        ];
        for granted in capabilities {
            let mut grants = CapabilityGrants::default();
            grants.set(granted, true);
            for mutation in mutations() {
                let command = command(mutation);
                let result = reduce_delegated_command(&state(grants), &command);
                assert_eq!(
                    result.is_ok(),
                    command.capability() == granted,
                    "{granted:?} unexpectedly handled {:?}",
                    command.mutation
                );
            }
        }
    }

    #[test]
    fn publication_preserves_local_capture_boundary_and_revisions() {
        let grants = CapabilityGrants {
            source_publication: true,
            ..CapabilityGrants::default()
        };
        let command = command(DelegatedMutation::SetSourcePublication {
            source_id: id("03030303030303030303030303030303"),
            publish_requested: true,
        });
        let transition = reduce_delegated_command(&state(grants), &command).unwrap();
        let source = transition
            .state
            .source(id("03030303030303030303030303030303"))
            .unwrap();
        assert!(source.publish_requested);
        assert!(!source.master_capture_enabled);
        assert_eq!(
            transition.state.member_config_revision,
            MemberConfigRevision::new(8)
        );
        assert_eq!(transition.state.catalog_revision, CatalogRevision::new(4));
    }

    #[test]
    fn stale_false_grant_is_reported_as_revoked() {
        let mut command = command(DelegatedMutation::SetAutoReconnect { enabled: false });
        command.expected.grant_revision = GrantRevision::new(1);
        let error =
            reduce_delegated_command(&state(CapabilityGrants::default()), &command).unwrap_err();
        assert_eq!(error.code, ErrorCode::CapabilityRevoked);
    }
}
