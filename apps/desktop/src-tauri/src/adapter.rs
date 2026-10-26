//! Maps the accepted participant model to the desktop presentation.
//!
//! Device dropdowns in the interface can hide a system source that matches the
//! current output. Native route reservations enforce the same separation
//! before opening capture or playback streams, including restored routes.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rustlanbus_engine::participant::discovery::AdvertisedRoom;
use rustlanbus_engine::participant::output_routing::OutputTarget;
#[cfg(test)]
use rustlanbus_engine::participant::room_session::RuleFilesState;
use rustlanbus_engine::participant::room_session::{
    EntryFailureKind, MemberView as SessionMember, Phase, ProjectedEndpoint, RuleFileStatus,
    RuleSaveState, SessionSnapshot,
};
use rustlanbus_engine::participant::rooms::{
    AssignmentOrigin, DEFAULT_CONTACT_PORT, DiscoveredRoom, GlobalRules, JoinMode, MixPatch,
    RoomCardView, RoomPlaybackRule, RoomProfile, RoomTarget, Rule, RuleMatch,
    default_contact_listener,
};
use rustlanbus_engine::participant::routing::PeerRoute;
use rustlanbus_engine::participant::{
    AudioEffect, ChannelLayoutState, Command, CommandOutcome, LocalSourceState, MemberContact,
    ParticipantSnapshot, PcmEncodingState, PublicationState, RoomControlState, SourceKind,
};
use serde::{Deserialize, Serialize};

const DIRECTORY_NOTE: &str =
    "联络服务帮助发现成员；音频收发由本机规则决定，混音和设备也由本机管理。";

#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UiAction {
    SetNetwork {
        policy: rustlanbus_engine::participant::network::NetworkPolicy,
        bind: Option<SocketAddr>,
    },
    ReadRoomCard {
        path: PathBuf,
    },
    SaveRoomCard {
        path: PathBuf,
        target: RoomTarget,
        expected_revision: String,
    },
    DeleteRoomCard {
        path: PathBuf,
        expected_revision: String,
    },
    ExportSettings {
        path: PathBuf,
    },
    SetName {
        name: String,
    },
    JoinGroup {
        name: String,
    },
    LeaveGroup {},
    SetPeerIntent {
        member_id: String,
        send: bool,
        receive: bool,
    },
    SetLocalSource {
        source_id: String,
        publish: bool,
        monitor: bool,
    },
    SetLocalOutputs {
        source_id: String,
        targets: Vec<OutputTarget>,
    },
    SelectSourceEndpoint {
        source_id: String,
        endpoint_id: String,
    },
    SelectOutput {
        endpoint_id: String,
    },
    SetSourceMix {
        member_id: Option<String>,
        source_id: String,
        gain_db: f32,
        pan: f32,
        mute: bool,
        solo: bool,
    },
    SetMaster {
        gain_db: f32,
        limiter_enabled: bool,
    },
    SetAutoReconnect {
        enabled: bool,
    },
    SetLoginStartup {
        enabled: bool,
    },
    RefreshLoginStartup {},
    AllowLanFirewall {},
    RefreshLanFirewall {},
    RemoveOwnLanFirewall {},
    RefreshDevices {},
    CreateRoom {
        profile: Box<RoomProfile>,
        bind: SocketAddr,
    },
    CreateSavedRoom {
        profile_path: PathBuf,
        bind: SocketAddr,
    },
    JoinSavedRoom {
        profile_path: PathBuf,
        mode: JoinMode,
    },
    JoinUnknownRoom {
        target: RoomTarget,
    },
    ChooseRoom {
        /// Canonical decimal text. JavaScript numbers are not accepted.
        generation: String,
        instance_id: String,
    },
    LeaveRoom {},
    CancelRoom {},
    ReloadRoomRules {},
    StartVirtualReturn {},
    StopVirtualReturn {},
    RefreshRooms {},
    EditRoomRule {
        rule: Rule,
    },
    ReplaceGlobalRules {
        rules: Box<GlobalRules>,
    },
    SetGlobalDefaults {
        send: Option<bool>,
        listen: Option<bool>,
    },
    /// Current SourceKind compatibility edit. This is not a general route.
    SetRoomSourceMix {
        condition: RuleMatch,
        source_kind: SourceKind,
        gain_db: Option<f32>,
        pan: Option<f32>,
        mute: Option<bool>,
        solo: Option<bool>,
    },
}

