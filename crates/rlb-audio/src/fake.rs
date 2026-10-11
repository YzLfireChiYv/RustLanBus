use core::mem;

use rlb_clock::{MonotonicDuration, MonotonicInstant};
use rlb_model::{SAMPLE_RATE_HZ, SourceId, StreamEpoch};

use crate::{
    AudioError, BackendStatus, CaptureBackend, CaptureClockDomain, CaptureDiscontinuity,
    CaptureMetadata, CaptureProcessBuffer, CaptureProcessResult, EndpointDescriptor, RenderBackend,
    SourceReadOutcome, SourceReader,
};

/// Deterministic, preallocated capture backend for tests and simulations.
pub struct FakeCapture {
    endpoint: EndpointDescriptor,
    status: BackendStatus,
    source_id: SourceId,
    stream_epoch: StreamEpoch,
    channels: u8,
    clock_domain: CaptureClockDomain,
    initial_capture_time: MonotonicInstant,
    samples: Box<[f32]>,
    total_frames: usize,
    callback_frames: usize,
    sample_cursor: usize,
    source_sample: u64,
    next_discontinuity: CaptureDiscontinuity,
}

impl FakeCapture {
    /// Takes ownership of compact planar source material.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: EndpointDescriptor,
        source_id: SourceId,
        stream_epoch: StreamEpoch,
        channels: u8,
        clock_domain: CaptureClockDomain,
        initial_capture_time: MonotonicInstant,
        planar_samples: Vec<f32>,
        total_frames: usize,
        callback_frames: usize,
    ) -> Result<Self, AudioError> {
        if !matches!(channels, 1 | 2) {
            return Err(AudioError::UnsupportedChannels { channels });
        }
        if total_frames == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "fake_capture_frames",
            });
        }
        if callback_frames == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "fake_capture_callback_frames",
            });
        }
        let required = total_frames.checked_mul(usize::from(channels)).ok_or(
            AudioError::CapacityOverflow {
                field: "fake_capture_samples",
            },
        )?;
        if planar_samples.len() != required {
            return Err(AudioError::PlanarStorageTooShort {
                required,
                actual: planar_samples.len(),
            });
        }
        Ok(Self {
            endpoint,
            status: BackendStatus::Running,
            source_id,
            stream_epoch,
            channels,
            clock_domain,
            initial_capture_time,
            samples: planar_samples.into_boxed_slice(),
            total_frames,
            callback_frames,
            sample_cursor: 0,
            source_sample: 0,
            next_discontinuity: CaptureDiscontinuity::Continuous,
        })
    }

    /// Advances source time without consuming fixture samples.
    ///
    /// The next captured block reports a known discontinuity of this duration.
    pub fn skip_source_frames(&mut self, frames: u64) -> Result<(), AudioError> {
        if frames == 0 {
            return Err(AudioError::EmptyDiscontinuity);
        }
        self.source_sample = self
            .source_sample
            .checked_add(frames)
            .ok_or(AudioError::SampleRangeOverflow)?;
        self.next_discontinuity = CaptureDiscontinuity::KnownGap {
            missing_frames: frames,
        };
        Ok(())
    }

    /// Returns source fixture frame capacity.
    #[must_use]
    pub const fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// Returns the number of fixture frames already emitted.
    #[must_use]
    pub const fn emitted_frames(&self) -> usize {
        self.sample_cursor
    }

    #[cfg(test)]
    fn storage_address(&self) -> usize {
        self.samples.as_ptr() as usize
    }
}

/// Helper reader that fills every plane with a constant sample.
///
/// Tests and offline mix-graph probes use this type as a prepared reader. The
/// value holds no heap beyond `Self`.
pub struct ConstantSourceReader {
    channels: u8,
    values: [f32; 2],
}

impl ConstantSourceReader {
    /// Creates a mono or stereo constant reader.
    pub fn new(channels: u8, values: [f32; 2]) -> Result<Self, AudioError> {
        if !matches!(channels, 1 | 2) {
            return Err(AudioError::UnsupportedChannels { channels });
        }
        Ok(Self { channels, values })
    }
}

