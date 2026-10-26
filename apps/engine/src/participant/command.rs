//! Absolute participant commands. Each value states the desired result.

use std::net::SocketAddr;
use std::path::PathBuf;

use rlb_model::{MemberId, RoomId, SourceId};
use serde::Deserialize;

use super::config::SourceKind;
use super::output_routing::OutputTarget;
use super::rooms::{GlobalRules, JoinMode, MixPatch, RoomProfile, RoomTarget, Rule, RuleMatch};
use crate::participant::snapshot::DirectoryStatus;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    SetNetwork {
        policy: super::network::NetworkPolicy,
        bind: Option<SocketAddr>,
    },
    CreateRoom {
        profile: RoomProfile,
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
        generation: u64,
        instance_id: RoomId,
    },
    LeaveRoom,
    CancelRoom,
    ReloadRoomRules,
    StartVirtualReturn,
    StopVirtualReturn,
    EditRoomRule {
        rule: Rule,
    },
    EditGlobalRule {
        rule: Rule,
    },
    SetRoomSourceMix {
        condition: RuleMatch,
        source_kind: SourceKind,
        playback: MixPatch,
    },
    ReplaceGlobalRules {
        rules: GlobalRules,
    },
    /// Patch global rule 1 in place; later rules keep their priority.
    SetGlobalDefaults {
        send: Option<bool>,
        listen: Option<bool>,
    },
    RefreshRooms,
    ReadRoomCard { path: PathBuf },
    SaveRoomCard { path: PathBuf, target: RoomTarget, expected_revision: String },
    DeleteRoomCard { path: PathBuf, expected_revision: String },
    ExportSettings { path: PathBuf },
    CheckPendingSettings,
    SetName {
        name: String,
    },
    JoinGroup {
        name: String,
    },
    LeaveGroup,
    SetPeerIntent {
        member_id: MemberId,
        send: bool,
        receive: bool,
    },
    SetAutoReconnect {
        enabled: bool,
    },
    SetLoginStartup {
        enabled: bool,
    },
    RefreshLoginStartup,
    AllowLanFirewall,
    RefreshLanFirewall,
    RemoveOwnLanFirewall,
    SetLocalSource {
        source_id: SourceId,
        publish: Option<bool>,
        monitor: Option<bool>,
    },
    SetLocalOutputs {
        source_id: SourceId,
        targets: Vec<OutputTarget>,
    },
    SelectSourceEndpoint {
        source_id: SourceId,
        endpoint_id: Option<String>,
    },
    SelectOutput {
        endpoint_id: Option<String>,
    },
    SetSourceMix {
        member_id: Option<MemberId>,
        source_id: SourceId,
        gain_db: f32,
        pan: f32,
        mute: bool,
        solo: bool,
    },
    SetMaster {
        gain_db: f32,
        limiter_enabled: bool,
    },
    RefreshDevices,
    UpsertContact {
        member_id: MemberId,
        address: SocketAddr,
        name: Option<String>,
    },
    Reconnect,
    DirectoryRemove {
        group_generation: u64,
        member_id: MemberId,
    },
    DirectoryUpsertContact {
        group_generation: u64,
        member_id: MemberId,
        address: SocketAddr,
        name: Option<String>,
    },
    DirectoryStatus {
        group_generation: u64,
        status: DirectoryStatus,
    },
    Quit,
}

