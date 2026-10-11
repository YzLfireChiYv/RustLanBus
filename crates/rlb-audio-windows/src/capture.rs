use rlb_audio::{
    AudioError, BackendStatus, CaptureBackend, CaptureClockDomain, CaptureDiscontinuity,
    CaptureMetadata, CaptureProcessBuffer, CaptureProcessResult, EndpointDescriptor,
    EndpointDirection,
};
use rlb_model::{SAMPLE_RATE_HZ, SourceId, StreamEpoch};
use windows::Win32::Media::Audio::IAudioCaptureClient;

use crate::client::{ClientUsage, EventWait, NativeClient, SharedPeriodPreference};
use crate::clock::{capture_instant, wasapi_clock_domain};
use crate::convert::{copy_interleaved_f32_to_planar, write_silence};
use crate::endpoint::{EndpointEnumerator, EndpointSelection, resolve_selection};
use crate::format::CapturePacketFlags;
use crate::{WasapiFormat, WindowsAudioError};

/// How a capture stream is bound to a Windows endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureFlow {
    /// Shared-mode capture of a microphone (or cable) endpoint.
    Microphone,
    /// Shared-mode loopback of a render endpoint.
    Loopback,
}

/// Supervisor request used to open a WASAPI capture stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureRequest {
    /// Follow-default or pinned endpoint identity.
    pub selection: EndpointSelection,
    /// Microphone capture or render-endpoint loopback.
    pub flow: CaptureFlow,
    /// Stable product source identity.
    pub source_id: SourceId,
    /// Current continuous capture timeline.
    pub stream_epoch: StreamEpoch,
    /// Engine channel count: stereo for system audio, mono for microphone.
    pub channels: u8,
}

/// Diagnostics recorded while a capture stream is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureStreamDiagnostics {
    /// Shared-engine mix format observed before initialize.
    pub mix_format: Option<WasapiFormat>,
    /// 48 kHz f32 format requested from WASAPI.
    pub engine_format: WasapiFormat,
    /// Whether Windows performed rate/channel conversion.
    pub autoconvert: bool,
    /// Whether `IAudioClient3::InitializeSharedAudioStream` succeeded.
    pub iaudioclient3: bool,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    /// Period asked of `IAudioClient3` before any fallback.
    pub requested_period_frames: u32,
    /// Negotiated callback period in frames.
    pub period_frames: u32,
    /// Default device period in 100 ns units.
    pub default_period_hns: i64,
    /// Minimum device period in 100 ns units.
    pub minimum_period_hns: i64,
    /// Packets delivered through [`WasapiCaptureStream::process`].
    pub captured_packets: u64,
    /// Packets dropped because they exceeded prepared storage.
    pub capacity_violations: u64,
    /// Packets that carried `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`.
    pub discontinuities: u64,
    /// Samples replaced with zero because they were non-finite.
    pub non_finite_samples: u64,
}

/// Event-driven shared-mode WASAPI capture stream.
pub struct WasapiCaptureStream {
    client: NativeClient,
    capture: IAudioCaptureClient,
    endpoint: EndpointDescriptor,
    selection: EndpointSelection,
    status: BackendStatus,
    source_id: SourceId,
    stream_epoch: StreamEpoch,
    channels: u8,
    clock_domain: CaptureClockDomain,
    source_sample: u64,
    pending_gap_frames: u64,
    next_device_position: Option<u64>,
    saw_packet: bool,
    captured_packets: u64,
    capacity_violations: u64,
    discontinuities: u64,
    non_finite_samples: u64,
}