impl SourceReader for ConstantSourceReader {
    fn read(
        &mut self,
        planar_output: &mut [f32],
        frames: usize,
        channels: u8,
    ) -> SourceReadOutcome {
        let planes = usize::from(channels.min(self.channels));
        for channel in 0..planes {
            let start = channel * frames;
            if let Some(plane) = planar_output.get_mut(start..start + frames) {
                plane.fill(self.values[channel]);
            }
        }
        SourceReadOutcome {
            frames_written: frames,
        }
    }
}

impl CaptureBackend for FakeCapture {
    fn status(&self) -> BackendStatus {
        self.status
    }

    fn endpoint(&self) -> &EndpointDescriptor {
        &self.endpoint
    }

    fn process(
        &mut self,
        destination: &mut CaptureProcessBuffer<'_>,
    ) -> Result<CaptureProcessResult, AudioError> {
        if self.status == BackendStatus::Stopped {
            return Ok(CaptureProcessResult::EndOfStream);
        }
        if self.status != BackendStatus::Running {
            return Err(AudioError::BackendUnavailable);
        }
        if destination.channel_capacity() < self.channels
            || destination.frame_capacity() < self.callback_frames
        {
            return Err(AudioError::BackendContractViolation);
        }
        let remaining = self.total_frames - self.sample_cursor;
        let frames = remaining.min(self.callback_frames);
        for channel in 0..self.channels {
            let source_start = usize::from(channel) * self.total_frames + self.sample_cursor;
            let source = &self.samples[source_start..source_start + frames];
            let target = destination
                .channel_mut(channel)
                .ok_or(AudioError::BackendContractViolation)?;
            target[..frames].copy_from_slice(source);
        }

        let start_sample = self.source_sample;
        let metadata = CaptureMetadata {
            source_id: self.source_id,
            stream_epoch: self.stream_epoch,
            start_sample,
            frames: u32::try_from(frames).map_err(|_| AudioError::CapacityOverflow {
                field: "fake_capture_callback_frames",
            })?,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: self.channels,
            clock_domain: self.clock_domain,
            capture_time: offset_time(
                self.initial_capture_time,
                self.sample_cursor,
                self.clock_domain,
            ),
            device_position: Some(start_sample),
            discontinuity: mem::take(&mut self.next_discontinuity),
            silent: self.fixture_range_is_silent(self.sample_cursor, frames),
        };
        metadata.validate()?;
        self.sample_cursor += frames;
        self.source_sample = metadata.end_sample()?;
        if self.sample_cursor == self.total_frames {
            self.status = BackendStatus::Stopped;
        }
        Ok(CaptureProcessResult::Captured(metadata))
    }
}

impl FakeCapture {
    fn fixture_range_is_silent(&self, start_frame: usize, frames: usize) -> bool {
        (0..self.channels).all(|channel| {
            let start = usize::from(channel) * self.total_frames + start_frame;
            self.samples[start..start + frames]
                .iter()
                .all(|sample| *sample == 0.0)
        })
    }
}

/// Deterministic, preallocated render backend retaining the latest callback.
pub struct FakeRender {
    endpoint: EndpointDescriptor,
    status: BackendStatus,
    samples: Box<[f32]>,
    frame_capacity: usize,
    last_frames: usize,
    callback_count: u64,
    rendered_frames: u64,
}

