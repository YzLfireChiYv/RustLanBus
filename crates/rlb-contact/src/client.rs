//! Blocking contact client for Windows embedding and the later room adapter.
//!
//! The caller owns the schedule. This type does not start a thread and does not
//! open a media socket. Send [`crate::KEEPALIVE_INTERVAL`] keepalives on an open
//! connection. [`ContactClient::since_server_bytes`] reports silence; the service
//! drops a silent registration after its contact timeout.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::error::ContactError;
use crate::event::{ContactEvent, DirectorySnapshot, JoinedRoom, OrderedContactReply};
use crate::framing::{CONTACT_PREFACE, DecoderPhase, FrameDecoder, FrameError};
use crate::media::{MediaSocket, normalize_media_alternates};
use crate::wire::{
    CONTACT_PROTOCOL_VERSION, ClientMessage, ContactCode, ContactVersion, ReturnAction,
    ReturnState, RoomInfo, ServerMessage, decode_server, encode_client, validate_display_name,
    validate_password, validate_room_name,
};
use rlb_control::strict_json::StrictJsonError;

/// One TCP control session with a contact service.
#[must_use = "dropping the client closes the control connection"]
pub struct ContactClient {
    stream: TcpStream,
    decoder: FrameDecoder,
    pending: VecDeque<ContactEvent>,
    last_server_bytes: Instant,
    io_timeout: Duration,
    preface_seen: bool,
}

impl ContactClient {
    /// Connects, sends the contact preface, and waits for the service preface.
    pub fn connect(address: SocketAddr, timeout: Duration) -> Result<Self, ContactError> {
        if timeout.is_zero() {
            return Err(ContactError::InvalidRequest(
                "The contact I/O timeout must be greater than zero.",
            ));
        }
        let stream = TcpStream::connect_timeout(&address, timeout)?;
        Self::from_stream(stream, timeout)
    }

    /// Starts the ordinary protocol on a caller-connected stream. Applications
    /// can constrain the local interface without duplicating contact framing.
    pub fn from_stream(mut stream: TcpStream, timeout: Duration) -> Result<Self, ContactError> {
        if timeout.is_zero() {
            return Err(ContactError::InvalidRequest(
                "The contact I/O timeout must be greater than zero.",
            ));
        }
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(CONTACT_PREFACE)?;
        stream.flush()?;
        let mut client = Self {
            stream,
            decoder: FrameDecoder::new(),
            pending: VecDeque::new(),
            last_server_bytes: Instant::now(),
            io_timeout: timeout,
            preface_seen: false,
        };
        client.read_preface()?;
        Ok(client)
    }

    /// Local TCP address of this control connection, including its ephemeral port.
    pub fn local_addr(&self) -> Result<SocketAddr, ContactError> {
        Ok(self.stream.local_addr()?)
    }

    /// Time since the last byte received from the service.
    #[must_use]
    pub fn since_server_bytes(&self) -> Duration {
        self.last_server_bytes.elapsed()
    }

    /// Reads room facts. This call does not register a member.
    pub fn inspect(&mut self) -> Result<RoomInfo, ContactError> {
        self.send(&ClientMessage::Inspect { v: ContactVersion })?;
        match self.take_reply(|event| {
            matches!(
                event,
                ContactEvent::RoomInfo(_)
                    | ContactEvent::Rejected { .. }
                    | ContactEvent::ServiceStopped
            )
        })? {
            ContactEvent::RoomInfo(info) => Ok(info),
            other => reply_error(other),
        }
    }

