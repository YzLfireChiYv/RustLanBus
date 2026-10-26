//! A bounded, direct-rate timeline for one original PCM source.
//!
//! Source frame numbers establish ordering. Local arrival times only enforce
//! freshness; they do not measure end-to-end or audible latency. Independent
//! capture and render clocks require a separate, qualified clock adapter.

use thiserror::Error;

/// Storage and freshness policy prepared outside the render path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectPlayoutConfig {
    pub sample_rate: u32,
    pub channels: u8,
    pub capacity_frames: usize,
    /// Desired queued source span when initially anchored or recovering.
    pub target_backlog_frames: usize,
    /// A larger queued span causes a forward jump to the target window.
    pub max_backlog_frames: usize,
    /// Maximum frames accepted by either one push or one render operation.
    pub max_operation_frames: usize,
    /// Frames with older local arrival observations are discarded.
    pub arrival_timeout_ms: u64,
}

/// An immutable packet borrowed from the source or network worker.
#[derive(Clone, Copy, Debug)]
pub struct PcmPacket<'a> {
    /// Strictly increasing per-source reset generation; zero is valid.
    pub epoch: u64,
    pub start_frame: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub samples: &'a [f32],
}

/// Validation failures leave both storage and output untouched.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PlayoutError {
    #[error("invalid direct PCM playout configuration")]
    InvalidConfig,
    #[error("packet format differs from the prepared direct-rate format")]
    FormatMismatch,
    #[error("interleaved sample count must contain complete frames")]
    InvalidShape,
    #[error("operation exceeds the prepared frame bound")]
    OperationTooLarge,
    #[error("source frame range overflows u64")]
    FrameRangeOverflow,
    #[error("PCM contains a non-finite sample")]
    NonFiniteSample,
    #[error("source epoch {received} precedes current epoch {current}")]
    OldEpoch { received: u64, current: u64 },
    #[error("local time {received} precedes prior observation {previous}")]
    TimeWentBackwards { received: u64, previous: u64 },
    #[error("packet arrival {arrival} exceeds local observation {observed}")]
    FutureArrival { arrival: u64, observed: u64 },
}

