use std::collections::HashSet;
use std::net::Ipv4Addr;

use rlb_control::{
    CanonicalRevisions, DelegatedMemberState, MemberSourceConfig, RemoteMemberMix, RemoteSourceMix,
    RequestBoundary, RequestCache, RequestCachePolicy, RequestScope, WarningCode,
};
use rlb_model::{
    Capability, CaptureMethod, CatalogRevision, Endpoint, EndpointMode, ErrorCode, GainDb,
    GrantRevision, IdentityRecord, LocalSourceConfig, MemberConfigRevision, MemberId,
    MemberRoomConfig, QualityProfile, RoomId, RoomRevision, SourceId, SourceKind,
};
use rlb_registry::{MemberCommandEffective, MemberCommandStatus};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

/// Persistent schema implemented by this member-session slice.
pub const MEMBER_CONFIG_SCHEMA_VERSION: u16 = 3;
/// Previous on-disk schema migrated on load.
pub const MEMBER_CONFIG_SCHEMA_V2: u16 = 2;

pub(crate) const MEMBER_AUTHORITY_PRINCIPAL: &str = "room_authority";

/// Persisted terminal response for one accepted delegated room command.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberCommandResultRecord {
    pub capability: Capability,
    pub status: MemberCommandStatus,
    pub revisions: CanonicalRevisions,
    pub code: Option<ErrorCode>,
    pub effective: MemberCommandEffective,
    pub warnings: Vec<WarningCode>,
}

impl MemberCommandResultRecord {
    fn validate(&self) -> Result<(), MemberConfigError> {
        if matches!(self.status, MemberCommandStatus::Rejected) != self.code.is_some()
            || self.warnings.len() > 16
            || self.revisions.room_revision.is_none()
            || self.revisions.catalog_revision.is_none()
            || self.revisions.member_config_revision.is_none()
            || self.revisions.grant_revision.is_none()
        {
            return Err(MemberConfigError::IdempotencyCache);
        }
        Ok(())
    }
}

fn empty_member_command_cache() -> RequestCache<MemberCommandResultRecord> {
    RequestCache::new(RequestCachePolicy::MEMBER)
}

/// Follow/pin selection persisted for one WASAPI slot.
///
/// `LocalSourceConfig` stays `Copy` and holds capture/publish/quality only.
/// Endpoint identity is a Windows string, so binding lives here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceBindingConfig {
    pub endpoint_mode: EndpointMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
}

impl Default for DeviceBindingConfig {
    fn default() -> Self {
        Self {
            endpoint_mode: EndpointMode::FollowDefault,
            endpoint_id: None,
        }
    }
}

impl DeviceBindingConfig {
    /// Follows the Windows console-role default.
    #[must_use]
    pub fn follow_default() -> Self {
        Self::default()
    }

    /// Builds a binding from a validated A command choice.
    #[must_use]
    pub fn from_choice(endpoint_mode: EndpointMode, endpoint_id: Option<String>) -> Self {
        Self {
            endpoint_mode,
            endpoint_id,
        }
    }

    /// `pinned` requires a nonempty endpoint ID; `follow_default` forbids one.
    pub fn validate(&self) -> Result<(), MemberConfigError> {
        match self.endpoint_mode {
            EndpointMode::FollowDefault => {
                if self.endpoint_id.is_some() {
                    Err(MemberConfigError::DeviceBinding)
                } else {
                    Ok(())
                }
            }
            EndpointMode::Pinned => match self.endpoint_id.as_deref() {
                Some(id) if !id.is_empty() && id.len() <= 4_096 && !id.contains('\0') => Ok(()),
                _ => Err(MemberConfigError::DeviceBinding),
            },
        }
    }
}

/// Local listen (monitor) knobs for one source. Absent `enabled` means unset.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMonitorConfig {
    pub enabled: bool,
    pub gain_db: GainDb,
}

/// User-selected OpenWrt join endpoints. Absent until `local.room.join`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JoinTarget {
    pub authority_url: String,
    pub member_registry_endpoint: Endpoint,
}

