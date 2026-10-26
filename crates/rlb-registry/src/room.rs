//! Single-room persistent authority state and pure candidate reducer.

use rlb_control::strict_json::{JsonLimits, StrictJsonError, parse_typed};
use rlb_control::{AdmissionReason, AuthorityMutation, ValidatedRoomCommand};
use rlb_model::{
    AdmissionState, ErrorCode, MemberId, QosPolicy, QualityProfile, RoomId, RoomRevision, SourceId,
    SourceKind,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::token::{
    ClaimSecret, CredentialError, MemberCredential, MemberToken, Sha256TokenVerifier, TokenDigest,
    TokenDigestVerifier,
};
use crate::wire::{DecimalU64, SourceSlot};

/// Maximum approved identities in the room.
pub const MAX_APPROVED_MEMBERS: usize = 32;
/// Maximum pending enrollment identities.
pub const MAX_PENDING_MEMBERS: usize = 32;
/// Maximum retained removed identities.
pub const MAX_REMOVED_MEMBERS: usize = 128;

/// Persisted authority-owned room object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentRoom {
    pub room_id: RoomId,
    pub name: String,
    pub room_revision: RoomRevision,
    pub quality_cap: QualityProfile,
    pub qos_policy: QosPolicy,
    pub room_send_muted: bool,
}

/// Persisted admission and credential record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberRecord {
    pub member_id: MemberId,
    pub admission: AdmissionState,
    pub last_display_name: String,
    pub source_slots: [SourceSlot; 2],
    pub token_sha256: Option<TokenDigest>,
    pub token_generation: Option<DecimalU64>,
    pub claim_sha256: Option<TokenDigest>,
    pub request_readmission: bool,
}

impl MemberRecord {
    /// Returns a scoped credential only for an approved claimed member.
    #[must_use]
    pub fn credential(&self, room_id: RoomId) -> Option<MemberCredential> {
        if self.admission != AdmissionState::Approved {
            return None;
        }
        Some(MemberCredential {
            room_id,
            member_id: self.member_id,
            token_generation: self.token_generation?.get(),
            token_sha256: self.token_sha256?,
        })
    }

    /// Returns the stable source kind for one source ID.
    #[must_use]
    pub fn source_kind(&self, source_id: SourceId) -> Option<SourceKind> {
        self.source_slots
            .iter()
            .find(|slot| slot.source_id == source_id)
            .map(|slot| slot.kind)
    }
}

/// Persisted deviations from default-visible source policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VisibilityOverride {
    pub publisher_member_id: MemberId,
    pub source_id: SourceId,
    pub hidden_viewer_ids: Vec<MemberId>,
}

/// Full V2 persistent authority snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(into = "RoomStateWire")]
pub struct RoomState {
    pub room: PersistentRoom,
    pub members: Vec<MemberRecord>,
    pub visibility_overrides: Vec<VisibilityOverride>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RoomStateWire {
    schema_version: u8,
    room: PersistentRoom,
    members: Vec<MemberRecord>,
    visibility_overrides: Vec<VisibilityOverride>,
}

impl From<RoomState> for RoomStateWire {
    fn from(value: RoomState) -> Self {
        Self {
            schema_version: 2,
            room: value.room,
            members: value.members,
            visibility_overrides: value.visibility_overrides,
        }
    }
}

impl TryFrom<RoomStateWire> for RoomState {
    type Error = RoomStateError;

    fn try_from(value: RoomStateWire) -> Result<Self, Self::Error> {
        if value.schema_version != 2 {
            return Err(RoomStateError::SchemaVersion);
        }
        let state = Self {
            room: value.room,
            members: value.members,
            visibility_overrides: value.visibility_overrides,
        };
        state.validate()?;
        Ok(state)
    }
}

impl<'de> Deserialize<'de> for RoomState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RoomStateWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl RoomState {
    /// Creates the one-room default state after first-install ID generation.
    pub fn new(room_id: RoomId, name: impl Into<String>) -> Result<Self, RoomStateError> {
        let name = normalize_name(name.into())?;
        let state = Self {
            room: PersistentRoom {
                room_id,
                name,
                room_revision: RoomRevision::new(1),
                quality_cap: QualityProfile::PcmF32V1,
                qos_policy: QosPolicy::AudioPriority,
                room_send_muted: false,
            },
            members: Vec::new(),
            visibility_overrides: Vec::new(),
        };
        state.validate()?;
        Ok(state)
    }

    /// Fully validates a candidate before persistence.
    pub fn validate(&self) -> Result<(), RoomStateError> {
        validate_name(&self.room.name)?;
        if self.members.len() > MAX_APPROVED_MEMBERS + MAX_PENDING_MEMBERS + MAX_REMOVED_MEMBERS {
            return Err(RoomStateError::MemberBounds);
        }
        if self.count(AdmissionState::Approved) > MAX_APPROVED_MEMBERS
            || self.count(AdmissionState::Pending) > MAX_PENDING_MEMBERS
            || self.count(AdmissionState::Removed) > MAX_REMOVED_MEMBERS
        {
            return Err(RoomStateError::MemberBounds);
        }

        let mut member_ids: Vec<_> = self.members.iter().map(|record| record.member_id).collect();
        member_ids.sort_unstable();
        if member_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RoomStateError::DuplicateMember);
        }

