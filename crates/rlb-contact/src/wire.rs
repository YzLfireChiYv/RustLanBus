//! Closed contact-version-2 JSON messages inside RLBCT1 length framing.
//!
//! Version 2 is a bounded address introduction exchange. It has no registry authority command,
//! admission token, or media payload.

use std::fmt;

use rlb_model::{MemberId, RoomId, RunId, SourceId};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::framing::{FrameError, decode_json_frame, encode_json_frame};
use crate::media::{MediaSocket, normalize_media_alternates};

/// Longest room or display name, in Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 80;
/// Longest room password, in Unicode scalar values.
pub const MAX_PASSWORD_CHARS: usize = 128;
/// Contact protocol version carried in every message.
pub const CONTACT_PROTOCOL_VERSION: u32 = 2;

/// Protocol marker that accepts and emits JSON number `2`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContactVersion;

impl Serialize for ContactVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(CONTACT_PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ContactVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionVisitor;

        impl Visitor<'_> for VersionVisitor {
            type Value = ContactVersion;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("contact protocol version number 2")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == u64::from(CONTACT_PROTOCOL_VERSION) {
                    Ok(ContactVersion)
                } else {
                    Err(E::custom("unsupported protocol version"))
                }
            }
        }

        deserializer.deserialize_u64(VersionVisitor)
    }
}

/// Closed error code for a rejected contact request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactCode {
    /// The frame was not a contact message.
    Malformed,
    /// `v` was a number other than the current contact protocol version.
    UnsupportedVersion,
    /// The join named a different room.
    RoomNameMismatch,
    /// The join password differed from the configured password.
    PasswordRejected,
    /// The display name was empty, too long, or contained a control character.
    InvalidName,
    /// The media address or UDP port cannot be published.
    InvalidMedia,
    /// This TCP connection already registered a different member.
    AlreadyJoined,
    /// Snapshot or leave arrived before a registration.
    NotJoined,
    /// The room already holds [`crate::MAX_MEMBERS`] registrations.
    RoomFull,
    /// A newer TCP connection now owns this member.
    Superseded,
    /// The service is stopping and will not take a new registration.
    ShuttingDown,
}

impl ContactCode {
    /// Fixed text for operators and clients. The text never includes a password.
    #[must_use]
    pub const fn public_message(self) -> &'static str {
        match self {
            Self::Malformed => "The contact message is not valid.",
            Self::UnsupportedVersion => "The contact protocol version is unsupported.",
            Self::RoomNameMismatch => "The room name does not match this contact service.",
            Self::PasswordRejected => "The room password was rejected.",
            Self::InvalidName => {
                "The display name must contain 1 to 80 characters and no control characters."
            }
            Self::InvalidMedia => {
                "Media candidates must be at most 16 unicast endpoints using the same UDP port from 1 to 65535."
            }
            Self::AlreadyJoined => "This connection already belongs to another member.",
            Self::NotJoined => "This connection has no current member registration.",
            Self::RoomFull => "The room already holds the maximum number of members.",
            Self::Superseded => "This connection was replaced by a newer registration.",
            Self::ShuttingDown => "The contact service is stopping.",
        }
    }
}

/// Why a contact-loss notice was produced.
///
/// Each cause describes directory visibility. Healthy direct media continues.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactLossCause {
    /// The member's control TCP connection closed.
    ControlDisconnect,
    /// The member sent no contact frame within the service timeout.
    Timeout,
    /// The contact service is stopping.
    ServiceStop,
    /// A later snapshot no longer lists the member.
    AbsentFromSnapshot,
}

/// Public facts for one running room. The password is absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomInfo {
    /// Contact protocol version. This implementation reports 2.
    pub protocol_version: u32,
    /// Display name configured at service start.
    pub room_name: String,
    /// Run-lifetime instance identity. A new service start generates a new value.
    ///
    /// This is [`RoomId`] so discovery can use the same 128-bit value GB03 stores
    /// on `DiscoveredRoom.instance_id`. It deduplicates advertisements of one
    /// running service. It is not a saved room key and it grants no membership.
    pub instance_id: RoomId,
    /// TCP port the service actually bound.
    pub port: u16,
}

/// One member's current contact record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberContact {
    /// Stable member identity supplied by the client.
    pub member_id: MemberId,
    /// Client process run supplied by the client.
    pub run_id: RunId,
    /// Display name supplied by the client, stored exactly.
    pub display_name: String,
    /// Direct-media UDP socket.
    pub media: MediaSocket,
    /// Ordered alternatives for the same UDP port; the preferred media remains first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_alternates: Vec<MediaSocket>,
}