impl JoinTarget {
    fn validate(&self) -> Result<(), MemberConfigError> {
        if self.authority_url.is_empty()
            || self.authority_url.len() > 1_024
            || self.authority_url.chars().any(char::is_control)
        {
            return Err(MemberConfigError::JoinTarget);
        }

        let authority =
            Url::parse(&self.authority_url).map_err(|_| MemberConfigError::JoinTarget)?;
        let has_user_info = authority_user_info_present(&self.authority_url);
        if !matches!(authority.scheme(), "http" | "https")
            || has_user_info
            || !authority.username().is_empty()
            || authority.password().is_some()
            || authority.query().is_some()
            || authority.fragment().is_some()
        {
            return Err(MemberConfigError::JoinTarget);
        }
        let Some(Host::Ipv4(authority_ip)) = authority.host() else {
            return Err(MemberConfigError::JoinTarget);
        };
        if !is_rfc1918(authority_ip)
            || !is_rfc1918(self.member_registry_endpoint.ip())
            || self.member_registry_endpoint.port() != 17_099
        {
            return Err(MemberConfigError::JoinTarget);
        }
        Ok(())
    }
}

fn authority_user_info_present(authority_url: &str) -> bool {
    let Some(authority_start) = authority_url.find("://").map(|position| position + 3) else {
        return false;
    };
    let authority_end = authority_url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(authority_url.len(), |position| authority_start + position);
    authority_url[authority_start..authority_end].contains('@')
}

const fn is_rfc1918(address: Ipv4Addr) -> bool {
    let [first, second, _, _] = address.octets();
    first == 10 || (first == 172 && second >= 16 && second <= 31) || (first == 192 && second == 168)
}

/// Member-owned logging selection. `directory: None` means the user has not set a path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    pub max_bytes: u64,
    pub retention_count: u16,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            max_bytes: 10_000_000,
            retention_count: 8,
        }
    }
}

impl LoggingConfig {
    fn validate(&self) -> Result<(), MemberConfigError> {
        if self.max_bytes < 1_024 || self.max_bytes > 1_000_000_000 {
            return Err(MemberConfigError::Logging);
        }
        if !(1..=64).contains(&self.retention_count) {
            return Err(MemberConfigError::Logging);
        }
        if let Some(directory) = &self.directory {
            if directory.is_empty() || directory.len() > 4_096 || directory.contains('\0') {
                return Err(MemberConfigError::Logging);
            }
        }
        Ok(())
    }
}

/// OS / UI startup selection written by `local.startup.set`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupConfig {
    pub start_with_os: bool,
    pub start_engine_on_ui_open: bool,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            start_with_os: false,
            start_engine_on_ui_open: true,
        }
    }
}

/// Selected driver package. `available` is runtime, not persisted.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DriverSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
    pub enabled: bool,
}

impl DriverSelection {
    fn validate(&self) -> Result<(), MemberConfigError> {
        if let Some(package_id) = &self.package_id {
            if package_id.is_empty() || package_id.len() > 256 || package_id.contains('\0') {
                return Err(MemberConfigError::Driver);
            }
        }
        Ok(())
    }
}

/// Member-owned persistent settings for one fixed local source slot.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSourceConfig {
    pub source_id: SourceId,
    pub kind: SourceKind,
    pub config: LocalSourceConfig,
    #[serde(default)]
    pub monitor: SourceMonitorConfig,
}

/// Member-owned persistent settings scoped to one room.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentRoomConfig {
    /// Authority room identity after a successful `local.room.join`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
    /// OpenWrt join endpoints. Absent until the user joins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<JoinTarget>,
    pub grant_revision: GrantRevision,
    pub settings: MemberRoomConfig,
    pub member_mix: Vec<RemoteMemberMix>,
    pub source_mix: Vec<RemoteSourceMix>,
    #[serde(default = "empty_member_command_cache")]
    pub member_command_cache: RequestCache<MemberCommandResultRecord>,
}