        for record in &self.members {
            validate_name(&record.last_display_name)?;
            validate_source_slots(&record.source_slots)?;
            match record.admission {
                AdmissionState::Pending
                    if record.claim_sha256.is_some()
                        && record.token_sha256.is_none()
                        && record.token_generation.is_none() => {}
                AdmissionState::Approved
                    if (record.claim_sha256.is_some()
                        && record.token_sha256.is_none()
                        && record.token_generation.is_none())
                        || (record.claim_sha256.is_none()
                            && record.token_sha256.is_some()
                            && record.token_generation.is_some()) => {}
                AdmissionState::Removed
                    if record.token_sha256.is_none()
                        && record.token_generation.is_none()
                        && !record.request_readmission => {}
                _ => return Err(RoomStateError::CredentialShape),
            }
        }

        let mut visibility_keys = Vec::with_capacity(self.visibility_overrides.len());
        for visibility in &self.visibility_overrides {
            let Some(publisher) = self.member(visibility.publisher_member_id) else {
                return Err(RoomStateError::VisibilityReference);
            };
            if publisher.admission != AdmissionState::Approved
                || publisher.source_kind(visibility.source_id).is_none()
                || visibility.hidden_viewer_ids.len() > 31
            {
                return Err(RoomStateError::VisibilityReference);
            }
            let mut viewers = visibility.hidden_viewer_ids.clone();
            viewers.sort_unstable();
            if viewers.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(RoomStateError::VisibilityDuplicate);
            }
            for viewer_id in viewers {
                let Some(viewer) = self.member(viewer_id) else {
                    return Err(RoomStateError::VisibilityReference);
                };
                if viewer.admission != AdmissionState::Approved
                    || viewer.member_id == publisher.member_id
                {
                    return Err(RoomStateError::VisibilityReference);
                }
            }
            visibility_keys.push((visibility.publisher_member_id, visibility.source_id));
        }
        visibility_keys.sort_unstable();
        if visibility_keys.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RoomStateError::VisibilityDuplicate);
        }
        Ok(())
    }

    /// Parses a strict persistent snapshot with duplicate-key and bounds checks.
    pub fn from_json(input: &[u8]) -> Result<Self, RoomStateLoadError> {
        Ok(parse_typed(input, JsonLimits::REGISTRY)?)
    }

    /// Serializes the validated full persistent snapshot.
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Finds one admission record.
    #[must_use]
    pub fn member(&self, member_id: MemberId) -> Option<&MemberRecord> {
        self.members
            .iter()
            .find(|record| record.member_id == member_id)
    }

    /// Verifies an approved member's current token and exact scope.
    pub fn verify_member_token(
        &self,
        room_id: RoomId,
        member_id: MemberId,
        token: &MemberToken,
    ) -> Result<(), CredentialError> {
        if room_id != self.room.room_id {
            return Err(CredentialError::RoomScope);
        }
        let Some(record) = self.member(member_id) else {
            return if self.token_belongs_to_another_member(member_id, token) {
                Err(CredentialError::MemberScope)
            } else {
                Err(CredentialError::InvalidToken)
            };
        };
        self.verify_member_bootstrap(
            room_id,
            member_id,
            record.token_generation.map_or(0, DecimalU64::get),
            record
                .token_sha256
                .unwrap_or_else(|| TokenDigest::of(token.expose_bytes())),
            token,
        )
    }

    /// Verifies every explicit credential component in an engine bootstrap.
    pub fn verify_member_bootstrap(
        &self,
        room_id: RoomId,
        member_id: MemberId,
        token_generation: u64,
        token_sha256: TokenDigest,
        token: &MemberToken,
    ) -> Result<(), CredentialError> {
        if room_id != self.room.room_id {
            return Err(CredentialError::RoomScope);
        }
        let Some(record) = self.member(member_id) else {
            return if self.token_belongs_to_another_member(member_id, token) {
                Err(CredentialError::MemberScope)
            } else {
                Err(CredentialError::InvalidToken)
            };
        };
        if record.admission == AdmissionState::Removed {
            return Err(CredentialError::Revoked);
        }
        let credential = record
            .credential(self.room.room_id)
            .ok_or(CredentialError::InvalidToken)?;
        match credential.verify_bootstrap(
            room_id,
            member_id,
            token_generation,
            token_sha256,
            token,
            &Sha256TokenVerifier,
        ) {
            Err(CredentialError::InvalidToken)
                if self.token_belongs_to_another_member(member_id, token) =>
            {
                Err(CredentialError::MemberScope)
            }
            result => result,
        }
    }

    /// Returns the provisioning-visible state after validating the pending
    /// claim proof. Claimed sessions use member-token authentication instead.
    pub fn enrollment_status(
        &self,
        room_id: RoomId,
        member_id: MemberId,
        claim_secret: &ClaimSecret,
    ) -> Result<EnrollmentView, RoomReduceError> {
        if room_id != self.room.room_id {
            return Err(RoomReduceError::RoomScope);
        }
        let record = self
            .member(member_id)
            .ok_or(RoomReduceError::MemberNotFound)?;
        if record.claim_sha256 != Some(claim_secret.digest()) {
            return Err(RoomReduceError::ClaimInvalid);
        }
        Ok(EnrollmentView {
            room_id: self.room.room_id,
            room_name: self.room.name.clone(),
            member_id,
            admission: record.admission,
            room_revision: self.room.room_revision,
        })
    }

    /// Returns the logical visible viewer set for an approved source.
    pub fn visible_viewers(
        &self,
        publisher_member_id: MemberId,
        source_id: SourceId,
    ) -> Result<Vec<MemberId>, RoomReduceError> {
        let publisher = self
            .member(publisher_member_id)
            .ok_or(RoomReduceError::MemberNotFound)?;
        if publisher.admission != AdmissionState::Approved
            || publisher.source_kind(source_id).is_none()
        {
            return Err(RoomReduceError::SourceNotFound);
        }
        let hidden = self
            .visibility_overrides
            .iter()
            .find(|entry| {
                entry.publisher_member_id == publisher_member_id && entry.source_id == source_id
            })
            .map_or(&[][..], |entry| entry.hidden_viewer_ids.as_slice());
        Ok(self
            .members
            .iter()
            .filter(|record| {
                record.admission == AdmissionState::Approved
                    && record.member_id != publisher_member_id
                    && !hidden.contains(&record.member_id)
            })
            .map(|record| record.member_id)
            .collect())
    }

    fn count(&self, admission: AdmissionState) -> usize {
        self.members
            .iter()
            .filter(|record| record.admission == admission)
            .count()
    }

    fn token_belongs_to_another_member(
        &self,
        claimed_member_id: MemberId,
        token: &MemberToken,
    ) -> bool {
        self.members.iter().any(|record| {
            record.member_id != claimed_member_id
                && record.admission == AdmissionState::Approved
                && record
                    .token_sha256
                    .is_some_and(|digest| Sha256TokenVerifier.verify(&digest, token))
        })
    }
}

