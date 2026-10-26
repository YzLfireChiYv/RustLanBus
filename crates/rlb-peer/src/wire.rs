//! Fixed little-endian RLC5 control. JSON and native struct padding are not on the wire.

use rlb_model::{BindNonce, DirectSessionId, HelloNonce, MemberId, RunId, SourceId};
use serde::{Deserialize, Serialize};

use crate::PeerError;
use crate::media::MAX_DATAGRAM_BYTES;
use crate::source::{
    Catalogue, ChannelLayout, PcmEncoding, PcmFormat, PublicationEpoch, PublicationState,
    SOURCE_RECORD_BYTES, SourceEntry, SourceKind,
};

pub(crate) const CONTROL_MAGIC: &[u8; 4] = b"RLC5";
const COMMON_BYTES: usize = 56;
const SNAPSHOT_HEADER_BYTES: usize = 32;
const ACK_BYTES: usize = 24;
const SUBSCRIPTIONS_HEADER_BYTES: usize = 56;
const GRANT_BYTES: usize = 36;
const MAX_SOURCES: usize = crate::source::MAX_SOURCES;
const KNOWN_ENCODING_BITS: u8 = 0b0111;

pub(crate) const F32_ENCODING_BIT: u8 = 1 << 0;

/// Persisted user choices for this member pair. Playback mute is independent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub send: bool,
    pub receive: bool,
}

impl Intent {
    fn flags(self) -> u8 {
        u8::from(self.send) | (u8::from(self.receive) << 1)
    }