/// Complete member-session configuration handed to an engine persistence
/// adapter as an immutable candidate.
///
/// Stable identity remains in the separate [`IdentityRecord`] required by the
/// architecture. This object carries every member-owned value interpreted by
/// the session reducer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberConfig {
    pub schema_version: u16,
    pub member_config_revision: MemberConfigRevision,
    pub local_emergency_stop: bool,
    pub system_capture_method: CaptureMethod,
    #[serde(default)]
    pub system_audio_binding: DeviceBindingConfig,
    #[serde(default)]
    pub microphone_binding: DeviceBindingConfig,
    #[serde(default)]
    pub playback_binding: DeviceBindingConfig,
    pub sources: [PersistentSourceConfig; 2],
    pub room: PersistentRoomConfig,
    #[serde(default)]
    pub startup: StartupConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub driver: DriverSelection,
    /// Last assigned local audit sequence. Query results remain a later slot.
    #[serde(default)]
    pub audit_seq: u64,
}

impl MemberConfig {
    fn source_slots(identity: IdentityRecord) -> [PersistentSourceConfig; 2] {
        [
            PersistentSourceConfig {
                source_id: identity.source_id(SourceKind::SystemAudio),
                kind: SourceKind::SystemAudio,
                config: LocalSourceConfig::default(),
                monitor: SourceMonitorConfig::default(),
            },
            PersistentSourceConfig {
                source_id: identity.source_id(SourceKind::Microphone),
                kind: SourceKind::Microphone,
                config: LocalSourceConfig::default(),
                monitor: SourceMonitorConfig::default(),
            },
        ]
    }

    /// First-start candidate: no durable authority room identity.
    #[must_use]
    pub fn unjoined(identity: IdentityRecord) -> Self {
        Self {
            schema_version: MEMBER_CONFIG_SCHEMA_VERSION,
            member_config_revision: MemberConfigRevision::new(1),
            local_emergency_stop: false,
            system_capture_method: CaptureMethod::WasapiLoopback,
            system_audio_binding: DeviceBindingConfig::follow_default(),
            microphone_binding: DeviceBindingConfig::follow_default(),
            playback_binding: DeviceBindingConfig::follow_default(),
            sources: Self::source_slots(identity),
            room: PersistentRoomConfig {
                room_id: None,
                join: None,
                grant_revision: GrantRevision::new(1),
                settings: MemberRoomConfig::default(),
                member_mix: Vec::new(),
                source_mix: Vec::new(),
                member_command_cache: empty_member_command_cache(),
            },
            startup: StartupConfig::default(),
            logging: LoggingConfig::default(),
            driver: DriverSelection::default(),
            audit_seq: 0,
        }
    }

    /// Constructs a joined-room candidate used by session unit tests.
    #[must_use]
    pub fn defaults(identity: IdentityRecord, room_id: RoomId) -> Self {
        let mut config = Self::unjoined(identity);
        config.room.room_id = Some(room_id);
        config
    }

    /// Authority room identity after join. Absent while unjoined.
    #[must_use]
    pub const fn attached_room_id(&self) -> Option<RoomId> {
        self.room.room_id
    }

    /// Join endpoints selected by `local.room.join`.
    #[must_use]
    pub const fn join_target(&self) -> Option<&JoinTarget> {
        self.room.join.as_ref()
    }

    /// Clears all room-scoped state while retaining machine-owned settings.
    pub fn detach_room(&mut self) -> Result<(), MemberConfigError> {
        let next_grant_revision = self
            .room
            .grant_revision
            .checked_next()
            .ok_or(MemberConfigError::GrantRevisionExhausted)?;
        self.room.room_id = None;
        self.room.join = None;
        self.room.grant_revision = next_grant_revision;
        self.room.settings = MemberRoomConfig::default();
        self.room.member_mix.clear();
        self.room.source_mix.clear();
        self.room.member_command_cache = empty_member_command_cache();
        for source in &mut self.sources {
            source.config.publish_requested = false;
        }
        Ok(())
    }

    pub(crate) fn has_room_scoped_state(&self) -> bool {
        self.room.room_id.is_some()
            || self.room.join.is_some()
            || self.room.settings != MemberRoomConfig::default()
            || !self.room.member_mix.is_empty()
            || !self.room.source_mix.is_empty()
            || self.room.member_command_cache != empty_member_command_cache()
            || self
                .sources
                .iter()
                .any(|source| source.config.publish_requested)
    }

