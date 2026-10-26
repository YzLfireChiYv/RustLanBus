//! Client-facing contact events.
//!
//! [`ContactEvent::explicit_departure`] is the signal a later participant
//! adapter may apply to one member's direct media. [`ContactEvent::contact_visibility`]
//! and [`snapshot_gaps`] describe the directory only. Healthy direct media
//! continues for those notices, including service stop.

use rlb_model::{MemberId, RunId};

use crate::wire::{ContactLossCause, MemberContact, ReturnState, RoomInfo};

/// A directory reply together with every queued event that preceded it.
///
/// Apply `before` in order before applying `value`. Later events remain queued
/// for [`crate::ContactClient::poll`]; replaying an older presence after the
/// directory reply could replace a newer address or registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderedContactReply<T> {
    /// Events received before this reply, in wire order.
    pub before: Vec<ContactEvent>,
    /// The requested directory reply.
    pub value: T,
}

/// Complete result of a successful join.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinedRoom {
    /// Room the service is running.
    pub room: RoomInfo,
    /// Registration owned by this connection.
    pub you: MemberContact,
    /// Every current member, including [`Self::you`].
    pub members: Vec<MemberContact>,
}

/// Complete directory at one moment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectorySnapshot {
    /// Room the service is running.
    pub room: RoomInfo,
    /// Every current member, ordered by member identity.
    pub members: Vec<MemberContact>,
}

/// A member present in an older snapshot and absent from a newer one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotGap {
    /// Stable member identity.
    pub member_id: MemberId,
    /// Run recorded in the older snapshot.
    pub run_id: RunId,
    /// Display name recorded in the older snapshot.
    pub display_name: String,
}

impl SnapshotGap {
    /// Snapshot absence is contact visibility.
    #[must_use]
    pub const fn cause(&self) -> ContactLossCause {
        ContactLossCause::AbsentFromSnapshot
    }
}

/// Directory visibility notice. Direct media stays up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContactVisibility {
    /// One member dropped out of the directory.
    Member {
        /// Stable member identity.
        member_id: MemberId,
        /// Run recorded for that registration.
        run_id: RunId,
        /// Why the directory lost sight of the member.
        cause: ContactLossCause,
    },
    /// The whole contact service closed its control sockets.
    ServiceStop,
}

/// One decoded service message, ready for the participant adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContactEvent {
    /// Current optional return participant for this connection's member.
    Return(ReturnState),
    /// Answer to inspect.
    RoomInfo(RoomInfo),
    /// This connection registered.
    Joined(JoinedRoom),
    /// Complete directory.
    Snapshot(DirectorySnapshot),
    /// A member appeared or replaced its contact record.
    MemberVisible(MemberContact),
    /// The member sent leave.
    ExplicitLeave {
        /// Stable member identity.
        member_id: MemberId,
        /// Run that left.
        run_id: RunId,
        /// Display name at departure.
        display_name: String,
    },
    /// The directory lost sight of the member.
    ContactHidden {
        /// Stable member identity.
        member_id: MemberId,
        /// Run that became hidden.
        run_id: RunId,
        /// Display name when the registration was current.
        display_name: String,
        /// Why the directory lost sight of the member.
        cause: ContactLossCause,
    },
    /// The service accepted a keepalive.
    Acknowledged,
    /// A newer connection replaced this connection's registration.
    Superseded,
    /// The service is closing control sockets.
    ServiceStopped,
    /// The service accepted leave.
    LeaveAccepted,
    /// The service rejected a request.
    Rejected {
        /// Closed code.
        code: crate::wire::ContactCode,
        /// Fixed explanation without the room password.
        message: String,
    },
}

impl ContactEvent {
    /// Member departure the participant adapter may apply to direct media.
    ///
    /// The result is present only for [`Self::ExplicitLeave`].
    #[must_use]
    pub fn explicit_departure(&self) -> Option<(MemberId, RunId)> {
        match self {
            Self::ExplicitLeave {
                member_id, run_id, ..
            } => Some((*member_id, *run_id)),
            _ => None,
        }
    }