impl FakeRender {
    /// Preallocates storage for one maximum-sized stereo callback.
    pub fn new(endpoint: EndpointDescriptor, frame_capacity: usize) -> Result<Self, AudioError> {
        if frame_capacity == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "fake_render_frames",
            });
        }
        let samples = frame_capacity
            .checked_mul(2)
            .ok_or(AudioError::CapacityOverflow {
                field: "fake_render_samples",
            })?;
        Ok(Self {
            endpoint,
            status: BackendStatus::Running,
            samples: vec![0.0; samples].into_boxed_slice(),
            frame_capacity,
            last_frames: 0,
            callback_count: 0,
            rendered_frames: 0,
        })
    }

    /// Stops accepting render callbacks.
    pub fn stop(&mut self) {
        self.status = BackendStatus::Stopped;
    }

    /// Returns the latest complete interleaved callback.
    #[must_use]
    pub fn last_interleaved(&self) -> &[f32] {
        &self.samples[..self.last_frames * 2]
    }

    /// Returns latest callback frame count.
    #[must_use]
    pub const fn last_frames(&self) -> usize {
        self.last_frames
    }

    /// Returns cumulative callback count.
    #[must_use]
    pub const fn callback_count(&self) -> u64 {
        self.callback_count
    }

    /// Returns cumulative rendered frames.
    #[must_use]
    pub const fn rendered_frames(&self) -> u64 {
        self.rendered_frames
    }

    #[cfg(test)]
    fn storage_address(&self) -> usize {
        self.samples.as_ptr() as usize
    }
}

impl RenderBackend for FakeRender {
    fn status(&self) -> BackendStatus {
        self.status
    }

    fn endpoint(&self) -> &EndpointDescriptor {
        &self.endpoint
    }

    fn render(&mut self, interleaved_stereo: &[f32], frames: usize) -> Result<(), AudioError> {
        if self.status != BackendStatus::Running {
            return Err(AudioError::BackendUnavailable);
        }
        if frames > self.frame_capacity {
            return Err(AudioError::RenderCapacityExceeded);
        }
        let required = frames.checked_mul(2).ok_or(AudioError::CapacityOverflow {
            field: "fake_render_samples",
        })?;
        if interleaved_stereo.len() != required {
            return Err(AudioError::RenderStorageLength {
                required,
                actual: interleaved_stereo.len(),
            });
        }
        self.samples[..required].copy_from_slice(interleaved_stereo);
        self.last_frames = frames;
        self.callback_count = self.callback_count.saturating_add(1);
        self.rendered_frames = self.rendered_frames.saturating_add(frames as u64);
        Ok(())
    }
}

