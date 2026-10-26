//! Bounded indexed original-PCM window.
//!
//! Samples below the retirement floor are not written again. A span that
//! starts beyond the prepared capacity is reported and does not grow storage.

use super::contract::RenderError;

#[derive(Clone, Debug)]
pub(crate) struct Window {
    channels: usize,
    capacity: usize,
    origin: u64,
    head: usize,
    samples: Vec<f64>,
    present: Vec<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StoreReport {
    pub stored: u64,
    pub duplicate: u64,
    pub stale: u64,
    pub ahead: u64,
}

impl Window {
    pub(crate) fn new(channels: usize, capacity: usize) -> Result<Self, RenderError> {
        if channels == 0 || capacity == 0 {
            return Err(RenderError::Capacity("the source window is empty"));
        }
        Ok(Self {
            channels,
            capacity,
            origin: 0,
            head: 0,
            samples: vec![0.0; capacity.saturating_mul(channels)],
            present: vec![false; capacity],
        })
    }

    #[must_use]
    pub(crate) fn capacity_frames(&self) -> usize {
        self.capacity
    }

    pub(crate) fn origin_frame(&self) -> u64 {
        self.origin
    }

    pub(crate) fn advance_origin(&mut self, new_origin: u64) {
        if new_origin <= self.origin {
            return;
        }
        let shift = new_origin - self.origin;
        if shift >= self.capacity as u64 {
            self.present.fill(false);
            self.origin = new_origin;
            self.head = 0;
            return;
        }
        let shift = shift as usize;
        for offset in 0..shift {
            self.present[(self.head + offset) % self.capacity] = false;
        }
        self.head = (self.head + shift) % self.capacity;
        self.origin = new_origin;
    }

    pub(crate) fn insert(
        &mut self,
        start: u64,
        samples: &[f64],
        floor: u64,
    ) -> Result<StoreReport, RenderError> {
        let channels = self.channels;
        if !samples.len().is_multiple_of(channels) {
            return Err(RenderError::MalformedRange);
        }
        let frames = (samples.len() / channels) as u64;
        let end = start
            .checked_add(frames)
            .ok_or(RenderError::MalformedRange)?;
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(RenderError::NonFinite);
        }
        let mut report = StoreReport {
            stored: 0,
            duplicate: 0,
            stale: 0,
            ahead: 0,
        };
        if end <= self.origin || end <= floor {
            report.stale = frames;
            return Ok(report);
        }
        if start >= self.origin.saturating_add(self.capacity as u64) {
            report.ahead = frames;
            return Ok(report);
        }
        for offset in 0..frames {
            let frame = start + offset;
            if frame < floor || frame < self.origin {
                report.stale += 1;
                continue;
            }
            let Some(slot) = self.slot(frame) else {
                report.ahead += 1;
                continue;
            };
            if self.present[slot] {
                report.duplicate += 1;
                continue;
            }
            for channel in 0..channels {
                let sample = samples[(offset as usize) * channels + channel];
                self.samples[slot * channels + channel] = sample;
            }
            self.present[slot] = true;
            report.stored += 1;
        }
        Ok(report)
    }

    #[must_use]
    pub(crate) fn span_present(&self, start: u64, frames: usize) -> bool {
        (0..frames).all(|offset| {
            start
                .checked_add(offset as u64)
                .and_then(|frame| self.slot(frame))
                .is_some_and(|slot| self.present[slot])
        })
    }

    #[must_use]
    pub(crate) fn missing(&self, start: u64, frames: usize) -> usize {
        (0..frames)
            .filter(|offset| {
                start
                    .checked_add(*offset as u64)
                    .and_then(|frame| self.slot(frame))
                    .is_none_or(|slot| !self.present[slot])
            })
            .count()
    }

    pub(crate) fn stage_f64(
        &self,
        start: u64,
        frames: usize,
        destination: &mut [f64],
    ) -> Result<usize, RenderError> {
        start
            .checked_add(frames as u64)
            .ok_or(RenderError::MalformedRange)?;
        let needed = frames.saturating_mul(self.channels);
        if destination.len() < needed {
            return Err(RenderError::Capacity(
                "the staged conversion buffer is short",
            ));
        }
        let mut missing = 0;
        for offset in 0..frames {
            let frame = start + offset as u64;
            let slot = self.slot(frame);
            for channel in 0..self.channels {
                let sample = match slot {
                    Some(slot) if self.present[slot] => {
                        self.samples[slot * self.channels + channel]
                    }
                    _ => {
                        missing += 1;
                        0.0
                    }
                };
                destination[offset * self.channels + channel] = sample;
            }
        }
        Ok(missing)
    }

    fn slot(&self, frame: u64) -> Option<usize> {
        let offset = frame.checked_sub(self.origin)?;
        if offset >= self.capacity as u64 {
            None
        } else {
            Some((self.head + offset as usize) % self.capacity)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_keep_the_first_sample_and_the_window_does_not_grow() {
        let mut window = Window::new(1, 4).unwrap();
        let first = window.insert(0, &[0.5, 0.25, 0.125, 0.0625], 0).unwrap();
        assert_eq!(first.stored, 4);
        let again = window.insert(1, &[9.0], 0).unwrap();
        assert_eq!(again.duplicate, 1);
        assert_eq!(window.samples[1], 0.25);
        let ahead = window.insert(10, &[1.0, 1.0], 0).unwrap();
        assert_eq!(ahead.ahead, 2);
        assert!(window.span_present(0, 4));
        window.advance_origin(2);
        assert!(window.span_present(2, 2));
        assert!(!window.span_present(0, 1));
        let stale = window.insert(0, &[3.0], 2).unwrap();
        assert_eq!(stale.stale, 1);
        let mut staged = [0.; 4];
        assert_eq!(window.stage_f64(2, 2, &mut staged).unwrap(), 0);
        assert_eq!(&staged[..2], &[0.125, 0.0625]);
        window.insert(4, &[0.03125, 0.015625], 2).unwrap();
        assert_eq!(window.stage_f64(2, 4, &mut staged).unwrap(), 0);
        assert_eq!(staged, [0.125, 0.0625, 0.03125, 0.015625]);
        window.advance_origin(4);
        assert_eq!(window.stage_f64(4, 2, &mut staged).unwrap(), 0);
        assert_eq!(&staged[..2], &[0.03125, 0.015625]);
    }
}
