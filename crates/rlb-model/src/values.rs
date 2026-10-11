use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Fixed V2 engine sample rate.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// Fixed V2 media-unit size in source frames.
pub const MEDIA_UNIT_FRAMES: u16 = 480;
/// Automatic receive target candidate in source frames.
pub const AUTOMATIC_TARGET_FRAMES: u16 = 960;
/// Automatic receive-target lower bound in source frames.
pub const AUTOMATIC_MIN_FRAMES: u16 = 960;
/// Automatic receive-target upper bound in source frames.
pub const AUTOMATIC_MAX_FRAMES: u16 = 2_400;
/// Logical packet/reorder capacity in source frames.
pub const RECEIVE_CAPACITY_FRAMES: u16 = 12_000;

/// One of the two stable source slots owned by a V2 member.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Shared-mode system-output capture.
    SystemAudio,
    /// Local microphone capture.
    Microphone,
}

impl SourceKind {
    /// Returns the fixed first-release channel count.
    #[must_use]
    pub const fn channels(self) -> u8 {
        match self {
            Self::SystemAudio => 2,
            Self::Microphone => 1,
        }
    }

    /// Returns the fixed first-release channel layout.
    #[must_use]
    pub const fn channel_layout(self) -> ChannelLayout {
        match self {
            Self::SystemAudio => ChannelLayout::Stereo,
            Self::Microphone => ChannelLayout::Mono,
        }
    }
}

/// Product-level send quality, independent of MEDIA wire codec numbers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityProfile {
    /// Signed 16-bit little-endian PCM; product rank 1.
    PcmS16V1,
    /// IEEE binary32 little-endian PCM; product rank 2.
    #[default]
    PcmF32V1,
}

impl QualityProfile {
    /// Returns the product rank used for policy comparisons.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::PcmS16V1 => 1,
            Self::PcmF32V1 => 2,
        }
    }

    /// Applies a room quality cap using product ranks.
    #[must_use]
    pub const fn capped_by(self, cap: Self) -> Self {
        if self.rank() <= cap.rank() { self } else { cap }
    }
}

/// Current authority-owned admission state of a member identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    /// Waiting for an operator decision.
    Pending,
    /// Allowed to join the room and establish media.
    Approved,
    /// Removed and unable to rejoin without explicit readmission.
    Removed,
}

/// A member-owned permission that may be granted to the room authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Change a source's room-scoped publication request.
    SourcePublication,
    /// Change a source's selected send quality.
    SendQuality,
    /// Change room-scoped member and source receive-mix controls.
    ReceiveMix,
    /// Change room-scoped automatic reconnect.
    AutoReconnect,
}

/// Complete capability-grant set for one room and member.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGrants {
    /// Whether source publication may be changed remotely.
    pub source_publication: bool,
    /// Whether selected send quality may be changed remotely.
    pub send_quality: bool,
    /// Whether receive-mix controls may be changed remotely.
    pub receive_mix: bool,
    /// Whether automatic reconnect may be changed remotely.
    pub auto_reconnect: bool,
}

impl CapabilityGrants {
    /// Returns the current value of one grant.
    #[must_use]
    pub const fn get(self, capability: Capability) -> bool {
        match capability {
            Capability::SourcePublication => self.source_publication,
            Capability::SendQuality => self.send_quality,
            Capability::ReceiveMix => self.receive_mix,
            Capability::AutoReconnect => self.auto_reconnect,
        }
    }

    /// Sets one grant without changing adjacent grants.
    pub const fn set(&mut self, capability: Capability, enabled: bool) {
        match capability {
            Capability::SourcePublication => self.source_publication = enabled,
            Capability::SendQuality => self.send_quality = enabled,
            Capability::ReceiveMix => self.receive_mix = enabled,
            Capability::AutoReconnect => self.auto_reconnect = enabled,
        }
    }
}

/// Room-managed packet-priority policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QosPolicy {
    /// Mark media DSCP EF and apply the room's bounded audio queue.
    #[default]
    AudioPriority,
    /// Use ordinary DSCP 0 traffic.
    Off,
}

/// Authority-owned policy included in room snapshots.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPolicy {
    /// Maximum product quality allowed in the room.
    pub quality_cap: QualityProfile,
    /// Packet-priority policy.
    pub qos_policy: QosPolicy,
    /// Whether all room media sending is paused.
    pub room_send_muted: bool,
}

/// Audio channel layout exposed by a source catalog.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    /// One interleaved channel.
    Mono,
    /// Left/right interleaved channels.
    Stereo,
}