/// One pure room-state mutation.
#[derive(Clone, Debug)]
pub enum RoomAction {
    Enroll {
        member_id: MemberId,
        display_name: String,
        source_slots: [SourceSlot; 2],
        claim_sha256: TokenDigest,
        request_readmission: bool,
    },
    Approve {
        member_id: MemberId,
    },
    Reject {
        member_id: MemberId,
    },
    Remove {
        member_id: MemberId,
        reason: AdmissionReason,
    },
    Claim {
        member_id: MemberId,
        claim_secret: ClaimSecret,
        issued_token: MemberToken,
    },
    SetName {
        name: String,
    },
    ReplaceVisibility {
        publisher_member_id: MemberId,
        source_id: SourceId,
        viewer_member_ids: Vec<MemberId>,
    },
    SetQualityCap {
        profile: QualityProfile,
    },
    SetQos {
        mode: QosPolicy,
    },
    SetSendMute {
        enabled: bool,
    },
}

/// Provisioning response projection; it deliberately excludes roster, lease,
/// session, endpoint, and token fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrollmentView {
    pub room_id: RoomId,
    pub room_name: String,
    pub member_id: MemberId,
    pub admission: AdmissionState,
    pub room_revision: RoomRevision,
}

/// Stable event emitted after a durable room-state candidate is installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PersistentRoomEvent {
    AdmissionChanged {
        member_id: MemberId,
        admission: AdmissionState,
        reason: Option<AdmissionReason>,
    },
    CredentialIssued {
        member_id: MemberId,
        token_generation: DecimalU64,
    },
    RoomNameChanged {
        name: String,
    },
    SourceVisibilityChanged {
        publisher_member_id: MemberId,
        source_id: SourceId,
        viewer_member_ids: Vec<MemberId>,
    },
    QualityCapChanged {
        profile: QualityProfile,
    },
    QosChanged {
        mode: QosPolicy,
    },
    SendMuteChanged {
        enabled: bool,
    },
}

/// Candidate state and event produced by the pure room reducer.
#[derive(Clone, Debug)]
pub struct RoomTransition {
    pub state: RoomState,
    pub event: Option<PersistentRoomEvent>,
    pub issued_token: Option<MemberToken>,
}

impl RoomTransition {
    /// Reports whether a durable write and room revision change are required.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.event.is_some()
    }
}