impl WasapiCaptureStream {
    /// Opens a capture stream on the current MTA thread.
    pub fn open(request: CaptureRequest) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_capture(request)
    }

    /// Maximum frames a caller must prepare before `process`.
    #[must_use]
    pub const fn callback_capacity_frames(&self) -> u32 {
        self.client.buffer_frames
    }

    /// Follow/pin selection used to open this stream.
    #[must_use]
    pub const fn selection(&self) -> &EndpointSelection {
        &self.selection
    }

    /// Current capture clock domain (WASAPI 100 ns QPC).
    #[must_use]
    pub const fn clock_domain(&self) -> CaptureClockDomain {
        self.clock_domain
    }

    /// Snapshot of format, period, and packet counters.
    #[must_use]
    pub const fn diagnostics(&self) -> CaptureStreamDiagnostics {
        CaptureStreamDiagnostics {
            mix_format: self.client.mix_format,
            engine_format: self.client.engine_format,
            autoconvert: self.client.autoconvert,
            iaudioclient3: self.client.iaudioclient3,
            buffer_frames: self.client.buffer_frames,
            requested_period_frames: self.client.requested_period_frames,
            period_frames: self.client.period_frames,
            default_period_hns: self.client.default_period_hns,
            minimum_period_hns: self.client.minimum_period_hns,
            captured_packets: self.captured_packets,
            capacity_violations: self.capacity_violations,
            discontinuities: self.discontinuities,
            non_finite_samples: self.non_finite_samples,
        }
    }

    /// Waits for the WASAPI event. This is the audio-worker wait, not `process`.
    pub fn wait(&self, timeout_ms: u32) -> Result<EventWait, WindowsAudioError> {
        self.client.wait(timeout_ms)
    }

    /// Starts capturing. The stream remains stopped until this succeeds.
    pub fn start(&mut self) -> Result<(), WindowsAudioError> {
        self.client.start()?;
        self.status = BackendStatus::Running;
        Ok(())
    }

    /// Stops capturing and keeps the endpoint configuration.
    pub fn stop(&mut self) -> Result<(), WindowsAudioError> {
        self.client.stop()?;
        self.status = BackendStatus::Stopped;
        Ok(())
    }

    /// Begins a new source epoch after an unknown discontinuity or reopen.
    pub fn begin_epoch(&mut self, stream_epoch: StreamEpoch) {
        self.stream_epoch = stream_epoch;
        self.source_sample = 0;
        self.pending_gap_frames = 0;
        self.next_device_position = None;
        self.saw_packet = false;
        if self.status == BackendStatus::Faulted {
            self.status = BackendStatus::Stopped;
        }
    }
}

impl EndpointEnumerator {
    /// Resolves `request` against the current catalog and opens a capture stream.
    pub fn open_capture(
        &self,
        request: CaptureRequest,
    ) -> Result<WasapiCaptureStream, WindowsAudioError> {
        self.open_capture_with_period(request, SharedPeriodPreference::Default)
    }

    /// Opens capture using `preference` for the shared-mode period.
    pub fn open_capture_with_period(
        &self,
        request: CaptureRequest,
        preference: SharedPeriodPreference,
    ) -> Result<WasapiCaptureStream, WindowsAudioError> {
        if !matches!(request.channels, 1 | 2) {
            return Err(AudioError::UnsupportedChannels {
                channels: request.channels,
            }
            .into());
        }
        let catalog = self.enumerate()?;
        let (bind_direction, backend_direction, usage) = match request.flow {
            CaptureFlow::Microphone => (
                EndpointDirection::Capture,
                EndpointDirection::Capture,
                ClientUsage::Capture,
            ),
            CaptureFlow::Loopback => (
                EndpointDirection::Render,
                EndpointDirection::LoopbackCapture,
                ClientUsage::LoopbackCapture,
            ),
        };
        let info = resolve_selection(&request.selection, bind_direction, &catalog)?;
        let endpoint = info.to_descriptor(backend_direction, request.channels)?;
        let client = NativeClient::open(
            self,
            info.endpoint_id(),
            request.channels,
            usage,
            preference,
        )?;
        let capture = client.capture_service()?;
        Ok(WasapiCaptureStream {
            client,
            capture,
            endpoint,
            selection: request.selection,
            status: BackendStatus::Ready,
            source_id: request.source_id,
            stream_epoch: request.stream_epoch,
            channels: request.channels,
            clock_domain: wasapi_clock_domain(),
            source_sample: 0,
            pending_gap_frames: 0,
            next_device_position: None,
            saw_packet: false,
            captured_packets: 0,
            capacity_violations: 0,
            discontinuities: 0,
            non_finite_samples: 0,
        })
    }
}

impl CaptureBackend for WasapiCaptureStream {
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
        match self.process_packet(destination) {
            Ok(result) => Ok(result),
            Err(error) => {
                if matches!(
                    error,
                    WindowsAudioError::UnknownDiscontinuity
                        | WindowsAudioError::CaptureCapacityExceeded
                ) {
                    if matches!(error, WindowsAudioError::UnknownDiscontinuity) {
                        self.status = BackendStatus::Faulted;
                    }
                } else {
                    self.status = BackendStatus::Faulted;
                }
                Err(error.to_capture_process_error())
            }
        }
    }
}

