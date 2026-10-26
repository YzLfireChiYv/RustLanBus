//! Identities and directed relationships shared by publishers and consumers.
//!
//! An address locates a member; a name labels it. Neither substitutes for an
//! identity. References describe data and carry no permission by themselves.

use rlb_model::{MemberId, RunId, SourceId, StreamEpoch};
use serde::{Deserialize, Serialize};

use crate::PublicationEpoch;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemberRun {
    pub member: MemberId,
    pub run: RunId,
}

/// A source slot belongs to its immediate publishing member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub member: MemberId,
    pub source: SourceId,
}

/// One continuous format/timeline in a member's current process run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicationRef {
    pub owner: MemberRun,
    pub source: SourceId,
    pub epoch: PublicationEpoch,
}

impl PublicationRef {
    pub const fn source_ref(self) -> SourceRef {
        SourceRef {
            member: self.owner.member,
            source: self.source,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeliveryRoute {
    pub publication: PublicationRef,
    pub receiver: MemberRun,
}

/// Describes the input to a publication; system/microphone is a separate role.
/// Received inputs are explicit and do not recursively create return routes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublicationInput {
    Capture {
        source: SourceId,
        acquisition: StreamEpoch,
    },
    Received {
        publication: PublicationRef,
    },
}

/// Frame correspondence for a value-preserving return publication. This is
/// metadata for a future control extension, not an additional PCM wire header.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpstreamMapping {
    pub input: PublicationRef,
    pub input_first_frame: u64,
    pub output_first_frame: u64,
}

impl UpstreamMapping {
    pub fn input_frame(self, output_frame: u64) -> Option<u64> {
        self.input_first_frame
            .checked_add(output_frame.checked_sub(self.output_first_frame)?)
    }
}
