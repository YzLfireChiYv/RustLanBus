use core::net::Ipv4Addr;
use core::num::NonZeroU16;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CatalogRevision, ChannelLayout, GrantRevision, MemberConfigRevision, MemberId,
    PresenceRevision, QualityProfile, RoomId, RoomPolicy, RoomRevision, RunId, SAMPLE_RATE_HZ,
    ServerGeneration, SourceId, SourceKind, StreamEpoch,
};

/// Why an IPv4 transport endpoint was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EndpointError {
    /// Port zero cannot identify a UDP or TCP peer.
    #[error("an endpoint port must be in 1..=65535")]
    ZeroPort,
    /// `0.0.0.0` is a bind address and cannot identify a peer.
    #[error("0.0.0.0 cannot be published as a peer endpoint")]
    UnspecifiedAddress,
}

/// A validated IPv4 peer or registry endpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Endpoint {
    ip: Ipv4Addr,
    port: NonZeroU16,
}

impl Endpoint {
    /// Creates a publishable endpoint.
    pub fn new(ip: Ipv4Addr, port: u16) -> Result<Self, EndpointError> {
        if ip.is_unspecified() {
            return Err(EndpointError::UnspecifiedAddress);
        }
        let port = NonZeroU16::new(port).ok_or(EndpointError::ZeroPort)?;
        Ok(Self { ip, port })
    }

    /// Returns the IPv4 address.
    #[must_use]
    pub const fn ip(self) -> Ipv4Addr {
        self.ip
    }

    /// Returns the nonzero UDP or TCP port.
    #[must_use]
    pub const fn port(self) -> u16 {
        self.port.get()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EndpointFamily {
    Ipv4,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointWire {
    family: EndpointFamily,
    ip: Ipv4Addr,
    port: u16,
}

impl Serialize for Endpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("Endpoint", 3)?;
        state.serialize_field("family", &EndpointFamily::Ipv4)?;
        state.serialize_field("ip", &self.ip)?;
        state.serialize_field("port", &self.port.get())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = EndpointWire::deserialize(deserializer)?;
        let EndpointFamily::Ipv4 = wire.family;
        Self::new(wire.ip, wire.port).map_err(serde::de::Error::custom)
    }
}

/// Why an identity record containing fixed source slots was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IdentityRecordError {
    /// Both source kinds referred to the same stable identity.
    #[error("system audio and microphone must have distinct source identities")]
    DuplicateSourceId,
}

/// Stable member identity and its two fixed first-release source slots.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "IdentityRecordWire", into = "IdentityRecordWire")]
pub struct IdentityRecord {
    member_id: MemberId,
    system_audio_source_id: SourceId,
    microphone_source_id: SourceId,
}

impl IdentityRecord {
    /// Creates an identity record with distinct fixed source slots.
    pub fn new(
        member_id: MemberId,
        system_audio_source_id: SourceId,
        microphone_source_id: SourceId,
    ) -> Result<Self, IdentityRecordError> {
        if system_audio_source_id == microphone_source_id {
            return Err(IdentityRecordError::DuplicateSourceId);
        }
        Ok(Self {
            member_id,
            system_audio_source_id,
            microphone_source_id,
        })
    }

    /// Returns the stable member identity.
    #[must_use]
    pub const fn member_id(self) -> MemberId {
        self.member_id
    }