impl MemberContact {
    /// Preferred endpoint followed by the normalized ordered alternatives.
    pub fn media_candidates(&self) -> impl Iterator<Item = MediaSocket> + '_ {
        std::iter::once(self.media).chain(self.media_alternates.iter().copied())
    }
}

/// Operation on the requesting member's optional virtual return participant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnAction {
    /// Create or resume this member's return participant.
    Start,
    /// Read current return correspondence.
    Status,
    /// Remove this member's return participant.
    Stop,
}

/// Correspondence between a returned publication and its original input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReturnMapping {
    /// Returned source identity.
    pub source_id: SourceId,
    /// Returned publication epoch.
    pub epoch: u64,
    /// Original member.
    pub upstream_member_id: MemberId,
    /// Original member's current run.
    pub upstream_run_id: RunId,
    /// Original source.
    pub upstream_source_id: SourceId,
    /// Original publication epoch.
    pub upstream_epoch: u64,
    /// First original frame in this correspondence.
    pub input_first_frame: u64,
    /// Corresponding returned frame.
    pub output_first_frame: u64,
}

/// Current optional return participant and its publications.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReturnState {
    /// Registered virtual participant, absent while stopped or unavailable.
    pub member: Option<MemberContact>,
    /// Current source correspondences.
    pub mappings: Vec<ReturnMapping>,
    /// Visible operation failure, without room credentials.
    pub error: Option<String>,
}

/// Parameters of one join.
#[derive(Clone, Eq, PartialEq)]
pub struct JoinParams {
    /// Room name the caller expects.
    pub room_name: String,
    /// Room password. An empty string matches a room configured without a password.
    pub password: String,
    /// Stable member identity.
    pub member_id: MemberId,
    /// Current client run.
    pub run_id: RunId,
    /// Display name. Duplicate names remain distinct through [`Self::member_id`].
    pub display_name: String,
    /// UDP port for direct media.
    pub media_port: u16,
    /// Pinned media IP. `None` uses the IP observed on the accepted TCP connection.
    pub media_ip: Option<std::net::IpAddr>,
    /// Sender-local scope for [`Self::media_ip`] when that address is IPv6.
    ///
    /// Zero with no explicit IP. Receiving members must select their own local
    /// interface scope when constructing a link-local media destination.
    pub media_scope_id: u32,
    /// Other addresses of this member's same UDP port, in preference order.
    pub media_alternates: Vec<MediaSocket>,
}

impl fmt::Debug for JoinParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinParams")
            .field("room_name", &self.room_name)
            .field("password", &"redacted")
            .field("member_id", &self.member_id)
            .field("run_id", &self.run_id)
            .field("display_name", &self.display_name)
            .field("media_port", &self.media_port)
            .field("media_ip", &self.media_ip)
            .field("media_scope_id", &self.media_scope_id)
            .field("media_alternates", &self.media_alternates)
            .finish()
    }
}

/// Small directory change pushed to current members.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresenceChange {
    /// A member is visible, or an existing member replaced its contact record.
    Visible {
        /// Current contact record.
        member: MemberContact,
    },
    /// The member sent `leave`. The participant adapter may end that member's direct media.
    ExplicitLeave {
        /// Stable member identity.
        member_id: MemberId,
        /// Run named in the registration that left.
        run_id: RunId,
        /// Display name stored at departure.
        display_name: String,
    },
    /// The directory lost sight of the member. Direct media stays up.
    ContactOnly {
        /// Stable member identity.
        member_id: MemberId,
        /// Run named in the registration that became hidden.
        run_id: RunId,
        /// Display name stored when the registration was current.
        display_name: String,
        /// Why the directory lost sight of the member.
        cause: ContactLossCause,
    },
}

/// The only service-stop effect. There is no audio-stop variant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStopEffect {
    /// Control sockets are closing. Direct media stays up.
    ContactVisibility,
}