    /// Validates fixed source identities, collection bounds, and uniqueness.
    pub fn validate(&self, identity: IdentityRecord) -> Result<(), MemberConfigError> {
        if self.schema_version != MEMBER_CONFIG_SCHEMA_VERSION {
            return Err(MemberConfigError::SchemaVersion);
        }
        for kind in [SourceKind::SystemAudio, SourceKind::Microphone] {
            let expected = identity.source_id(kind);
            let matches = self
                .sources
                .iter()
                .filter(|source| source.kind == kind && source.source_id == expected)
                .count();
            if matches != 1 {
                return Err(MemberConfigError::SourceIdentity);
            }
        }
        if self.sources[0].source_id == self.sources[1].source_id {
            return Err(MemberConfigError::SourceIdentity);
        }
        self.system_audio_binding.validate()?;
        self.microphone_binding.validate()?;
        self.playback_binding.validate()?;
        if let Some(join) = &self.room.join {
            join.validate()?;
            if self.room.room_id.is_none() {
                return Err(MemberConfigError::JoinTarget);
            }
        }
        self.logging.validate()?;
        self.driver.validate()?;
        if self.room.member_mix.len() > 31 || self.room.source_mix.len() > 62 {
            return Err(MemberConfigError::MixBounds);
        }
        if self.room.member_command_cache.policy != RequestCachePolicy::MEMBER
            || self.room.member_command_cache.records.len() > RequestCachePolicy::MEMBER.capacity
        {
            return Err(MemberConfigError::IdempotencyCache);
        }
        if self.room.room_id.is_none() && !self.room.member_command_cache.records.is_empty() {
            return Err(MemberConfigError::IdempotencyCache);
        }
        let mut request_keys = HashSet::new();
        for record in &self.room.member_command_cache.records {
            let Some(room_id) = self.room.room_id else {
                return Err(MemberConfigError::IdempotencyCache);
            };
            if record.key.principal() != MEMBER_AUTHORITY_PRINCIPAL
                || record.key.boundary() != RequestBoundary::Member
                || record.key.scope() != (RequestScope::Room { room_id })
                || record.response.is_none()
                || !request_keys.insert(record.key.clone())
            {
                return Err(MemberConfigError::IdempotencyCache);
            }
            record
                .response
                .as_ref()
                .expect("response presence was checked")
                .validate()?;
        }

        let mut member_keys: Vec<_> = self
            .room
            .member_mix
            .iter()
            .map(|entry| entry.remote_member_id)
            .collect();
        member_keys.sort_unstable();
        if member_keys.windows(2).any(|pair| pair[0] == pair[1])
            || member_keys
                .iter()
                .any(|member_id| *member_id == identity.member_id())
        {
            return Err(MemberConfigError::DuplicateOrLocalMemberMix);
        }

        let mut source_keys: Vec<_> = self
            .room
            .source_mix
            .iter()
            .map(|entry| (entry.remote_member_id, entry.source_id))
            .collect();
        source_keys.sort_unstable();
        if source_keys.windows(2).any(|pair| pair[0] == pair[1])
            || source_keys
                .iter()
                .any(|(member_id, _)| *member_id == identity.member_id())
        {
            return Err(MemberConfigError::DuplicateOrLocalSourceMix);
        }
        Ok(())
    }

    /// Returns one fixed local source setting.
    #[must_use]
    pub fn source(&self, source_id: SourceId) -> Option<&PersistentSourceConfig> {
        self.sources
            .iter()
            .find(|source| source.source_id == source_id)
    }

    /// Returns one mutable fixed local source setting.
    pub(crate) fn source_mut(
        &mut self,
        source_id: SourceId,
    ) -> Option<&mut PersistentSourceConfig> {
        self.sources
            .iter_mut()
            .find(|source| source.source_id == source_id)
    }

    /// Returns the persisted follow/pin selection for one local source kind.
    #[must_use]
    pub fn source_binding(&self, kind: SourceKind) -> &DeviceBindingConfig {
        match kind {
            SourceKind::SystemAudio => &self.system_audio_binding,
            SourceKind::Microphone => &self.microphone_binding,
        }
    }

