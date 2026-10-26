//! Public room-session commands, snapshots, and assembler outputs.
//!
//! A joined phase means the contact directory is current. It is not an audio
//! admission result. Passwords stay in the worker and in the room files.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use rlb_contact::{MediaSocket, ReturnState};
use rlb_model::{MemberId, RoomId, RunId};
use serde::Serialize;

use super::super::config::SourceKind;
use super::super::discovery::{AdvertisedRoom, DiscoveryConfig, InterfaceSelector};
use super::super::rooms::{
    DiscoveredRoom, GlobalRules, JoinMode, MixPatch, RoomPlaybackRule, RoomTarget, Rule, RuleMatch,
};

/// Caller-owned identity, media endpoint, and file locations.
///
/// `media_bind` is the UDP socket the caller already bound. This module does not
/// open that socket and does not choose its port. The display name is fixed
/// during an active registration. The supervisor may update the next-entry
/// name after leaving, while retaining the local rule owner.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub display_name: String,
    pub media_bind: SocketAddr,
    pub requested_bind: SocketAddr,
    pub media_addresses: Vec<SocketAddr>,
    pub interfaces: Vec<crate::participant::network::NetworkInterface>,
    pub local_scope: u32,
    pub global_rules_path: PathBuf,
    /// Disabling both address families skips discovery while keeping explicit
    /// address entry available. `with_discovery` replaces these defaults.
    pub discovery: DiscoveryConfig,
    pub network: crate::participant::network::NetworkPolicy,
    pub io_timeout: Duration,
    pub recovery_interval: Duration,
}

impl SessionConfig {
    pub fn try_new(
        member_id: MemberId,
        run_id: RunId,
        display_name: impl Into<String>,
        media: MediaSocket,
        global_rules_path: PathBuf,
    ) -> Result<Self, String> {
        let mut bound = SocketAddr::new(media.ip(), media.port());
        if let SocketAddr::V6(address) = &mut bound {
            address.set_scope_id(media.scope_id());
        }
        Self::from_bound(member_id, run_id, display_name, bound, global_rules_path)
    }

    /// Accept the actual bound socket, including a wildcard address. Resolving
    /// an advertised unicast address belongs to entry, not local preferences.
    pub fn from_bound(
        member_id: MemberId,
        run_id: RunId,
        display_name: impl Into<String>,
        media_bind: SocketAddr,
        global_rules_path: PathBuf,
    ) -> Result<Self, String> {
        if media_bind.port() == 0 {
            return Err("The actual bound media port must be nonzero.".to_owned());
        }
        Self::for_preferences(member_id, run_id, display_name, media_bind, global_rules_path)
    }

    /// Local rules remain usable while transport is waiting. Port zero records
    /// that absence explicitly; entry never advertises it as a real endpoint.
    pub(crate) fn for_preferences(
        member_id: MemberId,
        run_id: RunId,
        display_name: impl Into<String>,
        media_bind: SocketAddr,
        global_rules_path: PathBuf,
    ) -> Result<Self, String> {
        let display_name = display_name.into();
        let count = display_name.chars().count();
        if count == 0 || count > 80 || display_name.chars().any(char::is_control) {
            return Err(
                "The display name must contain 1 to 80 characters and no control characters."
                    .to_owned(),
            );
        }
        let discovery = if media_bind.ip().is_loopback() {
            DiscoveryConfig {
                enable_interfaces: Vec::new(),
                disable_interfaces: vec![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6],
            }
        } else if media_bind.is_ipv4() {
            DiscoveryConfig {
                enable_interfaces: vec![InterfaceSelector::Ipv4],
                disable_interfaces: vec![InterfaceSelector::Ipv6],
            }
        } else {
            DiscoveryConfig {
                enable_interfaces: vec![InterfaceSelector::Ipv6],
                disable_interfaces: vec![InterfaceSelector::Ipv4],
            }
        };
        Ok(Self {
            member_id,
            run_id,
            display_name,
            media_bind,
            requested_bind: media_bind,
            media_addresses: if media_bind.port() == 0 { Vec::new() } else { vec![media_bind] },
            interfaces: Vec::new(),
            local_scope: 0,
            global_rules_path,
            discovery,
            network: Default::default(),
            io_timeout: Duration::from_secs(2),
            recovery_interval: Duration::from_secs(2),
        })
    }

    pub fn with_local_scope(mut self, local_scope: u32) -> Self {
        self.local_scope = local_scope;
        self
    }