fn offset_time(
    initial: MonotonicInstant,
    frame_offset: usize,
    domain: CaptureClockDomain,
) -> MonotonicInstant {
    let ticks = (frame_offset as u128).saturating_mul(u128::from(domain.ticks_per_second().get()))
        / u128::from(SAMPLE_RATE_HZ);
    let Ok(ticks) = u64::try_from(ticks) else {
        return initial;
    };
    match initial.checked_add(MonotonicDuration::from_ticks(ticks)) {
        Some(adjusted) => adjusted,
        None => initial,
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU64;
    use std::sync::Arc;

    use rlb_model::{MEDIA_UNIT_FRAMES, SourceId, StreamEpoch};

    use crate::{
        AudioMetrics, BoundedSpsc, CaptureBlock, CaptureBlockPool, CaptureInput, CaptureReblocker,
        DefaultEndpointRoles, EndpointDirection, EndpointFormatRange, EndpointState, PlanarSamples,
    };

    use super::*;

    fn endpoint(direction: EndpointDirection) -> EndpointDescriptor {
        EndpointDescriptor::new(
            "fake-endpoint",
            "Fake endpoint",
            direction,
            DefaultEndpointRoles::default(),
            EndpointState::Active,
            EndpointFormatRange {
                minimum_sample_rate_hz: SAMPLE_RATE_HZ,
                maximum_sample_rate_hz: SAMPLE_RATE_HZ,
                minimum_channels: 1,
                maximum_channels: 2,
                supports_engine_f32: true,
            },
        )
        .expect("endpoint")
    }

    fn source_id() -> SourceId {
        SourceId::from_bytes([1; 16]).expect("source")
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_bytes([2; 16]).expect("epoch")
    }

    fn domain() -> CaptureClockDomain {
        CaptureClockDomain::new(
            NonZeroU64::new(1).expect("id"),
            NonZeroU64::new(1_000_000_000).expect("frequency"),
        )
    }

    #[test]
    fn fake_capture_drives_reblocker_without_storage_growth() {
        let total_frames = usize::from(MEDIA_UNIT_FRAMES);
        let mut fixture = vec![0.25; total_frames * 2];
        fixture[total_frames + 17] = f32::NAN;
        let mut capture = FakeCapture::new(
            endpoint(EndpointDirection::Capture),
            source_id(),
            epoch(),
            2,
            domain(),
            MonotonicInstant::from_ticks(100),
            fixture,
            total_frames,
            160,
        )
        .expect("capture");
        let capture_address = capture.storage_address();
        let mut process_samples = vec![0.0; 160 * 2];
        let process_capacity = process_samples.capacity();
        let metrics = Arc::new(AudioMetrics::new());
        let mut reblocker =
            CaptureReblocker::new(source_id(), epoch(), 2, domain(), Arc::clone(&metrics))
                .expect("reblocker");
        let mut pool = CaptureBlockPool::new(2, total_frames, 2).expect("pool");
        let mut queue: BoundedSpsc<CaptureBlock> = BoundedSpsc::new(2).expect("queue");

        for _ in 0..3 {
            let mut destination =
                CaptureProcessBuffer::new(&mut process_samples, 160, 2).expect("buffer");
            let CaptureProcessResult::Captured(metadata) =
                capture.process(&mut destination).expect("capture process")
            else {
                panic!("fixture should produce a block");
            };
            let planar = destination
                .captured(
                    usize::try_from(metadata.frames).expect("frames fit"),
                    metadata.channels,
                )
                .expect("captured view");
            reblocker
                .process(
                    CaptureInput::new(metadata, planar).expect("input"),
                    &mut pool,
                    &mut queue,
                )
                .expect("reblock");
        }

        assert_eq!(capture.status(), BackendStatus::Stopped);
        assert_eq!(capture.storage_address(), capture_address);
        assert_eq!(process_samples.capacity(), process_capacity);
        let block = queue.try_pop().expect("unit");
        assert_eq!(block.channel(1).expect("right")[17], 0.0);
        assert_eq!(metrics.snapshot().capture_non_finite_samples_total, 1);
        pool.release(block).expect("release");
    }

    #[test]
    fn fake_render_records_exact_callback_without_growth() {
        let mut render = FakeRender::new(endpoint(EndpointDirection::Render), 256).expect("render");
        let address = render.storage_address();
        let samples = vec![0.125; 128 * 2];
        render.render(&samples, 128).expect("render callback");
        assert_eq!(render.last_frames(), 128);
        assert_eq!(render.last_interleaved(), samples);
        assert_eq!(render.callback_count(), 1);
        assert_eq!(render.rendered_frames(), 128);
        assert_eq!(render.storage_address(), address);
    }

    #[test]
    fn fake_capture_reports_known_source_time_gap() {
        let mut capture = FakeCapture::new(
            endpoint(EndpointDirection::Capture),
            source_id(),
            epoch(),
            1,
            domain(),
            MonotonicInstant::from_ticks(0),
            vec![0.0; 20],
            20,
            10,
        )
        .expect("capture");
        let mut storage = vec![0.0; 10];
        let mut destination = CaptureProcessBuffer::new(&mut storage, 10, 1).expect("destination");
        let CaptureProcessResult::Captured(first) =
            capture.process(&mut destination).expect("first")
        else {
            panic!("first block");
        };
        assert_eq!(first.start_sample, 0);
        capture.skip_source_frames(7).expect("gap");
        let CaptureProcessResult::Captured(second) =
            capture.process(&mut destination).expect("second")
        else {
            panic!("second block");
        };
        assert_eq!(second.start_sample, 17);
        assert_eq!(
            second.discontinuity,
            CaptureDiscontinuity::KnownGap { missing_frames: 7 }
        );
    }

    #[test]
    fn planar_fixture_view_matches_backend_layout() {
        let samples = [1.0, 2.0, 3.0, 4.0];
        let planar = PlanarSamples::new(&samples, 2, 2, 2).expect("planar");
        assert_eq!(planar.channel(0), Some(&[1.0, 2.0][..]));
        assert_eq!(planar.channel(1), Some(&[3.0, 4.0][..]));
    }
}