    /// Returns the source identity assigned to one fixed kind.
    #[must_use]
    pub const fn source_id(self, kind: SourceKind) -> SourceId {
        match kind {
            SourceKind::SystemAudio => self.system_audio_source_id,
            SourceKind::Microphone => self.microphone_source_id,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecordWire {
    member_id: MemberId,
    system_audio_source_id: SourceId,
    microphone_source_id: SourceId,
}

impl TryFrom<IdentityRecordWire> for IdentityRecord {
    type Error = IdentityRecordError;

    fn try_from(value: IdentityRecordWire) -> Result<Self, Self::Error> {
        Self::new(
            value.member_id,
            value.system_audio_source_id,
            value.microphone_source_id,
        )
    }
}

impl From<IdentityRecord> for IdentityRecordWire {
    fn from(value: IdentityRecord) -> Self {
        Self {
            member_id: value.member_id,
            system_audio_source_id: value.system_audio_source_id,
            microphone_source_id: value.microphone_source_id,
        }
    }
}

/// Why a source format or catalog descriptor was rejected.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SourceDescriptorError {
    /// A display name was empty or exceeded 64 UTF-8 bytes.
    #[error("source display name must contain 1..=64 UTF-8 bytes")]
    InvalidDisplayNameLength,
    /// A display name contained a C0 or C1 control character.
    #[error("source display name cannot contain C0 or C1 control characters")]
    DisplayNameControlCharacter,
    /// A first-release active format used a sample rate other than 48 kHz.
    #[error("first-release active sources must use 48000 Hz")]
    UnsupportedSampleRate,
    /// Channel count and channel layout did not agree.
    #[error("channel count and channel layout do not agree")]
    ChannelLayoutMismatch,
    /// The active format did not match the source's fixed kind.
    #[error("active format does not match the fixed source kind")]
    SourceKindFormatMismatch,
    /// Active format fields were only partly present.
    #[error("active format fields and stream epoch must be all present or all absent")]
    PartialActiveFormat,
    /// Catalog profile did not equal the policy-derived effective profile.
    #[error("active profile must equal the effective profile")]
    ActiveProfileMismatch,
    /// The effective quality had a higher product rank than the selected value.
    #[error("effective profile cannot exceed the selected profile")]
    EffectiveProfileExceedsSelection,
    /// `send_ready` contradicted local source gates represented in the descriptor.
    #[error("send_ready requires available, master capture, and publish requested")]
    InvalidSendReady,
}

/// A validated active source format.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "SourceFormatWire")]
pub struct SourceFormat {
    sample_rate_hz: u32,
    channels: u8,
    channel_layout: ChannelLayout,
    profile: QualityProfile,
}

impl SourceFormat {
    /// Creates and validates a first-release source format.
    pub fn new(
        sample_rate_hz: u32,
        channels: u8,
        channel_layout: ChannelLayout,
        profile: QualityProfile,
    ) -> Result<Self, SourceDescriptorError> {
        if sample_rate_hz != SAMPLE_RATE_HZ {
            return Err(SourceDescriptorError::UnsupportedSampleRate);
        }
        let layout_channels = match channel_layout {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
        };
        if channels != layout_channels {
            return Err(SourceDescriptorError::ChannelLayoutMismatch);
        }
        Ok(Self {
            sample_rate_hz,
            channels,
            channel_layout,
            profile,
        })
    }

    /// Builds the fixed first-release format for one source kind.
    #[must_use]
    pub const fn for_source(kind: SourceKind, profile: QualityProfile) -> Self {
        Self {
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: kind.channels(),
            channel_layout: kind.channel_layout(),
            profile,
        }
    }

    /// Returns the sample rate.
    #[must_use]
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    /// Returns the interleaved channel count.
    #[must_use]
    pub const fn channels(self) -> u8 {
        self.channels
    }

    /// Returns the channel layout.
    #[must_use]
    pub const fn channel_layout(self) -> ChannelLayout {
        self.channel_layout
    }

    /// Returns the catalog quality profile.
    #[must_use]
    pub const fn profile(self) -> QualityProfile {
        self.profile
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFormatWire {
    sample_rate_hz: u32,
    channels: u8,
    channel_layout: ChannelLayout,
    profile: QualityProfile,
}

impl TryFrom<SourceFormatWire> for SourceFormat {
    type Error = SourceDescriptorError;

    fn try_from(value: SourceFormatWire) -> Result<Self, Self::Error> {
        Self::new(
            value.sample_rate_hz,
            value.channels,
            value.channel_layout,
            value.profile,
        )
    }
}

/// State used to construct a validated source catalog descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceState {
    available: bool,
    master_capture_enabled: bool,
    publish_requested: bool,
    selected_profile: QualityProfile,
    effective_profile: QualityProfile,
    send_ready: bool,
}

impl SourceState {
    /// Creates source state after validating profile and publication gates.
    pub fn new(
        available: bool,
        master_capture_enabled: bool,
        publish_requested: bool,
        selected_profile: QualityProfile,
        effective_profile: QualityProfile,
        send_ready: bool,
    ) -> Result<Self, SourceDescriptorError> {
        if effective_profile.rank() > selected_profile.rank() {
            return Err(SourceDescriptorError::EffectiveProfileExceedsSelection);
        }
        if send_ready && !(available && master_capture_enabled && publish_requested) {
            return Err(SourceDescriptorError::InvalidSendReady);
        }
        Ok(Self {
            available,
            master_capture_enabled,
            publish_requested,
            selected_profile,
            effective_profile,
            send_ready,
        })
    }

    /// Returns source availability.
    #[must_use]
    pub const fn available(self) -> bool {
        self.available
    }

    /// Returns the local capture request.
    #[must_use]
    pub const fn master_capture_enabled(self) -> bool {
        self.master_capture_enabled
    }

    /// Returns the room-scoped publication request.
    #[must_use]
    pub const fn publish_requested(self) -> bool {
        self.publish_requested
    }

    /// Returns the locally selected quality.
    #[must_use]
    pub const fn selected_profile(self) -> QualityProfile {
        self.selected_profile
    }

    /// Returns the policy-derived effective quality.
    #[must_use]
    pub const fn effective_profile(self) -> QualityProfile {
        self.effective_profile
    }

    /// Returns whether all non-viewer publication gates are open.
    #[must_use]
    pub const fn send_ready(self) -> bool {
        self.send_ready
    }
}

/// Complete catalog description of one stable source slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceDescriptor {
    source_id: SourceId,
    kind: SourceKind,
    display_name: String,
    state: SourceState,
    active: Option<(SourceFormat, StreamEpoch)>,
}

impl SourceDescriptor {
    /// Creates a descriptor and validates fixed V2 source invariants.
    pub fn new(
        source_id: SourceId,
        kind: SourceKind,
        display_name: impl Into<String>,
        state: SourceState,
        active: Option<(SourceFormat, StreamEpoch)>,
    ) -> Result<Self, SourceDescriptorError> {
        let display_name = display_name.into();
        validate_display_name(&display_name)?;

        if let Some((format, _)) = active {
            if format.channels() != kind.channels()
                || format.channel_layout() != kind.channel_layout()
            {
                return Err(SourceDescriptorError::SourceKindFormatMismatch);
            }
            if format.profile() != state.effective_profile() {
                return Err(SourceDescriptorError::ActiveProfileMismatch);
            }
        }

        Ok(Self {
            source_id,
            kind,
            display_name,
            state,
            active,
        })
    }

    /// Returns the stable source identity.
    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    /// Returns the immutable source-slot kind.
    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    /// Returns the user-visible source name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Returns the source's current local and policy state.
    #[must_use]
    pub const fn state(&self) -> SourceState {
        self.state
    }

    /// Returns the active format and timeline identity, when active.
    #[must_use]
    pub const fn active(&self) -> Option<(SourceFormat, StreamEpoch)> {
        self.active
    }
}

fn validate_display_name(display_name: &str) -> Result<(), SourceDescriptorError> {
    if display_name.is_empty() || display_name.len() > 64 {
        return Err(SourceDescriptorError::InvalidDisplayNameLength);
    }
    if display_name
        .chars()
        .any(|character| matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'))
    {
        return Err(SourceDescriptorError::DisplayNameControlCharacter);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceDescriptorWire {
    source_id: SourceId,
    kind: SourceKind,
    display_name: String,
    available: bool,
    master_capture_enabled: bool,
    publish_requested: bool,
    selected_profile: QualityProfile,
    effective_profile: QualityProfile,
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
    channel_layout: Option<ChannelLayout>,
    profile: Option<QualityProfile>,
    stream_epoch: Option<StreamEpoch>,
    send_ready: bool,
}

impl TryFrom<SourceDescriptorWire> for SourceDescriptor {
    type Error = SourceDescriptorError;

    fn try_from(value: SourceDescriptorWire) -> Result<Self, Self::Error> {
        let state = SourceState::new(
            value.available,
            value.master_capture_enabled,
            value.publish_requested,
            value.selected_profile,
            value.effective_profile,
            value.send_ready,
        )?;
        let active = match (
            value.sample_rate_hz,
            value.channels,
            value.channel_layout,
            value.profile,
            value.stream_epoch,
        ) {
            (None, None, None, None, None) => None,
            (
                Some(sample_rate_hz),
                Some(channels),
                Some(channel_layout),
                Some(profile),
                Some(stream_epoch),
            ) => Some((
                SourceFormat::new(sample_rate_hz, channels, channel_layout, profile)?,
                stream_epoch,
            )),
            _ => return Err(SourceDescriptorError::PartialActiveFormat),
        };
        Self::new(
            value.source_id,
            value.kind,
            value.display_name,
            state,
            active,
        )
    }
}

impl Serialize for SourceDescriptor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("SourceDescriptor", 14)?;
        state.serialize_field("source_id", &self.source_id)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("display_name", &self.display_name)?;
        state.serialize_field("available", &self.state.available)?;
        state.serialize_field("master_capture_enabled", &self.state.master_capture_enabled)?;
        state.serialize_field("publish_requested", &self.state.publish_requested)?;
        state.serialize_field("selected_profile", &self.state.selected_profile)?;
        state.serialize_field("effective_profile", &self.state.effective_profile)?;

        let (sample_rate_hz, channels, channel_layout, profile, stream_epoch) = match self.active {
            Some((format, stream_epoch)) => (
                Some(format.sample_rate_hz()),
                Some(format.channels()),
                Some(format.channel_layout()),
                Some(format.profile()),
                Some(stream_epoch),
            ),
            None => (None, None, None, None, None),
        };
        state.serialize_field("sample_rate_hz", &sample_rate_hz)?;
        state.serialize_field("channels", &channels)?;
        state.serialize_field("channel_layout", &channel_layout)?;
        state.serialize_field("profile", &profile)?;
        state.serialize_field("stream_epoch", &stream_epoch)?;
        state.serialize_field("send_ready", &self.state.send_ready)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SourceDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        SourceDescriptorWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// Authority identity and revisions attached to every room snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomSnapshotMeta {
    /// Current registry daemon generation.
    pub server_generation: ServerGeneration,
    /// Stable room identity.
    pub room_id: RoomId,
    /// Persistent room-state revision.
    pub room_revision: RoomRevision,
    /// Presence revision within `server_generation`.
    pub presence_revision: PresenceRevision,
}

/// Member authority and revisions attached to a member projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberSnapshotMeta {
    /// Stable room identity.
    pub room_id: RoomId,
    /// Stable member identity.
    pub member_id: MemberId,
    /// Current engine run identity.
    pub run_id: RunId,
    /// Persistent member configuration revision.
    pub member_config_revision: MemberConfigRevision,
    /// Current run's complete catalog revision.
    pub catalog_revision: CatalogRevision,
    /// Persistent room-scoped grant revision.
    pub grant_revision: GrantRevision,
}

/// Minimal validated room-policy snapshot shared across control boundaries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPolicySnapshot {
    /// Snapshot authority and revision domain.
    pub meta: RoomSnapshotMeta,
    /// Current authority-owned room policy.
    pub policy: RoomPolicy,
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use super::*;

    fn source_id() -> SourceId {
        SourceId::from_str("202122232425262728292a2b2c2d2e2f").unwrap()
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_str("404142434445464748494a4b4c4d4e4f").unwrap()
    }

    #[test]
    fn endpoint_json_is_closed_and_validated() {
        let endpoint = Endpoint::new(Ipv4Addr::new(192, 0, 2, 26), 17_001).unwrap();
        let json = serde_json::to_string(&endpoint).unwrap();
        assert_eq!(
            json,
            r#"{"family":"ipv4","ip":"192.0.2.26","port":17001}"#
        );
        assert_eq!(serde_json::from_str::<Endpoint>(&json).unwrap(), endpoint);
        assert!(Endpoint::new(Ipv4Addr::UNSPECIFIED, 17_001).is_err());
        assert!(Endpoint::new(Ipv4Addr::LOCALHOST, 0).is_err());
        assert!(
            serde_json::from_str::<Endpoint>(
                r#"{"family":"ipv4","ip":"192.0.2.26","port":17001,"extra":0}"#
            )
            .is_err()
        );
    }

    #[test]
    fn source_descriptor_roundtrips_in_catalog_shape() {
        let state = SourceState::new(
            true,
            true,
            true,
            QualityProfile::PcmF32V1,
            QualityProfile::PcmF32V1,
            true,
        )
        .unwrap();
        let descriptor = SourceDescriptor::new(
            source_id(),
            SourceKind::SystemAudio,
            "System audio",
            state,
            Some((
                SourceFormat::for_source(SourceKind::SystemAudio, QualityProfile::PcmF32V1),
                epoch(),
            )),
        )
        .unwrap();
        let json = serde_json::to_string(&descriptor).unwrap();
        assert_eq!(
            serde_json::from_str::<SourceDescriptor>(&json).unwrap(),
            descriptor
        );
        assert!(json.contains(r#""sample_rate_hz":48000"#));
        assert!(json.contains(r#""channels":2"#));
        assert!(json.contains(r#""stream_epoch":"404142434445464748494a4b4c4d4e4f""#));
    }

    #[test]
    fn source_descriptor_rejects_partial_or_mismatched_active_state() {
        let partial = format!(
            r#"{{
                "source_id":"{}",
                "kind":"microphone",
                "display_name":"Microphone",
                "available":true,
                "master_capture_enabled":true,
                "publish_requested":false,
                "selected_profile":"pcm_f32_v1",
                "effective_profile":"pcm_f32_v1",
                "sample_rate_hz":48000,
                "channels":null,
                "channel_layout":"mono",
                "profile":"pcm_f32_v1",
                "stream_epoch":"{}",
                "send_ready":false
            }}"#,
            source_id(),
            epoch()
        );
        assert!(serde_json::from_str::<SourceDescriptor>(&partial).is_err());

        let state = SourceState::new(
            true,
            true,
            false,
            QualityProfile::PcmF32V1,
            QualityProfile::PcmF32V1,
            false,
        )
        .unwrap();
        assert_eq!(
            SourceDescriptor::new(
                source_id(),
                SourceKind::Microphone,
                "Microphone",
                state,
                Some((
                    SourceFormat::for_source(SourceKind::SystemAudio, QualityProfile::PcmF32V1),
                    epoch()
                ))
            ),
            Err(SourceDescriptorError::SourceKindFormatMismatch)
        );
    }

    #[test]
    fn fixed_source_slots_must_be_distinct() {
        let member =
            MemberId::from_str("101112131415161718191a1b1c1d1e1f").expect("member fixture");
        assert_eq!(
            IdentityRecord::new(member, source_id(), source_id()),
            Err(IdentityRecordError::DuplicateSourceId)
        );
    }
}