    pub fn with_discovery(mut self, discovery: DiscoveryConfig) -> Self {
        self.discovery = discovery;
        self
    }

    pub(crate) fn with_network(mut self, network: crate::participant::network::NetworkPolicy) -> Self {
        self.discovery = network.for_discovery(self.requested_bind).unwrap_or_else(|_| DiscoveryConfig {
            enable_interfaces: Vec::new(),
            disable_interfaces: vec![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6],
        });
        self.network = network;
        self
    }

    pub(super) fn discovery_enabled(&self) -> bool {
        !self.discovery.enable_interfaces.is_empty() || ![InterfaceSelector::Ipv4, InterfaceSelector::Ipv6]
            .iter()
            .all(|family| self.discovery.disable_interfaces.contains(family))
    }

    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Result<Self, String> {
        if io_timeout.is_zero() {
            return Err("The contact I/O timeout must be greater than zero.".to_owned());
        }
        self.io_timeout = io_timeout;
        Ok(self)
    }

    pub fn with_recovery_interval(mut self, recovery_interval: Duration) -> Result<Self, String> {
        if recovery_interval.is_zero() {
            return Err("The contact recovery interval must be greater than zero.".to_owned());
        }
        self.recovery_interval = recovery_interval;
        Ok(self)
    }
}

/// Work the caller enqueues. The worker performs every blocking step.
#[derive(Clone)]
pub enum SessionCommand {
    UpdateTransport { bind: SocketAddr, requested_bind: SocketAddr, addresses: Vec<SocketAddr>, interfaces: Vec<crate::participant::network::NetworkInterface>, network: crate::participant::network::NetworkPolicy },
    /// Start a local contact service from a saved card. `bind` is the listening
    /// socket. The card's saved address is not applied to the operating system.
    Create {
        profile_path: PathBuf,
        bind: SocketAddr,
    },
    JoinSaved {
        mode: JoinMode,
        profile_path: PathBuf,
    },
    JoinUnknown {
        mode: JoinMode,
        target: RoomTarget,
        card_path: PathBuf,
        rules: Vec<Rule>,
    },
    /// Select one instance while the session is waiting in [`Phase::Choosing`].
    Choose {
        generation: u64,
        instance_id: RoomId,
    },
    Leave,
    Cancel,
    Reload,
    StartVirtualReturn,
    StopVirtualReturn,
    /// Apply an explicit name/IP and optional source condition, then persist
    /// the current room rules. The source selector is carried unchanged.
    EditRoomRule {
        rule: Rule,
    },
    EditGlobalRule {
        rule: Rule,
    },
    ReplaceGlobalRules {
        rules: GlobalRules,
    },
    SetGlobalDefaults {
        send: Option<bool>,
        listen: Option<bool>,
    },
    SetDisplayName {
        name: String,
    },
    SetRoomSourceMix {
        condition: RuleMatch,
        source_kind: SourceKind,
        playback: MixPatch,
    },
    UpdateCardTarget {
        path: PathBuf,
        target: RoomTarget,
        expected_revision: String,
        reply: std::sync::mpsc::SyncSender<Result<crate::participant::management::CardDocument, String>>,
    },
    SnapshotSettings {
        reply: std::sync::mpsc::SyncSender<Result<crate::participant::management::ActiveRoomSettings, String>>,
    },
    Stop,
}

/// Where the room session is in its contact lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Resolving,
    Choosing,
    Connecting,
    Joined,
    ContactUnavailable,
    Leaving,
    Stopped,
    Failed,
}

/// A member media destination for this machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedEndpoint {
    Ready(SocketAddr),
    /// IPv6 link-local. `local_scope` on the session was zero, so this machine
    /// has not named an interface. The stored sender scope is not used here.
    LocalScopeRequired {
        ip: Ipv6Addr,
        port: u16,
    },
}

/// One directory member. `visible` is contact visibility, separate from the
/// retained identity and path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MemberView {
    pub member_id: MemberId,
    pub run_id: RunId,
    pub display_name: String,
    pub endpoint: ProjectedEndpoint,
    pub alternatives: Vec<ProjectedEndpoint>,
    pub visible: bool,
}

/// Entry failure category; callers need no operator-message parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryFailureKind {
    Unavailable,
    NameMismatch,
    PasswordRejected,
    ProtocolMismatch,
    ContactRejected,
    Configuration,
    ListenerFailed,
    DiscoveryFailed,
    AddressRequired,
    SelectionExpired,
}