impl Command {
    /// User room operations that retire the current room document on acceptance.
    pub(crate) fn retires_room_document(&self) -> bool {
        matches!(
            self,
            Self::CreateRoom { .. }
                | Self::CreateSavedRoom { .. }
                | Self::JoinSavedRoom { .. }
                | Self::JoinUnknownRoom { .. }
                | Self::LeaveRoom
                | Self::CancelRoom
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::SetNetwork { .. } => "set_network",
            Self::CreateRoom { .. } => "create_room",
            Self::CreateSavedRoom { .. } => "create_saved_room",
            Self::JoinSavedRoom { .. } => "join_saved_room",
            Self::JoinUnknownRoom { .. } => "join_unknown_room",
            Self::ChooseRoom { .. } => "choose_room",
            Self::LeaveRoom => "leave_room",
            Self::CancelRoom => "cancel_room",
            Self::ReloadRoomRules => "reload_room_rules",
            Self::StartVirtualReturn => "start_virtual_return",
            Self::StopVirtualReturn => "stop_virtual_return",
            Self::EditRoomRule { .. } => "edit_room_rule",
            Self::EditGlobalRule { .. } => "edit_global_rule",
            Self::SetRoomSourceMix { .. } => "set_room_source_mix",
            Self::ReplaceGlobalRules { .. } => "replace_global_rules",
            Self::SetGlobalDefaults { .. } => "set_global_defaults",
            Self::RefreshRooms => "refresh_rooms",
            Self::ReadRoomCard { .. } => "read_room_card",
            Self::SaveRoomCard { .. } => "save_room_card",
            Self::DeleteRoomCard { .. } => "delete_room_card",
            Self::ExportSettings { .. } => "export_settings",
            Self::CheckPendingSettings => "check_pending_settings",
            Self::SetName { .. } => "set_name",
            Self::JoinGroup { .. } => "join_group",
            Self::LeaveGroup => "leave_group",
            Self::SetPeerIntent { .. } => "set_peer_intent",
            Self::SetAutoReconnect { .. } => "set_auto_reconnect",
            Self::SetLoginStartup { .. } => "set_login_startup",
            Self::RefreshLoginStartup => "refresh_login_startup",
            Self::AllowLanFirewall => "allow_lan_firewall",
            Self::RefreshLanFirewall => "refresh_lan_firewall",
            Self::RemoveOwnLanFirewall => "remove_own_lan_firewall",
            Self::SetLocalSource { .. } => "set_local_source",
            Self::SetLocalOutputs { .. } => "set_local_outputs",
            Self::SelectSourceEndpoint { .. } => "select_source_endpoint",
            Self::SelectOutput { .. } => "select_output",
            Self::SetSourceMix { .. } => "set_source_mix",
            Self::SetMaster { .. } => "set_master",
            Self::RefreshDevices => "refresh_devices",
            Self::UpsertContact { .. } => "upsert_contact",
            Self::Reconnect => "reconnect",
            Self::DirectoryRemove { .. } => "directory_remove",
            Self::DirectoryUpsertContact { .. } => "directory_upsert_contact",
            Self::DirectoryStatus { .. } => "directory_status",
            Self::Quit => "quit",
        }
    }
}

impl Command {
    /// Explicit external control contract. Internal directory/legacy messages
    /// remain usable by their owners without becoming supported JSON commands.
    pub fn is_user_command(&self) -> bool {
        matches!(self,
            Self::SetNetwork { .. }
            | Self::CreateRoom { .. } | Self::CreateSavedRoom { .. }
            | Self::JoinSavedRoom { .. } | Self::JoinUnknownRoom { .. }
            | Self::ChooseRoom { .. } | Self::LeaveRoom | Self::CancelRoom
            | Self::ReloadRoomRules | Self::StartVirtualReturn | Self::StopVirtualReturn
            | Self::EditRoomRule { .. } | Self::EditGlobalRule { .. }
            | Self::SetRoomSourceMix { .. } | Self::ReplaceGlobalRules { .. }
            | Self::SetGlobalDefaults { .. } | Self::RefreshRooms
            | Self::ReadRoomCard { .. } | Self::SaveRoomCard { .. }
            | Self::DeleteRoomCard { .. } | Self::ExportSettings { .. }
            | Self::CheckPendingSettings | Self::SetName { .. }
            | Self::SetAutoReconnect { .. } | Self::SetLoginStartup { .. }
            | Self::RefreshLoginStartup | Self::AllowLanFirewall
            | Self::RefreshLanFirewall | Self::RemoveOwnLanFirewall
            | Self::SetLocalSource { .. } | Self::SetLocalOutputs { .. }
            | Self::SelectSourceEndpoint { .. } | Self::SelectOutput { .. }
            | Self::SetSourceMix { member_id: None, .. }
            | Self::SetMaster { .. } | Self::RefreshDevices | Self::Reconnect | Self::Quit
        )
    }
}