impl WasapiCaptureStream {
    fn process_packet(
        &mut self,
        destination: &mut CaptureProcessBuffer<'_>,
    ) -> Result<CaptureProcessResult, WindowsAudioError> {
        let available = unsafe { self.capture.GetNextPacketSize() }
            .map_err(|error| crate::hresult("IAudioCaptureClient::GetNextPacketSize", error))?;
        if available == 0 {
            return Ok(CaptureProcessResult::Idle);
        }

        let mut data = std::ptr::null_mut();
        let mut frames = 0_u32;
        let mut flags = 0_u32;
        let mut device_position = 0_u64;
        let mut qpc_position = 0_u64;
        unsafe {
            self.capture.GetBuffer(
                &mut data,
                &mut frames,
                &mut flags,
                Some(&mut device_position),
                Some(&mut qpc_position),
            )
        }
        .map_err(|error| crate::hresult("IAudioCaptureClient::GetBuffer", error))?;
        if frames == 0 {
            let _ = unsafe { self.capture.ReleaseBuffer(0) };
            return Ok(CaptureProcessResult::Idle);
        }

        let result = self.consume_packet(
            destination,
            data,
            frames,
            flags,
            device_position,
            qpc_position,
        );
        let _ = unsafe { self.capture.ReleaseBuffer(frames) };
        result
    }

    fn consume_packet(
        &mut self,
        destination: &mut CaptureProcessBuffer<'_>,
        data: *mut u8,
        frames: u32,
        flags: u32,
        device_position: u64,
        qpc_position: u64,
    ) -> Result<CaptureProcessResult, WindowsAudioError> {
        let frames_usize = usize::try_from(frames).map_err(|_| AudioError::CapacityOverflow {
            field: "capture_frames",
        })?;
        let packet_flags = CapturePacketFlags::from_raw(flags);
        if packet_flags.data_discontinuity {
            self.discontinuities = self.discontinuities.saturating_add(1);
        }

        let device_discontinuity = self.discontinuity_for_packet(packet_flags, device_position)?;
        let timeline = CapturePacketTimeline::new(
            self.source_sample,
            self.pending_gap_frames,
            device_discontinuity,
            frames,
        )?;
        let capture_time = capture_instant(if packet_flags.timestamp_error {
            None
        } else {
            Some(qpc_position)
        });
        let device_position_out = if packet_flags.timestamp_error {
            None
        } else {
            Some(device_position)
        };

        if destination.frame_capacity() < frames_usize
            || destination.channel_capacity() < self.channels
        {
            self.note_consumed_packet(timeline, device_position_out, false);
            self.capacity_violations = self.capacity_violations.saturating_add(1);
            return Err(WindowsAudioError::CaptureCapacityExceeded);
        }

        let mut silent = packet_flags.silent;
        let non_finite = if silent || data.is_null() {
            if !silent && data.is_null() {
                return Err(AudioError::BackendContractViolation.into());
            }
            write_silence(frames_usize, self.channels, destination)?;
            silent = true;
            0
        } else {
            let sample_count = frames_usize.checked_mul(usize::from(self.channels)).ok_or(
                AudioError::CapacityOverflow {
                    field: "capture_interleaved_samples",
                },
            )?;
            let interleaved =
                unsafe { std::slice::from_raw_parts(data.cast::<f32>(), sample_count) };
            copy_interleaved_f32_to_planar(interleaved, frames_usize, self.channels, destination)?
        };
        self.non_finite_samples = self.non_finite_samples.saturating_add(non_finite);

        let metadata = CaptureMetadata {
            source_id: self.source_id,
            stream_epoch: self.stream_epoch,
            start_sample: timeline.start_sample,
            frames,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: self.channels,
            clock_domain: self.clock_domain,
            capture_time,
            device_position: device_position_out,
            discontinuity: timeline.discontinuity,
            silent,
        };
        metadata.validate()?;
        self.note_consumed_packet(timeline, device_position_out, true);
        self.captured_packets = self.captured_packets.saturating_add(1);
        Ok(CaptureProcessResult::Captured(metadata))
    }

    fn discontinuity_for_packet(
        &self,
        flags: CapturePacketFlags,
        device_position: u64,
    ) -> Result<CaptureDiscontinuity, WindowsAudioError> {
        if !flags.data_discontinuity {
            return Ok(CaptureDiscontinuity::Continuous);
        }
        if !self.saw_packet {
            return Ok(CaptureDiscontinuity::Continuous);
        }
        if flags.timestamp_error {
            return Err(WindowsAudioError::UnknownDiscontinuity);
        }
        match self.next_device_position {
            Some(expected) if device_position > expected => Ok(CaptureDiscontinuity::KnownGap {
                missing_frames: device_position - expected,
            }),
            Some(expected) if device_position == expected => Ok(CaptureDiscontinuity::Continuous),
            _ => Err(WindowsAudioError::UnknownDiscontinuity),
        }
    }

