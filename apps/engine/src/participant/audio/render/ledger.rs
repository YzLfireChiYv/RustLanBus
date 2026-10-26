//! Source-time retirement.
//!
//! `closed_before` and `high_water` live outside replaceable kernels and
//! output generations. Both move only forward.

use rlb_peer::PcmFormat;

use super::contract::{Permission, RenderError, TimelineId, frame_end};

#[derive(Clone, Debug)]
pub(crate) struct Ledger {
    pub timeline: TimelineId,
    pub permission: Permission,
    pub format: PcmFormat,
    pub closed_before: u64,
    pub high_water: u64,
}

impl Ledger {
    pub(crate) fn new(timeline: TimelineId, permission: Permission, format: PcmFormat) -> Self {
        Self {
            timeline,
            permission,
            format,
            closed_before: 0,
            high_water: 0,
        }
    }

    pub(crate) fn note_extent(&mut self, start: u64, frames: u64) -> Result<u64, RenderError> {
        let end = frame_end(start, frames)?;
        if end > self.high_water {
            self.high_water = end;
        }
        Ok(self.high_water)
    }

    pub(crate) fn close_through(&mut self, end: u64) {
        if end > self.closed_before {
            self.closed_before = end;
        }
    }

    /// Retire a discarded prefix. The floor never moves backward and never
    /// passes the validated extent.
    pub(crate) fn discard_through(&mut self, end: u64) {
        let end = end.min(self.high_water.max(self.closed_before));
        self.close_through(end);
    }

    #[must_use]
    pub(crate) fn reserve_frames(&self) -> i64 {
        (i128::from(self.high_water) - i128::from(self.closed_before))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::super::contract::{Permission, TimelineId};
    use super::*;
    use rlb_model::{RunId, SourceId, StreamEpoch};
    use rlb_peer::{ChannelLayout, PcmEncoding, PcmFormat};

    fn timeline() -> TimelineId {
        TimelineId::Local {
            run: RunId::from_bytes([1; 16]).expect("run"),
            source: SourceId::from_bytes([2; 16]).expect("source"),
            acquisition: StreamEpoch::from_bytes([3; 16]).expect("epoch"),
        }
    }

    fn ledger() -> Ledger {
        Ledger::new(
            timeline(),
            Permission::Local {
                monitor_revision: 1,
                capture_instance: 7,
            },
            PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).expect("format"),
        )
    }

    #[test]
    fn floor_and_extent_move_only_forward() {
        let mut ledger = ledger();
        ledger.note_extent(10, 5).unwrap();
        ledger.close_through(12);
        ledger.note_extent(0, 4).unwrap();
        ledger.close_through(9);
        assert_eq!(ledger.high_water, 15);
        assert_eq!(ledger.closed_before, 12);
        ledger.discard_through(14);
        assert_eq!(ledger.closed_before, 14);
        ledger.discard_through(2);
        assert_eq!(ledger.closed_before, 14);
        assert!(ledger.note_extent(u64::MAX - 1, 4).is_err());
        assert_eq!(ledger.high_water, 15);
    }
}
