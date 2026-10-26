//! In-memory member directory for one running contact service.
//!
//! A member is current only while the newest registration owns it. Closing an
//! older connection removes nothing when a newer registration has replaced it.

use std::collections::BTreeMap;

use rlb_model::MemberId;

use crate::wire::MemberContact;

struct Entry {
    owner: u64,
    contact: MemberContact,
}

#[derive(Default)]
pub(crate) struct Directory {
    members: BTreeMap<MemberId, Entry>,
}

impl Directory {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn contains(&self, member_id: MemberId) -> bool {
        self.members.contains_key(&member_id)
    }

    pub(crate) fn owner(&self, member_id: MemberId) -> Option<u64> {
        self.members.get(&member_id).map(|entry| entry.owner)
    }

    pub(crate) fn contact(&self, member_id: MemberId) -> Option<&MemberContact> {
        self.members.get(&member_id).map(|entry| &entry.contact)
    }

    /// Inserts or replaces the member. Returns the previous connection id.
    pub(crate) fn insert(&mut self, owner: u64, contact: MemberContact) -> Option<u64> {
        let member_id = contact.member_id;
        self.members
            .insert(member_id, Entry { owner, contact })
            .map(|entry| entry.owner)
    }

    /// Removes the member when `owner` is still the current registration.
    pub(crate) fn remove_owner(&mut self, owner: u64) -> Option<MemberContact> {
        let member_id = self
            .members
            .iter()
            .find(|(_, entry)| entry.owner == owner)
            .map(|(member_id, _)| *member_id)?;
        self.members.remove(&member_id).map(|entry| entry.contact)
    }

    pub(crate) fn members(&self) -> Vec<MemberContact> {
        self.members
            .values()
            .map(|entry| entry.contact.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use rlb_model::{MemberId, RunId};

    use super::*;
    use crate::media::MediaSocket;

    fn contact(member: u8, port: u16) -> MemberContact {
        MemberContact {
            member_id: member_id(member),
            run_id: run_id(member),
            display_name: "同名".to_owned(),
            media: MediaSocket::explicit(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port, 0)
                .unwrap(),
            media_alternates: vec![],
        }
    }

    fn member_id(value: u8) -> MemberId {
        let mut bytes = [0_u8; 16];
        bytes[15] = value;
        MemberId::from_bytes(bytes).unwrap()
    }

    fn run_id(value: u8) -> RunId {
        let mut bytes = [0_u8; 16];
        bytes[0] = value;
        RunId::from_bytes(bytes).unwrap()
    }

    #[test]
    fn an_older_owner_cannot_remove_the_replacement() {
        let mut directory = Directory::new();
        let member = contact(1, 100);
        assert_eq!(directory.insert(10, member), None);
        let previous = directory.insert(11, contact(1, 200));
        assert_eq!(previous, Some(10));
        assert!(directory.remove_owner(10).is_none());
        assert_eq!(directory.len(), 1);
        assert_eq!(directory.owner(member_id(1)), Some(11));
        let current = directory.remove_owner(11).unwrap();
        assert_eq!(current.media.port(), 200);
        assert!(directory.remove_owner(11).is_none());
    }

    #[test]
    fn duplicate_names_stay_on_distinct_member_ids() {
        let mut directory = Directory::new();
        directory.insert(1, contact(1, 1));
        directory.insert(2, contact(2, 2));
        let members = directory.members();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].display_name, members[1].display_name);
        assert_ne!(members[0].member_id, members[1].member_id);
    }
}