/// Message a client sends after the preface.
#[derive(Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    /// Request an operation for this connection's registered member.
    Return {
        /// Protocol version.
        v: ContactVersion,
        /// Requested operation.
        action: ReturnAction,
    },
    /// Read [`RoomInfo`] without registering.
    Inspect {
        /// Protocol version.
        v: ContactVersion,
    },
    /// Register or replace this member.
    Join {
        /// Protocol version.
        v: ContactVersion,
        /// Expected room name.
        room_name: String,
        /// Room password.
        password: String,
        /// Stable member identity.
        member_id: MemberId,
        /// Current client run.
        run_id: RunId,
        /// Display name.
        display_name: String,
        /// UDP port for direct media.
        media_port: u16,
        /// Optional pinned media IP.
        #[serde(default)]
        media_ip: Option<std::net::IpAddr>,
        /// Scope for an explicit IPv6 media IP.
        #[serde(default)]
        media_scope_id: u32,
        /// Ordered alternatives for the same UDP port.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media_alternates: Vec<MediaSocket>,
    },
    /// Reset the service's silence timer.
    Keepalive {
        /// Protocol version.
        v: ContactVersion,
    },
    /// Ask for the complete current directory.
    Snapshot {
        /// Protocol version.
        v: ContactVersion,
    },
    /// Leave the directory. This is the explicit departure.
    Leave {
        /// Protocol version.
        v: ContactVersion,
    },
}

impl fmt::Debug for ClientMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Return { v, action } => formatter
                .debug_struct("Return")
                .field("v", v)
                .field("action", action)
                .finish(),
            Self::Inspect { v } => formatter.debug_struct("Inspect").field("v", v).finish(),
            Self::Join {
                v,
                room_name,
                member_id,
                run_id,
                display_name,
                media_port,
                media_ip,
                media_scope_id,
                media_alternates,
                ..
            } => formatter
                .debug_struct("Join")
                .field("v", v)
                .field("room_name", room_name)
                .field("password", &"redacted")
                .field("member_id", member_id)
                .field("run_id", run_id)
                .field("display_name", display_name)
                .field("media_port", media_port)
                .field("media_ip", media_ip)
                .field("media_scope_id", media_scope_id)
                .field("media_alternates", media_alternates)
                .finish(),
            Self::Keepalive { v } => formatter.debug_struct("Keepalive").field("v", v).finish(),
            Self::Snapshot { v } => formatter.debug_struct("Snapshot").field("v", v).finish(),
            Self::Leave { v } => formatter.debug_struct("Leave").field("v", v).finish(),
        }
    }
}

impl ClientMessage {
    pub(crate) fn join(params: &JoinParams) -> Self {
        Self::Join {
            v: ContactVersion,
            room_name: params.room_name.clone(),
            password: params.password.clone(),
            member_id: params.member_id,
            run_id: params.run_id,
            display_name: params.display_name.clone(),
            media_port: params.media_port,
            media_ip: params.media_ip,
            media_scope_id: params.media_scope_id,
            media_alternates: params.media_alternates.clone(),
        }
    }
}

/// Message the service sends after its preface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    /// Answer to a return operation.
    Return {
        /// Protocol version.
        v: ContactVersion,
        /// Current state for the requesting member.
        state: ReturnState,
    },
    /// Answer to `inspect`.
    RoomInfo {
        /// Protocol version.
        v: ContactVersion,
        /// Display room name.
        room_name: String,
        /// Run-lifetime instance identity.
        instance_id: RoomId,
        /// Actual bound TCP port.
        port: u16,
    },
    /// Answer to a successful `join`, including every current member.
    Joined {
        /// Protocol version.
        v: ContactVersion,
        /// Display room name.
        room_name: String,
        /// Run-lifetime instance identity.
        instance_id: RoomId,
        /// Actual bound TCP port.
        port: u16,
        /// Registration created or replaced by this connection.
        you: MemberContact,
        /// Every current member, including `you`.
        members: Vec<MemberContact>,
    },
    /// Answer to `snapshot`.
    Snapshot {
        /// Protocol version.
        v: ContactVersion,
        /// Display room name.
        room_name: String,
        /// Run-lifetime instance identity.
        instance_id: RoomId,
        /// Actual bound TCP port.
        port: u16,
        /// Every current member.
        members: Vec<MemberContact>,
    },
    /// One directory change.
    Presence {
        /// Protocol version.
        v: ContactVersion,
        /// Visible member, explicit leave, or contact-only loss.
        change: PresenceChange,
    },
    /// Answer to `keepalive`.
    Ack {
        /// Protocol version.
        v: ContactVersion,
    },
    /// This connection's registration was replaced by a newer connection.
    Superseded {
        /// Protocol version.
        v: ContactVersion,
    },
    /// The service is closing every control socket.
    ServiceStop {
        /// Protocol version.
        v: ContactVersion,
        /// Visibility effect. Direct media stays up.
        effect: ServiceStopEffect,
    },
    /// The service rejected the request.
    Error {
        /// Protocol version.
        v: ContactVersion,
        /// Closed code.
        code: ContactCode,
        /// Fixed explanation without the room password.
        message: String,
    },
    /// Answer to an accepted `leave`.
    Goodbye {
        /// Protocol version.
        v: ContactVersion,
    },
}