/// Local system-audio capture method.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    /// WASAPI shared-mode render-endpoint loopback.
    #[default]
    WasapiLoopback,
    /// Capture from a separately installed cable endpoint.
    InstalledCableCapture,
    /// Capture from the project's test-signed virtual output.
    TestSignedVirtualOutput,
}

/// Whether a device binding follows the OS default or pins an endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointMode {
    /// Follow the relevant Windows default endpoint.
    #[default]
    FollowDefault,
    /// Keep a stable platform endpoint identifier.
    Pinned,
}

/// Why a validated numeric domain value was rejected.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum ValueError {
    /// A control value was NaN or infinite.
    #[error("{field} must be finite")]
    NonFinite {
        /// Stable field name.
        field: &'static str,
    },
    /// A finite control value was outside its inclusive range.
    #[error("{field} must be in {minimum}..={maximum}, got {actual}")]
    OutOfRange {
        /// Stable field name.
        field: &'static str,
        /// Inclusive lower bound.
        minimum: f32,
        /// Inclusive upper bound.
        maximum: f32,
        /// Rejected finite value.
        actual: f32,
    },
    /// A manual receive target was outside the fixed first-release set.
    #[error("manual receive target {actual} frames is unsupported")]
    InvalidManualTarget {
        /// Rejected target in source frames.
        actual: u16,
    },
}

/// Validated source or member gain in decibels.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct GainDb(f32);

impl GainDb {
    /// Inclusive minimum gain.
    pub const MIN: f32 = -60.0;
    /// Inclusive maximum gain.
    pub const MAX: f32 = 12.0;
    /// Unity gain.
    pub const UNITY: Self = Self(0.0);

    /// Validates a finite gain value.
    pub fn new(value: f32) -> Result<Self, ValueError> {
        if !value.is_finite() {
            return Err(ValueError::NonFinite { field: "gain_db" });
        }
        if !(Self::MIN..=Self::MAX).contains(&value) {
            return Err(ValueError::OutOfRange {
                field: "gain_db",
                minimum: Self::MIN,
                maximum: Self::MAX,
                actual: value,
            });
        }
        Ok(Self(value))
    }

    /// Returns the decibel value.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    /// Converts this value to a linear amplitude multiplier.
    #[must_use]
    pub fn linear(self) -> f32 {
        10.0_f32.powf(self.0 / 20.0)
    }
}

impl Default for GainDb {
    fn default() -> Self {
        Self::UNITY
    }
}