    fn note_consumed_packet(
        &mut self,
        timeline: CapturePacketTimeline,
        device_position_out: Option<u64>,
        delivered: bool,
    ) {
        self.source_sample = timeline.end_sample;
        self.pending_gap_frames = if delivered {
            0
        } else {
            timeline.gap_if_dropped
        };
        // An unreliable timestamp cannot remain the basis for a later gap.
        self.next_device_position = device_position_out
            .and_then(|position| position.checked_add(timeline.end_sample - timeline.start_sample));
        self.saw_packet = true;
    }
}

/// Sample positions and missing duration relative to the last delivered packet.
#[derive(Clone, Copy, Debug)]
struct CapturePacketTimeline {
    start_sample: u64,
    end_sample: u64,
    discontinuity: CaptureDiscontinuity,
    gap_if_dropped: u64,
}

impl CapturePacketTimeline {
    fn new(
        source_sample: u64,
        pending_gap_frames: u64,
        device_discontinuity: CaptureDiscontinuity,
        frames: u32,
    ) -> Result<Self, AudioError> {
        let device_gap = match device_discontinuity {
            CaptureDiscontinuity::Continuous => 0,
            CaptureDiscontinuity::KnownGap { missing_frames } => missing_frames,
        };
        let start_sample = source_sample
            .checked_add(device_gap)
            .ok_or(AudioError::SampleRangeOverflow)?;
        let end_sample = start_sample
            .checked_add(u64::from(frames))
            .ok_or(AudioError::SampleRangeOverflow)?;
        let missing_frames = pending_gap_frames
            .checked_add(device_gap)
            .ok_or(AudioError::SampleRangeOverflow)?;
        let gap_if_dropped = missing_frames
            .checked_add(u64::from(frames))
            .ok_or(AudioError::SampleRangeOverflow)?;
        Ok(Self {
            start_sample,
            end_sample,
            discontinuity: if missing_frames == 0 {
                CaptureDiscontinuity::Continuous
            } else {
                CaptureDiscontinuity::KnownGap { missing_frames }
            },
            gap_if_dropped,
        })
    }
}

// SAFETY: the stream is confined to one audio-worker thread after open. WASAPI
// MTA COM interfaces and the event handle are not shared with another owner.
unsafe impl Send for WasapiCaptureStream {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointRole;
    use rlb_audio::{
        AudioMetrics, BoundedSpsc, CaptureBlockPool, CaptureInput, CaptureReblocker, PlanarSamples,
    };
    use rlb_clock::MonotonicInstant;
    use std::sync::Arc;

    fn ids() -> (SourceId, StreamEpoch) {
        (
            SourceId::generate().expect("source"),
            StreamEpoch::generate().expect("epoch"),
        )
    }

    fn reblock_gaps(timelines: &[CapturePacketTimeline]) -> Vec<u64> {
        let (source_id, stream_epoch) = ids();
        let clock_domain = wasapi_clock_domain();
        let mut reblocker = CaptureReblocker::new(
            source_id,
            stream_epoch,
            1,
            clock_domain,
            Arc::new(AudioMetrics::new()),
        )
        .expect("reblocker");
        let mut pool = CaptureBlockPool::new(1, 480, 1).expect("pool");
        let mut queue = BoundedSpsc::new(1).expect("queue");
        let samples = [0.25; 480];
        let mut gaps = Vec::new();
        for timeline in timelines {
            assert_eq!(timeline.end_sample - timeline.start_sample, 480);
            let metadata = CaptureMetadata {
                source_id,
                stream_epoch,
                start_sample: timeline.start_sample,
                frames: 480,
                sample_rate_hz: SAMPLE_RATE_HZ,
                channels: 1,
                clock_domain,
                capture_time: MonotonicInstant::from_ticks(0),
                device_position: None,
                discontinuity: timeline.discontinuity,
                silent: false,
            };
            let input = CaptureInput::new(
                metadata,
                PlanarSamples::new(&samples, 480, 1, 480).expect("samples"),
            )
            .expect("input");
            let report = reblocker
                .process(input, &mut pool, &mut queue)
                .expect("capture timeline must satisfy the reblocker contract");
            gaps.push(report.gap_frames);
            let block = queue.try_pop().expect("complete block");
            assert_eq!(
                block.metadata().expect("metadata").start_sample,
                timeline.start_sample
            );
            pool.release(block).expect("recycle");
        }
        gaps
    }