impl ServerMessage {
    pub(crate) fn room_info(info: &RoomInfo) -> Self {
        Self::RoomInfo {
            v: ContactVersion,
            room_name: info.room_name.clone(),
            instance_id: info.instance_id,
            port: info.port,
        }
    }

    pub(crate) fn error(code: ContactCode) -> Self {
        Self::Error {
            v: ContactVersion,
            code,
            message: code.public_message().to_owned(),
        }
    }

    pub(crate) fn joined(info: &RoomInfo, you: MemberContact, members: Vec<MemberContact>) -> Self {
        Self::Joined {
            v: ContactVersion,
            room_name: info.room_name.clone(),
            instance_id: info.instance_id,
            port: info.port,
            you,
            members,
        }
    }

    pub(crate) fn snapshot(info: &RoomInfo, members: Vec<MemberContact>) -> Self {
        Self::Snapshot {
            v: ContactVersion,
            room_name: info.room_name.clone(),
            instance_id: info.instance_id,
            port: info.port,
            members,
        }
    }

    pub(crate) fn visible(member: MemberContact) -> Self {
        Self::Presence {
            v: ContactVersion,
            change: PresenceChange::Visible { member },
        }
    }

    pub(crate) fn explicit_leave(contact: &MemberContact) -> Self {
        Self::Presence {
            v: ContactVersion,
            change: PresenceChange::ExplicitLeave {
                member_id: contact.member_id,
                run_id: contact.run_id,
                display_name: contact.display_name.clone(),
            },
        }
    }

    pub(crate) fn contact_only(contact: &MemberContact, cause: ContactLossCause) -> Self {
        Self::Presence {
            v: ContactVersion,
            change: PresenceChange::ContactOnly {
                member_id: contact.member_id,
                run_id: contact.run_id,
                display_name: contact.display_name.clone(),
                cause,
            },
        }
    }

    pub(crate) fn ack() -> Self {
        Self::Ack { v: ContactVersion }
    }

    pub(crate) fn superseded() -> Self {
        Self::Superseded { v: ContactVersion }
    }

    pub(crate) fn service_stop() -> Self {
        Self::ServiceStop {
            v: ContactVersion,
            effect: ServiceStopEffect::ContactVisibility,
        }
    }

    pub(crate) fn goodbye() -> Self {
        Self::Goodbye { v: ContactVersion }
    }
}

/// Encodes a client message as one length-prefixed frame.
pub fn encode_client(message: &ClientMessage) -> Result<Vec<u8>, FrameError> {
    let mut message = message.clone();
    normalize_client_media(&mut message)?;
    encode_json_frame(&message)
}

/// Encodes a server message as one length-prefixed frame.
pub fn encode_server(message: &ServerMessage) -> Result<Vec<u8>, FrameError> {
    let mut message = message.clone();
    normalize_server_media(&mut message)?;
    encode_json_frame(&message)
}

/// Decodes a client payload.
pub fn decode_client(payload: &[u8]) -> Result<ClientMessage, FrameError> {
    let mut message = decode_json_frame(payload)?;
    normalize_client_media(&mut message)?;
    Ok(message)
}

/// Decodes a server payload.
pub fn decode_server(payload: &[u8]) -> Result<ServerMessage, FrameError> {
    let mut message = decode_json_frame(payload)?;
    normalize_server_media(&mut message)?;
    Ok(message)
}

fn normalize_client_media(message: &mut ClientMessage) -> Result<(), FrameError> {
    if let ClientMessage::Join {
        media_port,
        media_ip,
        media_scope_id,
        media_alternates,
        ..
    } = message
    {
        let primary = match *media_ip {
            Some(ip) => Some(
                MediaSocket::explicit(ip, *media_port, *media_scope_id)
                    .map_err(FrameError::InvalidMediaCandidates)?,
            ),
            None if *media_scope_id != 0 => {
                return Err(FrameError::InvalidMediaCandidates(crate::MediaError::Scope));
            }
            None => None,
        };
        *media_alternates = normalize_media_alternates(*media_port, primary, media_alternates)
            .map_err(FrameError::InvalidMediaCandidates)?;
    }
    Ok(())
}

fn normalize_member_media(member: &mut MemberContact) -> Result<(), FrameError> {
    member.media_alternates = normalize_media_alternates(
        member.media.port(),
        Some(member.media),
        &member.media_alternates,
    )
    .map_err(FrameError::InvalidMediaCandidates)?;
    Ok(())
}