/// Applies an action to a cloned candidate. The caller must durably persist the
/// returned state before broadcasting its event.
pub fn reduce_room(
    current: &RoomState,
    action: RoomAction,
) -> Result<RoomTransition, RoomReduceError> {
    let mut next = current.clone();
    let mut issued_token = None;
    let event = match action {
        RoomAction::Enroll {
            member_id,
            display_name,
            source_slots,
            claim_sha256,
            request_readmission,
        } => {
            let display_name =
                normalize_name(display_name).map_err(|_| RoomReduceError::InvalidValue)?;
            validate_source_slots(&source_slots).map_err(|_| RoomReduceError::InvalidValue)?;
            if let Some(index) = next
                .members
                .iter()
                .position(|record| record.member_id == member_id)
            {
                let record = &next.members[index];
                if record.source_slots != source_slots {
                    return Err(RoomReduceError::SourceIdentityChanged);
                }
                match record.admission {
                    AdmissionState::Removed if request_readmission => {
                        if next.count(AdmissionState::Pending) >= MAX_PENDING_MEMBERS {
                            return Err(RoomReduceError::PendingCapacity);
                        }
                        let record = &mut next.members[index];
                        record.admission = AdmissionState::Pending;
                        record.last_display_name = display_name;
                        record.claim_sha256 = Some(claim_sha256);
                        record.token_sha256 = None;
                        record.token_generation = None;
                        record.request_readmission = true;
                        Some(PersistentRoomEvent::AdmissionChanged {
                            member_id,
                            admission: AdmissionState::Pending,
                            reason: None,
                        })
                    }
                    AdmissionState::Removed => return Err(RoomReduceError::MemberRemoved),
                    AdmissionState::Pending
                        if !request_readmission
                            && record.claim_sha256 == Some(claim_sha256)
                            && record.last_display_name == display_name =>
                    {
                        None
                    }
                    AdmissionState::Pending => return Err(RoomReduceError::InvalidValue),
                    AdmissionState::Approved => return Err(RoomReduceError::InvalidState),
                }
            } else {
                if request_readmission {
                    return Err(RoomReduceError::InvalidValue);
                }
                if next.count(AdmissionState::Pending) >= MAX_PENDING_MEMBERS {
                    return Err(RoomReduceError::PendingCapacity);
                }
                next.members.push(MemberRecord {
                    member_id,
                    admission: AdmissionState::Pending,
                    last_display_name: display_name,
                    source_slots,
                    token_sha256: None,
                    token_generation: None,
                    claim_sha256: Some(claim_sha256),
                    request_readmission: false,
                });
                Some(PersistentRoomEvent::AdmissionChanged {
                    member_id,
                    admission: AdmissionState::Pending,
                    reason: None,
                })
            }
        }
        RoomAction::Approve { member_id } => {
            if next.count(AdmissionState::Approved) >= MAX_APPROVED_MEMBERS {
                return Err(RoomReduceError::RoomFull);
            }
            let record = next
                .members
                .iter_mut()
                .find(|record| record.member_id == member_id)
                .ok_or(RoomReduceError::MemberNotFound)?;
            if record.admission != AdmissionState::Pending {
                return Err(RoomReduceError::InvalidState);
            }
            record.admission = AdmissionState::Approved;
            record.request_readmission = false;
            Some(PersistentRoomEvent::AdmissionChanged {
                member_id,
                admission: AdmissionState::Approved,
                reason: None,
            })
        }
        RoomAction::Reject { member_id } => {
            if next.count(AdmissionState::Removed) >= MAX_REMOVED_MEMBERS {
                return Err(RoomReduceError::RemovedCapacity);
            }
            let record = next
                .members
                .iter_mut()
                .find(|record| record.member_id == member_id)
                .ok_or(RoomReduceError::MemberNotFound)?;
            if record.admission != AdmissionState::Pending {
                return Err(RoomReduceError::InvalidState);
            }
            make_removed(record, false);
            Some(PersistentRoomEvent::AdmissionChanged {
                member_id,
                admission: AdmissionState::Removed,
                reason: Some(AdmissionReason::OperatorRejected),
            })
        }
        RoomAction::Remove { member_id, reason } => {
            if reason == AdmissionReason::OperatorRejected {
                return Err(RoomReduceError::InvalidValue);
            }
            let index = next
                .members
                .iter()
                .position(|record| record.member_id == member_id)
                .ok_or(RoomReduceError::MemberNotFound)?;
            if next.members[index].admission == AdmissionState::Removed {
                None
            } else {
                if next.count(AdmissionState::Removed) >= MAX_REMOVED_MEMBERS {
                    return Err(RoomReduceError::RemovedCapacity);
                }
                make_removed(&mut next.members[index], true);
                next.visibility_overrides.retain_mut(|visibility| {
                    if visibility.publisher_member_id == member_id {
                        return false;
                    }
                    visibility
                        .hidden_viewer_ids
                        .retain(|viewer_id| *viewer_id != member_id);
                    true
                });
                Some(PersistentRoomEvent::AdmissionChanged {
                    member_id,
                    admission: AdmissionState::Removed,
                    reason: Some(reason),
                })
            }
        }
        RoomAction::Claim {
            member_id,
            claim_secret,
            issued_token: token,
        } => {
            let record = next
                .members
                .iter_mut()
                .find(|record| record.member_id == member_id)
                .ok_or(RoomReduceError::MemberNotFound)?;
            if record.admission != AdmissionState::Approved {
                return Err(RoomReduceError::InvalidState);
            }
            if record.claim_sha256 != Some(claim_secret.digest()) {
                return Err(RoomReduceError::ClaimInvalid);
            }
            let generation = DecimalU64::new(
                record
                    .token_generation
                    .map_or(1, |value| value.get().saturating_add(1)),
            );
            record.token_sha256 = Some(TokenDigest::of(token.expose_bytes()));
            record.token_generation = Some(generation);
            record.claim_sha256 = None;
            issued_token = Some(token);
            Some(PersistentRoomEvent::CredentialIssued {
                member_id,
                token_generation: generation,
            })
        }
        RoomAction::SetName { name } => {
            let name = normalize_name(name).map_err(|_| RoomReduceError::InvalidValue)?;
            if next.room.name == name {
                None
            } else {
                next.room.name = name.clone();
                Some(PersistentRoomEvent::RoomNameChanged { name })
            }
        }
        RoomAction::ReplaceVisibility {
            publisher_member_id,
            source_id,
            viewer_member_ids,
        } => {
            validate_visible_set(&next, publisher_member_id, source_id, &viewer_member_ids)?;
            let hidden_viewer_ids: Vec<_> = next
                .members
                .iter()
                .filter(|record| {
                    record.admission == AdmissionState::Approved
                        && record.member_id != publisher_member_id
                        && !viewer_member_ids.contains(&record.member_id)
                })
                .map(|record| record.member_id)
                .collect();
            let existing = next.visibility_overrides.iter().position(|entry| {
                entry.publisher_member_id == publisher_member_id && entry.source_id == source_id
            });
            let old_hidden = existing
                .map(|index| {
                    next.visibility_overrides[index]
                        .hidden_viewer_ids
                        .as_slice()
                })
                .unwrap_or(&[]);
            if same_set(old_hidden, &hidden_viewer_ids) {
                None
            } else {
                match (existing, hidden_viewer_ids.is_empty()) {
                    (Some(index), true) => {
                        next.visibility_overrides.remove(index);
                    }
                    (Some(index), false) => {
                        next.visibility_overrides[index].hidden_viewer_ids = hidden_viewer_ids;
                    }
                    (None, false) => next.visibility_overrides.push(VisibilityOverride {
                        publisher_member_id,
                        source_id,
                        hidden_viewer_ids,
                    }),
                    (None, true) => {}
                }
                Some(PersistentRoomEvent::SourceVisibilityChanged {
                    publisher_member_id,
                    source_id,
                    viewer_member_ids,
                })
            }
        }
        RoomAction::SetQualityCap { profile } => {
            if next.room.quality_cap == profile {
                None
            } else {
                next.room.quality_cap = profile;
                Some(PersistentRoomEvent::QualityCapChanged { profile })
            }
        }
        RoomAction::SetQos { mode } => {
            if next.room.qos_policy == mode {
                None
            } else {
                next.room.qos_policy = mode;
                Some(PersistentRoomEvent::QosChanged { mode })
            }
        }
        RoomAction::SetSendMute { enabled } => {
            if next.room.room_send_muted == enabled {
                None
            } else {
                next.room.room_send_muted = enabled;
                Some(PersistentRoomEvent::SendMuteChanged { enabled })
            }
        }
    };

    if event.is_some() {
        next.room.room_revision = current
            .room
            .room_revision
            .checked_next()
            .ok_or(RoomReduceError::RevisionExhausted)?;
    }
    next.validate()
        .map_err(|_| RoomReduceError::InvalidCandidate)?;
    Ok(RoomTransition {
        state: next,
        event,
        issued_token,
    })
}