impl TryFrom<f32> for GainDb {
    type Error = ValueError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<GainDb> for f32 {
    fn from(value: GainDb) -> Self {
        value.get()
    }
}

impl<'de> Deserialize<'de> for GainDb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Validated source pan in the inclusive range `-1.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Pan(f32);

impl Pan {
    /// Hard left.
    pub const LEFT: Self = Self(-1.0);
    /// Center.
    pub const CENTER: Self = Self(0.0);
    /// Hard right.
    pub const RIGHT: Self = Self(1.0);

    /// Validates a finite pan value.
    pub fn new(value: f32) -> Result<Self, ValueError> {
        if !value.is_finite() {
            return Err(ValueError::NonFinite { field: "pan" });
        }
        if !(-1.0..=1.0).contains(&value) {
            return Err(ValueError::OutOfRange {
                field: "pan",
                minimum: -1.0,
                maximum: 1.0,
                actual: value,
            });
        }
        Ok(Self(value))
    }

    /// Returns the normalized pan value.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Default for Pan {
    fn default() -> Self {
        Self::CENTER
    }
}

impl TryFrom<f32> for Pan {
    type Error = ValueError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Pan> for f32 {
    fn from(value: Pan) -> Self {
        value.get()
    }
}

impl<'de> Deserialize<'de> for Pan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// One of the fixed first-release manual receive-buffer targets.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct ManualBufferTarget(u16);

impl ManualBufferTarget {
    /// Validates a target against the fixed set `480, 720, 960, 1440, 2400`.
    pub const fn new(frames: u16) -> Result<Self, ValueError> {
        match frames {
            480 | 720 | 960 | 1_440 | 2_400 => Ok(Self(frames)),
            actual => Err(ValueError::InvalidManualTarget { actual }),
        }
    }

    /// Returns the target in source frames.
    #[must_use]
    pub const fn frames(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ManualBufferTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let frames = u16::deserialize(deserializer)?;
        Self::new(frames).map_err(serde::de::Error::custom)
    }
}

/// Receive-buffer mode and optional fixed manual target.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiveBufferConfig {
    /// Bounded automatic adaptation; starts at 960 frames.
    #[default]
    Automatic,
    /// Fixed user-selected target.
    Manual {
        /// Target in source frames.
        target_frames: ManualBufferTarget,
    },
}

/// Persistent receive-mix controls for one remote member.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberMix {
    /// Member-level gain.
    pub gain_db: GainDb,
    /// Whether this member is locally muted.
    pub mute: bool,
    /// Whether this member participates in the local solo set.
    pub solo: bool,
}

/// Persistent receive-mix controls for one remote source.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMix {
    /// Source-level gain.
    pub gain_db: GainDb,
    /// Source pan.
    pub pan: Pan,
    /// Whether this source is locally muted.
    pub mute: bool,
}

/// Member-owned persistent settings for one local source.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSourceConfig {
    /// Whether the local capture backend should run.
    pub master_capture_enabled: bool,
    /// Whether publication to the current room is requested.
    pub publish_requested: bool,
    /// Locally selected send quality.
    pub selected_profile: QualityProfile,
}

/// Member-owned defaults scoped to one approved room.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberRoomConfig {
    /// Whether the engine reconnects after an unplanned control loss.
    pub auto_reconnect: bool,
    /// Four independently delegated capabilities.
    pub grants: CapabilityGrants,
    /// Receiver buffer behavior.
    pub receive_buffer: ReceiveBufferConfig,
}

impl Default for MemberRoomConfig {
    fn default() -> Self {
        Self {
            auto_reconnect: true,
            grants: CapabilityGrants::default(),
            receive_buffer: ReceiveBufferConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_rank_is_independent_and_policy_ordered() {
        assert_eq!(QualityProfile::PcmS16V1.rank(), 1);
        assert_eq!(QualityProfile::PcmF32V1.rank(), 2);
        assert_eq!(
            QualityProfile::PcmF32V1.capped_by(QualityProfile::PcmS16V1),
            QualityProfile::PcmS16V1
        );
        assert_eq!(
            QualityProfile::PcmS16V1.capped_by(QualityProfile::PcmF32V1),
            QualityProfile::PcmS16V1
        );
    }

    #[test]
    fn numeric_controls_reject_non_finite_and_out_of_range_values() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(matches!(
                GainDb::new(value),
                Err(ValueError::NonFinite { field: "gain_db" })
            ));
            assert!(matches!(
                Pan::new(value),
                Err(ValueError::NonFinite { field: "pan" })
            ));
        }
        assert!(GainDb::new(-60.01).is_err());
        assert!(GainDb::new(12.01).is_err());
        assert!(Pan::new(-1.01).is_err());
        assert!(Pan::new(1.01).is_err());
        assert!(serde_json::from_str::<GainDb>("13.0").is_err());
        assert!(serde_json::from_str::<Pan>("2.0").is_err());
    }

    #[test]
    fn first_release_defaults_are_safe() {
        assert_eq!(QualityProfile::default(), QualityProfile::PcmF32V1);
        assert_eq!(QosPolicy::default(), QosPolicy::AudioPriority);
        assert_eq!(
            RoomPolicy::default(),
            RoomPolicy {
                quality_cap: QualityProfile::PcmF32V1,
                qos_policy: QosPolicy::AudioPriority,
                room_send_muted: false,
            }
        );
        assert!(!LocalSourceConfig::default().master_capture_enabled);
        assert!(!LocalSourceConfig::default().publish_requested);
        assert!(MemberRoomConfig::default().auto_reconnect);
        assert_eq!(
            MemberRoomConfig::default().grants,
            CapabilityGrants::default()
        );
        assert_eq!(GainDb::default(), GainDb::UNITY);
        assert_eq!(Pan::default(), Pan::CENTER);
    }

    #[test]
    fn manual_targets_are_closed_and_serializable() {
        for frames in [480, 720, 960, 1_440, 2_400] {
            let target = ManualBufferTarget::new(frames).unwrap();
            let json = serde_json::to_string(&target).unwrap();
            assert_eq!(
                serde_json::from_str::<ManualBufferTarget>(&json).unwrap(),
                target
            );
        }
        assert!(ManualBufferTarget::new(481).is_err());
        assert!(serde_json::from_str::<ManualBufferTarget>("0").is_err());
    }

    #[test]
    fn capability_updates_are_independent() {
        let mut grants = CapabilityGrants::default();
        grants.set(Capability::ReceiveMix, true);
        assert!(grants.get(Capability::ReceiveMix));
        assert!(!grants.get(Capability::SourcePublication));
        assert!(!grants.get(Capability::SendQuality));
        assert!(!grants.get(Capability::AutoReconnect));
    }
}
