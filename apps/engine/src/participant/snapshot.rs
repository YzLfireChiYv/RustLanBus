//! Observable participant state. Contact readiness is separate from audio availability.

use std::net::SocketAddr;

use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};
use serde::{Deserialize, Serialize};

use crate::participant::config::{MixIntent, SourceKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryStatus {
    Available,
    Unavailable,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupContact {
    Offline,
    Contacting,
    Connected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberContact {
    Offline,
    Contacting,
    Connected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEffect {
    NotApplicable,
    Requested,
    PendingIntegration,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LocalSourceState {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
    pub publish: bool,
    pub monitor: bool,
    pub output_targets: Vec<super::output_routing::OutputTarget>,
    pub endpoint_id: Option<String>,
    pub playback: MixIntent,
    pub capture_state: &'static str,
    pub availability_reason: String,
    pub publication: PublicationState,
    pub capture: Option<CaptureDetails>,
    pub format: Option<PcmFormatState>,
    /// Recent captured peak before this application's playback gain.
    pub peak_db: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CaptureDetails {
    pub actual_endpoint_id: Option<String>,
    pub acquisition_epoch: Option<StreamEpoch>,
    pub capture_instance: Option<u64>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub channel_mask: Option<u32>,
    pub period_frames: Option<u32>,
    pub buffer_frames: Option<u32>,
    pub captured_frames: u64,
    pub dropped_blocks: u64,
    pub last_source_frame_end: Option<u64>,
    pub first_frame_qpc_100ns: Option<u64>,
    pub observed_on_owner_qpc_100ns: Option<u64>,
    pub scheduling_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemoteMixState {
    pub source_id: SourceId,
    pub playback: MixIntent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Disabled,
    Unavailable,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PcmEncodingState {
    F32Le,
    Signed32Le,
    F64Le,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayoutState {
    Mono,
    StereoLeftRight,
    Speakers,
}

/// A presentation copy of the remote descriptor; it does not open a decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PcmFormatState {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub layout: ChannelLayoutState,
    pub channel_mask: u32,
    pub encoding: PcmEncodingState,
    pub sample_bytes: u8,
    pub valid_bits: u8,
}

/// Last installed remote catalogue metadata. Contact and local playback
/// availability remain separate from the remote publisher's reported state.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemoteSourceState {
    pub id: SourceId,
    pub kind: SourceKind,
    pub publication: PublicationState,
    pub publication_epoch: u64,
    pub format: Option<PcmFormatState>,
    pub receive_allowed: bool,
    pub receive_ready: bool,
    /// Recent admitted original peak before this application's playback gain.
    pub peak_db: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MemberState {
    pub id: MemberId,
    pub name: String,
    pub address: Option<SocketAddr>,
    pub send_allowed: bool,
    pub receive_allowed: bool,
    pub contact: MemberContact,
    pub send_ready: bool,
    pub receive_ready: bool,
    /// Local publication IDs with actual acknowledged send eligibility.
    pub sending_sources: Vec<SourceId>,
    pub parked: bool,
    pub playback: MixIntent,
    pub source_mix_overrides: Vec<RemoteMixState>,
    pub sources: Vec<RemoteSourceState>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceOutputError {
    /// None identifies a local original; Some identifies its remote member.
    pub member_id: Option<MemberId>,
    pub source_id: SourceId,
    pub detail: String,
}

/// Current per-source processing at one output device. This describes PCM
/// progression and local mix intent; it makes no physical-listening claim.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourcePlayoutState {
    pub member_id: Option<MemberId>,
    pub source_id: SourceId,
    /// awaiting_output, unsupported, muted, priming, awaiting_current, running.
    pub phase: &'static str,
    pub last_arrival_age_ms: Option<u64>,
    pub closed_before: u64,
    pub high_water: u64,
    /// Converted frames included in successful native submissions in this
    /// output generation. This is a count, not a source/output clock position.
    pub submitted_frames: u64,
    pub dropped_blocks: u64,
    /// Source/member mute, solo and gain, multiplied by the master target.
    pub effective_gain: f64,
    pub segment_restarts: u64,
    pub last_error: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputState {
    pub phase: &'static str,
    pub generation: u64,
    pub actual_endpoint_id: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub channel_mask: Option<u32>,
    pub period_frames: Option<u32>,
    pub target_padding_frames: Option<u32>,
    pub buffer_frames: Option<u32>,
    pub dropped_blocks: u64,
    pub last_error: Option<String>,
    pub source_errors: Vec<SourceOutputError>,
    pub sources: Vec<SourcePlayoutState>,
    pub clock_position_ticks: Option<u64>,
    pub clock_ticks_per_second: Option<u64>,
    pub clock_correlated_qpc_100ns: Option<u64>,
    pub clock_result_code: Option<u32>,
    pub clock_error: Option<String>,
    pub scheduling_error: Option<String>,
    /// Recent mixed peak after source and master gains, before an optional limiter.
    pub peak_db: Option<f32>,
}

impl Default for OutputState {
    fn default() -> Self {
        Self {
            phase: "idle",
            generation: 0,
            actual_endpoint_id: None,
            sample_rate_hz: None,
            channels: None,
            channel_mask: None,
            period_frames: None,
            target_padding_frames: None,
            buffer_frames: None,
            dropped_blocks: 0,
            last_error: None,
            source_errors: Vec::new(),
            sources: Vec::new(),
            clock_position_ticks: None,
            clock_ticks_per_second: None,
            clock_correlated_qpc_100ns: None,
            clock_result_code: None,
            clock_error: None,
            scheduling_error: None,
            peak_db: None,
        }
    }
}

/// Meter observations expire with no new PCM; zero maps to a finite UI floor.
/// This is presentation freshness, with no smoothing or audio-path buffering.
pub(crate) fn recent_peak_db(
    peak: Option<f64>,
    observed: Option<std::time::Instant>,
) -> Option<f32> {
    let observed = observed?;
    if observed.elapsed() > std::time::Duration::from_secs(2) {
        return None;
    }
    let peak = peak.filter(|value| value.is_finite() && *value >= 0.0)?;
    Some(if peak == 0.0 {
        -120.0
    } else {
        (20.0 * peak.log10()).max(-120.0) as f32
    })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputDeviceState {
    pub endpoint_id: String,
    #[serde(flatten)]
    pub state: OutputState,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReturnLoopState {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub returned_packets: u64,
    pub packet_rtt_ms: Option<f64>,
    pub minimum_packet_rtt_ms: Option<f64>,
    pub capture_to_receive_ms: Option<f64>,
    pub minimum_capture_to_receive_ms: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParticipantSnapshot {
    pub room: super::controller::RoomControlState,
    pub running: bool,
    pub local_member_id: MemberId,
    pub local_name: String,
    pub bind: SocketAddr,
    pub local_address: Option<SocketAddr>,
    /// Concrete, currently owned UDP source sockets. The primary legacy field
    /// does not represent the full IPv4/IPv6 listener set.
    pub local_addresses: Vec<SocketAddr>,
    pub network: super::network::NetworkState,
    pub network_revision: u64,
    pub run_id: Option<RunId>,
    pub group_name: String,
    pub joined: bool,
    /// Local runtime fence for directory replies; never persisted or put on the peer wire.
    pub group_generation: u64,
    pub group_contact: GroupContact,
    pub auto_reconnect: bool,
    pub login_startup: super::login_startup::LoginStartupState,
    pub lan_firewall: super::firewall::LanFirewallState,
    pub directory: DirectoryStatus,
    pub local_sources: Vec<LocalSourceState>,
    pub output_endpoint_id: Option<String>,
    pub endpoints: Vec<AudioEndpoint>,
    pub master_gain_db: f32,
    pub limiter_enabled: bool,
    pub overloaded: Option<bool>,
    pub members: Vec<MemberState>,
    pub unsaved: bool,
    pub persistence_error: Option<String>,
    pub socket_error: Option<String>,
    pub capture_error: Option<String>,
    pub rejected_unconfigured: u64,
    pub rejected_malformed: u64,
    pub media_unavailable: u64,
    pub socket_errors: u64,
    pub audio: &'static str,
    pub group_authentication: &'static str,
    /// Contact-directory phase. This is not an audio-readiness claim.
    pub room_phase: &'static str,
    pub output_submitted_frames: u64,
    pub output: OutputState,
    pub output_devices: Vec<OutputDeviceState>,
    pub main_output_id: Option<String>,
    pub admission_revision: u64,
    pub return_loop: Option<ReturnLoopState>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AudioEndpoint {
    pub id: String,
    pub name: String,
    pub direction: &'static str,
    pub available: bool,
    pub supported: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommandOutcome {
    pub command: String,
    pub applied: bool,
    pub audio: AudioEffect,
    pub message: String,
    pub persistence_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management: Option<super::management::ManagementResult>,
    pub snapshot: ParticipantSnapshot,
}