    /// Contact-path notice. Healthy direct media continues.
    #[must_use]
    pub fn contact_visibility(&self) -> Option<ContactVisibility> {
        match self {
            Self::ContactHidden {
                member_id,
                run_id,
                cause,
                ..
            } => Some(ContactVisibility::Member {
                member_id: *member_id,
                run_id: *run_id,
                cause: *cause,
            }),
            Self::ServiceStopped => Some(ContactVisibility::ServiceStop),
            _ => None,
        }
    }
}

/// Members listed in `previous` whose member identity is missing from `current`.
///
/// A changed run, name, or media socket is still contact for that member identity.
/// Each returned gap uses [`ContactLossCause::AbsentFromSnapshot`].
#[must_use]
pub fn snapshot_gaps(previous: &[MemberContact], current: &[MemberContact]) -> Vec<SnapshotGap> {
    previous
        .iter()
        .filter(|member| {
            current
                .iter()
                .all(|candidate| candidate.member_id != member.member_id)
        })
        .map(|member| SnapshotGap {
            member_id: member.member_id,
            run_id: member.run_id,
            display_name: member.display_name.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use rlb_model::{MemberId, RunId};

    use super::*;
    use crate::media::MediaSocket;
    use crate::wire::ContactLossCause;

    fn contact(member: u8, port: u16) -> MemberContact {
        let mut member_bytes = [0_u8; 16];
        member_bytes[15] = member;
        let mut run_bytes = [0_u8; 16];
        run_bytes[0] = member;
        MemberContact {
            member_id: MemberId::from_bytes(member_bytes).unwrap(),
            run_id: RunId::from_bytes(run_bytes).unwrap(),
            display_name: "甲".to_owned(),
            media: MediaSocket::explicit(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port, 0)
                .unwrap(),
            media_alternates: vec![],
        }
    }

    #[test]
    fn only_explicit_leave_names_a_departure() {
        let member = contact(1, 9);
        let leave = ContactEvent::ExplicitLeave {
            member_id: member.member_id,
            run_id: member.run_id,
            display_name: member.display_name.clone(),
        };
        assert_eq!(
            leave.explicit_departure(),
            Some((member.member_id, member.run_id))
        );
        assert!(leave.contact_visibility().is_none());

        let hidden = ContactEvent::ContactHidden {
            member_id: member.member_id,
            run_id: member.run_id,
            display_name: member.display_name.clone(),
            cause: ContactLossCause::Timeout,
        };
        assert!(hidden.explicit_departure().is_none());
        assert_eq!(
            hidden.contact_visibility(),
            Some(ContactVisibility::Member {
                member_id: member.member_id,
                run_id: member.run_id,
                cause: ContactLossCause::Timeout,
            })
        );
        let stopped = ContactEvent::ServiceStopped;
        assert!(stopped.explicit_departure().is_none());
        assert_eq!(
            stopped.contact_visibility(),
            Some(ContactVisibility::ServiceStop)
        );
    }

    #[test]
    fn snapshot_gaps_follow_member_identity() {
        let older = contact(1, 10);
        let mut moved = older.clone();
        moved.media =
            MediaSocket::explicit(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 11, 0).unwrap();
        moved.run_id = RunId::from_bytes({
            let mut bytes = [0_u8; 16];
            bytes[0] = 9;
            bytes
        })
        .unwrap();
        assert!(snapshot_gaps(std::slice::from_ref(&older), &[moved]).is_empty());
        let gaps = snapshot_gaps(&[older.clone(), contact(2, 12)], &[older]);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].member_id, contact(2, 12).member_id);
        assert_eq!(gaps[0].cause(), ContactLossCause::AbsentFromSnapshot);
    }
}