fn normalize_server_media(message: &mut ServerMessage) -> Result<(), FrameError> {
    match message {
        ServerMessage::Joined { you, members, .. } => {
            if members.len() > crate::MAX_MEMBERS {
                return Err(FrameError::InvalidMetadata(
                    "member count exceeds the room bound",
                ));
            }
            normalize_member_media(you)?;
            for member in members {
                normalize_member_media(member)?;
            }
        }
        ServerMessage::Snapshot { members, .. } => {
            if members.len() > crate::MAX_MEMBERS {
                return Err(FrameError::InvalidMetadata(
                    "member count exceeds the room bound",
                ));
            }
            for member in members {
                normalize_member_media(member)?;
            }
        }
        ServerMessage::Presence {
            change: PresenceChange::Visible { member },
            ..
        } => normalize_member_media(member)?,
        ServerMessage::Return { state, .. } => {
            if let Some(member) = &mut state.member {
                normalize_member_media(member)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn validate_room_name(name: &str) -> Result<(), &'static str> {
    validate_name(
        name,
        "The room name must contain 1 to 80 characters and no control characters.",
    )
}

pub(crate) fn validate_display_name(name: &str) -> Result<(), &'static str> {
    validate_name(
        name,
        "The display name must contain 1 to 80 characters and no control characters.",
    )
}

pub(crate) fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() > MAX_PASSWORD_CHARS || password.chars().any(char::is_control) {
        return Err(
            "The room password must contain at most 128 characters and no control characters.",
        );
    }
    Ok(())
}

fn validate_name(name: &str, message: &'static str) -> Result<(), &'static str> {
    let count = name.chars().count();
    if count == 0 || count > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{CONTACT_PREFACE, FrameDecoder};

    fn sample_member() -> MemberContact {
        let media = crate::media::MediaSocket::explicit(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            9_001,
            0,
        )
        .unwrap();
        MemberContact {
            member_id: "00000000000000000000000000000001".parse().unwrap(),
            run_id: "00000000000000000000000000000002".parse().unwrap(),
            display_name: "同名".to_owned(),
            media,
            media_alternates: vec![],
        }
    }

    #[test]
    fn roundtrip_keeps_duplicate_names_and_omits_any_password() {
        let secret = "contact-test-secret";
        let member = sample_member();
        let info_message = ServerMessage::RoomInfo {
            v: ContactVersion,
            room_name: "Headphones".to_owned(),
            instance_id: "0000000000000000000000000000000a".parse().unwrap(),
            port: 47_101,
        };
        let joined = ServerMessage::joined(
            &RoomInfo {
                protocol_version: CONTACT_PROTOCOL_VERSION,
                room_name: "Headphones".to_owned(),
                instance_id: "0000000000000000000000000000000a".parse().unwrap(),
                port: 47_101,
            },
            member.clone(),
            vec![member.clone()],
        );
        let encoded = encode_server(&joined).unwrap();
        let text = String::from_utf8(encoded[4..].to_vec()).unwrap();
        assert!(!text.contains(secret));
        assert!(!text.contains("password"));
        let payload = FrameDecoder::new()
            .feed(&[CONTACT_PREFACE.as_slice(), encoded.as_slice()].concat())
            .unwrap();
        assert_eq!(decode_server(&payload[0]).unwrap(), joined);
        let room = encode_server(&info_message).unwrap();
        let room_text = String::from_utf8(room[4..].to_vec()).unwrap();
        assert!(!room_text.contains("password"));
        assert!(room_text.contains("\"port\":47101"));
    }

    #[test]
    fn unknown_fields_versions_and_names_are_rejected() {
        assert!(decode_client(br#"{"v":2,"type":"inspect","extra":true}"#).is_err());
        assert!(decode_client(br#"{"v":1,"type":"inspect"}"#).is_err());
        assert!(decode_client(br#"{"v":2,"type":"kick"}"#).is_err());
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name(&"甲".repeat(81)).is_err());
        assert!(validate_display_name("同名").is_ok());
        assert!(validate_display_name("甲\n乙").is_err());
        assert!(validate_password("").is_ok());
        assert!(validate_password("has\tcontrol").is_err());
        let debug = format!(
            "{:?}",
            ClientMessage::join(&JoinParams {
                room_name: "Headphones".to_owned(),
                password: "contact-test-secret".to_owned(),
                member_id: sample_member().member_id,
                run_id: sample_member().run_id,
                display_name: "同名".to_owned(),
                media_port: 1,
                media_ip: None,
                media_scope_id: 0,
                media_alternates: vec![],
            })
        );
        assert!(!debug.contains("contact-test-secret"));
        assert!(debug.contains("redacted"));
    }
}
