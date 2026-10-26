//! Saved intent, UDP contact and independent native audio owners.
//! The supervisor handles preferences; capture, media and output own their
//! separate event-driven paths.

pub const BUILD_ID: &str = env!("RLB_ENGINE_BUILD_ID");

pub mod audio;
pub mod cli;
pub mod cli_grammar;
pub mod management;
pub mod network;
pub mod login_startup;
pub mod firewall;
mod command;
mod config;
mod controller;
pub mod discovery;
pub mod output_routing;
pub mod room_session;
pub mod rooms;
pub mod routing;
mod snapshot;

pub use command::Command;
pub use config::{
    LocalSource, MixIntent, ParticipantConfig, RoomPreferences, RoomStartup, SavedContact,
    SavedGroup, SavedMember, SourceKind, SourceMixOverride, ResolvedPath, create_config_file, load_config,
};
pub use controller::{
    ParticipantHandle, RoomControlState, RunningParticipant, start, start_with_room,
};
pub use output_routing::{
    OutputTarget, SourceOutputTargets, default_output_targets, validate_output_targets,
};
pub use snapshot::{
    AudioEffect, AudioEndpoint, CaptureDetails, ChannelLayoutState, CommandOutcome,
    DirectoryStatus, GroupContact, LocalSourceState, MemberContact, MemberState, OutputDeviceState,
    OutputState, ParticipantSnapshot, PcmEncodingState, PcmFormatState, PublicationState,
    RemoteMixState, RemoteSourceState, ReturnLoopState, SourceOutputError, SourcePlayoutState,
};