    /// Registers this member and returns the current directory.
    ///
    /// Apply the returned `before` events in order before applying `value`.
    /// Events after the reply remain queued for [`Self::poll`].
    pub fn join(
        &mut self,
        mut params: crate::wire::JoinParams,
    ) -> Result<OrderedContactReply<JoinedRoom>, ContactError> {
        validate_room_name(&params.room_name).map_err(ContactError::InvalidRequest)?;
        validate_password(&params.password).map_err(ContactError::InvalidRequest)?;
        validate_display_name(&params.display_name).map_err(ContactError::InvalidRequest)?;
        if params.media_ip.is_none() && params.media_scope_id != 0 {
            return Err(ContactError::InvalidRequest(
                "A media scope requires an explicit media address.",
            ));
        }
        if let Some(ip) = params.media_ip {
            MediaSocket::explicit(ip, params.media_port, params.media_scope_id).map_err(|_| {
                ContactError::InvalidRequest(ContactCode::InvalidMedia.public_message())
            })?;
        } else if params.media_port == 0 {
            return Err(ContactError::InvalidRequest(
                ContactCode::InvalidMedia.public_message(),
            ));
        }
        let primary = params
            .media_ip
            .map(|ip| MediaSocket::explicit(ip, params.media_port, params.media_scope_id))
            .transpose()
            .map_err(|_| {
                ContactError::InvalidRequest(ContactCode::InvalidMedia.public_message())
            })?;
        params.media_alternates =
            normalize_media_alternates(params.media_port, primary, &params.media_alternates)
                .map_err(|_| {
                    ContactError::InvalidRequest(ContactCode::InvalidMedia.public_message())
                })?;
        self.send(&ClientMessage::join(&params))?;
        let reply = self.take_ordered_reply(|event| {
            matches!(
                event,
                ContactEvent::Joined(_)
                    | ContactEvent::Rejected { .. }
                    | ContactEvent::ServiceStopped
                    | ContactEvent::Superseded
            )
        })?;
        match reply.value {
            ContactEvent::Joined(value) => Ok(OrderedContactReply {
                before: reply.before,
                value,
            }),
            other => {
                self.restore_pending_prefix(reply.before);
                reply_error(other)
            }
        }
    }

    /// Resets the service silence timer and waits for the acknowledgement.
    pub fn keepalive(&mut self) -> Result<(), ContactError> {
        self.send(&ClientMessage::Keepalive { v: ContactVersion })?;
        match self.take_reply(|event| {
            matches!(
                event,
                ContactEvent::Acknowledged
                    | ContactEvent::Rejected { .. }
                    | ContactEvent::ServiceStopped
            )
        })? {
            ContactEvent::Acknowledged => Ok(()),
            other => reply_error(other),
        }
    }

    /// Reads the complete current directory.
    ///
    /// Apply the returned `before` events in order before applying `value`.
    /// Events after the reply remain queued for [`Self::poll`].
    pub fn refresh(&mut self) -> Result<OrderedContactReply<DirectorySnapshot>, ContactError> {
        self.send(&ClientMessage::Snapshot { v: ContactVersion })?;
        let reply = self.take_ordered_reply(|event| {
            matches!(
                event,
                ContactEvent::Snapshot(_)
                    | ContactEvent::Rejected { .. }
                    | ContactEvent::ServiceStopped
            )
        })?;
        match reply.value {
            ContactEvent::Snapshot(value) => Ok(OrderedContactReply {
                before: reply.before,
                value,
            }),
            other => {
                self.restore_pending_prefix(reply.before);
                reply_error(other)
            }
        }
    }

    /// Announces an explicit departure and waits for acceptance.
    pub fn leave(&mut self) -> Result<(), ContactError> {
        self.send(&ClientMessage::Leave { v: ContactVersion })?;
        match self.take_reply(|event| {
            matches!(
                event,
                ContactEvent::LeaveAccepted
                    | ContactEvent::Rejected { .. }
                    | ContactEvent::ServiceStopped
                    | ContactEvent::Superseded
            )
        })? {
            ContactEvent::LeaveAccepted => Ok(()),
            other => reply_error(other),
        }
    }

    /// Operates this registered member's optional virtual return participant.
    ///
    /// A timed-out request closes this connection so its late response cannot
    /// be mistaken for the reply to a later operation.
    pub fn return_action(&mut self, action: ReturnAction) -> Result<ReturnState, ContactError> {
        self.send(&ClientMessage::Return {
            v: ContactVersion,
            action,
        })?;
        let reply = self.take_reply(|event| {
            matches!(
                event,
                ContactEvent::Return(_)
                    | ContactEvent::Rejected { .. }
                    | ContactEvent::ServiceStopped
                    | ContactEvent::Superseded
            )
        });
        match reply {
            Ok(ContactEvent::Return(state)) => Ok(state),
            Ok(other) => reply_error(other),
            Err(error) => {
                let _ = self.stream.shutdown(Shutdown::Both);
                Err(error)
            }
        }
    }