/// One authoritative persistence outcome for a local rule file.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuleSaveState {
    #[default]
    Unloaded,
    Saved,
    Pending,
    ExternalChange {
        detail: String,
    },
    WriteFailed {
        detail: String,
    },
}

impl RuleSaveState {
    pub fn unsaved(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::ExternalChange { .. } | Self::WriteFailed { .. }
        )
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::ExternalChange { detail } | Self::WriteFailed { detail } => Some(detail),
            _ => None,
        }
    }

    pub(super) fn failed(failure: super::super::rooms::SaveFailure) -> Self {
        use super::super::rooms::SaveFailureKind;
        match failure.kind {
            SaveFailureKind::ExternalChange => Self::ExternalChange {
                detail: failure.detail,
            },
            SaveFailureKind::WriteFailed => Self::WriteFailed {
                detail: failure.detail,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RuleFileStatus {
    pub path: Option<PathBuf>,
    #[serde(flatten)]
    pub outcome: RuleSaveState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RuleFilesState {
    pub room: RuleFileStatus,
    pub global: RuleFileStatus,
}

/// Coalesced view. The newest snapshot replaces the previous one.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub generation: u64,
    pub phase: Phase,
    pub room_name: Option<String>,
    pub instance_id: Option<RoomId>,
    pub local_service: Option<SocketAddr>,
    pub profile_path: Option<PathBuf>,
    pub discovered_rooms: Vec<DiscoveredRoom>,
    pub choices: Vec<DiscoveredRoom>,
    pub selected_endpoints: Vec<SocketAddr>,
    pub members: Vec<MemberView>,
    pub global_rules: Option<GlobalRules>,
    pub rules: Vec<Rule>,
    pub playback_rules: Vec<RoomPlaybackRule>,
    pub rules_unsaved: bool,
    pub rules_save_error: Option<String>,
    pub rule_files: RuleFilesState,
    pub saved_address: Option<IpAddr>,
    pub card_persisted: Option<bool>,
    pub pending_target_change: bool,
    pub failure: Option<String>,
    pub failure_kind: Option<EntryFailureKind>,
    pub output_overflow: bool,
    /// Advances whenever a terminal output had to be discarded. Consumers
    /// retire old pair intervals on every new revision, including later bursts.
    pub output_loss_revision: u64,
    pub discovery_failure: Option<String>,
    pub local_advertisement: Option<AdvertisedRoom>,
    pub virtual_return: Option<ReturnState>,
    pub operation_notice: Option<String>,
}

impl SessionSnapshot {
    pub(crate) fn idle() -> Self {
        Self {
            generation: 0,
            phase: Phase::Idle,
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
}

/// Ordered notice for the later audio assembler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOutput {
    pub generation: u64,
    pub body: OutputBody,
}

/// Contact and session facts. None of these variants pauses a direct peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputBody {
    MemberVisible(MemberView),
    ExplicitDeparture {
        member_id: MemberId,
        run_id: RunId,
        display_name: String,
    },
    ContactVisibility {
        member_id: MemberId,
        run_id: RunId,
        display_name: String,
        cause: rlb_contact::ContactLossCause,
    },
    DirectoryLost,
    LocalSessionEnded,
    ChoiceRequired {
        rooms: Vec<DiscoveredRoom>,
    },
    AddressRequired,
    Failure {
        message: String,
    },
    /// Entry status is authoritative in the snapshot, separate from a rule or
    /// card operation error that may need to remain visible after joining.
    EntryFailed {
        kind: EntryFailureKind,
        message: String,
    },
    CardSaved {
        path: PathBuf,
    },
    CardUnsaved {
        path: PathBuf,
        message: String,
    },
    Reloaded {
        pending_target_change: bool,
    },
    /// Active rules changed even when persistence needs user attention.
    RulesChanged {
        persisted: bool,
    },
}

impl SessionOutput {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self.body,
            OutputBody::ExplicitDeparture { .. }
                | OutputBody::LocalSessionEnded
                | OutputBody::DirectoryLost
                | OutputBody::AddressRequired
                | OutputBody::Failure { .. }
                | OutputBody::EntryFailed { .. }
                | OutputBody::CardUnsaved { .. }
                | OutputBody::ChoiceRequired { .. }
        )
    }
}

/// Why `submit` did not enqueue the command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    Busy,
    Stopped,
    Invalid(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter
                .write_str("The room session command queue is full. The command was not accepted."),
            Self::Stopped => formatter.write_str("The room session has stopped."),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SessionError {}