/// Applies a validated B authority command after checking its expected revision.
pub fn reduce_authority_command(
    current: &RoomState,
    command: &ValidatedRoomCommand,
) -> Result<RoomTransition, RoomReduceError> {
    let ValidatedRoomCommand::Authority {
        room_id,
        expected_room_revision,
        mutation,
        ..
    } = command
    else {
        return Err(RoomReduceError::CommandBoundary);
    };
    if *room_id != current.room.room_id {
        return Err(RoomReduceError::RoomScope);
    }
    if *expected_room_revision != current.room.room_revision {
        return Err(RoomReduceError::StaleRoomRevision);
    }
    let action = match mutation {
        AuthorityMutation::SetName { name } => RoomAction::SetName { name: name.clone() },
        AuthorityMutation::Approve { member_id } => RoomAction::Approve {
            member_id: *member_id,
        },
        AuthorityMutation::Reject { member_id } => RoomAction::Reject {
            member_id: *member_id,
        },
        AuthorityMutation::Remove { member_id, reason } => RoomAction::Remove {
            member_id: *member_id,
            reason: *reason,
        },
        AuthorityMutation::ReplaceVisibility {
            publisher_member_id,
            source_id,
            viewer_member_ids,
        } => RoomAction::ReplaceVisibility {
            publisher_member_id: *publisher_member_id,
            source_id: *source_id,
            viewer_member_ids: viewer_member_ids.clone(),
        },
        AuthorityMutation::SetQualityCap { profile } => {
            RoomAction::SetQualityCap { profile: *profile }
        }
        AuthorityMutation::SetQos { mode } => RoomAction::SetQos { mode: *mode },
        AuthorityMutation::SetSendMute { enabled } => RoomAction::SetSendMute { enabled: *enabled },
    };
    reduce_room(current, action)
}