    /// Returns the next queued event, waiting up to `timeout` for a new frame.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<ContactEvent>, ContactError> {
        if self.pending.is_empty() {
            self.pull(timeout)?;
        }
        Ok(self.pending.pop_front())
    }

    fn read_preface(&mut self) -> Result<(), ContactError> {
        let deadline = Instant::now() + self.io_timeout;
        while !self.preface_seen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ContactError::TimedOut);
            }
            self.pull(remaining)?;
            if !self.preface_seen && Instant::now() >= deadline {
                return Err(ContactError::TimedOut);
            }
        }
        Ok(())
    }

    fn send(&mut self, message: &ClientMessage) -> Result<(), ContactError> {
        let bytes = encode_client(message)?;
        self.stream.write_all(&bytes)?;
        Ok(())
    }

    fn pull(&mut self, timeout: Duration) -> Result<(), ContactError> {
        self.stream.set_read_timeout(Some(timeout))?;
        let mut buffer = [0_u8; 8192];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    if self.pending.is_empty() {
                        return Err(ContactError::ControlLost);
                    }
                    return Ok(());
                }
                Ok(size) => {
                    self.last_server_bytes = Instant::now();
                    let before = self.pending.len();
                    let preface_was_seen = self.preface_seen;
                    self.ingest(&buffer[..size])?;
                    if self.pending.len() > before || (!preface_was_seen && self.preface_seen) {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if error.kind() == ErrorKind::TimedOut
                        || error.kind() == ErrorKind::WouldBlock =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn ingest(&mut self, bytes: &[u8]) -> Result<(), ContactError> {
        let frames = self.decoder.feed(bytes)?;
        if self.decoder.phase() == DecoderPhase::Frames {
            self.preface_seen = true;
        }
        for frame in frames {
            let message = match decode_server(&frame) {
                Err(FrameError::Json(StrictJsonError::ProtocolVersion)) => {
                    return Err(ContactError::Rejected {
                        code: ContactCode::UnsupportedVersion,
                        message: ContactCode::UnsupportedVersion.public_message().to_owned(),
                    });
                }
                result => result?,
            };
            self.pending.push_back(event_from(message)?);
        }
        Ok(())
    }

    fn next_matching(
        &mut self,
        deadline: Instant,
        mut predicate: impl FnMut(&ContactEvent) -> bool,
    ) -> Result<OrderedContactReply<ContactEvent>, ContactError> {
        loop {
            if let Some(reply) = take_matching_prefix(&mut self.pending, &mut predicate) {
                return Ok(reply);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ContactError::TimedOut);
            }
            let before = self.pending.len();
            self.pull(remaining)?;
            if self.pending.len() == before {
                return Err(ContactError::TimedOut);
            }
        }
    }

    fn take_reply(
        &mut self,
        predicate: impl FnMut(&ContactEvent) -> bool,
    ) -> Result<ContactEvent, ContactError> {
        let reply = self.take_ordered_reply(predicate)?;
        self.restore_pending_prefix(reply.before);
        Ok(reply.value)
    }

    fn take_ordered_reply(
        &mut self,
        predicate: impl FnMut(&ContactEvent) -> bool,
    ) -> Result<OrderedContactReply<ContactEvent>, ContactError> {
        self.next_matching(Instant::now() + self.io_timeout, predicate)
    }

    fn restore_pending_prefix(&mut self, events: Vec<ContactEvent>) {
        for event in events.into_iter().rev() {
            self.pending.push_front(event);
        }
    }
}

fn take_matching_prefix(
    pending: &mut VecDeque<ContactEvent>,
    mut predicate: impl FnMut(&ContactEvent) -> bool,
) -> Option<OrderedContactReply<ContactEvent>> {
    let index = pending.iter().position(&mut predicate)?;
    let value = pending.remove(index)?;
    let before = pending.drain(..index).collect();
    Some(OrderedContactReply { before, value })
}