    pub(crate) fn delegated_state(
        &self,
        identity: IdentityRecord,
        room_revision: RoomRevision,
        catalog_revision: CatalogRevision,
        quality_cap: QualityProfile,
    ) -> Result<DelegatedMemberState, MemberConfigError> {
        let room_id = self.room.room_id.ok_or(MemberConfigError::RoomScope)?;
        Ok(DelegatedMemberState {
            room_id,
            member_id: identity.member_id(),
            room_revision,
            member_config_revision: self.member_config_revision,
            catalog_revision,
            grant_revision: self.room.grant_revision,
            grants: self.room.settings.grants,
            quality_cap,
            auto_reconnect: self.room.settings.auto_reconnect,
            sources: self
                .sources
                .iter()
                .map(|source| MemberSourceConfig {
                    source_id: source.source_id,
                    config: source.config,
                })
                .collect(),
            member_mix: self.room.member_mix.clone(),
            source_mix: self.room.source_mix.clone(),
        })
    }

    pub(crate) fn apply_delegated_state(
        &mut self,
        identity: IdentityRecord,
        state: &DelegatedMemberState,
    ) -> Result<(), MemberConfigError> {
        if self.room.room_id != Some(state.room_id) {
            return Err(MemberConfigError::RoomScope);
        }
        if state.member_id != identity.member_id() {
            return Err(MemberConfigError::MemberScope);
        }
        state
            .validate()
            .map_err(|_| MemberConfigError::DelegatedState)?;
        for source in &state.sources {
            self.source_mut(source.source_id)
                .ok_or(MemberConfigError::SourceIdentity)?
                .config = source.config;
        }
        self.member_config_revision = state.member_config_revision;
        self.room.grant_revision = state.grant_revision;
        self.room.settings.grants = state.grants;
        self.room.settings.auto_reconnect = state.auto_reconnect;
        self.room.member_mix.clone_from(&state.member_mix);
        self.room.source_mix.clone_from(&state.source_mix);
        self.validate(identity)
    }

    pub(crate) fn member_mix(&self, member_id: MemberId) -> rlb_model::MemberMix {
        self.room
            .member_mix
            .iter()
            .find(|entry| entry.remote_member_id == member_id)
            .map_or_else(rlb_model::MemberMix::default, |entry| entry.mix)
    }

    pub(crate) fn source_mix(
        &self,
        member_id: MemberId,
        source_id: SourceId,
    ) -> rlb_model::SourceMix {
        self.room
            .source_mix
            .iter()
            .find(|entry| entry.remote_member_id == member_id && entry.source_id == source_id)
            .map_or_else(rlb_model::SourceMix::default, |entry| entry.mix)
    }
}

/// Invalid persistent member-session candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberConfigError {
    SchemaVersion,
    SourceIdentity,
    DeviceBinding,
    MixBounds,
    DuplicateOrLocalMemberMix,
    DuplicateOrLocalSourceMix,
    RoomScope,
    MemberScope,
    DelegatedState,
    IdempotencyCache,
    JoinTarget,
    GrantRevisionExhausted,
    Logging,
    Driver,
}

impl std::fmt::Display for MemberConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SchemaVersion => "member config schema version is unsupported",
            Self::SourceIdentity => {
                "member config source identities do not match the identity record"
            }
            Self::DeviceBinding => {
                "device binding must be follow_default without an id, or pinned with an endpoint id"
            }
            Self::MixBounds => "member config mix collections exceed room bounds",
            Self::DuplicateOrLocalMemberMix => {
                "member mix keys are duplicated or refer to the local member"
            }
            Self::DuplicateOrLocalSourceMix => {
                "source mix keys are duplicated or refer to the local member"
            }
            Self::RoomScope => "delegated state room does not match member config",
            Self::MemberScope => "delegated state member does not match identity",
            Self::DelegatedState => "delegated member state is invalid",
            Self::IdempotencyCache => "member command idempotency cache is invalid",
            Self::JoinTarget => "join target is missing a room identity or uses an invalid URL",
            Self::GrantRevisionExhausted => "member grant revision is exhausted",
            Self::Logging => "logging directory or retention selection is invalid",
            Self::Driver => "driver package selection is invalid",
        })
    }
}

impl std::error::Error for MemberConfigError {}