fn make_removed(record: &mut MemberRecord, clear_claim: bool) {
    record.admission = AdmissionState::Removed;
    record.token_sha256 = None;
    record.token_generation = None;
    if clear_claim {
        record.claim_sha256 = None;
    }
    record.request_readmission = false;
}

fn validate_visible_set(
    state: &RoomState,
    publisher_member_id: MemberId,
    source_id: SourceId,
    viewer_member_ids: &[MemberId],
) -> Result<(), RoomReduceError> {
    if viewer_member_ids.len() > 31 || !all_unique(viewer_member_ids) {
        return Err(RoomReduceError::InvalidValue);
    }
    let publisher = state
        .member(publisher_member_id)
        .ok_or(RoomReduceError::MemberNotFound)?;
    if publisher.admission != AdmissionState::Approved || publisher.source_kind(source_id).is_none()
    {
        return Err(RoomReduceError::SourceNotFound);
    }
    for viewer_id in viewer_member_ids {
        let viewer = state
            .member(*viewer_id)
            .ok_or(RoomReduceError::MemberNotFound)?;
        if viewer.admission != AdmissionState::Approved || viewer.member_id == publisher_member_id {
            return Err(RoomReduceError::InvalidValue);
        }
    }
    Ok(())
}

fn all_unique(values: &[MemberId]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    !sorted.windows(2).any(|pair| pair[0] == pair[1])
}

fn same_set(left: &[MemberId], right: &[MemberId]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

fn validate_source_slots(slots: &[SourceSlot; 2]) -> Result<(), RoomStateError> {
    if slots[0].source_id == slots[1].source_id
        || slots[0].kind == slots[1].kind
        || !slots
            .iter()
            .any(|slot| slot.kind == SourceKind::SystemAudio)
        || !slots.iter().any(|slot| slot.kind == SourceKind::Microphone)
    {
        Err(RoomStateError::SourceSlots)
    } else {
        Ok(())
    }
}

fn validate_name(value: &str) -> Result<(), RoomStateError> {
    if value.is_empty()
        || value.len() > 64
        || value.nfc().ne(value.chars())
        || value
            .chars()
            .any(|character| matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'))
    {
        Err(RoomStateError::Name)
    } else {
        Ok(())
    }
}

fn normalize_name(value: String) -> Result<String, RoomStateError> {
    let normalized: String = value.nfc().collect();
    validate_name(&normalized)?;
    Ok(normalized)
}

/// Persistent snapshot invariant failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RoomStateError {
    #[error("persistent schema version is unsupported")]
    SchemaVersion,
    #[error("room or member display name is invalid")]
    Name,
    #[error("admission collection exceeds a hard bound")]
    MemberBounds,
    #[error("member identity appears more than once")]
    DuplicateMember,
    #[error("source slots are not the fixed two-kind set")]
    SourceSlots,
    #[error("credential fields do not match admission state")]
    CredentialShape,
    #[error("visibility references an ineligible identity or source")]
    VisibilityReference,
    #[error("visibility key or viewer identity is duplicated")]
    VisibilityDuplicate,
}

/// Strict snapshot load failure.
#[derive(Debug, Error)]
pub enum RoomStateLoadError {
    #[error(transparent)]
    Json(#[from] StrictJsonError),
}

impl RoomStateLoadError {
    /// Persistent state load failures all enter the stable corrupt-state mode.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        ErrorCode::RegistryStateCorrupt
    }
}

/// Stable room reducer rejection.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RoomReduceError {
    #[error("room scope does not match")]
    RoomScope,
    #[error("expected room revision is stale")]
    StaleRoomRevision,
    #[error("member does not exist")]
    MemberNotFound,
    #[error("source does not exist")]
    SourceNotFound,
    #[error("member is removed")]
    MemberRemoved,
    #[error("pending admission capacity is full")]
    PendingCapacity,
    #[error("approved admission capacity is full")]
    RoomFull,
    #[error("removed admission capacity is full")]
    RemovedCapacity,
    #[error("source identity changed")]
    SourceIdentityChanged,
    #[error("claim proof is invalid")]
    ClaimInvalid,
    #[error("state transition is invalid")]
    InvalidState,
    #[error("value is invalid")]
    InvalidValue,
    #[error("room revision is exhausted")]
    RevisionExhausted,
    #[error("candidate state failed full validation")]
    InvalidCandidate,
    #[error("member-owned command reached room reducer")]
    CommandBoundary,
}