/// Cumulative frame counts, suitable for display outside the callback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlayoutStats {
    pub accepted_frames: u64,
    pub late_frames: u64,
    pub duplicate_frames: u64,
    pub capacity_dropped_frames: u64,
    pub backlog_dropped_frames: u64,
    pub stall_dropped_frames: u64,
    pub expired_frames: u64,
    pub expired_input_frames: u64,
    pub epoch_dropped_frames: u64,
    pub rendered_frames: u64,
    pub silent_frames: u64,
    pub reanchors: u64,
    pub epoch_changes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlayoutPushReport {
    /// Frames copied into storage, including any subsequently trimmed window.
    pub accepted_frames: usize,
    pub late_frames: usize,
    pub duplicate_frames: usize,
    pub capacity_dropped_frames: usize,
    pub backlog_dropped_frames: usize,
    pub expired_frames: usize,
    /// Frames refused because this packet's actual local arrival is too old.
    pub expired_input_frames: usize,
    pub epoch_dropped_frames: usize,
    pub reanchored: bool,
    pub epoch_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlayoutRenderReport {
    pub rendered_frames: usize,
    pub silent_frames: usize,
    pub expired_frames: usize,
    pub backlog_dropped_frames: usize,
    pub stall_dropped_frames: usize,
    /// Source positions skipped after freshness expiry or a rendering stall.
    pub skipped_source_frames: u64,
    pub reanchored: bool,
    pub epoch: Option<u64>,
    pub start_frame: Option<u64>,
    pub next_frame: Option<u64>,
    pub newest_frame_end: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameTag {
    frame: Option<u64>,
    arrival_ms: u64,
}

/// One source's preallocated PCM ring, with independent reset and drop policy.
///
/// `push` and `render` perform bounded work without allocation or locking.
/// Accepted finite samples are copied bit-for-bit at unity gain. Missing frames
/// become digital silence, and the consumed source position keeps advancing.
/// Old packets cannot restart a source after silence or a freshness timeout.
///
/// The caller supplies one shared, nondecreasing local millisecond clock to all
/// operations. Format changes require replacing this instance outside rendering.
#[derive(Debug)]
pub struct DirectPcmPlayout {
    config: DirectPlayoutConfig,
    samples: Vec<f32>,
    tags: Vec<FrameTag>,
    epoch: Option<u64>,
    newest_end: Option<u64>,
    retired_before: u64,
    cursor: Option<u64>,
    last_now_ms: Option<u64>,
    last_fresh_ms: Option<u64>,
    last_render: Option<(u64, usize, Option<u64>)>,
    stats: PlayoutStats,
}

impl DirectPcmPlayout {
    pub fn new(config: DirectPlayoutConfig) -> Result<Self, PlayoutError> {
        let valid = (8_000..=384_000).contains(&config.sample_rate)
            && matches!(config.channels, 1 | 2)
            && config.capacity_frames > 0
            && config.target_backlog_frames > 0
            && config.target_backlog_frames <= config.max_backlog_frames
            && config.max_backlog_frames <= config.capacity_frames
            && config.max_operation_frames > 0
            && config.max_operation_frames <= config.capacity_frames
            && config.arrival_timeout_ms > 0;
        if !valid {
            return Err(PlayoutError::InvalidConfig);
        }
        let sample_capacity = config
            .capacity_frames
            .checked_mul(usize::from(config.channels))
            .filter(|capacity| *capacity <= isize::MAX as usize / size_of::<f32>())
            .ok_or(PlayoutError::InvalidConfig)?;
        if config.capacity_frames > isize::MAX as usize / size_of::<FrameTag>() {
            return Err(PlayoutError::InvalidConfig);
        }
        Ok(Self {
            config,
            samples: vec![0.0; sample_capacity],
            tags: vec![FrameTag::default(); config.capacity_frames],
            epoch: None,
            newest_end: None,
            retired_before: 0,
            cursor: None,
            last_now_ms: None,
            last_fresh_ms: None,
            last_render: None,
            stats: PlayoutStats::default(),
        })
    }

    #[must_use]
    pub const fn config(&self) -> DirectPlayoutConfig {
        self.config
    }

    #[must_use]
    pub const fn stats(&self) -> PlayoutStats {
        self.stats
    }

    /// Accepts current-source frames, rejecting malformed data before mutation.
    pub fn push(
        &mut self,
        packet: PcmPacket<'_>,
        now_ms: u64,
    ) -> Result<PlayoutPushReport, PlayoutError> {
        self.push_received(packet, now_ms, now_ms)
    }

    /// Accepts queued PCM while retaining its true local arrival observation.
    ///
    /// `now_ms` is the current operation time and must remain nondecreasing.
    /// `arrival_ms` may precede earlier operations, and must be at most `now_ms`.
    /// Already-expired input retires its source range without refreshing stored
    /// audio or discarding newer source frames beyond that range.
    pub fn push_received(
        &mut self,
        packet: PcmPacket<'_>,
        arrival_ms: u64,
        now_ms: u64,
    ) -> Result<PlayoutPushReport, PlayoutError> {
        let frames = self.validate_packet(packet)?;
        self.validate_now(now_ms)?;
        if arrival_ms > now_ms {
            return Err(PlayoutError::FutureArrival {
                arrival: arrival_ms,
                observed: now_ms,
            });
        }
        if let Some(current) = self.epoch {
            if packet.epoch < current {
                return Err(PlayoutError::OldEpoch {
                    received: packet.epoch,
                    current,
                });
            }
        }
        let mut report = PlayoutPushReport::default();
        if self.epoch != Some(packet.epoch) {
            report.epoch_dropped_frames = self.reset_epoch(packet.epoch, packet.start_frame);
            report.epoch_changed = true;
        }
        self.last_now_ms = Some(now_ms);
        let (expired, _, changed) = self.expire_storage(now_ms);
        report.expired_frames = expired;
        report.reanchored = changed;

        if now_ms - arrival_ms >= self.config.arrival_timeout_ms {
            let end = packet.start_frame + frames as u64;
            self.newest_end = Some(self.newest_end.unwrap_or(0).max(end));
            let (_, dropped, changed) = self.reanchor(end);
            report.expired_frames += dropped;
            report.expired_input_frames = frames;
            report.reanchored |= changed;
            self.stats.expired_frames = self.stats.expired_frames.saturating_add(dropped as u64);
            self.stats.expired_input_frames = self
                .stats
                .expired_input_frames
                .saturating_add(frames as u64);
            return Ok(report);
        }

        let channels = usize::from(self.config.channels);
        for offset in 0..frames {
            let frame = packet.start_frame + offset as u64;
            if frame < self.retired_before {
                report.late_frames += 1;
                continue;
            }
            let slot = self.slot(frame);
            if self.tags[slot].frame == Some(frame) {
                report.duplicate_frames += 1;
                continue;
            }
            if self.tags[slot].frame.is_some() {
                report.capacity_dropped_frames += 1;
            }
            let input = offset * channels;
            let output = slot * channels;
            self.samples[output..output + channels]
                .copy_from_slice(&packet.samples[input..input + channels]);
            self.tags[slot] = FrameTag {
                frame: Some(frame),
                arrival_ms,
            };
            self.newest_end = Some(self.newest_end.unwrap_or(0).max(frame + 1));
            report.accepted_frames += 1;
        }
        if report.accepted_frames > 0 {
            self.last_fresh_ms = Some(self.last_fresh_ms.unwrap_or(0).max(arrival_ms));
            let newest = self
                .newest_end
                .expect("accepted frames establish a watermark");
            let current = self.cursor.unwrap_or(self.retired_before);
            if newest.saturating_sub(current) > self.config.max_backlog_frames as u64 {
                let target = newest.saturating_sub(self.config.target_backlog_frames as u64);
                let (_, dropped, changed) = self.reanchor(target);
                report.backlog_dropped_frames = dropped;
                report.reanchored |= changed;
            }
        }
        self.stats.accepted_frames = self
            .stats
            .accepted_frames
            .saturating_add(report.accepted_frames as u64);
        self.stats.late_frames = self
            .stats
            .late_frames
            .saturating_add(report.late_frames as u64);
        self.stats.duplicate_frames = self
            .stats
            .duplicate_frames
            .saturating_add(report.duplicate_frames as u64);
        self.stats.capacity_dropped_frames = self
            .stats
            .capacity_dropped_frames
            .saturating_add(report.capacity_dropped_frames as u64);
        self.stats.backlog_dropped_frames = self
            .stats
            .backlog_dropped_frames
            .saturating_add(report.backlog_dropped_frames as u64);
        Ok(report)
    }

    /// Renders complete interleaved frames at the prepared rate, with no DSP.
    pub fn render(
        &mut self,
        now_ms: u64,
        output: &mut [f32],
    ) -> Result<PlayoutRenderReport, PlayoutError> {
        let frames = self.validate_shape(output.len(), false)?;
        self.validate_now(now_ms)?;
        // Derive the exact next cursor before touching output or storage.
        // A push may have already moved it; stall recovery uses the prior render
        // observation's position so that the same elapsed time is skipped once.
        let stall_target = if frames > 0 {
            self.stall_target(now_ms)?
        } else {
            None
        };
        let mut planned_cursor = self.cursor;
        if self.storage_expired(now_ms) {
            if let Some(newest) = self.newest_end {
                planned_cursor = Some(planned_cursor.unwrap_or(self.retired_before).max(newest));
            }
        }
        if frames > 0 {
            if let Some(cursor) = planned_cursor {
                if let Some(target) = stall_target {
                    let latest_target = self
                        .newest_end
                        .unwrap_or(cursor)
                        .saturating_sub(self.config.target_backlog_frames as u64);
                    planned_cursor = Some(cursor.max(target).max(latest_target));
                }
            } else if let Some(newest) = self.newest_end {
                if newest.saturating_sub(self.retired_before)
                    >= self.config.target_backlog_frames as u64
                {
                    planned_cursor =
                        Some(newest.saturating_sub(self.config.target_backlog_frames as u64));
                }
            }
        }
        if planned_cursor.is_some_and(|cursor| cursor.checked_add(frames as u64).is_none()) {
            return Err(PlayoutError::FrameRangeOverflow);
        }
        self.last_now_ms = Some(now_ms);
        let (expired, skipped, changed) = self.expire_storage(now_ms);
        let mut report = PlayoutRenderReport {
            expired_frames: expired,
            skipped_source_frames: skipped,
            reanchored: changed,
            epoch: self.epoch,
            newest_frame_end: self.newest_end,
            ..PlayoutRenderReport::default()
        };
        if frames == 0 {
            report.next_frame = self.cursor;
            return Ok(report);
        }
        if let Some(cursor) = self.cursor {
            if let Some(stall_target) = stall_target {
                let latest_target = self
                    .newest_end
                    .unwrap_or(cursor)
                    .saturating_sub(self.config.target_backlog_frames as u64);
                let target = cursor.max(stall_target).max(latest_target);
                let (skipped, dropped, changed) = self.reanchor(target);
                report.skipped_source_frames = report.skipped_source_frames.saturating_add(skipped);
                report.stall_dropped_frames = dropped;
                report.reanchored |= changed;
            }
        } else if let Some(newest) = self.newest_end {
            if newest.saturating_sub(self.retired_before)
                >= self.config.target_backlog_frames as u64
            {
                let target = newest.saturating_sub(self.config.target_backlog_frames as u64);
                let (_, dropped, changed) = self.reanchor(target);
                report.backlog_dropped_frames = dropped;
                report.reanchored |= changed;
            }
        }
        output.fill(0.0);
        report.start_frame = self.cursor;
        if let Some(start) = self.cursor {
            let channels = usize::from(self.config.channels);
            for offset in 0..frames {
                let frame = start + offset as u64;
                let slot = self.slot(frame);
                if self.tags[slot].frame == Some(frame) {
                    if now_ms.saturating_sub(self.tags[slot].arrival_ms)
                        >= self.config.arrival_timeout_ms
                    {
                        report.expired_frames += 1;
                        self.stats.expired_frames = self.stats.expired_frames.saturating_add(1);
                    } else {
                        let input = slot * channels;
                        let destination = offset * channels;
                        output[destination..destination + channels]
                            .copy_from_slice(&self.samples[input..input + channels]);
                        report.rendered_frames += 1;
                    }
                    self.tags[slot].frame = None;
                }
            }
            let next = start + frames as u64;
            self.cursor = Some(next);
            self.retired_before = next;
        }
        report.silent_frames = frames - report.rendered_frames;
        report.next_frame = self.cursor;
        self.last_render = Some((now_ms, frames, self.cursor));
        self.stats.rendered_frames = self
            .stats
            .rendered_frames
            .saturating_add(report.rendered_frames as u64);
        self.stats.silent_frames = self
            .stats
            .silent_frames
            .saturating_add(report.silent_frames as u64);
        self.stats.stall_dropped_frames = self
            .stats
            .stall_dropped_frames
            .saturating_add(report.stall_dropped_frames as u64);
        self.stats.backlog_dropped_frames = self
            .stats
            .backlog_dropped_frames
            .saturating_add(report.backlog_dropped_frames as u64);
        Ok(report)
    }

    fn validate_packet(&self, packet: PcmPacket<'_>) -> Result<usize, PlayoutError> {
        if packet.sample_rate != self.config.sample_rate || packet.channels != self.config.channels
        {
            return Err(PlayoutError::FormatMismatch);
        }
        let frames = self.validate_shape(packet.samples.len(), true)?;
        packet
            .start_frame
            .checked_add(frames as u64)
            .ok_or(PlayoutError::FrameRangeOverflow)?;
        if packet.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(PlayoutError::NonFiniteSample);
        }
        Ok(frames)
    }

    fn validate_shape(&self, samples: usize, nonempty: bool) -> Result<usize, PlayoutError> {
        let channels = usize::from(self.config.channels);
        if !samples.is_multiple_of(channels) || (nonempty && samples == 0) {
            return Err(PlayoutError::InvalidShape);
        }
        let frames = samples / channels;
        if frames > self.config.max_operation_frames {
            return Err(PlayoutError::OperationTooLarge);
        }
        Ok(frames)
    }

    fn validate_now(&self, now_ms: u64) -> Result<(), PlayoutError> {
        if let Some(previous) = self.last_now_ms {
            if now_ms < previous {
                return Err(PlayoutError::TimeWentBackwards {
                    received: now_ms,
                    previous,
                });
            }
        }
        Ok(())
    }

    fn reset_epoch(&mut self, epoch: u64, start: u64) -> usize {
        let mut dropped = 0;
        for tag in &mut self.tags {
            if tag.frame.take().is_some() {
                dropped += 1;
            }
        }
        self.epoch = Some(epoch);
        self.newest_end = None;
        self.retired_before = start;
        self.cursor = None;
        self.last_fresh_ms = None;
        self.last_render = None;
        self.stats.epoch_changes = self.stats.epoch_changes.saturating_add(1);
        self.stats.epoch_dropped_frames = self
            .stats
            .epoch_dropped_frames
            .saturating_add(dropped as u64);
        dropped
    }

    fn slot(&self, frame: u64) -> usize {
        (frame % self.config.capacity_frames as u64) as usize
    }

    fn storage_expired(&self, now_ms: u64) -> bool {
        self.last_fresh_ms
            .is_some_and(|last| now_ms.saturating_sub(last) >= self.config.arrival_timeout_ms)
    }

    fn expire_storage(&mut self, now_ms: u64) -> (usize, u64, bool) {
        if !self.storage_expired(now_ms) {
            return (0, 0, false);
        }
        let mut dropped = 0;
        for tag in &mut self.tags {
            if tag.frame.take().is_some() {
                dropped += 1;
            }
        }
        let (skipped, _, changed) = self
            .newest_end
            .map(|newest| self.reanchor(newest))
            .unwrap_or((0, 0, false));
        self.last_fresh_ms = None;
        self.stats.expired_frames = self.stats.expired_frames.saturating_add(dropped as u64);
        (dropped, skipped, changed)
    }

    fn reanchor(&mut self, target: u64) -> (u64, usize, bool) {
        let previous = self.cursor.unwrap_or(self.retired_before);
        let next = target.max(previous).max(self.retired_before);
        let changed = self.cursor.is_none() || next > previous;
        let mut dropped = 0;
        for tag in &mut self.tags {
            if tag.frame.is_some_and(|frame| frame < next) {
                tag.frame = None;
                dropped += 1;
            }
        }
        self.cursor = Some(next);
        self.retired_before = next;
        if changed {
            self.stats.reanchors = self.stats.reanchors.saturating_add(1);
        }
        (next - previous, dropped, changed)
    }

    fn stall_target(&self, now_ms: u64) -> Result<Option<u64>, PlayoutError> {
        let Some((previous_ms, previous_frames, Some(previous_end))) = self.last_render else {
            return Ok(None);
        };
        let elapsed = u128::from(now_ms.saturating_sub(previous_ms))
            * u128::from(self.config.sample_rate)
            / 1_000;
        let elapsed = u64::try_from(elapsed).unwrap_or(u64::MAX);
        let overdue = elapsed.saturating_sub(previous_frames as u64);
        if overdue > self.config.max_backlog_frames as u64 {
            previous_end
                .checked_add(overdue)
                .map(Some)
                .ok_or(PlayoutError::FrameRangeOverflow)
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playout(timeout: u64) -> DirectPcmPlayout {
        DirectPcmPlayout::new(DirectPlayoutConfig {
            sample_rate: 8_000,
            channels: 2,
            capacity_frames: 32,
            target_backlog_frames: 4,
            max_backlog_frames: 8,
            max_operation_frames: 32,
            arrival_timeout_ms: timeout,
        })
        .unwrap()
    }

    fn packet(epoch: u64, start_frame: u64, samples: &[f32]) -> PcmPacket<'_> {
        PcmPacket {
            epoch,
            start_frame,
            sample_rate: 8_000,
            channels: 2,
            samples,
        }
    }

    #[test]
    fn finite_samples_keep_all_bits_and_invalid_input_has_no_side_effects() {
        let mut source = playout(100);
        let samples = [
            0.0,
            -0.0,
            f32::from_bits(1),
            -f32::from_bits(1),
            1.25,
            -0.75,
            f32::MAX,
            f32::MIN_POSITIVE,
        ];
        source.push(packet(1, 0, &samples), 0).unwrap();
        let before = source.stats();
        assert_eq!(
            source.push(packet(2, 0, &[f32::NAN, 0.0]), 0),
            Err(PlayoutError::NonFiniteSample)
        );
        assert_eq!(
            source.push(packet(2, 0, &[0.0]), 0),
            Err(PlayoutError::InvalidShape)
        );
        assert_eq!(
            source.push(
                PcmPacket {
                    sample_rate: 48_000,
                    ..packet(2, 0, &samples)
                },
                0
            ),
            Err(PlayoutError::FormatMismatch)
        );
        assert_eq!(source.stats(), before);
        let mut output = [9.0; 8];
        let report = source.render(0, &mut output).unwrap();
        assert_eq!(report.rendered_frames, 4);
        assert_eq!(output.map(f32::to_bits), samples.map(f32::to_bits));
        let before = source.stats();
        assert_eq!(
            source.render(0, &mut [9.0; 65]),
            Err(PlayoutError::InvalidShape)
        );
        assert_eq!(source.stats(), before);
    }

    #[test]
    fn duplicates_late_packets_and_silence_cannot_restart_consumed_audio() {
        let mut source = playout(100);
        let samples = [0.25; 8];
        source.push(packet(1, 10, &samples), 0).unwrap();
        assert_eq!(
            source
                .push(packet(1, 10, &samples), 0)
                .unwrap()
                .duplicate_frames,
            4
        );
        let mut output = [0.0; 8];
        source.render(0, &mut output).unwrap();
        assert_eq!(source.render(1, &mut output).unwrap().silent_frames, 4);
        let report = source.push(packet(1, 10, &samples), 1).unwrap();
        assert_eq!(report.late_frames, 4);
        assert_eq!(report.accepted_frames, 0);
        assert_eq!(source.render(2, &mut output).unwrap().next_frame, Some(22));
        assert_eq!(output, [0.0; 8]);
    }

    #[test]
    fn higher_epoch_restarts_only_this_source_and_old_time_is_rejected() {
        let mut source = playout(100);
        let mut other = playout(100);
        source.push(packet(7, 100, &[0.25; 8]), 10).unwrap();
        other.push(packet(7, 100, &[0.75; 8]), 10).unwrap();
        assert!(
            source
                .push(packet(8, 0, &[0.5; 8]), 10)
                .unwrap()
                .epoch_changed
        );
        let before = source.stats();
        assert_eq!(
            source.push(packet(7, 104, &[0.25; 8]), 10),
            Err(PlayoutError::OldEpoch {
                received: 7,
                current: 8
            })
        );
        assert_eq!(
            source.push(packet(9, 0, &[0.25; 8]), 9),
            Err(PlayoutError::TimeWentBackwards {
                received: 9,
                previous: 10
            })
        );
        assert_eq!(source.stats(), before);
        let mut output = [0.0; 8];
        assert_eq!(source.render(10, &mut output).unwrap().start_frame, Some(0));
        assert_eq!(output, [0.5; 8]);
        assert_eq!(
            other.render(10, &mut output).unwrap().start_frame,
            Some(100)
        );
        assert_eq!(output, [0.75; 8]);
    }

    #[test]
    fn overflow_stall_and_expiration_return_to_current_frames() {
        let mut source = playout(100);
        let samples: Vec<f32> = (0..24)
            .flat_map(|frame| [frame as f32, frame as f32])
            .collect();
        let report = source.push(packet(1, 0, &samples), 0).unwrap();
        assert_eq!(report.backlog_dropped_frames, 20);
        let mut output = [0.0; 8];
        assert_eq!(source.render(0, &mut output).unwrap().start_frame, Some(20));
        assert_eq!(output, [20.0, 20.0, 21.0, 21.0, 22.0, 22.0, 23.0, 23.0]);
        source.push(packet(1, 96, &[0.5; 16]), 10).unwrap();
        let report = source.render(10, &mut output).unwrap();
        assert_eq!(report.start_frame, Some(100));
        assert_eq!(report.rendered_frames, 4);
        assert_eq!(output, [0.5; 8]);
        assert_eq!(
            source
                .push(packet(1, 96, &[0.5; 8]), 10)
                .unwrap()
                .late_frames,
            4
        );

        let mut source = playout(5);
        source.push(packet(1, 0, &[0.75; 8]), 0).unwrap();
        assert_eq!(
            source
                .push(packet(1, 0, &[0.25; 8]), 4)
                .unwrap()
                .duplicate_frames,
            4
        );
        let report = source.render(5, &mut output).unwrap();
        assert_eq!(report.expired_frames, 4);
        assert_eq!(report.silent_frames, 4);
        assert_eq!(
            source
                .push(packet(1, 0, &[0.25; 8]), 5)
                .unwrap()
                .late_frames,
            4
        );
        source.push(packet(1, 8, &[0.5; 8]), 5).unwrap();
        assert_eq!(source.render(5, &mut output).unwrap().rendered_frames, 4);
        assert_eq!(output, [0.5; 8]);
    }

    #[test]
    fn queued_arrival_is_preserved_and_expired_input_cannot_refresh_or_replay() {
        let mut source = playout(100);
        let mut output = [9.0; 8];
        source.render(12, &mut output).unwrap();
        assert_eq!(
            source
                .push_received(packet(1, 0, &[0.25; 8]), 10, 12)
                .unwrap()
                .accepted_frames,
            4
        );
        let before = source.stats();
        assert_eq!(
            source.push_received(packet(2, 0, &[0.75; 8]), 13, 12),
            Err(PlayoutError::FutureArrival {
                arrival: 13,
                observed: 12
            })
        );
        assert_eq!(source.stats(), before);
        assert_eq!(source.render(12, &mut output).unwrap().rendered_frames, 4);
        assert_eq!(output, [0.25; 8]);

        // A fresher, later source range remains available when an expired old
        // queued range is processed after it.
        source
            .push_received(packet(1, 12, &[0.75; 8]), 200, 200)
            .unwrap();
        let report = source
            .push_received(packet(1, 4, &[0.5; 8]), 10, 200)
            .unwrap();
        assert_eq!(report.expired_input_frames, 4);
        assert_eq!(report.accepted_frames, 0);
        let report = source.render(200, &mut output).unwrap();
        assert_eq!(report.start_frame, Some(1_504));
        assert_eq!(report.rendered_frames, 0);
        assert_eq!(
            source
                .push_received(packet(1, 4, &[0.5; 8]), 200, 200)
                .unwrap()
                .late_frames,
            4
        );

        // Without a prior running cursor, an expired packet retires only its
        // old source range and retains a fresh range farther ahead.
        let mut source = playout(100);
        source
            .push_received(packet(1, 12, &[0.75; 8]), 200, 200)
            .unwrap();
        source
            .push_received(packet(1, 4, &[0.5; 8]), 10, 200)
            .unwrap();
        assert_eq!(source.render(200, &mut output).unwrap().rendered_frames, 4);
        assert_eq!(output, [0.75; 8]);
    }
}
