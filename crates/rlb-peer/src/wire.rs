use rlb_model::{BindNonce, DirectSessionId, HelloNonce, MemberId, RunId};
use serde::{Deserialize, Serialize};

use crate::{MAX_DATAGRAM_BYTES, PeerError};

pub(crate) const CONTROL_MAGIC: &[u8; 4] = b"RLC2";

/// Persisted user choices for this particular peer; playback mute is separate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub send: bool,
    pub receive: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub intent: Intent,
    pub revision: u64,
    pub send_generation: u64,
    pub receive_generation: u64,
}

impl Snapshot {
    pub fn new(intent: Intent) -> Self {
        Self {
            intent,
            revision: 1,
            send_generation: 1,
            receive_generation: 1,
        }
    }

    pub fn valid(self) -> bool {
        self.revision > 0
            && self.send_generation > 0
            && self.receive_generation > 0
            && self.send_generation <= self.revision
            && self.receive_generation <= self.revision
    }

    pub fn follows(self, previous: Self) -> bool {
        self.valid()
            && self.revision > previous.revision
            && self.send_generation >= previous.send_generation
            && self.receive_generation >= previous.receive_generation
            && (self.send_generation != previous.send_generation
                || self.intent.send == previous.intent.send)
            && (self.receive_generation != previous.receive_generation
                || self.intent.receive == previous.intent.receive)
    }

    pub fn ack(self) -> Ack {
        Ack {
            send_generation: self.send_generation,
            receive_generation: self.receive_generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ack {
    pub send_generation: u64,
    pub receive_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Control {
    pub from: MemberId,
    pub to: MemberId,
    pub run: RunId,
    pub body: Body,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
}

impl Body {
    pub fn state(&self) -> Snapshot {
        match self {
            Self::Hello { state, .. }
            | Self::Offer { state, .. }
            | Self::Confirm { state, .. }
            | Self::Ready { state, .. }
            | Self::State { state, .. } => *state,
        }
    }
}

pub(crate) fn encode(control: &Control) -> Result<Vec<u8>, PeerError> {
    let json = serde_json::to_vec(control).map_err(|_| PeerError::MalformedControl)?;
    if json.len() + CONTROL_MAGIC.len() > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    let mut bytes = Vec::with_capacity(CONTROL_MAGIC.len() + json.len());
    bytes.extend_from_slice(CONTROL_MAGIC);
    bytes.extend_from_slice(&json);
    Ok(bytes)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Control, PeerError> {
    if bytes.len() > MAX_DATAGRAM_BYTES {
        return Err(PeerError::DatagramTooLarge);
    }
    if !bytes.starts_with(CONTROL_MAGIC) {
        return Err(PeerError::MalformedControl);
    }
    let control: Control =
        serde_json::from_slice(&bytes[4..]).map_err(|_| PeerError::MalformedControl)?;
    if !control.body.state().valid() {
        return Err(PeerError::MalformedControl);
    }
    Ok(control)
}