impl RoomReduceError {
    /// Returns the stable registry protocol code.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::RoomScope => ErrorCode::RoomScopeMismatch,
            Self::StaleRoomRevision => ErrorCode::StaleRoomRevision,
            Self::MemberNotFound => ErrorCode::MemberNotFound,
            Self::SourceNotFound => ErrorCode::SourceNotFound,
            Self::MemberRemoved => ErrorCode::MemberRemoved,
            Self::PendingCapacity => ErrorCode::PendingCapacity,
            Self::RoomFull => ErrorCode::RoomFull,
            Self::RemovedCapacity => ErrorCode::RemovedCapacity,
            Self::SourceIdentityChanged => ErrorCode::SourceIdentityChanged,
            Self::ClaimInvalid => ErrorCode::ClaimInvalid,
            Self::InvalidState => ErrorCode::InvalidStateTransition,
            Self::InvalidValue => ErrorCode::InvalidValue,
            Self::RevisionExhausted | Self::InvalidCandidate => ErrorCode::RegistryStateCorrupt,
            Self::CommandBoundary => ErrorCode::CommandBoundaryViolation,
        }
    }
}

#[cfg(test)]
mod tests {
    use rlb_model::{MemberId, RoomId, SourceId};

    use super::*;

    fn room_id() -> RoomId {
        RoomId::from_bytes(1_u128.to_be_bytes()).unwrap()
    }

    fn member_id(index: u128) -> MemberId {
        MemberId::from_bytes((index + 100).to_be_bytes()).unwrap()
    }

    fn source_id(index: u128, microphone: bool) -> SourceId {
        let offset = if microphone { 10_000 } else { 1_000 };
        SourceId::from_bytes((index + offset).to_be_bytes()).unwrap()
    }

    fn slots(index: u128) -> [SourceSlot; 2] {
        [
            SourceSlot {
                source_id: source_id(index, false),
                kind: SourceKind::SystemAudio,
            },
            SourceSlot {
                source_id: source_id(index, true),
                kind: SourceKind::Microphone,
            },
        ]
    }

    fn enroll(state: &RoomState, index: u128) -> Result<RoomTransition, RoomReduceError> {
        reduce_room(
            state,
            RoomAction::Enroll {
                member_id: member_id(index),
                display_name: format!("member-{index}"),
                source_slots: slots(index),
                claim_sha256: TokenDigest::of(&index.to_be_bytes()),
                request_readmission: false,
            },
        )
    }

    #[test]
    fn admission_token_removal_and_readmission_are_explicit() {
        let state = RoomState::new(room_id(), "家庭房间").unwrap();
        let state = enroll(&state, 1).unwrap().state;
        assert_eq!(
            state.member(member_id(1)).unwrap().admission,
            AdmissionState::Pending
        );
        let state = reduce_room(
            &state,
            RoomAction::Approve {
                member_id: member_id(1),
            },
        )
        .unwrap()
        .state;

        let claim = ClaimSecret::parse(&ClaimSecret::generate().unwrap().expose_encoded()).unwrap();
        let state = reduce_room(
            &state,
            RoomAction::Enroll {
                member_id: member_id(2),
                display_name: "member-2".to_owned(),
                source_slots: slots(2),
                claim_sha256: claim.digest(),
                request_readmission: false,
            },
        )
        .unwrap()
        .state;
        let state = reduce_room(
            &state,
            RoomAction::Approve {
                member_id: member_id(2),
            },
        )
        .unwrap()
        .state;
        let token = MemberToken::from_bytes(vec![7; 32]).unwrap();
        let claimed = reduce_room(
            &state,
            RoomAction::Claim {
                member_id: member_id(2),
                claim_secret: claim,
                issued_token: token.clone(),
            },
        )
        .unwrap();
        assert_eq!(claimed.issued_token, Some(token.clone()));
        assert!(
            claimed
                .state
                .verify_member_token(room_id(), member_id(2), &token)
                .is_ok()
        );

        let removed = reduce_room(
            &claimed.state,
            RoomAction::Remove {
                member_id: member_id(2),
                reason: AdmissionReason::OperatorRemoved,
            },
        )
        .unwrap()
        .state;
        assert_eq!(
            removed.verify_member_token(room_id(), member_id(2), &token),
            Err(CredentialError::Revoked)
        );
        assert_eq!(
            reduce_room(
                &removed,
                RoomAction::Enroll {
                    member_id: member_id(2),
                    display_name: "member-2".to_owned(),
                    source_slots: slots(2),
                    claim_sha256: TokenDigest::of(&[9; 32]),
                    request_readmission: false,
                }
            )
            .unwrap_err(),
            RoomReduceError::MemberRemoved
        );
        let readmitted = reduce_room(
            &removed,
            RoomAction::Enroll {
                member_id: member_id(2),
                display_name: "member-2".to_owned(),
                source_slots: slots(2),
                claim_sha256: TokenDigest::of(&[9; 32]),
                request_readmission: true,
            },
        )
        .unwrap();
        assert_eq!(
            readmitted.state.member(member_id(2)).unwrap().admission,
            AdmissionState::Pending
        );
    }