    fn from_flags(flags: u8) -> Result<Self, PeerError> {
        if flags & !0b0011 != 0 {
            return Err(PeerError::MalformedControl);
        }
        Ok(Self {
            send: flags & 0b0001 != 0,
            receive: flags & 0b0010 != 0,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub revision: u64,
    pub send_generation: u64,
    pub receive_generation: u64,
    pub intent: Intent,
    pub receive_encodings: u8,
    pub sources: Catalogue,
    /// One monotonic source-send token per sorted catalogue entry; odd is open.
    pub send_tokens: Vec<u32>,
}

impl Snapshot {
    pub(crate) fn new(intent: Intent, sources: Catalogue) -> Self {
        let send_tokens = vec![u32::from(intent.send); sources.as_slice().len()];
        Self {
            revision: 1,
            send_generation: 1,
            receive_generation: 1,
            intent,
            receive_encodings: KNOWN_ENCODING_BITS,
            sources,
            send_tokens,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_send_tokens(
        intent: Intent,
        sources: Catalogue,
        send_tokens: Vec<u32>,
    ) -> Result<Self, PeerError> {
        if send_tokens.len() != sources.as_slice().len() {
            return Err(PeerError::MalformedControl);
        }
        let mut state = Self::new(intent, sources);
        state.send_tokens = send_tokens;
        Ok(state)
    }

    pub(crate) fn send_token(&self, id: SourceId) -> u32 {
        self.sources
            .as_slice()
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
            .and_then(|index| self.send_tokens.get(index).copied())
            .unwrap_or(0)
    }

    pub(crate) fn ack(&self) -> Ack {
        Ack {
            send_generation: self.send_generation,
            receive_generation: self.receive_generation,
            state_revision: self.revision,
        }
    }

    pub(crate) fn follows(&self, previous: &Self) -> bool {
        self.send_tokens.len() == self.sources.as_slice().len()
            && previous.send_tokens.len() == previous.sources.as_slice().len()
            && self.revision > previous.revision
            && self.send_generation >= previous.send_generation
            && self.receive_generation >= previous.receive_generation
            && self.send_generation <= self.revision
            && self.receive_generation <= self.revision
            && (self.intent.send == previous.intent.send
                || self.send_generation != previous.send_generation)
            && (self.intent.receive == previous.intent.receive
                || self.receive_generation != previous.receive_generation)
            && self.receive_encodings == previous.receive_encodings
            && sources_follow(previous.sources.as_slice(), self.sources.as_slice())
            && previous
                .sources
                .as_slice()
                .iter()
                .all(|entry| self.send_token(entry.id) >= previous.send_token(entry.id))
    }
}

fn sources_follow(previous: &[SourceEntry], next: &[SourceEntry]) -> bool {
    previous.iter().all(|prior| {
        next.iter().any(|entry| {
            entry.id == prior.id
                && entry.kind == prior.kind
                && entry.epoch >= prior.epoch
                && (entry.epoch != prior.epoch || entry == prior)
        })
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Ack {
    pub send_generation: u64,
    pub receive_generation: u64,
    pub state_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReceiveGrant {
    pub source: SourceId,
    pub epoch: PublicationEpoch,
    /// The current positive odd send token this receive grant authorizes.
    pub sender_token: u32,
    pub token: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Control {
    pub from: MemberId,
    pub to: MemberId,
    pub run: RunId,
    pub body: Body,
}

#[derive(Clone, Debug)]
pub(crate) enum Body {
    Hello {
        contact: HelloNonce,
        state: Snapshot,
    },
    Offer {
        session: DirectSessionId,
        target_run: RunId,
        contact: HelloNonce,
        target_contact: HelloNonce,
        state: Snapshot,
    },
    Confirm {
        session: DirectSessionId,
        target_run: RunId,
        contact: HelloNonce,
        target_contact: HelloNonce,
        nonce: BindNonce,
        state: Snapshot,
    },
    Ready {
        session: DirectSessionId,
        target_run: RunId,
        contact: HelloNonce,
        target_contact: HelloNonce,
        nonce: BindNonce,
        state: Snapshot,
        ack: Ack,
    },
    State {
        session: DirectSessionId,
        target_run: RunId,
        state: Snapshot,
        ack: Ack,
    },
    Subscriptions {
        session: DirectSessionId,
        target_run: RunId,
        revision: u64,
        ack: u64,
        grants: Vec<ReceiveGrant>,
    },
}

impl Body {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn state(&self) -> Option<&Snapshot> {
        match self {
            Self::Hello { state, .. }
            | Self::Offer { state, .. }
            | Self::Confirm { state, .. }
            | Self::Ready { state, .. }
            | Self::State { state, .. } => Some(state),
            Self::Subscriptions { .. } => None,
        }
    }

    fn kind(&self) -> u8 {
        match self {
            Self::Hello { .. } => 1,
            Self::Offer { .. } => 2,
            Self::Confirm { .. } => 3,
            Self::Ready { .. } => 4,
            Self::State { .. } => 5,
            Self::Subscriptions { .. } => 6,
        }
    }
}

pub(crate) fn encode(control: &Control) -> Result<Vec<u8>, PeerError> {
    let mut bytes = Vec::with_capacity(192);
    bytes.extend_from_slice(CONTROL_MAGIC);
    bytes.push(control.body.kind());
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes.extend_from_slice(control.from.as_bytes());
    bytes.extend_from_slice(control.to.as_bytes());
    bytes.extend_from_slice(control.run.as_bytes());
    match &control.body {
        Body::Hello { contact, state } => {
            bytes.extend_from_slice(contact.as_bytes());
            write_snapshot(&mut bytes, state)?;
        }
        Body::Offer {
            session,
            target_run,
            contact,
            target_contact,
            state,
        } => {
            bytes.extend_from_slice(session.as_bytes());
            bytes.extend_from_slice(target_run.as_bytes());
            bytes.extend_from_slice(contact.as_bytes());
            bytes.extend_from_slice(target_contact.as_bytes());
            write_snapshot(&mut bytes, state)?;
        }
        Body::Confirm {
            session,
            target_run,
            contact,
            target_contact,
            nonce,
            state,
        } => {
            bytes.extend_from_slice(session.as_bytes());
            bytes.extend_from_slice(target_run.as_bytes());
            bytes.extend_from_slice(contact.as_bytes());
            bytes.extend_from_slice(target_contact.as_bytes());
            bytes.extend_from_slice(nonce.as_bytes());
            write_snapshot(&mut bytes, state)?;
        }
        Body::Ready {
            session,
            target_run,
            contact,
            target_contact,
            nonce,
            state,
            ack,
        } => {
            bytes.extend_from_slice(session.as_bytes());
            bytes.extend_from_slice(target_run.as_bytes());
            bytes.extend_from_slice(contact.as_bytes());
            bytes.extend_from_slice(target_contact.as_bytes());
            bytes.extend_from_slice(nonce.as_bytes());
            write_snapshot(&mut bytes, state)?;
            write_ack(&mut bytes, *ack);
        }
        Body::State {
            session,
            target_run,
            state,
            ack,
        } => {
            bytes.extend_from_slice(session.as_bytes());
            bytes.extend_from_slice(target_run.as_bytes());
            write_snapshot(&mut bytes, state)?;
            write_ack(&mut bytes, *ack);
        }
        Body::Subscriptions {
            session,
            target_run,
            revision,
            ack,
            grants,
        } => {
            validate_grants(*revision, grants)?;
            bytes.extend_from_slice(session.as_bytes());
            bytes.extend_from_slice(target_run.as_bytes());
            bytes.extend_from_slice(&revision.to_le_bytes());
            bytes.extend_from_slice(&ack.to_le_bytes());
            bytes.extend_from_slice(&(grants.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&[0; 6]);
            for grant in grants {
                bytes.extend_from_slice(grant.source.as_bytes());
                bytes.extend_from_slice(&grant.epoch.get().to_le_bytes());
                bytes.extend_from_slice(&grant.sender_token.to_le_bytes());
                bytes.extend_from_slice(&grant.token.to_le_bytes());
            }
        }
    }
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Control, PeerError> {
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    if bytes.len() < COMMON_BYTES || !(bytes.starts_with(CONTROL_MAGIC) && bytes[5..8] == [0, 0, 0])
    {
        return Err(PeerError::MalformedControl);
    }
    let from = id(bytes, 8)?;
    let to = id(bytes, 24)?;
    let run = id(bytes, 40)?;
    let body = match bytes[4] {
        1 => {
            let contact = id(bytes, COMMON_BYTES)?;
            let state = finish_snapshot(bytes, COMMON_BYTES + 16)?;
            Body::Hello { contact, state }
        }
        2 => {
            let session = id(bytes, COMMON_BYTES)?;
            let target_run = id(bytes, COMMON_BYTES + 16)?;
            let contact = id(bytes, COMMON_BYTES + 32)?;
            let target_contact = id(bytes, COMMON_BYTES + 48)?;
            let state = finish_snapshot(bytes, COMMON_BYTES + 64)?;
            Body::Offer {
                session,
                target_run,
                contact,
                target_contact,
                state,
            }
        }
        3 => {
            let session = id(bytes, COMMON_BYTES)?;
            let target_run = id(bytes, COMMON_BYTES + 16)?;
            let contact = id(bytes, COMMON_BYTES + 32)?;
            let target_contact = id(bytes, COMMON_BYTES + 48)?;
            let nonce = id(bytes, COMMON_BYTES + 64)?;
            let state = finish_snapshot(bytes, COMMON_BYTES + 80)?;
            Body::Confirm {
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
            }
        }
        4 => {
            let session = id(bytes, COMMON_BYTES)?;
            let target_run = id(bytes, COMMON_BYTES + 16)?;
            let contact = id(bytes, COMMON_BYTES + 32)?;
            let target_contact = id(bytes, COMMON_BYTES + 48)?;
            let nonce = id(bytes, COMMON_BYTES + 64)?;
            let (state, end) = read_snapshot_end(bytes, COMMON_BYTES + 80)?;
            let ack = read_ack(bytes, end, true)?;
            Body::Ready {
                session,
                target_run,
                contact,
                target_contact,
                nonce,
                state,
                ack,
            }
        }
        5 => {
            let session = id(bytes, COMMON_BYTES)?;
            let target_run = id(bytes, COMMON_BYTES + 16)?;
            let (state, end) = read_snapshot_end(bytes, COMMON_BYTES + 32)?;
            let ack = read_ack(bytes, end, true)?;
            Body::State {
                session,
                target_run,
                state,
                ack,
            }
        }
        6 => read_subscriptions(bytes)?,
        _ => return Err(PeerError::MalformedControl),
    };
    Ok(Control {
        from,
        to,
        run,
        body,
    })
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), PeerError> {
    if snapshot.revision == 0
        || snapshot.send_generation == 0
        || snapshot.receive_generation == 0
        || snapshot.send_generation > snapshot.revision
        || snapshot.receive_generation > snapshot.revision
        || snapshot.receive_encodings & !KNOWN_ENCODING_BITS != 0
        || snapshot.send_tokens.len() != snapshot.sources.as_slice().len()
    {
        return Err(PeerError::MalformedControl);
    }
    Ok(())
}

fn validate_grants(revision: u64, grants: &[ReceiveGrant]) -> Result<(), PeerError> {
    if revision == 0
        || grants.len() > MAX_SOURCES
        || grants
            .iter()
            .any(|grant| grant.token == 0 || grant.sender_token == 0 || grant.sender_token & 1 == 0)
        || grants
            .windows(2)
            .any(|pair| pair[0].source >= pair[1].source)
    {
        return Err(PeerError::MalformedControl);
    }
    Ok(())
}

fn read_subscriptions(bytes: &[u8]) -> Result<Body, PeerError> {
    let header = bytes
        .get(COMMON_BYTES..COMMON_BYTES + SUBSCRIPTIONS_HEADER_BYTES)
        .ok_or(PeerError::MalformedControl)?;
    let session = id(bytes, COMMON_BYTES)?;
    let target_run = id(bytes, COMMON_BYTES + 16)?;
    let revision = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let ack = u64::from_le_bytes(header[40..48].try_into().unwrap());
    let count = u16::from_le_bytes(header[48..50].try_into().unwrap()) as usize;
    if count > MAX_SOURCES
        || header[50..56] != [0; 6]
        || bytes.len() != COMMON_BYTES + SUBSCRIPTIONS_HEADER_BYTES + count * GRANT_BYTES
    {
        return Err(PeerError::MalformedControl);
    }
    let mut grants = Vec::with_capacity(count);
    for record in bytes[COMMON_BYTES + SUBSCRIPTIONS_HEADER_BYTES..].chunks_exact(GRANT_BYTES) {
        grants.push(ReceiveGrant {
            source: id(record, 0)?,
            epoch: PublicationEpoch::new(u64::from_le_bytes(record[16..24].try_into().unwrap()))
                .map_err(|_| PeerError::MalformedControl)?,
            sender_token: u32::from_le_bytes(record[24..28].try_into().unwrap()),
            token: u64::from_le_bytes(record[28..36].try_into().unwrap()),
        });
    }
    validate_grants(revision, &grants)?;
    Ok(Body::Subscriptions {
        session,
        target_run,
        revision,
        ack,
        grants,
    })
}

fn write_snapshot(bytes: &mut Vec<u8>, snapshot: &Snapshot) -> Result<(), PeerError> {
    validate_snapshot(snapshot)?;
    bytes.extend_from_slice(&snapshot.revision.to_le_bytes());
    bytes.extend_from_slice(&snapshot.send_generation.to_le_bytes());
    bytes.extend_from_slice(&snapshot.receive_generation.to_le_bytes());
    bytes.push(snapshot.intent.flags());
    bytes.push(snapshot.receive_encodings);
    bytes.extend_from_slice(&(snapshot.sources.as_slice().len() as u16).to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    for (entry, token) in snapshot
        .sources
        .as_slice()
        .iter()
        .zip(&snapshot.send_tokens)
    {
        write_record(bytes, entry, *token);
    }
    Ok(())
}

fn write_record(bytes: &mut Vec<u8>, entry: &SourceEntry, token: u32) {
    bytes.extend_from_slice(entry.id.as_bytes());
    bytes.extend_from_slice(&entry.epoch.get().to_le_bytes());
    bytes.push(entry.kind as u8);
    bytes.push(entry.state as u8);
    if let Some(format) = entry.format {
        bytes.push(format.encoding().wire_tag());
        bytes.push(format.valid_bits());
        bytes.extend_from_slice(&format.sample_rate_hz().to_le_bytes());
        bytes.extend_from_slice(&format.layout().channel_mask().to_le_bytes());
    } else {
        bytes.extend_from_slice(&[0; 10]);
    }
    bytes.extend_from_slice(&token.to_le_bytes());
}

fn write_ack(bytes: &mut Vec<u8>, ack: Ack) {
    bytes.extend_from_slice(&ack.send_generation.to_le_bytes());
    bytes.extend_from_slice(&ack.receive_generation.to_le_bytes());
    bytes.extend_from_slice(&ack.state_revision.to_le_bytes());
}

fn finish_snapshot(bytes: &[u8], offset: usize) -> Result<Snapshot, PeerError> {
    let (snapshot, end) = read_snapshot_end(bytes, offset)?;
    if end != bytes.len() {
        return Err(PeerError::MalformedControl);
    }
    Ok(snapshot)
}

fn read_snapshot_end(bytes: &[u8], offset: usize) -> Result<(Snapshot, usize), PeerError> {
    let header = bytes
        .get(offset..offset + SNAPSHOT_HEADER_BYTES)
        .ok_or(PeerError::MalformedControl)?;
    let revision = u64::from_le_bytes(header[0..8].try_into().unwrap());
    let send_generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let receive_generation = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let intent = Intent::from_flags(header[24])?;
    let receive_encodings = header[25];
    let count = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
    if header[28..32] != [0, 0, 0, 0]
        || receive_encodings & !KNOWN_ENCODING_BITS != 0
        || count > MAX_SOURCES
        || revision == 0
        || send_generation == 0
        || receive_generation == 0
        || send_generation > revision
        || receive_generation > revision
    {
        return Err(PeerError::MalformedControl);
    }
    let records_bytes = count
        .checked_mul(SOURCE_RECORD_BYTES)
        .ok_or(PeerError::MalformedControl)?;
    let records_end = offset
        .checked_add(SNAPSHOT_HEADER_BYTES)
        .and_then(|start| start.checked_add(records_bytes))
        .ok_or(PeerError::MalformedControl)?;
    if bytes.len() < records_end {
        return Err(PeerError::MalformedControl);
    }
    let mut entries = Vec::with_capacity(count);
    let mut send_tokens = Vec::with_capacity(count);
    for index in 0..count {
        let start = offset + SNAPSHOT_HEADER_BYTES + index * SOURCE_RECORD_BYTES;
        entries.push(read_record(&bytes[start..start + SOURCE_RECORD_BYTES])?);
        send_tokens.push(u32::from_le_bytes(
            bytes[start + 36..start + 40].try_into().unwrap(),
        ));
    }
    let sources = Catalogue::from_sorted(entries).map_err(|_| PeerError::MalformedControl)?;
    Ok((
        Snapshot {
            revision,
            send_generation,
            receive_generation,
            intent,
            receive_encodings,
            sources,
            send_tokens,
        },
        records_end,
    ))
}

fn read_record(bytes: &[u8]) -> Result<SourceEntry, PeerError> {
    if bytes.len() != SOURCE_RECORD_BYTES {
        return Err(PeerError::MalformedControl);
    }
    let id = rlb_model::SourceId::from_bytes(bytes[0..16].try_into().unwrap())
        .map_err(|_| PeerError::MalformedControl)?;
    let epoch = PublicationEpoch::new(u64::from_le_bytes(bytes[16..24].try_into().unwrap()))
        .map_err(|_| PeerError::MalformedControl)?;
    let kind = match bytes[24] {
        1 => SourceKind::System,
        2 => SourceKind::Microphone,
        _ => return Err(PeerError::MalformedControl),
    };
    let state = match bytes[25] {
        1 => PublicationState::Disabled,
        2 => PublicationState::Unavailable,
        3 => PublicationState::Active,
        _ => return Err(PeerError::MalformedControl),
    };
    let format = if state == PublicationState::Active {
        Some(read_format(bytes)?)
    } else if bytes[26..36] == [0; 10] {
        None
    } else {
        return Err(PeerError::MalformedControl);
    };
    let entry = SourceEntry {
        id,
        epoch,
        kind,
        state,
        format,
    };
    entry.validate().map_err(|_| PeerError::MalformedControl)?;
    Ok(entry)
}

fn read_format(bytes: &[u8]) -> Result<PcmFormat, PeerError> {
    let encoding = match bytes[26] {
        1 => PcmEncoding::F32Le,
        2 => PcmEncoding::Signed32Le,
        3 => PcmEncoding::F64Le,
        _ => return Err(PeerError::MalformedControl),
    };
    let mask = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
    let layout = ChannelLayout::from_mask(mask.count_ones() as u16, mask)
        .map_err(|_| PeerError::MalformedControl)?;
    PcmFormat::new(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), layout, encoding, bytes[27])
        .map_err(|_| PeerError::MalformedControl)
}

fn read_ack(bytes: &[u8], offset: usize, terminal: bool) -> Result<Ack, PeerError> {
    let end = offset + ACK_BYTES;
    if bytes.len() < end || (terminal && bytes.len() != end) {
        return Err(PeerError::MalformedControl);
    }
    let ack = Ack {
        send_generation: u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
        receive_generation: u64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().unwrap()),
        state_revision: u64::from_le_bytes(bytes[offset + 16..offset + 24].try_into().unwrap()),
    };
    let zeros = u8::from(ack.send_generation == 0)
        + u8::from(ack.receive_generation == 0)
        + u8::from(ack.state_revision == 0);
    if zeros != 0 && zeros != 3
        || ack.send_generation > ack.state_revision
        || ack.receive_generation > ack.state_revision
    {
        return Err(PeerError::MalformedControl);
    }
    Ok(ack)
}

fn id<T>(bytes: &[u8], offset: usize) -> Result<T, PeerError>
where
    T: TryFrom<[u8; 16]>,
{
    let slice = bytes
        .get(offset..offset + 16)
        .ok_or(PeerError::MalformedControl)?;
    T::try_from(slice.try_into().unwrap()).map_err(|_| PeerError::MalformedControl)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(byte: u8) -> SourceEntry {
        SourceEntry {
            id: SourceId::from_bytes([byte; 16]).unwrap(),
            epoch: PublicationEpoch::new(1).unwrap(),
            kind: SourceKind::System,
            state: PublicationState::Unavailable,
            format: None,
        }
    }

    fn control(body: Body) -> Control {
        Control {
            from: MemberId::from_bytes([1; 16]).unwrap(),
            to: MemberId::from_bytes([2; 16]).unwrap(),
            run: RunId::from_bytes([3; 16]).unwrap(),
            body,
        }
    }

    fn subscriptions(grants: Vec<ReceiveGrant>) -> Control {
        control(Body::Subscriptions {
            session: DirectSessionId::from_bytes([4; 16]).unwrap(),
            target_run: RunId::from_bytes([5; 16]).unwrap(),
            revision: 1,
            ack: 0,
            grants,
        })
    }

    fn grant(byte: u8) -> ReceiveGrant {
        ReceiveGrant {
            source: source(byte).id,
            epoch: PublicationEpoch::new(7).unwrap(),
            sender_token: 1,
            token: u64::from(byte),
        }
    }

    #[test]
    fn subscriptions_bound_sorted_grants_and_round_trip_exactly() {
        let grants: Vec<_> = (1..=25).map(grant).collect();
        let bytes = encode(&subscriptions(grants.clone())).unwrap();
        assert_eq!(bytes.len(), 1012);
        assert_eq!(&bytes[..4], b"RLC5");
        let decoded = decode(&bytes).unwrap();
        assert!(decoded.body.state().is_none());
        match decoded.body {
            Body::Subscriptions {
                revision,
                ack,
                grants: decoded,
                ..
            } => {
                assert_eq!(revision, 1);
                assert_eq!(ack, 0);
                assert_eq!(decoded, grants);
            }
            _ => panic!("expected subscriptions"),
        }
        assert_eq!(encode(&subscriptions(Vec::new())).unwrap().len(), 112);
        assert!(encode(&subscriptions((1..=26).map(grant).collect())).is_err());
        assert!(encode(&subscriptions(vec![grant(1), grant(1)])).is_err());
        assert!(encode(&subscriptions(vec![grant(2), grant(1)])).is_err());
        for sender_token in [0, 2] {
            assert!(
                encode(&subscriptions(vec![ReceiveGrant {
                    sender_token,
                    ..grant(1)
                }]))
                .is_err()
            );
        }
        assert!(
            encode(&subscriptions(vec![ReceiveGrant {
                token: 0,
                ..grant(1)
            }]))
            .is_err()
        );
        let mut invalid = subscriptions(Vec::new());
        if let Body::Subscriptions { revision, .. } = &mut invalid.body {
            *revision = 0;
        }
        assert!(encode(&invalid).is_err());
    }

    #[test]
    fn subscriptions_reject_truncation_trailing_bytes_and_invalid_fields() {
        let bytes = encode(&subscriptions(vec![grant(1), grant(2)])).unwrap();
        for length in 0..bytes.len() {
            assert!(decode(&bytes[..length]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        for (offset, length) in [(88, 8), (112, 16), (128, 8), (136, 4), (140, 8)] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + length].fill(0);
            assert!(decode(&invalid).is_err());
        }
        let mut reserved = bytes.clone();
        reserved[106] = 1;
        assert!(decode(&reserved).is_err());
        let mut too_many = bytes.clone();
        too_many[104..106].copy_from_slice(&26_u16.to_le_bytes());
        assert!(decode(&too_many).is_err());
        let mut duplicate = bytes.clone();
        duplicate[148..164].copy_from_slice(&bytes[112..128]);
        assert!(decode(&duplicate).is_err());
        let mut even_sender_token = bytes.clone();
        even_sender_token[136..140].copy_from_slice(&2_u32.to_le_bytes());
        assert!(decode(&even_sender_token).is_err());
        let mut old_version = bytes;
        old_version[3] = b'3';
        assert!(decode(&old_version).is_err());
    }

    #[test]
    fn snapshot_source_tokens_use_existing_record_tail_and_bound_encoding() {
        let sources = Catalogue::new((1..=25).map(source).collect()).unwrap();
        let mut state = Snapshot::with_send_tokens(
            Intent {
                send: true,
                receive: false,
            },
            sources,
            (1..=25).collect(),
        )
        .unwrap();
        assert_eq!(state.send_token(source(4).id), 4);
        assert_eq!(state.send_token(source(26).id), 0);
        let ready = control(Body::Ready {
            session: DirectSessionId::from_bytes([4; 16]).unwrap(),
            target_run: RunId::from_bytes([5; 16]).unwrap(),
            contact: HelloNonce::from_bytes([6; 16]).unwrap(),
            target_contact: HelloNonce::from_bytes([7; 16]).unwrap(),
            nonce: BindNonce::from_bytes([8; 16]).unwrap(),
            ack: state.ack(),
            state: state.clone(),
        });
        let bytes = encode(&ready).unwrap();
        assert_eq!(bytes.len(), 1192);
        let first_token = COMMON_BYTES + 80 + SNAPSHOT_HEADER_BYTES + 36;
        assert_eq!(&bytes[first_token..first_token + 4], &1_u32.to_le_bytes());
        assert_eq!(decode(&bytes).unwrap().body.state(), Some(&state));
        state.send_tokens.pop();
        assert!(
            encode(&control(Body::Hello {
                contact: HelloNonce::from_bytes([6; 16]).unwrap(),
                state,
            }))
            .is_err()
        );
        assert!(
            Snapshot::with_send_tokens(
                Intent::default(),
                Catalogue::new(vec![source(1)]).unwrap(),
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn source_tokens_start_from_intent_and_only_advance_for_retained_sources() {
        let sources = Catalogue::new(vec![source(1)]).unwrap();
        assert_eq!(
            Snapshot::new(Intent::default(), sources.clone()).send_tokens,
            [0]
        );
        let previous = Snapshot::new(
            Intent {
                send: true,
                receive: false,
            },
            sources,
        );
        assert_eq!(previous.send_tokens, [1]);
        let mut next = previous.clone();
        next.revision = 2;
        next.send_tokens[0] = 3;
        assert!(next.follows(&previous));
        next.send_tokens[0] = 1;
        assert!(next.follows(&previous));
        next.send_tokens[0] = 0;
        assert!(!next.follows(&previous));
        next.send_tokens.clear();
        assert!(!next.follows(&previous));
    }
}