impl UiAction {
    pub fn into_command(self) -> Result<Command, String> {
        let command = match self {
            Self::SetNetwork { policy, bind } => Command::SetNetwork { policy, bind },
            Self::ReadRoomCard { path } => Command::ReadRoomCard { path },
            Self::SaveRoomCard {
                path,
                target,
                expected_revision,
            } => Command::SaveRoomCard {
                path,
                target,
                expected_revision,
            },
            Self::DeleteRoomCard {
                path,
                expected_revision,
            } => Command::DeleteRoomCard {
                path,
                expected_revision,
            },
            Self::ExportSettings { path } => Command::ExportSettings { path },
            Self::SetName { name } => Command::SetName { name },
            Self::JoinGroup { name } => Command::JoinGroup { name },
            Self::LeaveGroup {} => Command::LeaveGroup,
            Self::SetPeerIntent {
                member_id,
                send,
                receive,
            } => Command::SetPeerIntent {
                member_id: parse_id(&member_id, "成员标识")?,
                send,
                receive,
            },
            Self::SetLocalSource {
                source_id,
                publish,
                monitor,
            } => Command::SetLocalSource {
                source_id: parse_id(&source_id, "来源标识")?,
                publish: Some(publish),
                monitor: Some(monitor),
            },
            Self::SetLocalOutputs { source_id, targets } => Command::SetLocalOutputs {
                source_id: parse_id(&source_id, "来源标识")?,
                targets,
            },
            Self::SelectSourceEndpoint {
                source_id,
                endpoint_id,
            } => Command::SelectSourceEndpoint {
                source_id: parse_id(&source_id, "来源标识")?,
                endpoint_id: blank_to_none(endpoint_id),
            },
            Self::SelectOutput { endpoint_id } => Command::SelectOutput {
                endpoint_id: blank_to_none(endpoint_id),
            },
            Self::SetSourceMix {
                member_id,
                source_id,
                gain_db,
                pan,
                mute,
                solo,
            } => Command::SetSourceMix {
                member_id: match member_id {
                    Some(member_id) => Some(parse_id(&member_id, "成员标识")?),
                    None => None,
                },
                source_id: parse_id(&source_id, "来源标识")?,
                gain_db,
                pan,
                mute,
                solo,
            },
            Self::SetMaster {
                gain_db,
                limiter_enabled,
            } => Command::SetMaster {
                gain_db,
                limiter_enabled,
            },
            Self::SetAutoReconnect { enabled } => Command::SetAutoReconnect { enabled },
            Self::SetLoginStartup { enabled } => Command::SetLoginStartup { enabled },
            Self::RefreshLoginStartup {} => Command::RefreshLoginStartup,
            Self::AllowLanFirewall {} => Command::AllowLanFirewall,
            Self::RefreshLanFirewall {} => Command::RefreshLanFirewall,
            Self::RemoveOwnLanFirewall {} => Command::RemoveOwnLanFirewall,
            Self::RefreshDevices {} => Command::RefreshDevices,
            Self::CreateRoom { profile, bind } => Command::CreateRoom {
                profile: *profile,
                bind,
            },
            Self::CreateSavedRoom { profile_path, bind } => {
                Command::CreateSavedRoom { profile_path, bind }
            }
            Self::JoinSavedRoom { profile_path, mode } => {
                Command::JoinSavedRoom { profile_path, mode }
            }
            Self::JoinUnknownRoom { target } => Command::JoinUnknownRoom { target },
            Self::ChooseRoom {
                generation,
                instance_id,
            } => Command::ChooseRoom {
                generation: parse_generation(&generation)?,
                instance_id: parse_id(&instance_id, "房间实例标识")?,
            },
            Self::LeaveRoom {} => Command::LeaveRoom,
            Self::CancelRoom {} => Command::CancelRoom,
            Self::ReloadRoomRules {} => Command::ReloadRoomRules,
            Self::StartVirtualReturn {} => Command::StartVirtualReturn,
            Self::StopVirtualReturn {} => Command::StopVirtualReturn,
            Self::RefreshRooms {} => Command::RefreshRooms,
            Self::EditRoomRule { rule } => Command::EditRoomRule { rule },
            Self::ReplaceGlobalRules { rules } => Command::ReplaceGlobalRules { rules: *rules },
            Self::SetGlobalDefaults { send, listen } => Command::SetGlobalDefaults { send, listen },
            Self::SetRoomSourceMix {
                condition,
                source_kind,
                gain_db,
                pan,
                mute,
                solo,
            } => Command::SetRoomSourceMix {
                condition,
                source_kind,
                playback: MixPatch {
                    gain_db,
                    pan,
                    mute,
                    solo,
                },
            },
        };
        if !command.is_user_command() {
            return Err("此控制入口已停用，请使用正式的房间、来源和规则操作。".into());
        }
        Ok(command)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewState {
    pub network: rustlanbus_engine::participant::network::NetworkState,
    pub requested_bind: String,
    pub local_media_addresses: Vec<String>,
    pub engine: &'static str,
    pub message: Option<String>,
    pub local_name: String,
    pub group: ViewGroup,
    pub local_sources: Vec<ViewLocalSource>,
    pub members: Vec<ViewMember>,
    pub endpoints: Vec<ViewEndpoint>,
    pub output_endpoint_id: Option<String>,
    pub main_output_id: Option<String>,
    pub output_devices: Vec<ViewOutputDevice>,
    pub master_gain_db: f32,
    pub limiter_enabled: bool,
    pub output_peak_db: Option<f32>,
    pub overloaded: bool,
    pub auto_reconnect: bool,
    pub login_startup: rustlanbus_engine::participant::login_startup::LoginStartupState,
    pub lan_firewall: rustlanbus_engine::participant::firewall::LanFirewallState,
    pub room: ViewRoomControl,
    pub return_loop: Option<ViewReturnLoop>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewReturnLoop {
    pub member_id: String,
    pub run_id: String,
    pub returned_packets: String,
    pub packet_rtt_ms: Option<f64>,
    pub minimum_packet_rtt_ms: Option<f64>,
    pub capture_to_receive_ms: Option<f64>,
    pub minimum_capture_to_receive_ms: Option<f64>,
}

/// Room owner projection. `session: null` means there is no room session.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRoomControl {
    pub session: Option<ViewRoomSession>,
    pub cards: Vec<ViewRoomCard>,
    pub routes: Vec<ViewPeerRoute>,
    pub global_rules_path: String,
    pub auto_enter: bool,
    pub error: Option<String>,
    pub creation_defaults: ViewRoomCreationDefaults,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRoomCreationDefaults {
    pub contact_port: u16,
    pub listen_address: Option<String>,
}

/// One contact session. Acceptance of a command is not a field here.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRoomSession {
    /// Decimal text so a generation above 2^53-1 is not rounded.
    pub generation: String,
    pub phase: &'static str,
    pub room_name: Option<String>,
    pub instance_id: Option<String>,
    /// Creator contact listener, including a wildcard bind. Not a saved card address.
    pub local_service: Option<String>,
    pub profile_path: Option<String>,
    pub discovered_rooms: Vec<ViewDiscoveredRoom>,
    pub choices: Vec<ViewDiscoveredRoom>,
    /// Runtime endpoints actually selected for this session.
    pub selected_endpoints: Vec<String>,
    pub members: Vec<ViewRoomMember>,
    pub global_rules: Option<GlobalRules>,
    pub rules: Vec<Rule>,
    pub playback_rules: Vec<RoomPlaybackRule>,
    pub rules_unsaved: bool,
    pub rules_save_error: Option<String>,
    pub rule_files: ViewRuleFiles,
    /// Saved card IP. JSON null means unset and is not the listener address.
    pub saved_address: Option<String>,
    pub card_persisted: Option<bool>,
    pub pending_target_change: bool,
    pub failure: Option<String>,
    pub failure_kind: Option<EntryFailureKind>,
    pub output_overflow: bool,
    pub output_loss_revision: String,
    pub discovery_failure: Option<String>,
    pub local_advertisement: Option<ViewLocalAdvertisement>,
    pub virtual_return: Option<ViewVirtualReturn>,
    pub operation_notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewVirtualReturn {
    pub member: Option<ViewVirtualReturnMember>,
    pub mappings: Vec<ViewReturnMapping>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewVirtualReturnMember {
    pub member_id: String,
    pub run_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewReturnMapping {
    pub source_id: String,
    pub epoch: String,
    pub upstream_member_id: String,
    pub upstream_run_id: String,
    pub upstream_source_id: String,
    pub upstream_epoch: String,
    pub input_first_frame: String,
    pub output_first_frame: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewDiscoveredRoom {
    pub name: String,
    pub instance_id: String,
    pub candidates: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRoomMember {
    pub member_id: String,
    pub run_id: String,
    pub display_name: String,
    pub endpoint: ViewProjectedEndpoint,
    pub directory_visible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewProjectedEndpoint {
    Ready { address: String },
    LocalScopeRequired { ip: String, port: u16 },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewPeerRoute {
    pub member_id: String,
    pub run_id: String,
    pub name: String,
    pub endpoint: ViewProjectedEndpoint,
    pub directory_visible: bool,
    pub send: bool,
    pub listen: bool,
    pub send_from: Option<AssignmentOrigin>,
    pub listen_from: Option<AssignmentOrigin>,
    pub outputs: Vec<OutputTarget>,
    pub outputs_from: Option<AssignmentOrigin>,
    pub sources: Vec<ViewSourceDirections>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewSourceDirections {
    pub kind: &'static str,
    pub send: bool,
    pub listen: bool,
    pub send_from: Option<AssignmentOrigin>,
    pub listen_from: Option<AssignmentOrigin>,
    pub outputs: Vec<OutputTarget>,
    pub outputs_from: Option<AssignmentOrigin>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ViewRuleFiles {
    pub room: ViewRuleFileStatus,
    pub global: ViewRuleFileStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ViewRuleFileStatus {
    pub path: Option<String>,
    #[serde(flatten)]
    pub outcome: RuleSaveState,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRoomCard {
    pub path: String,
    pub name: Option<String>,
    /// Entered device address. JSON null means the card saved no address.
    pub entered_address: Option<String>,
    pub port: Option<u16>,
    pub strict_auto_join: bool,
    pub has_password: bool,
    pub rule_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewLocalAdvertisement {
    pub instance_id: String,
    pub display_name: String,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewGroup {
    pub name: String,
    pub joined: bool,
    pub contact: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewLocalSource {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    pub available: bool,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub peak_db: Option<f32>,
    pub capture_state: &'static str,
    pub availability_reason: String,
    pub publication: &'static str,
    pub publish: bool,
    pub monitor: bool,
    pub monitor_ready: bool,
    pub monitor_availability_reason: String,
    pub endpoint_id: Option<String>,
    pub output_targets: Vec<OutputTarget>,
    pub format: Option<ViewPcmFormat>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewMember {
    pub id: String,
    pub name: String,
    pub contact: &'static str,
    pub send_allowed: bool,
    pub receive_allowed: bool,
    pub send_ready: bool,
    pub sending_source_ids: Vec<String>,
    pub receive_ready: bool,
    pub sources: Vec<ViewRemoteSource>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewRemoteSource {
    pub id: String,
    pub kind: &'static str,
    pub name: &'static str,
    pub publication: &'static str,
    /// Preserve the complete u64 value when crossing the JavaScript boundary.
    pub publication_epoch: String,
    pub receive_allowed: bool,
    pub receive_ready: bool,
    pub format: Option<ViewPcmFormat>,
    pub available: bool,
    pub availability_reason: String,
    pub output_targets: Vec<OutputTarget>,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub peak_db: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewPcmFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub layout: &'static str,
    pub channel_mask: u32,
    pub encoding: &'static str,
    pub sample_bytes: u8,
    pub valid_bits: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewEndpoint {
    pub id: String,
    pub name: String,
    pub direction: &'static str,
    pub available: bool,
    pub supported: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewOutputDevice {
    pub endpoint_id: String,
    pub phase: &'static str,
    pub actual_endpoint_id: Option<String>,
    pub generation: String,
    pub rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub last_error: Option<String>,
    pub peak_db: Option<f32>,
    pub overloaded: bool,
    pub sources: Vec<ViewSourcePlayout>,
    pub source_errors: Vec<ViewSourceOutputError>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewSourcePlayout {
    pub member_id: Option<String>,
    pub source_id: String,
    pub phase: &'static str,
    pub last_arrival_age_ms: Option<u64>,
    pub closed_before: String,
    pub high_water: String,
    pub submitted_frames: String,
    pub dropped_blocks: String,
    pub segment_restarts: String,
    pub effective_gain: f64,
    pub last_error: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewSourceOutputError {
    pub member_id: Option<String>,
    pub source_id: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandView {
    pub applied: bool,
    pub message: String,
    pub persistence_error: Option<String>,
    pub state: ViewState,
    pub management: Option<rustlanbus_engine::participant::management::ManagementResult>,
}

pub fn unavailable_view(message: impl Into<String>) -> ViewState {
    ViewState {
        network: Default::default(),
        requested_bind: "0.0.0.0:0".into(),
        local_media_addresses: Vec::new(),
        engine: "unavailable",
        message: Some(message.into()),
        local_name: String::new(),
        group: ViewGroup {
            name: String::new(),
            joined: false,
            contact: "offline",
        },
        local_sources: Vec::new(),
        members: Vec::new(),
        endpoints: Vec::new(),
        output_endpoint_id: None,
        main_output_id: None,
        output_devices: Vec::new(),
        master_gain_db: 0.0,
        limiter_enabled: false,
        output_peak_db: None,
        overloaded: false,
        auto_reconnect: true,
        login_startup: Default::default(),
        lan_firewall: Default::default(),
        room: empty_room(),
        return_loop: None,
    }
}

pub fn map_snapshot(snapshot: &ParticipantSnapshot) -> ViewState {
    let live = snapshot.running && !snapshot.local_addresses.is_empty();
    ViewState {
        network: snapshot.network.clone(),
        requested_bind: snapshot.bind.to_string(),
        local_media_addresses: snapshot.local_addresses.iter().map(SocketAddr::to_string).collect(),
        engine: if snapshot.running {
            "running"
        } else if snapshot.socket_error.is_some() {
            "error"
        } else {
            "unavailable"
        },
        message: status_message(snapshot),
        local_name: snapshot.local_name.clone(),
        group: ViewGroup {
            name: snapshot.group_name.clone(),
            joined: snapshot.joined,
            contact: if live {
                match snapshot.group_contact {
                    rustlanbus_engine::participant::GroupContact::Offline => "offline",
                    rustlanbus_engine::participant::GroupContact::Contacting => "contacting",
                    rustlanbus_engine::participant::GroupContact::Connected => "connected",
                }
            } else {
                "offline"
            },
        },
        local_sources: snapshot
            .local_sources
            .iter()
            .map(|source| {
                let capture_state = source_capture_state(source);
                let (monitor_output_ready, monitor_output_error) = selected_source_outputs(
                    snapshot, &source.output_targets,
                    |error| error.member_id.is_none() && error.source_id == source.id,
                );
                ViewLocalSource {
                    id: source.id.to_string(),
                    kind: match source.kind {
                        SourceKind::System => "system",
                        SourceKind::Microphone => "microphone",
                    },
                    name: source.name.clone(),
                    available: snapshot.running && capture_state == "ready",
                    gain_db: source.playback.gain_db,
                    pan: source.playback.pan,
                    mute: source.playback.mute,
                    solo: source.playback.solo,
                    peak_db: source.peak_db,
                    capture_state,
                    availability_reason: source.availability_reason.clone(),
                    publication: publication_state(source.publication),
                    publish: source.publish,
                    monitor: source.monitor,
                    monitor_ready: source.monitor
                        && capture_state == "ready"
                        && monitor_output_ready,
                    monitor_availability_reason: if !source.monitor {
                        "本机监听已关闭。".into()
                    } else if source.output_targets.is_empty() {
                        "尚未选择输出设备。".into()
                    } else if let Some(detail) = monitor_output_error {
                        if monitor_output_ready {
                            format!("部分输出不支持此来源的声道布局，其它已就绪输出仍可监听：{detail}")
                        } else {
                            format!("所选输出不支持此来源的声道布局，采集与发送继续：{detail}")
                        }
                    } else if !monitor_output_ready {
                        "所选输出设备尚未就绪。".into()
                    } else if capture_state != "ready" {
                        "本机声音来源尚未就绪。".into()
                    } else {
                        "本机监听已就绪。".into()
                    },
                    endpoint_id: source.endpoint_id.clone(),
                    output_targets: source.output_targets.clone(),
                    format: source.format.map(map_pcm_format),
                }
            })
            .collect(),
        members: snapshot
            .members
            .iter()
            .map(|member| ViewMember {
                id: member.id.to_string(),
                name: member.name.clone(),
                contact: if live {
                    match member.contact {
                        MemberContact::Offline => "offline",
                        MemberContact::Contacting => "contacting",
                        MemberContact::Connected => "connected",
                    }
                } else {
                    "offline"
                },
                send_allowed: member.send_allowed,
                receive_allowed: member.receive_allowed,
                send_ready: live && member.send_ready,
                sending_source_ids: if live {
                    member
                        .sending_sources
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                } else {
                    Vec::new()
                },
                receive_ready: live && member.receive_ready,
                sources: member
                    .sources
                    .iter()
                    .map(|source| {
                        let targets = snapshot
                            .room
                            .routes
                            .iter()
                            .find(|route| route.remote.member == member.id)
                            .map(|route| {
                                route
                                    .sources
                                    .iter()
                                    .find(|choice| choice.kind == source.kind)
                                    .map_or(route.outputs.as_slice(), |choice| {
                                        choice.outputs.as_slice()
                                    })
                            })
                            .unwrap_or(&[]);
                        let (output_running, source_output_error) = selected_source_outputs(
                            snapshot, targets,
                            |error| error.member_id == Some(member.id) && error.source_id == source.id,
                        );
                        let playback = member
                            .source_mix_overrides
                            .iter()
                            .find(|choice| choice.source_id == source.id)
                            .map_or(member.playback, |choice| choice.playback);
                        ViewRemoteSource {
                            id: source.id.to_string(),
                            kind: source.kind.as_str(),
                            name: match source.kind {
                                SourceKind::System => "系统声音",
                                SourceKind::Microphone => "麦克风",
                            },
                            publication: publication_state(source.publication),
                            publication_epoch: source.publication_epoch.to_string(),
                            receive_allowed: source.receive_allowed,
                            receive_ready: live && source.receive_ready,
                            format: source.format.map(map_pcm_format),
                            // Contact, publication and native output are separate
                            // observations. Availability carries no playback meter
                            // or claim that this source has already been heard.
                            available: live
                                && snapshot.joined
                                && output_running
                                && source.receive_allowed
                                && source.receive_ready
                                && !member.parked
                                && member.contact == MemberContact::Connected
                                && source.publication == PublicationState::Active
                                && source.format.is_some(),
                            availability_reason: if targets.is_empty() {
                                "尚未选择输出设备。".into()
                            } else if let Some(detail) = source_output_error {
                                if output_running {
                                    format!("部分输出不支持此来源的声道布局，其它已就绪输出仍可播放：{detail}")
                                } else {
                                    format!("所选输出不支持此来源的声道布局，音频接收继续：{detail}")
                                }
                            } else if !output_running {
                                "所选输出设备尚未就绪。".into()
                            } else {
                                "所选输出设备已就绪；音频仍取决于来源与收发状态。".into()
                            },
                            output_targets: targets.to_vec(),
                            gain_db: playback.gain_db,
                            pan: playback.pan,
                            mute: playback.mute,
                            solo: playback.solo,
                            peak_db: source.peak_db,
                        }
                    })
                    .collect(),
            })
            .collect(),
        endpoints: snapshot
            .endpoints
            .iter()
            .map(|endpoint| ViewEndpoint {
                id: endpoint.id.clone(),
                name: endpoint.name.clone(),
                direction: endpoint.direction,
                available: endpoint.available,
                supported: endpoint.supported,
                reason: endpoint.reason.clone(),
            })
            .collect(),
        output_endpoint_id: snapshot.output_endpoint_id.clone(),
        main_output_id: snapshot.main_output_id.clone(),
        output_devices: snapshot
            .output_devices
            .iter()
            .map(|output| ViewOutputDevice {
                endpoint_id: output.endpoint_id.clone(),
                phase: output.state.phase,
                actual_endpoint_id: output.state.actual_endpoint_id.clone(),
                generation: output.state.generation.to_string(),
                rate_hz: output.state.sample_rate_hz,
                channels: output.state.channels,
                last_error: output.state.last_error.clone(),
                peak_db: output.state.peak_db,
                overloaded: output.state.peak_db.is_some_and(|peak| peak > 0.0),
                source_errors: output.state.source_errors.iter().map(|source| ViewSourceOutputError {
                    member_id: source.member_id.map(|id| id.to_string()),
                    source_id: source.source_id.to_string(),
                    detail: source.detail.clone(),
                }).collect(),
                sources: output.state.sources.iter().map(|source| ViewSourcePlayout {
                    member_id: source.member_id.map(|id| id.to_string()),
                    source_id: source.source_id.to_string(),
                    phase: source.phase,
                    last_arrival_age_ms: source.last_arrival_age_ms,
                    closed_before: source.closed_before.to_string(),
                    high_water: source.high_water.to_string(),
                    submitted_frames: source.submitted_frames.to_string(),
                    dropped_blocks: source.dropped_blocks.to_string(),
                    segment_restarts: source.segment_restarts.to_string(),
                    effective_gain: source.effective_gain,
                    last_error: source.last_error,
                }).collect(),
            })
            .collect(),
        master_gain_db: snapshot.master_gain_db,
        limiter_enabled: snapshot.limiter_enabled,
        output_peak_db: snapshot.output.peak_db,
        overloaded: snapshot.overloaded.unwrap_or(false),
        auto_reconnect: snapshot.auto_reconnect,
        login_startup: snapshot.login_startup.clone(),
        lan_firewall: snapshot.lan_firewall.clone(),
        room: map_room(&snapshot.room, Some(snapshot.bind)),
        return_loop: snapshot.return_loop.as_ref().map(|value| ViewReturnLoop {
            member_id: value.member_id.to_string(),
            run_id: value.run_id.to_string(),
            returned_packets: value.returned_packets.to_string(),
            packet_rtt_ms: value.packet_rtt_ms,
            minimum_packet_rtt_ms: value.minimum_packet_rtt_ms,
            capture_to_receive_ms: value.capture_to_receive_ms,
            minimum_capture_to_receive_ms: value.minimum_capture_to_receive_ms,
        }),
    }
}

/// Resolve routing intent against the actual output owners, never the legacy
/// representative output. A healthy unrelated device cannot make a route ready.
#[cfg(test)]
fn selected_output_running(snapshot: &ParticipantSnapshot, targets: &[OutputTarget]) -> bool {
    selected_source_outputs(snapshot, targets, |_| false).0
}

fn selected_source_outputs(
    snapshot: &ParticipantSnapshot,
    targets: &[OutputTarget],
    matches_source: impl Fn(&rustlanbus_engine::participant::SourceOutputError) -> bool,
) -> (bool, Option<String>) {
    let mut ready = false;
    let mut errors = Vec::new();
    for output in &snapshot.output_devices {
        let selected = targets.iter().any(|target| match target {
            OutputTarget::Main => {
                snapshot.main_output_id.as_deref() == Some(output.endpoint_id.as_str())
            }
            OutputTarget::Endpoint(id) => id == &output.endpoint_id,
        });
        if !selected {
            continue;
        }
        if let Some(error) = output
            .state
            .source_errors
            .iter()
            .find(|error| matches_source(error))
        {
            errors.push(format!("{}：{}", output.endpoint_id, error.detail));
        } else if output.state.phase == "running"
            && output.state.actual_endpoint_id.as_deref() == Some(output.endpoint_id.as_str())
        {
            ready = true;
        }
    }
    (
        snapshot.running && ready,
        (!errors.is_empty()).then(|| errors.join("；")),
    )
}

fn map_pcm_format(format: rustlanbus_engine::participant::PcmFormatState) -> ViewPcmFormat {
    ViewPcmFormat {
        sample_rate_hz: format.sample_rate_hz,
        channels: format.channels,
        channel_mask: format.channel_mask,
        layout: match format.layout {
            ChannelLayoutState::Mono => "mono",
            ChannelLayoutState::StereoLeftRight => "stereo_left_right",
            ChannelLayoutState::Speakers => "speakers",
        },
        encoding: match format.encoding {
            PcmEncodingState::F32Le => "f32_le",
            PcmEncodingState::Signed32Le => "signed32_le",
            PcmEncodingState::F64Le => "f64_le",
        },
        sample_bytes: format.sample_bytes,
        valid_bits: format.valid_bits,
    }
}

pub fn map_outcome(outcome: CommandOutcome) -> CommandView {
    let mut state = map_snapshot(&outcome.snapshot);
    if outcome.audio == AudioEffect::PendingIntegration {
        state.message = Some(join_message(
            state.message.as_deref(),
            "这项音频功能尚未接入运行链路。",
        ));
    }
    if outcome.audio == AudioEffect::Requested {
        state.message = Some(join_message(
            state.message.as_deref(),
            "音频设置已提交，采集、发布和播放状态会随后更新。",
        ));
    }
    if outcome.audio == AudioEffect::Unsupported || !outcome.applied {
        state.message = Some(join_message(state.message.as_deref(), &outcome.message));
    }
    CommandView {
        applied: outcome.applied,
        message: outcome.message,
        persistence_error: outcome.persistence_error,
        state,
        management: outcome.management,
    }
}

pub fn sanitized_details(snapshot: &ParticipantSnapshot) -> String {
    let mut lines = vec![
        format!("本机成员 {}", snapshot.local_member_id),
        format!("显示名称 {}", snapshot.local_name),
        format!(
            "绑定 {}，主要实际地址 {}",
            snapshot.bind,
            snapshot
                .local_address
                .map(|address| address.to_string())
                .unwrap_or_else(|| "无".into())
        ),
        format!(
            "小组 {}，已加入 {}，传输 {}",
            snapshot.group_name,
            if snapshot.joined { "是" } else { "否" },
            match snapshot.group_contact {
                rustlanbus_engine::participant::GroupContact::Offline => "离线",
                rustlanbus_engine::participant::GroupContact::Contacting => "正在联系",
                rustlanbus_engine::participant::GroupContact::Connected => "已接通",
            }
        ),
        format!(
            "控制器 {}，目录 {}",
            if snapshot.running {
                "运行中"
            } else {
                "已停止"
            },
            match snapshot.directory {
                rustlanbus_engine::participant::DirectoryStatus::Available => "可用",
                rustlanbus_engine::participant::DirectoryStatus::Unavailable => "未接入",
                rustlanbus_engine::participant::DirectoryStatus::Stale => "信息过旧",
            }
        ),
        format!(
            "未保存 {}，媒体不可用次数 {}，拒绝的陌生数据包 {}，拒绝的无效数据包 {}",
            if snapshot.unsaved { "是" } else { "否" },
            snapshot.media_unavailable,
            snapshot.rejected_unconfigured,
            snapshot.rejected_malformed
        ),
        format!(
            "主要输出摘要 {}，输出代次 {}，已提交帧数 {}，实际输出设备 {}",
            snapshot.output.phase,
            snapshot.output.generation,
            snapshot.output_submitted_frames,
            snapshot
                .output
                .actual_endpoint_id
                .as_deref()
                .unwrap_or("无")
        ),
    ];
    lines.push(format!(
        "实际媒体地址 {}",
        if snapshot.local_addresses.is_empty() {
            "无".to_owned()
        } else {
            snapshot
                .local_addresses
                .iter()
                .map(SocketAddr::to_string)
                .collect::<Vec<_>>()
                .join("、")
        }
    ));
    lines.push(format!(
        "网络策略 {}，网络代次 {}",
        match &snapshot.network.policy {
            rustlanbus_engine::participant::network::NetworkPolicy::Auto => "自动".to_owned(),
            rustlanbus_engine::participant::network::NetworkPolicy::Interface { name } =>
                format!("严格使用网卡 {name}"),
        },
        snapshot.network_revision,
    ));
    for selected in &snapshot.network.selections {
        lines.push(format!(
            "网络绑定 {}，网卡 {}，本机接口索引 {:?}",
            selected.bind,
            selected.interface.as_deref().unwrap_or("自动"),
            selected.interface_index,
        ));
    }
    if snapshot.network.selections.is_empty() {
        if let Some(selected) = &snapshot.network.selection {
            lines.push(format!(
                "主要网络绑定 {}，网卡 {}，本机接口索引 {:?}",
                selected.bind,
                selected.interface.as_deref().unwrap_or("自动"),
                selected.interface_index,
            ));
        }
    }
    for interface in &snapshot.network.interfaces {
        lines.push(format!(
            "网卡 {} 标识 {} 连接 {} 回环 {} IPv4索引 {:?} IPv6索引 {:?} 地址 {}",
            interface.display_name,
            interface.name,
            interface.up,
            interface.loopback,
            interface.ipv4_index,
            interface.ipv6_index,
            interface
                .addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、"),
        ));
    }
    if let Some(error) = &snapshot.network.error {
        lines.push(format!("网络选择错误 {error}"));
    }
    if let Some(error) = &snapshot.persistence_error {
        lines.push(format!("保存错误 {error}"));
    }
    if let Some(error) = &snapshot.socket_error {
        lines.push(format!("套接字错误 {error}"));
    }
    if let Some(error) = &snapshot.output.last_error {
        lines.push(format!("播放错误 {error}"));
    }
    if let Some(error) = &snapshot.capture_error {
        lines.push(format!("采集错误 {error}"));
    }
    lines.push("输出中的已提交帧数表示软件已成功提交给设备；物理播放和听音由用户确认。".into());
    for device in &snapshot.output_devices {
        let output = &device.state;
        lines.push(format!(
            "输出选择 {}，实际设备 {}，状态 {}，代次 {}，采样率 {:?}，声道 {:?}，声道掩码 {:?}，周期帧数 {:?}，目标填充帧数 {:?}，缓冲帧数 {:?}，丢弃块数 {}，峰值dB {:?}",
            device.endpoint_id,
            output.actual_endpoint_id.as_deref().unwrap_or("无"),
            output.phase,
            output.generation,
            output.sample_rate_hz,
            output.channels,
            output.channel_mask,
            output.period_frames,
            output.target_padding_frames,
            output.buffer_frames,
            output.dropped_blocks,
            output.peak_db,
        ));
        lines.push(format!(
            "输出 {} 时钟位置 {:?} 时钟频率 {:?} 关联QPC100ns {:?} 时钟返回码 {:?}",
            device.endpoint_id,
            output.clock_position_ticks,
            output.clock_ticks_per_second,
            output.clock_correlated_qpc_100ns,
            output.clock_result_code,
        ));
        for (kind, error) in [
            ("播放", output.last_error.as_deref()),
            ("时钟", output.clock_error.as_deref()),
            ("调度", output.scheduling_error.as_deref()),
        ] {
            if let Some(error) = error {
                lines.push(format!("输出 {} {kind}错误 {error}", device.endpoint_id));
            }
        }
        for source in &output.sources {
            let member = source
                .member_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "本机".into());
            lines.push(format!(
                "输出 {} 来源 {}／{} 状态 {} 最近到达ms {:?} 关闭水位 {} 最高水位 {} 已提交帧数 {} 丢弃块数 {} 分段重启数 {} 有效增益 {}",
                device.endpoint_id,
                member,
                source.source_id,
                source.phase,
                source.last_arrival_age_ms,
                source.closed_before,
                source.high_water,
                source.submitted_frames,
                source.dropped_blocks,
                source.segment_restarts,
                source.effective_gain,
            ));
            if let Some(error) = source.last_error {
                lines.push(format!(
                    "输出 {} 来源 {}／{} 处理错误 {error}",
                    device.endpoint_id, member, source.source_id
                ));
            }
        }
        for error in &output.source_errors {
            lines.push(format!(
                "输出 {} 来源 {}／{} 适配错误 {}",
                device.endpoint_id,
                error
                    .member_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "本机".into()),
                error.source_id,
                error.detail,
            ));
        }
    }
    for source in &snapshot.local_sources {
        lines.push(format!(
            "本地来源 {} {} 发布选择 {} 发布状态 {} 监听 {} 采集状态 {} 说明 {}",
            source.id,
            source.name,
            if source.publish { "开" } else { "关" },
            publication_state(source.publication),
            if source.monitor { "开" } else { "关" },
            source_capture_state(source),
            source.availability_reason,
        ));
        if let Some(capture) = &source.capture {
            lines.push(format!(
                "采集设备 {}，采样率 {:?}，声道 {:?}，周期帧数 {:?}，已采集帧数 {}，丢弃块数 {}",
                capture.actual_endpoint_id.as_deref().unwrap_or("无"),
                capture.sample_rate_hz,
                capture.channels,
                capture.period_frames,
                capture.captured_frames,
                capture.dropped_blocks,
            ));
            if let Some(error) = &capture.scheduling_error {
                lines.push(format!("采集调度提示 {error}"));
            }
        }
    }
    for member in &snapshot.members {
        lines.push(format!(
            "成员 {} {} 地址 {} 允许发送 {} 允许接收 {} 发送就绪 {} 接收就绪 {}",
            member.id,
            member.name,
            member
                .address
                .map(|address| address.to_string())
                .unwrap_or_else(|| "无".into()),
            member.send_allowed,
            member.receive_allowed,
            member.send_ready,
            member.receive_ready
        ));
    }
    append_room_details(&mut lines, &snapshot.room);
    lines.join("\n")
}

fn status_message(snapshot: &ParticipantSnapshot) -> Option<String> {
    let mut parts = Vec::new();
    if !snapshot.running {
        parts.push(
            "控制器已经停止。已保存的小组和收发选择会在下次打开时恢复，当前没有接通的联系。"
                .to_owned(),
        );
    }
    if let Some(error) = &snapshot.persistence_error {
        parts.push(format!(
            "设置已经在这次运行里生效，但没有写入配置文件。{error}"
        ));
    }
    if let Some(error) = &snapshot.socket_error {
        parts.push(format!("连接出现问题。{error}"));
    }
    if snapshot.running {
        for source in &snapshot.local_sources {
            let capture = match source_capture_state(source) {
                "ready" => "采集已启动",
                "idle" => "等待接听或本机监听，联系保持",
                "starting" => "正在打开采集设备",
                "missing" if source.endpoint_id.is_none() => "尚未选择采集设备",
                "missing" => "正在等待所选采集设备",
                "unsupported" => "所选采集设备暂不支持",
                "error" => "采集暂不可用",
                _ => "采集已关闭",
            };
            let publication = match source.publication {
                PublicationState::Active => "发布已启用",
                PublicationState::Unavailable => "发布暂不可用",
                PublicationState::Disabled => "发布已关闭",
            };
            parts.push(format!("{}：{capture}，{publication}。", source.name));
        }
        parts.push(match snapshot.output.phase {
            "idle" => "接收播放当前空闲。".to_owned(),
            "opening" => "正在打开输出设备。".to_owned(),
            "awaiting-device" => "正在等待所选输出设备。".to_owned(),
            "reconfiguring" => "正在重新配置接收播放。".to_owned(),
            "running" => "接收播放已启动。".to_owned(),
            "unavailable" => "接收播放暂不可用。".to_owned(),
            "stopped" => "接收播放已停止。".to_owned(),
            phase => format!("接收播放状态：{phase}。"),
        });
        parts.push(DIRECTORY_NOTE.to_owned());
    }
    if let Some(error) = &snapshot.output.last_error {
        parts.push(format!("播放出现问题。{error}"));
    }
    if let Some(error) = &snapshot.capture_error {
        parts.push(format!("采集出现问题。{error}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn join_message(current: Option<&str>, addition: &str) -> String {
    match current {
        Some(current) if !current.is_empty() => format!("{current} {addition}"),
        _ => addition.to_owned(),
    }
}

fn known_capture(value: &str) -> &'static str {
    match value {
        "disabled" => "disabled",
        "idle" => "idle",
        "starting" => "starting",
        "ready" => "ready",
        "missing" => "missing",
        "unsupported" => "unsupported",
        "error" => "error",
        _ => "disabled",
    }
}

fn source_capture_state(source: &LocalSourceState) -> &'static str {
    if (source.publish || source.monitor) && source.endpoint_id.is_none() {
        "missing"
    } else {
        known_capture(source.capture_state)
    }
}

fn publication_state(value: PublicationState) -> &'static str {
    match value {
        PublicationState::Disabled => "disabled",
        PublicationState::Unavailable => "unavailable",
        PublicationState::Active => "active",
    }
}

fn blank_to_none(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn parse_id<T>(value: &str, label: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|_| {
        format!("{label} {value} 无法识别。请使用配置里显示的完整标识，不要改成显示名称。")
    })
}

fn parse_generation(value: &str) -> Result<u64, String> {
    if !canonical_u64(value) {
        return Err(format!(
            "房间代次 {value} 无法识别。请使用不带前导零的十进制整数字符串，不要使用小数、科学计数法或约数。"
        ));
    }
    value.parse::<u64>().map_err(|_| {
        format!(
            "房间代次 {value} 超出可接受范围。请使用完整十进制字符串，系统不会截断或改成近似值。"
        )
    })
}

fn canonical_u64(value: &str) -> bool {
    let bytes = value.as_bytes();
    match bytes {
        [b'0'] => true,
        [first, rest @ ..]
            if (b'1'..=b'9').contains(first) && rest.iter().all(u8::is_ascii_digit) =>
        {
            true
        }
        _ => false,
    }
}

fn empty_room() -> ViewRoomControl {
    ViewRoomControl {
        session: None,
        cards: Vec::new(),
        routes: Vec::new(),
        global_rules_path: String::new(),
        auto_enter: false,
        error: None,
        creation_defaults: room_creation_defaults(None),
    }
}

fn room_creation_defaults(media_bind: Option<SocketAddr>) -> ViewRoomCreationDefaults {
    ViewRoomCreationDefaults {
        contact_port: DEFAULT_CONTACT_PORT,
        listen_address: media_bind.map(|address| default_contact_listener(address).to_string()),
    }
}

fn map_room(room: &RoomControlState, media_bind: Option<SocketAddr>) -> ViewRoomControl {
    ViewRoomControl {
        session: room.session.as_ref().map(map_session),
        cards: room.cards.iter().map(map_card).collect(),
        routes: room.routes.iter().map(map_route).collect(),
        global_rules_path: path_text(&room.global_rules_path),
        auto_enter: room.auto_enter,
        error: room.error.clone(),
        creation_defaults: room_creation_defaults(media_bind),
    }
}

fn map_session(session: &SessionSnapshot) -> ViewRoomSession {
    ViewRoomSession {
        generation: session.generation.to_string(),
        phase: phase_name(session.phase),
        room_name: session.room_name.clone(),
        instance_id: session.instance_id.map(|id| id.to_string()),
        local_service: session.local_service.map(|address| address.to_string()),
        profile_path: session.profile_path.as_ref().map(|path| path_text(path)),
        discovered_rooms: session
            .discovered_rooms
            .iter()
            .map(map_discovered)
            .collect(),
        choices: session.choices.iter().map(map_discovered).collect(),
        selected_endpoints: session
            .selected_endpoints
            .iter()
            .map(ToString::to_string)
            .collect(),
        members: session.members.iter().map(map_session_member).collect(),
        global_rules: session.global_rules.clone(),
        rules: session.rules.clone(),
        playback_rules: session.playback_rules.clone(),
        rules_unsaved: session.rules_unsaved,
        rules_save_error: session.rules_save_error.clone(),
        rule_files: ViewRuleFiles {
            room: map_rule_file(&session.rule_files.room),
            global: map_rule_file(&session.rule_files.global),
        },
        saved_address: session.saved_address.map(|address| address.to_string()),
        card_persisted: session.card_persisted,
        pending_target_change: session.pending_target_change,
        failure: session.failure.clone(),
        failure_kind: session.failure_kind,
        output_overflow: session.output_overflow,
        output_loss_revision: session.output_loss_revision.to_string(),
        discovery_failure: session.discovery_failure.clone(),
        local_advertisement: session.local_advertisement.as_ref().map(map_advertisement),
        virtual_return: session
            .virtual_return
            .as_ref()
            .map(|state| ViewVirtualReturn {
                member: state.member.as_ref().map(|member| ViewVirtualReturnMember {
                    member_id: member.member_id.to_string(),
                    run_id: member.run_id.to_string(),
                    display_name: member.display_name.clone(),
                }),
                mappings: state
                    .mappings
                    .iter()
                    .map(|mapping| ViewReturnMapping {
                        source_id: mapping.source_id.to_string(),
                        epoch: mapping.epoch.to_string(),
                        upstream_member_id: mapping.upstream_member_id.to_string(),
                        upstream_run_id: mapping.upstream_run_id.to_string(),
                        upstream_source_id: mapping.upstream_source_id.to_string(),
                        upstream_epoch: mapping.upstream_epoch.to_string(),
                        input_first_frame: mapping.input_first_frame.to_string(),
                        output_first_frame: mapping.output_first_frame.to_string(),
                    })
                    .collect(),
                error: state.error.clone(),
            }),
        operation_notice: session.operation_notice.clone(),
    }
}

fn map_discovered(room: &DiscoveredRoom) -> ViewDiscoveredRoom {
    ViewDiscoveredRoom {
        name: room.name.clone(),
        instance_id: room.instance_id.to_string(),
        candidates: room.candidates.iter().map(ToString::to_string).collect(),
    }
}

fn map_rule_file(status: &RuleFileStatus) -> ViewRuleFileStatus {
    ViewRuleFileStatus {
        path: status.path.as_ref().map(|path| path_text(path)),
        outcome: status.outcome.clone(),
    }
}

fn map_session_member(member: &SessionMember) -> ViewRoomMember {
    ViewRoomMember {
        member_id: member.member_id.to_string(),
        run_id: member.run_id.to_string(),
        display_name: member.display_name.clone(),
        endpoint: map_endpoint(member.endpoint),
        directory_visible: member.visible,
    }
}

fn map_route(route: &PeerRoute) -> ViewPeerRoute {
    ViewPeerRoute {
        member_id: route.remote.member.to_string(),
        run_id: route.remote.run.to_string(),
        name: route.name.clone(),
        endpoint: map_endpoint(route.endpoint),
        directory_visible: route.directory_visible,
        send: route.send,
        listen: route.listen,
        send_from: route.send_from,
        listen_from: route.listen_from,
        outputs: route.outputs.clone(),
        outputs_from: route.outputs_from,
        sources: route
            .sources
            .iter()
            .map(|source| ViewSourceDirections {
                kind: source.kind.as_str(),
                send: source.send,
                listen: source.listen,
                send_from: source.send_from,
                listen_from: source.listen_from,
                outputs: source.outputs.clone(),
                outputs_from: source.outputs_from,
            })
            .collect(),
    }
}

fn map_card(card: &RoomCardView) -> ViewRoomCard {
    ViewRoomCard {
        path: path_text(&card.path),
        name: card.name.clone(),
        entered_address: card.entered_address.map(|address| address.to_string()),
        port: card.port,
        strict_auto_join: card.strict_auto_join,
        has_password: card.has_password,
        rule_count: card.rule_count,
        error: card.error.clone(),
    }
}

fn map_advertisement(room: &AdvertisedRoom) -> ViewLocalAdvertisement {
    ViewLocalAdvertisement {
        instance_id: room.instance_id.to_string(),
        display_name: room.display_name.clone(),
        port: room.port,
    }
}

fn map_endpoint(endpoint: ProjectedEndpoint) -> ViewProjectedEndpoint {
    match endpoint {
        ProjectedEndpoint::Ready(address) => ViewProjectedEndpoint::Ready {
            address: address.to_string(),
        },
        ProjectedEndpoint::LocalScopeRequired { ip, port } => {
            ViewProjectedEndpoint::LocalScopeRequired {
                ip: ip.to_string(),
                port,
            }
        }
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Resolving => "resolving",
        Phase::Choosing => "choosing",
        Phase::Connecting => "connecting",
        Phase::Joined => "joined",
        Phase::ContactUnavailable => "contact_unavailable",
        Phase::Leaving => "leaving",
        Phase::Stopped => "stopped",
        Phase::Failed => "failed",
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn append_room_details(lines: &mut Vec<String>, room: &RoomControlState) {
    lines.push(format!(
        "全局规则文件 {}，自动进入 {}",
        path_text(&room.global_rules_path),
        if room.auto_enter { "是" } else { "否" }
    ));
    if let Some(error) = &room.error {
        lines.push(format!("房间控制错误 {error}"));
    }
    for card in &room.cards {
        lines.push(format!(
            "房间卡片 {} 名称 {} 填写地址 {} 端口 {} 有密码 {} 规则数 {} 错误 {}",
            path_text(&card.path),
            card.name.as_deref().unwrap_or("无"),
            card.entered_address
                .map(|address| address.to_string())
                .unwrap_or_else(|| "未填写".into()),
            card.port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "未填写".into()),
            if card.has_password { "是" } else { "否" },
            card.rule_count,
            card.error.as_deref().unwrap_or("无"),
        ));
    }
    match &room.session {
        None => lines.push("房间会话 无".into()),
        Some(session) => append_session_details(lines, session),
    }
    for route in &room.routes {
        lines.push(format!(
            "房间路由 {} {} 目录可见 {} 规则发送 {} 规则收听 {} 端点 {}",
            route.remote.member,
            route.name,
            if route.directory_visible {
                "是"
            } else {
                "否"
            },
            if route.send { "开" } else { "关" },
            if route.listen { "开" } else { "关" },
            endpoint_text(route.endpoint),
        ));
    }
}

fn append_session_details(lines: &mut Vec<String>, session: &SessionSnapshot) {
    lines.push(format!(
        "房间会话 代次 {}，阶段 {}（{}），房间名称 {}",
        session.generation,
        phase_name(session.phase),
        phase_label(session.phase),
        session.room_name.as_deref().unwrap_or("无"),
    ));
    if session.phase == Phase::ContactUnavailable {
        lines.push("联络暂不可用只说明目录联系中断，不改写成员的发送就绪或接收就绪。".into());
    }
    lines.push(format!(
        "房间实例 {}，本机监听 {}，实际选中端点 {}，已保存地址 {}",
        session
            .instance_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "无".into()),
        session
            .local_service
            .map(|address| address.to_string())
            .unwrap_or_else(|| "无".into()),
        if session.selected_endpoints.is_empty() {
            "无".into()
        } else {
            session
                .selected_endpoints
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("，")
        },
        session
            .saved_address
            .map(|address| address.to_string())
            .unwrap_or_else(|| "未填写".into()),
    ));
    lines.push(format!(
        "房间档案 {}，卡片写入 {}，规则未保存 {}，外部目标变化 {}",
        session
            .profile_path
            .as_ref()
            .map(|path| path_text(path))
            .unwrap_or_else(|| "无".into()),
        match session.card_persisted {
            None => "未知",
            Some(true) => "已写入",
            Some(false) => "尚未写入",
        },
        if session.rules_unsaved { "是" } else { "否" },
        if session.pending_target_change {
            "是"
        } else {
            "否"
        },
    ));
    if session.pending_target_change {
        lines.push(
            "外部房间目标已变化，当前收发规则仍是会话里的规则，要到下次进入才改用新目标。".into(),
        );
    }
    if let Some(error) = &session.rules_save_error {
        lines.push(format!("规则保存错误 {error}"));
    }
    if let Some(failure) = &session.failure {
        lines.push(format!("房间失败说明 {failure}"));
    }
    if let Some(failure) = &session.discovery_failure {
        lines.push(format!("房间发现失败 {failure}"));
    }
    if session.output_overflow {
        lines.push(format!(
            "房间输出曾丢弃通知，丢失修订 {}",
            session.output_loss_revision
        ));
    }
    for room in &session.discovered_rooms {
        lines.push(format!(
            "发现房间 {} {} 候选 {}",
            room.name,
            room.instance_id,
            endpoint_list(&room.candidates),
        ));
    }
    for room in &session.choices {
        lines.push(format!(
            "待选房间 {} {} 候选 {}",
            room.name,
            room.instance_id,
            endpoint_list(&room.candidates),
        ));
    }
    for member in &session.members {
        lines.push(format!(
            "目录成员 {} {} 可见 {} 端点 {}",
            member.member_id,
            member.display_name,
            if member.visible { "是" } else { "否" },
            endpoint_text(member.endpoint),
        ));
    }
    if let Some(rules) = &session.global_rules {
        for rule in &rules.rules {
            lines.push(format!("全局规则 {}", rule_text(rule)));
        }
    }
    for rule in &session.rules {
        lines.push(format!("房间规则 {}", rule_text(rule)));
    }
    for rule in &session.playback_rules {
        let mut assignments = Vec::new();
        if let Some(value) = rule.playback.gain_db {
            assignments.push(format!("增益 {value} dB"));
        }
        if let Some(value) = rule.playback.pan {
            assignments.push(format!("声像 {value}"));
        }
        if let Some(value) = rule.playback.mute {
            assignments.push(format!("静音 {}", if value { "开启" } else { "关闭" }));
        }
        if let Some(value) = rule.playback.solo {
            assignments.push(format!("独奏 {}", if value { "开启" } else { "关闭" }));
        }
        lines.push(format!(
            "房间来源混音 条件 {} 类型 {} {}",
            match_text(&rule.condition),
            rule.source_kind.as_str(),
            assignments.join("，"),
        ));
    }
}

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "空闲",
        Phase::Resolving => "正在解析",
        Phase::Choosing => "需要选择",
        Phase::Connecting => "正在连接",
        Phase::Joined => "目录已加入",
        Phase::ContactUnavailable => "联络暂不可用",
        Phase::Leaving => "正在离开",
        Phase::Stopped => "会话已停止",
        Phase::Failed => "未能完成",
    }
}

fn rule_text(rule: &Rule) -> String {
    format!(
        "{} 发送 {} 收听 {}",
        match_text(&rule.r#match),
        direction_text(rule.send),
        direction_text(rule.listen),
    )
}

fn match_text(value: &RuleMatch) -> String {
    match value {
        RuleMatch::All => "匹配全部".into(),
        RuleMatch::Condition { ip, name } => format!(
            "匹配名称 {}，地址 {}",
            name.as_deref().unwrap_or("省略"),
            ip.map(|address| address.to_string())
                .unwrap_or_else(|| "省略".into()),
        ),
    }
}

fn direction_text(value: Option<bool>) -> &'static str {
    match value {
        None => "省略",
        Some(true) => "开",
        Some(false) => "关",
    }
}

fn endpoint_text(endpoint: ProjectedEndpoint) -> String {
    match endpoint {
        ProjectedEndpoint::Ready(address) => address.to_string(),
        ProjectedEndpoint::LocalScopeRequired { ip, port } => {
            format!("需要本机接口范围 {ip} 端口 {port}")
        }
    }
}

fn endpoint_list(addresses: &[SocketAddr]) -> String {
    if addresses.is_empty() {
        "无".into()
    } else {
        addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("，")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlanbus_engine::participant::{
        AudioEffect, Command, DirectoryStatus, GroupContact, LocalSourceState, MemberContact,
        MemberState, MixIntent, ParticipantConfig, PcmFormatState, RemoteMixState,
        RemoteSourceState, RoomControlState, SourceKind,
    };
    use std::net::SocketAddr;

    #[test]
    fn maps_actual_audio_states_and_unsaved_warning() {
        let local = ParticipantConfig::create("书房", "127.0.0.1:47000".parse().unwrap()).unwrap();
        let remote = ParticipantConfig::create("客厅", "127.0.0.1:47001".parse().unwrap()).unwrap();
        let system = local.sources[0].id;
        let microphone = local.sources[1].id;
        let member = remote.member_id;
        let remote_system = remote.sources[0].id;
        let remote_microphone = remote.sources[1].id;
        let snapshot = ParticipantSnapshot {
            room: Default::default(),
            running: true,
            local_member_id: local.member_id,
            local_name: "书房".into(),
            bind: "127.0.0.1:47000".parse::<SocketAddr>().unwrap(),
            local_address: Some("127.0.0.1:47000".parse().unwrap()),
            local_addresses: vec!["127.0.0.1:47000".parse().unwrap()],
            network: Default::default(),
            network_revision: 0,
            run_id: None,
            group_name: "家庭".into(),
            joined: true,
            group_generation: 7,
            group_contact: GroupContact::Contacting,
            auto_reconnect: true,
            login_startup: Default::default(),
            lan_firewall: Default::default(),
            directory: DirectoryStatus::Unavailable,
            local_sources: vec![
                LocalSourceState {
                    id: system,
                    kind: SourceKind::System,
                    name: "系统声音".into(),
                    publish: true,
                    monitor: false,
                    output_targets: vec![OutputTarget::Main],
                    endpoint_id: None,
                    playback: MixIntent::unity(),
                    capture_state: "disabled",
                    availability_reason: "No capture endpoint selected.".into(),
                    publication: PublicationState::Unavailable,
                    capture: None,
                    format: None,
                    peak_db: None,
                },
                LocalSourceState {
                    id: microphone,
                    kind: SourceKind::Microphone,
                    name: "麦克风".into(),
                    publish: false,
                    monitor: false,
                    output_targets: vec![OutputTarget::Main],
                    endpoint_id: None,
                    playback: MixIntent::unity(),
                    capture_state: "disabled",
                    availability_reason: "This source is disabled.".into(),
                    publication: PublicationState::Disabled,
                    capture: None,
                    format: None,
                    peak_db: None,
                },
            ],
            output_endpoint_id: Some("render-a".into()),
            main_output_id: Some("render-a".into()),
            output_devices: Vec::new(),
            endpoints: vec![rustlanbus_engine::participant::AudioEndpoint {
                id: "render-a".into(),
                name: "Playback device".into(),
                direction: "render",
                available: true,
                supported: true,
                reason: None,
            }],
            master_gain_db: -3.0,
            limiter_enabled: false,
            overloaded: None,
            members: vec![MemberState {
                id: member,
                name: "客厅".into(),
                address: Some("127.0.0.1:47001".parse().unwrap()),
                send_allowed: true,
                receive_allowed: false,
                contact: rustlanbus_engine::participant::MemberContact::Contacting,
                send_ready: false,
                sending_sources: Vec::new(),
                receive_ready: true,
                parked: false,
                playback: MixIntent {
                    gain_db: -3.0,
                    mute: true,
                    ..MixIntent::unity()
                },
                source_mix_overrides: vec![RemoteMixState {
                    source_id: remote_microphone,
                    playback: MixIntent {
                        gain_db: -12.0,
                        solo: true,
                        ..MixIntent::unity()
                    },
                }],
                sources: vec![
                    RemoteSourceState {
                        id: remote_system,
                        peak_db: None,
                        kind: SourceKind::System,
                        receive_allowed: false,
                        receive_ready: false,
                        publication: PublicationState::Unavailable,
                        publication_epoch: 1,
                        format: None,
                    },
                    RemoteSourceState {
                        id: remote_microphone,
                        peak_db: None,
                        kind: SourceKind::Microphone,
                        receive_allowed: false,
                        receive_ready: false,
                        publication: PublicationState::Active,
                        publication_epoch: u64::MAX,
                        format: Some(PcmFormatState {
                            sample_rate_hz: 48_000,
                            channels: 2,
                            layout: ChannelLayoutState::StereoLeftRight,
                            channel_mask: 3,
                            encoding: PcmEncodingState::F32Le,
                            sample_bytes: 4,
                            valid_bits: 32,
                        }),
                    },
                ],
                last_error: None,
            }],
            unsaved: true,
            persistence_error: Some("disk full".into()),
            socket_error: None,
            capture_error: None,
            rejected_unconfigured: 0,
            rejected_malformed: 0,
            media_unavailable: 2,
            socket_errors: 0,
            audio: "unavailable",
            group_authentication: "controller_membership_only",
            room_phase: "idle",
            output_submitted_frames: 0,
            output: Default::default(),
            admission_revision: 0,
            return_loop: None,
        };
        let outcome = map_outcome(CommandOutcome {
            command: "set_peer_intent".into(),
            applied: true,
            audio: AudioEffect::Requested,
            message: "applied".into(),
            persistence_error: snapshot.persistence_error.clone(),
            snapshot: snapshot.clone(),
            management: None,
        });
        assert!(outcome.applied);
        assert_eq!(outcome.state.local_sources[0].id, system.to_string());
        assert_eq!(outcome.state.local_sources[1].id, microphone.to_string());
        assert!(
            outcome
                .state
                .local_sources
                .iter()
                .all(|source| !source.available && source.peak_db.is_none())
        );
        assert_eq!(outcome.state.local_sources[0].capture_state, "missing");
        assert_eq!(outcome.state.local_sources[0].publication, "unavailable");
        assert_eq!(
            outcome.state.local_sources[0].availability_reason,
            "No capture endpoint selected."
        );
        assert_eq!(outcome.state.local_sources[1].capture_state, "disabled");
        assert_eq!(outcome.state.local_sources[1].publication, "disabled");
        assert_eq!(outcome.state.members[0].id, member.to_string());
        let remote_sources = &outcome.state.members[0].sources;
        assert_eq!(remote_sources.len(), 2);
        assert_eq!(remote_sources[0].id, remote_system.to_string());
        assert_eq!(remote_sources[0].kind, "system");
        assert_eq!(remote_sources[0].publication, "unavailable");
        assert!(remote_sources[0].format.is_none());
        assert_eq!(remote_sources[0].gain_db, -3.0);
        assert!(remote_sources[0].mute);
        assert_eq!(remote_sources[1].id, remote_microphone.to_string());
        assert_eq!(remote_sources[1].kind, "microphone");
        assert_eq!(remote_sources[1].publication, "active");
        assert_eq!(remote_sources[1].publication_epoch, u64::MAX.to_string());
        assert!(!remote_sources[1].receive_allowed);
        assert!(!remote_sources[1].receive_ready);
        assert_eq!(remote_sources[1].gain_db, -12.0);
        assert!(!remote_sources[1].mute);
        assert!(remote_sources[1].solo);
        let format = remote_sources[1].format.as_ref().unwrap();
        assert_eq!(format.sample_rate_hz, 48_000);
        assert_eq!(format.channels, 2);
        assert_eq!(format.layout, "stereo_left_right");
        assert_eq!(format.encoding, "f32_le");
        assert_eq!(format.sample_bytes, 4);
        assert_eq!(format.valid_bits, 32);
        assert!(
            remote_sources
                .iter()
                .all(|source| !source.available && source.peak_db.is_none())
        );
        assert!(!outcome.state.members[0].receive_allowed);
        assert!(outcome.state.members[0].receive_ready);
        assert!(!outcome.state.overloaded);
        assert!(outcome.state.output_peak_db.is_none());
        assert_eq!(outcome.state.endpoints.len(), 1);
        assert_eq!(outcome.state.endpoints[0].id, "render-a");
        assert!(outcome.state.endpoints[0].supported);
        let message = outcome.state.message.as_deref().expect("status");
        assert!(message.contains("没有写入配置文件"));
        assert!(message.contains("系统声音：尚未选择采集设备，发布暂不可用"));
        assert!(message.contains("麦克风：采集已关闭，发布已关闭"));
        assert!(!message.contains("采集尚未接入"));
        assert!(message.contains("接收播放当前空闲"));
        assert!(!message.contains("这项选择已经保存"));
        let encoded = serde_json::to_string(&outcome.state).unwrap();
        assert!(!encoded.contains("group_generation"));
        assert!(!encoded.contains("groupGeneration"));
        let mut receiving = snapshot.clone();
        receiving.output.phase = "running";
        receiving.output.generation = 1;
        receiving.output.actual_endpoint_id = Some("render-a".into());
        receiving.output_devices = vec![rustlanbus_engine::participant::OutputDeviceState {
            endpoint_id: "render-a".into(),
            state: receiving.output.clone(),
        }];
        let mut session = empty_session(Phase::Joined, 7);
        session.members.push(SessionMember {
            member_id: member,
            run_id: RUN_REMOTE.parse().unwrap(),
            display_name: "客厅".into(),
            endpoint: ProjectedEndpoint::Ready("127.0.0.1:47001".parse().unwrap()),
            alternatives: Vec::new(),
            visible: true,
        });
        receiving.room.routes = project_peers(local.member_id, &session);
        receiving.members[0].contact = MemberContact::Connected;
        receiving.members[0].receive_allowed = true;
        receiving.members[0].sending_sources = vec![system];
        for source in &mut receiving.members[0].sources {
            source.receive_allowed = true;
            source.receive_ready = true;
        }
        receiving.local_sources[0].endpoint_id = Some("capture-a".into());
        receiving.local_sources[0].capture_state = "ready";
        receiving.local_sources[0].availability_reason = "Native capture is running.".into();
        receiving.local_sources[0].publication = PublicationState::Active;
        let requested = map_outcome(CommandOutcome {
            command: "select_output".into(),
            applied: true,
            audio: AudioEffect::Requested,
            message: "requested".into(),
            persistence_error: receiving.persistence_error.clone(),
            snapshot: receiving.clone(),
            management: None,
        });
        assert!(!requested.state.members[0].sources[0].available);
        assert!(requested.state.members[0].sources[1].available);
        assert_eq!(
            requested.state.members[0].sending_source_ids,
            vec![system.to_string()]
        );
        let member_json = serde_json::to_value(&requested.state.members[0]).unwrap();
        assert_eq!(
            member_json["sendingSourceIds"],
            serde_json::json!([system.to_string()])
        );
        let mut source_only = receiving.clone();
        source_only.members[0].receive_allowed = false;
        source_only.members[0].receive_ready = false;
        let source_view = map_snapshot(&source_only);
        assert!(source_view.members[0].sources[1].available);
        let source_json = serde_json::to_value(&source_view.members[0].sources[1]).unwrap();
        assert_eq!(source_json["receiveAllowed"], true);
        assert_eq!(source_json["receiveReady"], true);
        source_only.members[0].sources[1].receive_ready = false;
        assert!(!map_snapshot(&source_only).members[0].sources[1].available);
        source_only.members[0].sources[1].receive_ready = true;
        source_only.room.routes[0]
            .sources
            .iter_mut()
            .find(|choice| choice.kind == SourceKind::Microphone)
            .unwrap()
            .outputs
            .clear();
        let no_output = map_snapshot(&source_only);
        assert!(no_output.members[0].sources[1].receive_ready);
        assert!(!no_output.members[0].sources[1].available);
        assert!(no_output.members[0].sources[1].output_targets.is_empty());
        assert_eq!(
            no_output.members[0].sources[1].availability_reason,
            "尚未选择输出设备。"
        );
        source_only.local_sources[0].monitor = true;
        source_only.local_sources[0].output_targets =
            vec![OutputTarget::Endpoint("missing-monitor".into())];
        let missing_monitor = map_snapshot(&source_only);
        assert!(missing_monitor.local_sources[0].available);
        assert!(!missing_monitor.local_sources[0].monitor_ready);
        source_only.local_sources[0].output_targets = vec![OutputTarget::Main];
        assert!(map_snapshot(&source_only).local_sources[0].monitor_ready);
        assert!(requested.state.output_peak_db.is_none());
        assert!(requested.state.local_sources[0].available);
        assert_eq!(requested.state.local_sources[0].publication, "active");
        assert!(!requested.state.local_sources[1].available);
        let message = requested.state.message.as_deref().unwrap();
        assert!(message.contains("接收播放已启动"));
        assert!(message.contains("音频设置已提交"));
        assert!(message.contains("系统声音：采集已启动，发布已启用"));
        assert!(!message.contains("播放仍不可用"));
        receiving.output.phase = "unavailable";
        receiving.output.last_error = Some("selected endpoint is missing".into());
        receiving.output_devices[0].state = receiving.output.clone();
        receiving.capture_error = Some("capture worker is unavailable".into());
        let unavailable = map_snapshot(&receiving);
        assert!(unavailable.members[0].receive_ready);
        assert!(
            unavailable.members[0]
                .sources
                .iter()
                .all(|source| !source.available)
        );
        let message = unavailable.message.as_deref().unwrap();
        assert!(message.contains("接收播放暂不可用"));
        assert!(message.contains("selected endpoint is missing"));
        assert!(message.contains("capture worker is unavailable"));
        let stopped = ParticipantSnapshot {
            running: false,
            socket_error: Some("closed".into()),
            ..snapshot
        };
        let stopped_view = map_snapshot(&stopped);
        assert_eq!(stopped_view.engine, "error");
        assert!(stopped_view.group.joined);
        assert_eq!(stopped_view.group.contact, "offline");
        assert!(!stopped_view.members[0].send_ready);
        assert!(!stopped_view.members[0].receive_ready);
    }

    use rustlanbus_engine::participant::room_session::{
        MemberView as SessionMember, Phase, ProjectedEndpoint, SessionSnapshot,
    };
    use rustlanbus_engine::participant::rooms::{
        DiscoveredRoom, GlobalRules, JoinMode, RoomCardView, RoomPlaybackRule, RoomProfile,
        RoomTarget, Rule, RuleMatch,
    };
    use rustlanbus_engine::participant::routing::project_peers;
    use std::path::PathBuf;

    const BIG_GENERATION: u64 = 9_007_199_254_740_993;
    const BIG_GENERATION_TEXT: &str = "9007199254740993";
    const SENTINEL_PASSWORD: &str = "sentinel-password-do-not-leak";
    const INSTANCE_A: &str = "11111111111111111111111111111111";
    const INSTANCE_B: &str = "22222222222222222222222222222222";
    const MEMBER_REMOTE: &str = "33333333333333333333333333333333";
    const RUN_REMOTE: &str = "44444444444444444444444444444444";

    fn empty_session(phase: Phase, generation: u64) -> SessionSnapshot {
        SessionSnapshot {
            generation,
            phase,
            room_name: None,
            instance_id: None,
            local_service: None,
            profile_path: None,
            discovered_rooms: Vec::new(),
            choices: Vec::new(),
            selected_endpoints: Vec::new(),
            members: Vec::new(),
            global_rules: None,
            rules: Vec::new(),
            playback_rules: Vec::new(),
            rules_unsaved: false,
            rules_save_error: None,
            rule_files: RuleFilesState::default(),
            saved_address: None,
            card_persisted: None,
            pending_target_change: false,
            failure: None,
            failure_kind: None,
            output_overflow: false,
            output_loss_revision: 0,
            discovery_failure: None,
            local_advertisement: None,
            virtual_return: None,
            operation_notice: None,
        }
    }

    fn room_shell(session: Option<SessionSnapshot>) -> RoomControlState {
        RoomControlState {
            session,
            cards: Vec::new(),
            routes: Vec::new(),
            global_rules_path: PathBuf::from("participant.rooms/global.json"),
            auto_enter: false,
            error: None,
        }
    }

    fn snapshot_with(room: RoomControlState, members: Vec<MemberState>) -> ParticipantSnapshot {
        let local = ParticipantConfig::create("书房", "127.0.0.1:47000".parse().unwrap()).unwrap();
        ParticipantSnapshot {
            room,
            running: true,
            local_member_id: local.member_id,
            local_name: "书房".into(),
            bind: "127.0.0.1:47000".parse().unwrap(),
            local_address: Some("127.0.0.1:47000".parse().unwrap()),
            local_addresses: vec!["127.0.0.1:47000".parse().unwrap()],
            network: Default::default(),
            network_revision: 0,
            run_id: None,
            group_name: "家庭".into(),
            joined: true,
            group_generation: 7,
            group_contact: GroupContact::Connected,
            auto_reconnect: true,
            login_startup: Default::default(),
            lan_firewall: Default::default(),
            directory: DirectoryStatus::Available,
            local_sources: Vec::new(),
            output_endpoint_id: None,
            main_output_id: None,
            output_devices: Vec::new(),
            endpoints: Vec::new(),
            master_gain_db: 0.0,
            limiter_enabled: false,
            overloaded: None,
            members,
            unsaved: false,
            persistence_error: None,
            socket_error: None,
            capture_error: None,
            rejected_unconfigured: 0,
            rejected_malformed: 0,
            media_unavailable: 0,
            socket_errors: 0,
            audio: "unavailable",
            group_authentication: "controller_membership_only",
            room_phase: "idle",
            output_submitted_frames: 0,
            output: Default::default(),
            admission_revision: 0,
            return_loop: None,
        }
    }

    fn ready_member() -> MemberState {
        MemberState {
            id: MEMBER_REMOTE.parse().unwrap(),
            name: "客厅".into(),
            address: Some("192.0.2.20:5004".parse().unwrap()),
            send_allowed: true,
            receive_allowed: false,
            contact: MemberContact::Connected,
            send_ready: true,
            sending_sources: Vec::new(),
            receive_ready: true,
            parked: false,
            playback: MixIntent::unity(),
            source_mix_overrides: Vec::new(),
            sources: Vec::new(),
            last_error: None,
        }
    }

    fn discovered(name: &str, instance: &str, candidates: &[&str]) -> DiscoveredRoom {
        DiscoveredRoom {
            name: name.into(),
            instance_id: instance.parse().unwrap(),
            candidates: candidates
                .iter()
                .map(|candidate| candidate.parse().unwrap())
                .collect(),
        }
    }

    fn name_only(name: &str) -> RuleMatch {
        RuleMatch::Condition {
            ip: None,
            name: Some(name.into()),
        }
    }

    fn queued(command: &str, snapshot: ParticipantSnapshot) -> CommandOutcome {
        CommandOutcome {
            command: command.into(),
            applied: true,
            audio: AudioEffect::Requested,
            message: "The room command was queued. Observe room phase and rule-save state for completion.".into(),
            persistence_error: None,
            snapshot,
            management: None,
        }
    }

    #[test]
    fn projects_no_session_without_clearing_direct_readiness() {
        let mut room = room_shell(None);
        room.error = Some("Room card directory could not be read.".into());
        let mut snapshot = snapshot_with(room, vec![ready_member()]);
        snapshot.directory = DirectoryStatus::Unavailable;
        let view = map_snapshot(&snapshot);
        assert!(view.members[0].send_ready);
        assert!(view.members[0].receive_ready);
        assert!(view.group.joined);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["room"]["session"].is_null());
        assert!(json["room"].get("phase").is_none());
        assert_eq!(
            json["room"]["error"],
            "Room card directory could not be read."
        );
        assert_eq!(
            json["room"]["globalRulesPath"],
            PathBuf::from("participant.rooms/global.json")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(json["room"]["autoEnter"], false);
        assert_eq!(json["members"][0]["sendReady"], true);
        assert_eq!(json["members"][0]["receiveReady"], true);
        let details = sanitized_details(&snapshot);
        assert!(details.contains("房间会话 无"));
        assert!(!details.contains("直连音频已停止"));
        assert!(details.contains("发送就绪 true"));
        assert!(details.contains("接收就绪 true"));
    }

    #[test]
    fn projects_invalid_saved_card_error_without_a_password() {
        let mut profile = RoomProfile::create("Studio").unwrap();
        profile.room.password = SENTINEL_PASSWORD.into();
        let mut room = room_shell(None);
        room.cards.push(RoomCardView {
            path: PathBuf::from("cards/broken.json"),
            name: None,
            entered_address: None,
            port: None,
            strict_auto_join: false,
            has_password: false,
            rule_count: 0,
            error: Some(
                "The room profile could not be read or validated. Check its JSON and room fields; the file was left unchanged."
                    .into(),
            ),
        });
        let snapshot = snapshot_with(room, Vec::new());
        let view = map_snapshot(&snapshot);
        let json = serde_json::to_value(&view).unwrap();
        let card = &json["room"]["cards"][0];
        assert!(
            card["error"]
                .as_str()
                .unwrap()
                .contains("could not be read or validated")
        );
        assert_eq!(card["hasPassword"], false);
        assert!(card["name"].is_null());
        assert!(card.get("password").is_none());
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(
            serde_json::to_string(&profile)
                .unwrap()
                .contains(SENTINEL_PASSWORD)
        );
        assert!(!encoded.contains(SENTINEL_PASSWORD));
        assert!(!encoded.contains("\"password\""));
        let details = sanitized_details(&snapshot);
        assert!(details.contains("could not be read or validated"));
        assert!(!details.contains(SENTINEL_PASSWORD));
    }

    #[test]
    fn projects_resolving_then_same_name_choices_without_treating_acceptance_as_joined() {
        let mut resolving = empty_session(Phase::Resolving, 4);
        resolving.room_name = Some("Studio".into());
        resolving.discovered_rooms = vec![discovered("Studio", INSTANCE_A, &["192.0.2.10:47101"])];
        let mut resolving_snapshot = snapshot_with(room_shell(Some(resolving)), Vec::new());
        resolving_snapshot.joined = false;
        let accepted = map_outcome(queued("join_saved_room", resolving_snapshot));
        assert!(accepted.applied);
        let session = accepted
            .state
            .room
            .session
            .as_ref()
            .expect("resolving session");
        assert_eq!(session.phase, "resolving");
        assert!(session.choices.is_empty());
        assert_eq!(session.discovered_rooms.len(), 1);
        assert!(!accepted.state.group.joined);
        assert_ne!(session.phase, "joined");

        let mut choosing = empty_session(Phase::Choosing, 4);
        choosing.room_name = Some("Studio".into());
        choosing.discovered_rooms = vec![
            discovered("Studio", INSTANCE_A, &["192.0.2.10:47101"]),
            discovered("Studio", INSTANCE_B, &["192.0.2.20:47101"]),
            discovered(
                "Other",
                "55555555555555555555555555555555",
                &["192.0.2.30:47101"],
            ),
        ];
        choosing.choices = vec![
            discovered("Studio", INSTANCE_A, &["192.0.2.10:47101"]),
            discovered(
                "Studio",
                INSTANCE_B,
                &["192.0.2.20:47101", "192.0.2.21:47101"],
            ),
        ];
        let mut choosing_snapshot = snapshot_with(room_shell(Some(choosing)), Vec::new());
        choosing_snapshot.joined = false;
        let view = map_snapshot(&choosing_snapshot);
        let session = view.room.session.expect("choosing session");
        assert_eq!(session.phase, "choosing");
        assert!(session.instance_id.is_none());
        assert_eq!(session.choices.len(), 2);
        assert_eq!(session.choices[0].name, "Studio");
        assert_eq!(session.choices[1].name, "Studio");
        assert_ne!(
            session.choices[0].instance_id,
            session.choices[1].instance_id
        );
        assert_eq!(session.discovered_rooms.len(), 3);
        let accepted = map_outcome(queued("join_saved_room", choosing_snapshot));
        assert!(accepted.applied);
        assert_eq!(
            accepted
                .state
                .room
                .session
                .as_ref()
                .expect("choosing session")
                .phase,
            "choosing"
        );
        assert!(!accepted.state.group.joined);
        let json = serde_json::to_value(&accepted.state).unwrap();
        assert_eq!(
            json["room"]["session"]["choices"].as_array().unwrap().len(),
            2
        );
        assert!(json["room"]["session"].get("joined").is_none());
    }

    #[test]
    fn generation_above_js_safe_integer_round_trips_as_decimal_text() {
        let mut session = empty_session(Phase::Choosing, BIG_GENERATION);
        session.output_loss_revision = BIG_GENERATION;
        session.choices = vec![discovered("Studio", INSTANCE_A, &["192.0.2.10:47101"])];
        let view = map_snapshot(&snapshot_with(room_shell(Some(session)), Vec::new()));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["room"]["session"]["generation"], BIG_GENERATION_TEXT);
        assert_eq!(
            json["room"]["session"]["outputLossRevision"],
            BIG_GENERATION_TEXT
        );
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(encoded.contains("\"generation\":\"9007199254740993\""));
        assert!(!encoded.contains("\"generation\":9007199254740993"));
        assert!(!encoded.contains("9007199254740992"));
        let details = sanitized_details(&snapshot_with(
            room_shell(Some(empty_session(Phase::Choosing, BIG_GENERATION))),
            Vec::new(),
        ));
        assert!(details.contains(BIG_GENERATION_TEXT));
        assert!(!details.contains("9007199254740992"));

        let action: UiAction = serde_json::from_value(serde_json::json!({
            "type": "choose_room",
            "generation": BIG_GENERATION_TEXT,
            "instanceId": INSTANCE_A,
        }))
        .unwrap();
        match action.into_command().unwrap() {
            Command::ChooseRoom {
                generation,
                instance_id,
            } => {
                assert_eq!(generation, BIG_GENERATION);
                assert_eq!(instance_id.to_string(), INSTANCE_A);
            }
            other => panic!("expected choose_room, got {other:?}"),
        }
        assert!(
            serde_json::from_value::<UiAction>(serde_json::json!({
                "type": "choose_room",
                "generation": BIG_GENERATION,
                "instanceId": INSTANCE_A,
            }))
            .is_err()
        );
        for token in [
            "9007199254740993.0",
            "9.007199254740993e15",
            " 9007199254740993",
            "9007199254740993 ",
            "+9007199254740993",
            "01",
            "",
            "18446744073709551616",
        ] {
            let action: UiAction = serde_json::from_value(serde_json::json!({
                "type": "choose_room",
                "generation": token,
                "instanceId": INSTANCE_A,
            }))
            .unwrap();
            let error = action.into_command().unwrap_err();
            assert!(error.contains(token), "{error}");
            assert!(!error.contains("18446744073709551615"), "{error}");
        }
    }

    #[test]
    fn omitted_ip_stays_distinct_from_json_null() {
        let missing_address = serde_json::json!({
            "type": "create_room",
            "bind": "0.0.0.0:47101",
            "profile": {
                "schema": 1,
                "room": {
                    "name": "Studio",
                    "password": "",
                    "strict_auto_join": false
                },
                "rules": []
            }
        });
        let explicit_null = serde_json::json!({
            "type": "create_room",
            "bind": "0.0.0.0:47101",
            "profile": {
                "schema": 1,
                "room": {
                    "name": "Studio",
                    "address": null,
                    "password": "",
                    "strict_auto_join": false
                },
                "rules": [{ "match": { "name": "Ada" }, "send": false }]
            }
        });
        for (label, value) in [
            ("omitted address", missing_address),
            ("json null", explicit_null),
        ] {
            let action: UiAction = serde_json::from_value(value).unwrap();
            match action.into_command().unwrap() {
                Command::CreateRoom { profile, bind } => {
                    assert!(
                        profile.room.address.is_none(),
                        "{label} stays unset and does not copy the wildcard bind"
                    );
                    assert!(bind.ip().is_unspecified(), "{label}");
                    assert_eq!(bind.port(), 47101, "{label}");
                    assert_ne!(
                        profile.room.address,
                        Some("0.0.0.0".parse().unwrap()),
                        "{label}"
                    );
                    if label == "json null" {
                        assert!(profile.rules[0].listen.is_none());
                        assert!(matches!(
                            &profile.rules[0].r#match,
                            RuleMatch::Condition { ip: None, name: Some(name) } if name == "Ada"
                        ));
                    }
                }
                other => panic!("{label} expected create_room, got {other:?}"),
            }
        }

        let mut session = empty_session(Phase::Joined, 8);
        session.local_service = Some("0.0.0.0:47101".parse().unwrap());
        session.selected_endpoints = vec!["198.51.100.10:5004".parse().unwrap()];
        session.saved_address = None;
        session.rules = vec![
            Rule {
                outputs: None,
                source_kind: None,
                r#match: name_only("Ada"),
                send: Some(false),
                listen: None,
            },
            Rule {
                outputs: None,
                source_kind: None,
                r#match: RuleMatch::Condition {
                    ip: Some("192.0.2.10".parse().unwrap()),
                    name: None,
                },
                send: None,
                listen: Some(true),
            },
        ];
        session.playback_rules = vec![RoomPlaybackRule {
            condition: name_only("Ada"),
            source_kind: SourceKind::System,
            playback: MixIntent::unity().into(),
        }];
        let mut room = room_shell(Some(session));
        room.cards.push(RoomCardView {
            path: PathBuf::from("cards/studio.json"),
            name: Some("Studio".into()),
            entered_address: None,
            port: None,
            strict_auto_join: false,
            has_password: false,
            rule_count: 2,
            error: None,
        });
        let snapshot = snapshot_with(room, Vec::new());
        let view = map_snapshot(&snapshot);
        let json = serde_json::to_value(&view).unwrap();
        let session = &json["room"]["session"];
        let session_object = session.as_object().unwrap();
        assert!(session_object.contains_key("savedAddress"));
        assert!(session["savedAddress"].is_null());
        assert_eq!(session["localService"], "0.0.0.0:47101");
        assert_eq!(session["selectedEndpoints"][0], "198.51.100.10:5004");
        assert!(session["rules"][0]["match"].get("ip").is_none());
        assert_eq!(session["rules"][0]["match"]["name"], "Ada");
        assert!(session["rules"][0].get("listen").is_none());
        assert_eq!(session["rules"][0]["send"], false);
        assert_eq!(session["rules"][1]["match"]["ip"], "192.0.2.10");
        assert!(session["rules"][1]["match"].get("name").is_none());
        assert!(session["rules"][1].get("send").is_none());
        assert!(session["playbackRules"][0]["condition"].get("ip").is_none());
        let card = json["room"]["cards"][0].as_object().unwrap();
        assert!(card.contains_key("enteredAddress"));
        assert!(json["room"]["cards"][0]["enteredAddress"].is_null());
        assert!(json["room"]["cards"][0]["port"].is_null());
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(encoded.contains("\"savedAddress\":null"));
        assert!(encoded.contains("\"enteredAddress\":null"));
        assert!(!encoded.contains("\"ip\":null"));
        assert!(!encoded.contains("\"savedAddress\":\"0.0.0.0\""));
        let details = sanitized_details(&snapshot);
        assert!(details.contains("本机监听 0.0.0.0:47101"));
        assert!(details.contains("已保存地址 未填写"));
        assert!(details.contains("填写地址 未填写"));
        assert!(details.contains("地址 省略"));
        assert!(!details.contains("已保存地址 0.0.0.0"));
    }

    #[test]
    fn stale_choice_token_is_not_projected_as_selected() {
        let mut session = empty_session(Phase::Choosing, BIG_GENERATION);
        session.room_name = Some("Studio".into());
        session.choices = vec![
            discovered("Studio", INSTANCE_A, &["192.0.2.10:47101"]),
            discovered("Studio", INSTANCE_B, &["192.0.2.20:47101"]),
        ];
        let snapshot = snapshot_with(room_shell(Some(session)), Vec::new());
        let bad: UiAction = serde_json::from_value(serde_json::json!({
            "type": "choose_room",
            "generation": "not-a-generation",
            "instanceId": INSTANCE_B,
        }))
        .unwrap();
        assert!(bad.into_command().is_err());
        let still = map_snapshot(&snapshot);
        let session = still.room.session.expect("choice session");
        assert_eq!(session.phase, "choosing");
        assert_eq!(session.generation, BIG_GENERATION_TEXT);
        assert!(session.instance_id.is_none());
        assert_eq!(session.choices.len(), 2);

        let stale: UiAction = serde_json::from_value(serde_json::json!({
            "type": "choose_room",
            "generation": "1",
            "instanceId": INSTANCE_B,
        }))
        .unwrap();
        match stale.into_command().unwrap() {
            Command::ChooseRoom {
                generation,
                instance_id,
            } => {
                assert_eq!(generation, 1);
                assert_ne!(generation, BIG_GENERATION);
                assert_eq!(instance_id.to_string(), INSTANCE_B);
            }
            other => panic!("expected choose_room, got {other:?}"),
        }
        let accepted = map_outcome(queued("choose_room", snapshot));
        assert!(accepted.applied);
        let session = accepted.state.room.session.unwrap();
        assert_eq!(session.phase, "choosing");
        assert_eq!(session.generation, BIG_GENERATION_TEXT);
        assert!(session.instance_id.is_none());
        assert_eq!(session.choices.len(), 2);
        assert_ne!(session.choices[1].instance_id, session.generation);
    }

    #[test]
    fn typed_file_states_and_actual_rule_origins_reach_the_view() {
        let mut session = empty_session(Phase::Joined, 5);
        session.rule_files = RuleFilesState {
            room: RuleFileStatus {
                path: Some(PathBuf::from("cards/studio.json")),
                outcome: RuleSaveState::Saved,
            },
            global: RuleFileStatus {
                path: Some(PathBuf::from("global.json")),
                outcome: RuleSaveState::ExternalChange {
                    detail: "external bytes".into(),
                },
            },
        };
        let view = serde_json::to_value(map_session(&session)).unwrap();
        assert_eq!(view["ruleFiles"]["room"]["state"], "saved");
        assert!(view["ruleFiles"]["room"].get("detail").is_none());
        assert_eq!(view["ruleFiles"]["global"]["state"], "external_change");
        assert_eq!(view["ruleFiles"]["global"]["path"], "global.json");
        assert_eq!(view["ruleFiles"]["global"]["detail"], "external bytes");
        let mut global = GlobalRules::baseline();
        global.rules.push(Rule {
            outputs: None,
            source_kind: None,
            r#match: name_only("Desk"),
            send: Some(false),
            listen: None,
        });
        session.global_rules = Some(global);
        session.rules = vec![
            Rule {
                outputs: None,
                source_kind: None,
                r#match: name_only("Other"),
                send: Some(false),
                listen: None,
            },
            Rule {
                outputs: None,
                source_kind: None,
                r#match: name_only("Other"),
                send: None,
                listen: Some(false),
            },
            Rule {
                outputs: None,
                source_kind: None,
                r#match: name_only("Desk"),
                send: None,
                listen: Some(true),
            },
        ];
        session.members = vec![SessionMember {
            member_id: MEMBER_REMOTE.parse().unwrap(),
            run_id: RUN_REMOTE.parse().unwrap(),
            display_name: "Desk".into(),
            endpoint: ProjectedEndpoint::Ready("192.0.2.4:5004".parse().unwrap()),
            alternatives: Vec::new(),
            visible: true,
        }];
        session.rules.push(Rule {
            outputs: None,
            r#match: name_only("Desk"),
            source_kind: Some(SourceKind::Microphone),
            send: Some(true),
            listen: Some(false),
        });
        session.rules.push(Rule {
            outputs: Some(vec![OutputTarget::Endpoint("headphones".into())]),
            r#match: name_only("Desk"),
            source_kind: Some(SourceKind::Microphone),
            send: None,
            listen: None,
        });
        let local = snapshot_with(room_shell(None), Vec::new()).local_member_id;
        let route =
            rustlanbus_engine::participant::routing::project_peers(local, &session).remove(0);
        let view = serde_json::to_value(map_route(&route)).unwrap();
        assert_eq!(
            view["sendFrom"],
            serde_json::json!({"list":"global","index":1})
        );
        assert_eq!(
            view["listenFrom"],
            serde_json::json!({"list":"room","index":2})
        );
        let microphone = view["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["kind"] == "microphone")
            .unwrap();
        assert_eq!(microphone["send"], true);
        assert_eq!(microphone["listen"], false);
        assert_eq!(
            microphone["sendFrom"],
            serde_json::json!({"list":"room","index":3})
        );
        assert_eq!(
            microphone["listenFrom"],
            serde_json::json!({"list":"room","index":3})
        );
        assert_eq!(view["outputs"], serde_json::json!(["main"]));
        assert!(view["outputsFrom"].is_null());
        assert_eq!(
            microphone["outputs"],
            serde_json::json!([{"endpoint":"headphones"}])
        );
        assert_eq!(
            microphone["outputsFrom"],
            serde_json::json!({"list":"room","index":4})
        );
    }

    #[test]
    fn output_readiness_uses_selected_owners_and_keeps_disconnected_intent() {
        use rustlanbus_engine::participant::{OutputDeviceState, OutputState};
        let mut snapshot = snapshot_with(room_shell(None), Vec::new());
        snapshot.main_output_id = Some("headphones".into());
        // The legacy representative may describe a healthy, unrelated owner.
        snapshot.output.phase = "running";
        snapshot.output.actual_endpoint_id = Some("speakers".into());
        snapshot.output_devices = vec![
            OutputDeviceState {
                endpoint_id: "speakers".into(),
                state: snapshot.output.clone(),
            },
            OutputDeviceState {
                endpoint_id: "headphones".into(),
                state: OutputState {
                    phase: "unavailable",
                    last_error: Some("endpoint missing".into()),
                    ..OutputState::default()
                },
            },
        ];
        assert!(!selected_output_running(&snapshot, &[]));
        assert!(!selected_output_running(&snapshot, &[OutputTarget::Main]));
        assert!(selected_output_running(
            &snapshot,
            &[OutputTarget::Endpoint("speakers".into())]
        ));
        let view = serde_json::to_value(map_snapshot(&snapshot)).unwrap();
        assert_eq!(view["mainOutputId"], "headphones");
        assert_eq!(view["outputDevices"][1]["endpointId"], "headphones");
        assert_eq!(view["outputDevices"][1]["phase"], "unavailable");
        assert_eq!(view["outputDevices"][1]["lastError"], "endpoint missing");
        snapshot.output_devices[1].state.phase = "running";
        snapshot.output_devices[1].state.actual_endpoint_id = Some("headphones".into());
        assert!(selected_output_running(&snapshot, &[OutputTarget::Main]));
        snapshot.running = false;
        assert!(!selected_output_running(&snapshot, &[OutputTarget::Main]));
    }

    #[test]
    fn local_output_action_preserves_explicit_destinations_and_empty_selection() {
        for targets in [
            serde_json::json!([]),
            serde_json::json!(["main", {"endpoint":"headphones"}]),
        ] {
            let action: UiAction = serde_json::from_value(serde_json::json!({
                "type": "set_local_outputs", "sourceId": MEMBER_REMOTE,
                "targets": targets.clone(),
            }))
            .unwrap();
            let Command::SetLocalOutputs {
                source_id,
                targets: mapped,
            } = action.into_command().unwrap()
            else {
                panic!("expected local output routing");
            };
            assert_eq!(source_id.to_string(), MEMBER_REMOTE);
            assert_eq!(serde_json::to_value(mapped).unwrap(), targets);
        }
    }

    #[test]
    fn accepted_command_keeps_later_room_failure() {
        let mut session = empty_session(Phase::Failed, 6);
        session.room_name = Some("Studio".into());
        session.failure = Some("The contact service rejected the room.".into());
        session.failure_kind = Some(EntryFailureKind::ContactRejected);
        session.card_persisted = Some(false);
        let mut snapshot = snapshot_with(room_shell(Some(session)), Vec::new());
        snapshot.joined = false;
        let outcome = map_outcome(queued("join_unknown_room", snapshot.clone()));
        assert!(outcome.applied);
        let session = outcome.state.room.session.as_ref().expect("failed session");
        assert_eq!(session.phase, "failed");
        assert_eq!(
            session.failure.as_deref(),
            Some("The contact service rejected the room.")
        );
        assert!(!outcome.state.group.joined);
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["applied"], true);
        assert_eq!(json["state"]["room"]["session"]["phase"], "failed");
        assert_eq!(
            json["state"]["room"]["session"]["failureKind"],
            "contact_rejected"
        );
        assert_eq!(
            json["state"]["room"]["session"]["failure"],
            "The contact service rejected the room."
        );
        assert!(json["state"]["group"]["joined"] == false);
        let details = sanitized_details(&snapshot);
        assert!(details.contains("房间失败说明 The contact service rejected the room."));
        assert!(details.contains("failed"));
        assert!(!details.contains("直连音频已停止"));
    }

    #[test]
    fn creation_defaults_come_from_core_and_the_actual_media_socket() {
        let mut snapshot = snapshot_with(room_shell(None), Vec::new());
        for (media, expected) in [
            (Some("0.0.0.0:5004"), Some("0.0.0.0:47101")),
            (Some("[::]:5004"), Some("[::]:47101")),
            (Some("127.0.0.1:5004"), Some("127.0.0.1:47101")),
            (Some("[::1]:5004"), Some("[::1]:47101")),
            (None, None),
        ] {
            snapshot.local_address = media.map(|value| value.parse().unwrap());
            let view = map_snapshot(&snapshot);
            assert_eq!(
                view.room.creation_defaults.contact_port,
                DEFAULT_CONTACT_PORT
            );
            assert_eq!(
                view.room.creation_defaults.listen_address.as_deref(),
                expected
            );
            let json = serde_json::to_value(&view).unwrap();
            assert_eq!(
                json["room"]["creationDefaults"]["listenAddress"],
                serde_json::json!(expected)
            );
        }
    }

    #[test]
    fn directory_loss_keeps_direct_member_readiness() {
        let local = ParticipantConfig::create("书房", "127.0.0.1:47000".parse().unwrap()).unwrap();
        let mut session = empty_session(Phase::ContactUnavailable, 9);
        session.room_name = Some("Studio".into());
        session.rules.push(Rule {
            outputs: None,
            source_kind: None,
            r#match: name_only("客厅"),
            send: None,
            listen: Some(true),
        });
        session.members.push(SessionMember {
            member_id: MEMBER_REMOTE.parse().unwrap(),
            run_id: RUN_REMOTE.parse().unwrap(),
            display_name: "客厅".into(),
            endpoint: ProjectedEndpoint::Ready("192.0.2.20:5004".parse().unwrap()),
            alternatives: Vec::new(),
            visible: false,
        });
        let routes = project_peers(local.member_id, &session);
        assert_eq!(routes.len(), 1);
        assert!(!routes[0].directory_visible);
        let mut room = room_shell(Some(session));
        room.routes = routes;
        let mut member = ready_member();
        member.receive_allowed = true;
        let mut snapshot = snapshot_with(room, vec![member]);
        snapshot.directory = DirectoryStatus::Stale;
        snapshot.joined = true;
        let view = map_snapshot(&snapshot);
        assert!(view.members[0].send_ready);
        assert!(view.members[0].receive_ready);
        assert!(view.members[0].receive_allowed);
        let session = view.room.session.as_ref().expect("contact session");
        assert_eq!(session.phase, "contact_unavailable");
        assert!(!session.members[0].directory_visible);
        assert!(!view.room.routes[0].directory_visible);
        assert!(view.room.routes[0].send);
        assert!(view.room.routes[0].listen);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["members"][0]["sendReady"], true);
        assert_eq!(json["members"][0]["receiveReady"], true);
        assert_eq!(json["room"]["session"]["phase"], "contact_unavailable");
        assert_eq!(
            json["room"]["session"]["members"][0]["directoryVisible"],
            false
        );
        assert_eq!(json["room"]["routes"][0]["directoryVisible"], false);
        let details = sanitized_details(&snapshot);
        assert!(details.contains("contact_unavailable"));
        assert!(details.contains("联络暂不可用只说明目录联系中断"));
        assert!(details.contains("发送就绪 true"));
        assert!(details.contains("接收就绪 true"));
        assert!(details.contains("目录可见 否"));
        assert!(!details.contains("直连音频已停止"));
    }

    #[test]
    fn external_replacement_stays_unsaved_instead_of_rolled_back() {
        let mut session = empty_session(Phase::Joined, 11);
        session.room_name = Some("Studio".into());
        session.rules_unsaved = true;
        session.rules_save_error =
            Some("The room rules could not be saved after the external profile change.".into());
        session.pending_target_change = true;
        session.rules = vec![Rule {
            outputs: None,
            source_kind: None,
            r#match: name_only("Ada"),
            send: Some(true),
            listen: None,
        }];
        let mut room = room_shell(Some(session));
        room.cards.push(RoomCardView {
            path: PathBuf::from("cards/studio.json"),
            name: Some("External saved name".into()),
            entered_address: Some("192.0.2.10".parse().unwrap()),
            port: Some(47111),
            strict_auto_join: false,
            has_password: false,
            rule_count: 1,
            error: None,
        });
        let view = map_snapshot(&snapshot_with(room, Vec::new()));
        let json = serde_json::to_value(&view).unwrap();
        let session = &json["room"]["session"];
        assert_eq!(session["phase"], "joined");
        assert_eq!(session["roomName"], "Studio");
        assert_eq!(session["rulesUnsaved"], true);
        assert_eq!(
            session["rulesSaveError"],
            "The room rules could not be saved after the external profile change."
        );
        assert_eq!(session["pendingTargetChange"], true);
        assert_eq!(session["rules"][0]["send"], true);
        assert!(session["rules"][0].get("listen").is_none());
        assert_eq!(json["room"]["cards"][0]["name"], "External saved name");
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(!encoded.contains("回滚"));
        assert!(!encoded.contains("rolled back"));
        let details = sanitized_details(&snapshot_with(
            {
                let mut session = empty_session(Phase::Joined, 11);
                session.room_name = Some("Studio".into());
                session.rules_unsaved = true;
                session.rules_save_error = Some(
                    "The room rules could not be saved after the external profile change.".into(),
                );
                session.pending_target_change = true;
                session.rules = vec![Rule {
                    outputs: None,
                    source_kind: None,
                    r#match: name_only("Ada"),
                    send: Some(true),
                    listen: None,
                }];
                let mut room = room_shell(Some(session));
                room.cards.push(RoomCardView {
                    path: PathBuf::from("cards/studio.json"),
                    name: Some("External saved name".into()),
                    entered_address: Some("192.0.2.10".parse().unwrap()),
                    port: Some(47111),
                    strict_auto_join: false,
                    has_password: false,
                    rule_count: 1,
                    error: None,
                });
                room
            },
            Vec::new(),
        ));
        assert!(details.contains("规则未保存 是"));
        assert!(details.contains(
            "规则保存错误 The room rules could not be saved after the external profile change."
        ));
        assert!(details.contains("外部房间目标已变化"));
        assert!(details.contains("当前收发规则仍是会话里的规则"));
        assert!(!details.contains("回滚"));
        assert!(!details.contains("rolled back"));
    }

    #[test]
    fn joined_unknown_room_can_stay_unpersisted() {
        let mut session = empty_session(Phase::Joined, 12);
        session.room_name = Some("Pop-up".into());
        session.card_persisted = Some(false);
        session.profile_path = Some(PathBuf::from("cards/pending.json"));
        session.saved_address = None;
        session.local_service = Some("192.0.2.40:47101".parse().unwrap());
        let view = map_snapshot(&snapshot_with(
            room_shell(Some(session.clone())),
            Vec::new(),
        ));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["room"]["session"]["phase"], "joined");
        assert_eq!(json["room"]["session"]["cardPersisted"], false);
        assert!(json["room"]["session"]["savedAddress"].is_null());
        assert_eq!(
            json["room"]["session"]["profilePath"],
            PathBuf::from("cards/pending.json")
                .to_string_lossy()
                .as_ref()
        );
        assert_ne!(
            json["room"]["session"]["localService"],
            json["room"]["session"]["savedAddress"]
        );
        let details = sanitized_details(&snapshot_with(room_shell(Some(session)), Vec::new()));
        assert!(details.contains("尚未写入"));
        assert!(details.contains("joined"));
        assert!(!details.contains("卡片写入 已写入"));
    }

    #[test]
    fn sentinel_password_is_absent_from_view_and_details() {
        let mut profile = RoomProfile::create("Studio").unwrap();
        profile.room.password = SENTINEL_PASSWORD.into();
        profile.rules.push(Rule {
            outputs: None,
            source_kind: None,
            r#match: RuleMatch::All,
            send: Some(true),
            listen: Some(false),
        });
        let create = UiAction::CreateRoom {
            profile: Box::new(profile.clone()),
            bind: "0.0.0.0:47101".parse().unwrap(),
        };
        match create.into_command().unwrap() {
            Command::CreateRoom { profile, bind } => {
                assert_eq!(profile.room.password, SENTINEL_PASSWORD);
                assert!(profile.room.address.is_none());
                assert!(bind.ip().is_unspecified());
            }
            other => panic!("expected create_room, got {other:?}"),
        }
        let join = UiAction::JoinUnknownRoom {
            target: RoomTarget {
                name: "Studio".into(),
                address: None,
                port: None,
                password: SENTINEL_PASSWORD.into(),
                strict_auto_join: false,
            },
        };
        match join.into_command().unwrap() {
            Command::JoinUnknownRoom { target } => {
                assert_eq!(target.password, SENTINEL_PASSWORD);
                assert!(target.address.is_none());
            }
            other => panic!("expected join_unknown_room, got {other:?}"),
        }
        assert!(
            serde_json::to_string(&profile)
                .unwrap()
                .contains(SENTINEL_PASSWORD)
        );

        let mut session = empty_session(Phase::Joined, 2);
        session.room_name = Some("Studio".into());
        session.card_persisted = Some(true);
        session.profile_path = Some(PathBuf::from("cards/studio.json"));
        let mut room = room_shell(Some(session));
        room.cards.push(RoomCardView {
            path: PathBuf::from("cards/studio.json"),
            name: Some("Studio".into()),
            entered_address: None,
            port: None,
            strict_auto_join: false,
            has_password: true,
            rule_count: profile.rules.len() + profile.playback.len(),
            error: None,
        });
        let snapshot = snapshot_with(room, Vec::new());
        let view = map_snapshot(&snapshot);
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(encoded.contains("\"hasPassword\":true"));
        assert!(!encoded.contains(SENTINEL_PASSWORD));
        assert!(!encoded.contains("\"password\""));
        let details = sanitized_details(&snapshot);
        assert!(details.contains("有密码 是"));
        assert!(!details.contains(SENTINEL_PASSWORD));
        assert!(!details.contains("\"password\""));
    }

    #[test]
    fn member_direction_edit_sets_only_the_changed_direction() {
        for (payload, expected_send, expected_listen) in [
            (
                serde_json::json!({"type":"edit_room_rule", "rule":{"match":{"name":"Ada"},"send":false}}),
                Some(false),
                None,
            ),
            (
                serde_json::json!({"type":"edit_room_rule", "rule":{"match":{"ip":"192.0.2.10"},"listen":true}}),
                None,
                Some(true),
            ),
        ] {
            let command = serde_json::from_value::<UiAction>(payload)
                .unwrap()
                .into_command()
                .unwrap();
            let Command::EditRoomRule { rule } = command else {
                panic!("expected scoped rule edit");
            };
            assert_eq!(rule.send, expected_send);
            assert_eq!(rule.listen, expected_listen);
            let encoded = serde_json::to_value(rule).unwrap();
            if expected_send.is_none() {
                assert!(encoded.get("send").is_none());
            }
            if expected_listen.is_none() {
                assert!(encoded.get("listen").is_none());
            }
        }
    }

    #[test]
    fn room_actions_target_public_commands() {
        let saved: UiAction = serde_json::from_value(serde_json::json!({
            "type": "create_saved_room",
            "profilePath": "cards/studio.json",
            "bind": "0.0.0.0:47101"
        }))
        .unwrap();
        match saved.into_command().unwrap() {
            Command::CreateSavedRoom { profile_path, bind } => {
                assert_eq!(profile_path, PathBuf::from("cards/studio.json"));
                assert!(bind.ip().is_unspecified());
            }
            other => panic!("expected create_saved_room, got {other:?}"),
        }
        assert!(
            serde_json::from_value::<UiAction>(serde_json::json!({
                "type": "join_saved_room",
                "profilePath": "cards/studio.json",
                "mode": "Manual"
            }))
            .is_err()
        );
        let join: UiAction = serde_json::from_value(serde_json::json!({
            "type": "join_saved_room",
            "profilePath": "cards/studio.json",
            "mode": "automatic"
        }))
        .unwrap();
        assert!(matches!(
            join.into_command().unwrap(),
            Command::JoinSavedRoom {
                mode: JoinMode::Automatic,
                ..
            }
        ));
        assert!(matches!(
            UiAction::LeaveRoom {}.into_command().unwrap(),
            Command::LeaveRoom
        ));
        assert!(matches!(
            UiAction::CancelRoom {}.into_command().unwrap(),
            Command::CancelRoom
        ));
        assert!(matches!(
            UiAction::ReloadRoomRules {}.into_command().unwrap(),
            Command::ReloadRoomRules
        ));
        assert!(matches!(
            UiAction::RefreshRooms {}.into_command().unwrap(),
            Command::RefreshRooms
        ));
        for (value, expected) in [
            (
                serde_json::json!({"type":"set_global_defaults", "send":false}),
                Command::SetGlobalDefaults {
                    send: Some(false),
                    listen: None,
                },
            ),
            (
                serde_json::json!({"type":"set_global_defaults", "listen":true}),
                Command::SetGlobalDefaults {
                    send: None,
                    listen: Some(true),
                },
            ),
        ] {
            let action: UiAction = serde_json::from_value(value).unwrap();
            assert_eq!(action.into_command().unwrap(), expected);
        }
        let global: UiAction = serde_json::from_value(serde_json::json!({
            "type": "replace_global_rules",
            "rules": {
                "schema": 1,
                "rules": [{ "match": "all", "send": true, "listen": false }]
            }
        }))
        .unwrap();
        match global.into_command().unwrap() {
            Command::ReplaceGlobalRules { rules } => assert_eq!(rules, GlobalRules::baseline()),
            other => panic!("expected replace_global_rules, got {other:?}"),
        }
        let mix: UiAction = serde_json::from_value(serde_json::json!({
            "type": "set_room_source_mix",
            "condition": { "name": "Ada" },
            "sourceKind": "system",
            "gainDb": -6.0,
            "pan": 0.0,
            "mute": false,
            "solo": false
        }))
        .unwrap();
        match mix.into_command().unwrap() {
            Command::SetRoomSourceMix {
                condition,
                source_kind,
                playback,
            } => {
                assert!(matches!(
                    condition,
                    RuleMatch::Condition { ip: None, name: Some(name) } if name == "Ada"
                ));
                assert_eq!(source_kind, SourceKind::System);
                assert_eq!(playback.gain_db, Some(-6.0));
            }
            other => panic!("expected set_room_source_mix, got {other:?}"),
        }
        assert!(
            serde_json::from_value::<UiAction>(serde_json::json!({
                "type": "set_room_source_mix",
                "condition": { "name": "Ada" },
                "sourceId": "not-a-kind",
                "gainDb": 0,
                "pan": 0,
                "mute": false,
                "solo": false
            }))
            .is_err()
        );
    }

    #[test]
    fn extra_fields_cannot_silently_change_action_scope() {
        let mut action = serde_json::json!({
            "type": "set_room_source_mix", "condition": {"name": "Ada"},
            "sourceKind": "system", "gainDb": -6, "pan": 0,
            "mute": false, "solo": false
        });
        assert!(serde_json::from_value::<UiAction>(action.clone()).is_ok());
        action["sourceId"] = serde_json::json!("one-specific-stream");
        assert!(serde_json::from_value::<UiAction>(action).is_err());
        for kind in [
            "leave_group",
            "refresh_devices",
            "leave_room",
            "cancel_room",
            "reload_room_rules",
            "refresh_rooms",
        ] {
            assert!(serde_json::from_value::<UiAction>(serde_json::json!({"type": kind})).is_ok());
            assert!(
                serde_json::from_value::<UiAction>(
                    serde_json::json!({"type": kind, "generation": "stale"})
                )
                .is_err(),
                "{kind}"
            );
        }
    }
}