    #[test]
    fn rejected_pending_member_can_poll_removed_status_with_claim_proof() {
        let claim = ClaimSecret::generate().unwrap();
        let state = RoomState::new(room_id(), "room").unwrap();
        let state = reduce_room(
            &state,
            RoomAction::Enroll {
                member_id: member_id(1),
                display_name: "member".to_owned(),
                source_slots: slots(1),
                claim_sha256: claim.digest(),
                request_readmission: false,
            },
        )
        .unwrap()
        .state;
        assert_eq!(
            state
                .enrollment_status(room_id(), member_id(1), &claim)
                .unwrap()
                .admission,
            AdmissionState::Pending
        );
        let state = reduce_room(
            &state,
            RoomAction::Reject {
                member_id: member_id(1),
            },
        )
        .unwrap()
        .state;
        assert_eq!(
            state
                .enrollment_status(room_id(), member_id(1), &claim)
                .unwrap()
                .admission,
            AdmissionState::Removed
        );
    }

    #[test]
    fn pending_and_removed_bounds_never_evict_records() {
        let mut state = RoomState::new(room_id(), "room").unwrap();
        for index in 1..=MAX_PENDING_MEMBERS as u128 {
            state = enroll(&state, index).unwrap().state;
        }
        let before = state.clone();
        assert_eq!(
            enroll(&state, 99).unwrap_err(),
            RoomReduceError::PendingCapacity
        );
        assert_eq!(state, before);

        let mut removed = RoomState::new(room_id(), "room").unwrap();
        for index in 1..=MAX_REMOVED_MEMBERS as u128 {
            removed.members.push(MemberRecord {
                member_id: member_id(index),
                admission: AdmissionState::Removed,
                last_display_name: format!("removed-{index}"),
                source_slots: slots(index),
                token_sha256: None,
                token_generation: None,
                claim_sha256: None,
                request_readmission: false,
            });
        }
        removed.members.push(MemberRecord {
            member_id: member_id(500),
            admission: AdmissionState::Pending,
            last_display_name: "pending".to_owned(),
            source_slots: slots(500),
            token_sha256: None,
            token_generation: None,
            claim_sha256: Some(TokenDigest::of(&[1; 32])),
            request_readmission: false,
        });
        removed.validate().unwrap();
        assert_eq!(
            reduce_room(
                &removed,
                RoomAction::Reject {
                    member_id: member_id(500)
                }
            )
            .unwrap_err(),
            RoomReduceError::RemovedCapacity
        );
    }

    #[test]
    fn approved_bound_preserves_the_pending_candidate() {
        let mut state = RoomState::new(room_id(), "room").unwrap();
        for index in 1..=MAX_APPROVED_MEMBERS as u128 {
            state = enroll(&state, index).unwrap().state;
            state = reduce_room(
                &state,
                RoomAction::Approve {
                    member_id: member_id(index),
                },
            )
            .unwrap()
            .state;
        }
        state = enroll(&state, 500).unwrap().state;
        let before = state.clone();
        assert_eq!(
            reduce_room(
                &state,
                RoomAction::Approve {
                    member_id: member_id(500),
                },
            )
            .unwrap_err(),
            RoomReduceError::RoomFull
        );
        assert_eq!(state, before);
        assert_eq!(
            state.member(member_id(500)).unwrap().admission,
            AdmissionState::Pending
        );
    }

    #[test]
    fn persistent_json_is_strict_and_revision_changes_only_on_mutation() {
        let state = RoomState::new(room_id(), "room").unwrap();
        let unchanged = reduce_room(
            &state,
            RoomAction::SetName {
                name: "room".to_owned(),
            },
        )
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.state.room.room_revision, state.room.room_revision);

        let mut value: serde_json::Value =
            serde_json::from_slice(&state.to_json().unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), serde_json::json!(true));
        assert!(matches!(
            RoomState::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(RoomStateLoadError::Json(StrictJsonError::UnknownField))
        ));
    }

    #[test]
    fn room_and_member_names_are_persisted_in_nfc() {
        let state = RoomState::new(room_id(), "Cafe\u{301}").unwrap();
        assert_eq!(state.room.name, "Café");
        let state = reduce_room(
            &state,
            RoomAction::Enroll {
                member_id: member_id(1),
                display_name: "Jose\u{301}".to_owned(),
                source_slots: slots(1),
                claim_sha256: TokenDigest::of(&[1; 32]),
                request_readmission: false,
            },
        )
        .unwrap()
        .state;
        assert_eq!(
            state.member(member_id(1)).unwrap().last_display_name,
            "José"
        );
        assert!(state.validate().is_ok());
    }

    #[test]
    fn persistent_non_nfc_names_are_rejected_on_load() {
        let state = RoomState::new(room_id(), "room").unwrap();
        let json = String::from_utf8(state.to_json().unwrap())
            .unwrap()
            .replace("\"name\":\"room\"", "\"name\":\"Cafe\\u0301\"");
        assert!(RoomState::from_json(json.as_bytes()).is_err());
    }
}
