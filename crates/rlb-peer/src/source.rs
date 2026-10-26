//! Validated original-source descriptors for the RLC4/RLP4 transport.
//! Source identities retain the model's opaque 16-byte representation.

use core::fmt;

use rlb_model::SourceId;

use crate::MAX_DATAGRAM_BYTES;

pub const MEDIA_HEADER_BYTES: usize = 80;
pub const SOURCE_RECORD_BYTES: usize = 40;
pub const MAX_SOURCES: usize = 25;
pub const MAX_ORIGINAL_FRAMES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SourceKind {
    System = 1,
    Microphone = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PublicationState {
    Disabled = 1,
    Unavailable = 2,
    Active = 3,
}

pub use rlb_model::pcm::{ChannelLayout, PcmEncoding, PcmFormat};

/// Packet capacity derives from the source's actual sample width and channels.
pub fn max_packet_frames(format: PcmFormat) -> usize {
    (MAX_DATAGRAM_BYTES - MEDIA_HEADER_BYTES) / format.frame_bytes()
}

/// A publication incarnation within one process run and persistent source.
/// This numeric wire epoch is separate from the local acquisition StreamEpoch.
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PublicationEpoch(core::num::NonZeroU64);

impl PublicationEpoch {
    pub fn new(value: u64) -> Result<Self, SourceError> {
        core::num::NonZeroU64::new(value)
            .map(Self)
            .ok_or(SourceError::InvalidEpoch)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceEntry {
    pub id: SourceId,
    pub epoch: PublicationEpoch,
    pub kind: SourceKind,
    pub state: PublicationState,
    pub format: Option<PcmFormat>,
}

impl SourceEntry {
    pub fn validate(self) -> Result<(), SourceError> {
        if (self.state == PublicationState::Active) != self.format.is_some() {
            return Err(SourceError::InvalidEntry);
        }
        Ok(())
    }
}

/// A bounded catalogue with unique, bytewise-sorted persistent source IDs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Catalogue(Vec<SourceEntry>);

impl Catalogue {
    /// Canonicalizes caller order. Wire parsers use `from_sorted` instead.
    pub fn new(mut entries: Vec<SourceEntry>) -> Result<Self, SourceError> {
        if entries.len() > MAX_SOURCES {
            return Err(SourceError::Capacity);
        }
        entries.sort_unstable_by_key(|entry| entry.id);
        Self::from_sorted(entries)
    }

    /// Requires the received order to already be strictly increasing.
    /// This validates semantic records; wire reserved/zero fields are checked
    /// separately before constructing these records.
    pub fn from_sorted(entries: Vec<SourceEntry>) -> Result<Self, SourceError> {
        Self::validate_sorted(&entries)?;
        Ok(Self(entries))
    }

    pub fn validate_sorted(entries: &[SourceEntry]) -> Result<(), SourceError> {
        if entries.len() > MAX_SOURCES {
            return Err(SourceError::Capacity);
        }
        for entry in entries {
            entry.validate()?;
        }
        if entries.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(SourceError::UnsortedOrDuplicate);
        }
        Ok(())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[SourceEntry] {
        &self.0
    }

    #[must_use]
    pub fn source(&self, id: SourceId) -> Option<&SourceEntry> {
        self.0
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
            .map(|index| &self.0[index])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceError {
    InvalidFormat,
    InvalidEpoch,
    InvalidEntry,
    UnsortedOrDuplicate,
    Capacity,
    DuplicateSource,
    UnknownSource,
    PublicationDisabled,
    UnsupportedFormat,
    StaleBinding,
    InvalidOriginal,
    StorageTooSmall,
    NonFiniteSample,
    TimelineWentBackwards,
    CounterExhausted,
    Quiesced,
}

impl From<rlb_model::pcm::PcmError> for SourceError {
    fn from(error: rlb_model::pcm::PcmError) -> Self {
        use rlb_model::pcm::PcmError;
        match error {
            PcmError::InvalidFormat => Self::InvalidFormat,
            PcmError::InvalidSamples | PcmError::PrecisionLoss => Self::InvalidOriginal,
            PcmError::StorageTooSmall => Self::StorageTooSmall,
            PcmError::NonFiniteSample => Self::NonFiniteSample,
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidFormat => "invalid original PCM descriptor",
            Self::InvalidEpoch => "publication epoch zero is reserved",
            Self::InvalidEntry => "publication state and format disagree",
            Self::UnsortedOrDuplicate => "source IDs must be strictly increasing",
            Self::Capacity => "source catalogue capacity exceeded",
            Self::DuplicateSource => "source identity is already registered",
            Self::UnknownSource => "source identity is not registered",
            Self::PublicationDisabled => "source publication is disabled",
            Self::UnsupportedFormat => "this publisher currently accepts original F32 only",
            Self::StaleBinding => "acquisition or publication binding is no longer current",
            Self::InvalidOriginal => "invalid original frame span",
            Self::StorageTooSmall => "prepared storage cannot hold this original",
            Self::NonFiniteSample => "original PCM contains a nonfinite sample",
            Self::TimelineWentBackwards => "original span precedes the accepted source timeline",
            Self::CounterExhausted => "source counter exhausted; publisher is quiesced",
            Self::Quiesced => "publisher is quiesced and requires a fresh run",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SourceError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_widths_and_source_catalogue_bounds_are_exact() {
        for (encoding, valid_bits, layout, maximum) in [
            (PcmEncoding::F32Le, 32, ChannelLayout::Mono, 280),
            (PcmEncoding::F32Le, 32, ChannelLayout::StereoLeftRight, 140),
            (PcmEncoding::Signed32Le, 24, ChannelLayout::Mono, 280),
            (
                PcmEncoding::Signed32Le,
                32,
                ChannelLayout::StereoLeftRight,
                140,
            ),
            (PcmEncoding::F64Le, 64, ChannelLayout::Mono, 140),
            (PcmEncoding::F64Le, 64, ChannelLayout::StereoLeftRight, 70),
        ] {
            let format = PcmFormat::new(48_000, layout, encoding, valid_bits).unwrap();
            assert_eq!(max_packet_frames(format), maximum);
            assert_eq!(
                MEDIA_HEADER_BYTES
                    + maximum * usize::from(format.channels()) * usize::from(format.sample_bytes()),
                MAX_DATAGRAM_BYTES
            );
        }
        assert!(PcmFormat::new(7_999, ChannelLayout::Mono, PcmEncoding::F32Le, 32).is_err());
        assert!(PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 24).is_err());
        assert!(PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::Signed32Le, 0).is_err());
        assert!(PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::Signed32Le, 33).is_err());
        assert!(PublicationEpoch::new(0).is_err());
        assert_eq!(192 + SOURCE_RECORD_BYTES * MAX_SOURCES, 1192);
        const { assert!(192 + SOURCE_RECORD_BYTES * (MAX_SOURCES + 1) > MAX_DATAGRAM_BYTES) };

        let make_entry = |byte| SourceEntry {
            id: SourceId::from_bytes([byte; 16]).unwrap(),
            epoch: PublicationEpoch::new(1).unwrap(),
            kind: SourceKind::System,
            state: PublicationState::Unavailable,
            format: None,
        };
        let entries = vec![make_entry(2), make_entry(1)];
        assert!(Catalogue::from_sorted(entries.clone()).is_err());
        let catalogue = Catalogue::new(entries).unwrap();
        assert_eq!(catalogue.as_slice()[0].id, make_entry(1).id);
        assert!(Catalogue::new(vec![make_entry(1), make_entry(1)]).is_err());
        assert!(Catalogue::new((1..=26).map(make_entry).collect()).is_err());
        assert!(
            Catalogue::new(vec![SourceEntry {
                state: PublicationState::Active,
                ..make_entry(1)
            }])
            .is_err()
        );
    }
}