impl Drop for ContactClient {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn event_from(message: ServerMessage) -> Result<ContactEvent, ContactError> {
    Ok(match message {
        ServerMessage::Return { state, .. } => ContactEvent::Return(state),
        ServerMessage::RoomInfo {
            room_name,
            instance_id,
            port,
            ..
        } => ContactEvent::RoomInfo(room_info(room_name, instance_id, port)?),
        ServerMessage::Joined {
            room_name,
            instance_id,
            port,
            you,
            members,
            ..
        } => ContactEvent::Joined(JoinedRoom {
            room: room_info(room_name, instance_id, port)?,
            you,
            members,
        }),
        ServerMessage::Snapshot {
            room_name,
            instance_id,
            port,
            members,
            ..
        } => ContactEvent::Snapshot(DirectorySnapshot {
            room: room_info(room_name, instance_id, port)?,
            members,
        }),
        ServerMessage::Presence { change, .. } => match change {
            crate::wire::PresenceChange::Visible { member } => ContactEvent::MemberVisible(member),
            crate::wire::PresenceChange::ExplicitLeave {
                member_id,
                run_id,
                display_name,
            } => ContactEvent::ExplicitLeave {
                member_id,
                run_id,
                display_name,
            },
            crate::wire::PresenceChange::ContactOnly {
                member_id,
                run_id,
                display_name,
                cause,
            } => ContactEvent::ContactHidden {
                member_id,
                run_id,
                display_name,
                cause,
            },
        },
        ServerMessage::Ack { .. } => ContactEvent::Acknowledged,
        ServerMessage::Superseded { .. } => ContactEvent::Superseded,
        ServerMessage::ServiceStop { .. } => ContactEvent::ServiceStopped,
        ServerMessage::Goodbye { .. } => ContactEvent::LeaveAccepted,
        ServerMessage::Error { code, message, .. } => ContactEvent::Rejected { code, message },
    })
}

fn room_info(
    room_name: String,
    instance_id: rlb_model::RoomId,
    port: u16,
) -> Result<RoomInfo, ContactError> {
    if port == 0 {
        return Err(ContactError::Protocol(
            "The contact service reported port 0.",
        ));
    }
    Ok(RoomInfo {
        protocol_version: CONTACT_PROTOCOL_VERSION,
        room_name,
        instance_id,
        port,
    })
}

fn reply_error<T>(event: ContactEvent) -> Result<T, ContactError> {
    match event {
        ContactEvent::Rejected { code, message } => Err(ContactError::Rejected { code, message }),
        ContactEvent::ServiceStopped => Err(ContactError::Stopped),
        ContactEvent::Superseded => Err(ContactError::Rejected {
            code: ContactCode::Superseded,
            message: ContactCode::Superseded.public_message().to_owned(),
        }),
        _ => Err(ContactError::Protocol("unexpected contact reply")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_reply_drains_its_earlier_events_and_keeps_later_events() {
        let older = crate::MemberContact {
            member_id: rlb_model::MemberId::from_bytes([1; 16]).unwrap(),
            run_id: rlb_model::RunId::from_bytes([2; 16]).unwrap(),
            display_name: "member".to_owned(),
            media: MediaSocket::explicit("192.0.2.10".parse().unwrap(), 9_001, 0).unwrap(),
            media_alternates: vec![],
        };
        let mut newer = older.clone();
        newer.media = MediaSocket::explicit("192.0.2.11".parse().unwrap(), 9_001, 0).unwrap();
        let snapshot = ContactEvent::Snapshot(DirectorySnapshot {
            room: RoomInfo {
                protocol_version: CONTACT_PROTOCOL_VERSION,
                room_name: "Headphones".to_owned(),
                instance_id: rlb_model::RoomId::from_bytes([3; 16]).unwrap(),
                port: 47_101,
            },
            members: vec![newer.clone()],
        });
        let later = ContactEvent::MemberVisible(newer);
        let mut pending = VecDeque::from([
            ContactEvent::Acknowledged,
            ContactEvent::MemberVisible(older.clone()),
            snapshot.clone(),
            later.clone(),
        ]);
        let original = pending.clone();
        assert!(
            take_matching_prefix(&mut pending, |event| matches!(
                event,
                ContactEvent::Joined(_)
            ))
            .is_none()
        );
        assert_eq!(pending, original);

        let reply = take_matching_prefix(&mut pending, |event| {
            matches!(event, ContactEvent::Snapshot(_))
        })
        .unwrap();
        assert_eq!(
            reply.before,
            vec![
                ContactEvent::Acknowledged,
                ContactEvent::MemberVisible(older)
            ]
        );
        assert_eq!(reply.value, snapshot);
        assert_eq!(pending.pop_front(), Some(later));
        assert!(pending.is_empty());
    }
}