    #[test]
    fn capture_timeline_known_gap_advances_sample_position_once() {
        let first = CapturePacketTimeline::new(0, 0, CaptureDiscontinuity::Continuous, 480)
            .expect("first packet");
        let gapped = CapturePacketTimeline::new(
            first.end_sample,
            0,
            CaptureDiscontinuity::KnownGap { missing_frames: 13 },
            480,
        )
        .expect("gapped packet");
        let next =
            CapturePacketTimeline::new(gapped.end_sample, 0, CaptureDiscontinuity::Continuous, 480)
                .expect("continuous packet");
        assert_eq!(gapped.start_sample, 493);
        assert_eq!(gapped.end_sample, 973);
        assert_eq!(next.start_sample, 973);
        assert_eq!(reblock_gaps(&[first, gapped, next]), [0, 13, 0]);
    }

    #[test]
    fn capture_timeline_capacity_drop_preserves_full_gap_once() {
        let first = CapturePacketTimeline::new(0, 0, CaptureDiscontinuity::Continuous, 480)
            .expect("first packet");
        let dropped = CapturePacketTimeline::new(
            first.end_sample,
            0,
            CaptureDiscontinuity::KnownGap { missing_frames: 7 },
            480,
        )
        .expect("capacity-dropped packet");
        let resumed = CapturePacketTimeline::new(
            dropped.end_sample,
            dropped.gap_if_dropped,
            CaptureDiscontinuity::KnownGap { missing_frames: 11 },
            480,
        )
        .expect("resumed packet");
        let next = CapturePacketTimeline::new(
            resumed.end_sample,
            0,
            CaptureDiscontinuity::Continuous,
            480,
        )
        .expect("continuous packet");
        assert_eq!(resumed.start_sample, 978);
        assert_eq!(reblock_gaps(&[first, resumed, next]), [0, 498, 0]);
    }

    #[test]
    fn default_loopback_delivers_engine_f32_packets() {
        let (source_id, stream_epoch) = ids();
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        let catalog = enumerator.enumerate().expect("catalog");
        if catalog
            .default_endpoint(EndpointDirection::Render, EndpointRole::Console)
            .is_none()
        {
            return;
        }
        let mut stream = enumerator
            .open_capture(CaptureRequest {
                selection: EndpointSelection::follow_default(),
                flow: CaptureFlow::Loopback,
                source_id,
                stream_epoch,
                channels: 2,
            })
            .expect("open loopback");
        assert!(
            stream.diagnostics().engine_format.is_engine_f32(2),
            "engine format {:?}",
            stream.diagnostics().engine_format
        );
        stream.start().expect("start");
        let mut storage = vec![0.0_f32; stream.callback_capacity_frames() as usize * 2];
        let mut destination =
            CaptureProcessBuffer::new(&mut storage, stream.callback_capacity_frames() as usize, 2)
                .expect("dest");
        let mut captured = 0_u32;
        for _ in 0..20 {
            match stream.wait(1_000) {
                Ok(EventWait::Signaled) => {}
                Ok(EventWait::Timeout) => continue,
                Err(error) => panic!("wait failed: {error}"),
            }
            match stream.process(&mut destination) {
                Ok(CaptureProcessResult::Captured(metadata)) => {
                    assert_eq!(metadata.source_id, source_id);
                    assert_eq!(metadata.stream_epoch, stream_epoch);
                    assert_eq!(metadata.sample_rate_hz, 48_000);
                    assert_eq!(metadata.channels, 2);
                    captured = captured.saturating_add(metadata.frames);
                    if captured >= 480 {
                        break;
                    }
                }
                Ok(CaptureProcessResult::Idle) => {}
                other => panic!("unexpected capture result: {other:?}"),
            }
        }
        stream.stop().expect("stop");
        assert!(
            captured > 0,
            "loopback should deliver at least one packet of shared-engine audio"
        );
    }

    #[test]
    fn missing_pinned_endpoint_stays_unavailable() {
        let (source_id, stream_epoch) = ids();
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        let error = enumerator
            .open_capture(CaptureRequest {
                selection: EndpointSelection::fixed("missing-endpoint"),
                flow: CaptureFlow::Microphone,
                source_id,
                stream_epoch,
                channels: 1,
            })
            .err()
            .expect("missing pin");
        assert!(matches!(error, WindowsAudioError::DeviceUnavailable { .. }));
        assert_eq!(error.stable_code(), "device_unavailable");
    }
}
