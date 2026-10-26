use rlb_audio::{
    AudioError, BackendStatus, CaptureBackend, CaptureClockDomain, CaptureDiscontinuity,
    CaptureMetadata, CaptureProcessBuffer, CaptureProcessResult, CaptureTiming, EndpointDescriptor,
    EndpointDirection,
};
use rlb_model::{SAMPLE_RATE_HZ, SourceId, StreamEpoch};
use windows::Win32::Media::Audio::IAudioCaptureClient;

use crate::client::{
    ClientUsage, EventWait, NativeClient, SharedEnginePeriods, SharedPeriodPreference,
    WasapiCallFailure,
};
use crate::clock::{observe_wasapi_packet_timing, wasapi_clock_domain, wasapi_qpc_100ns_now};
use crate::convert::{
    NativeCopyFailure, copy_finite_f32_le, copy_interleaved_f32_to_planar, copy_native_to_f64,
    write_silence,
};
use crate::endpoint::{ComApartment, EndpointEnumerator, EndpointSelection, resolve_selection};
use crate::format::{CapturePacketFlags, NativeF32Format};
use crate::wake::{AudioWake, NativeWake};
use crate::{WasapiFormat, WindowsAudioError};

enum NativeDestination<'a> {
    F32(&'a mut [f32]),
    F64(&'a mut [f64]),
}

impl NativeDestination<'_> {
    fn len(&self) -> usize {
        match self {
            Self::F32(samples) => samples.len(),
            Self::F64(samples) => samples.len(),
        }
    }

    fn silence(&mut self, samples: usize) {
        match self {
            Self::F32(destination) => destination[..samples].fill(0.0),
            Self::F64(destination) => destination[..samples].fill(0.0),
        }
    }

    fn copy(&mut self, format: NativeF32Format, bytes: &[u8]) -> Result<(), NativeCopyFailure> {
        match self {
            Self::F32(destination) => copy_finite_f32_le(bytes, destination),
            Self::F64(destination) => copy_native_to_f64(format, bytes, destination),
        }
    }
}

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

/// Opens the selected endpoint at its current 32-bit float mix format.
///
/// The mix format supplies the channel count. A microphone may be stereo.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCaptureRequest {
    /// Follow-default or pinned endpoint identity.
    pub selection: EndpointSelection,
    /// Microphone capture or render-endpoint loopback.
    pub flow: CaptureFlow,
    /// Stable product source identity.
    pub source_id: SourceId,
    /// Current continuous capture timeline.
    pub stream_epoch: StreamEpoch,
}

/// One native capture packet copied into caller-owned interleaved storage.
///
/// `timing.first_frame` is absent when the device timestamp is missing or
/// marked unreliable. The samples remain usable. `timing.observed_on_owner`
/// is the owner's separate reading and is not a substitute first-frame time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeCapturePacket {
    /// Stable product source identity.
    pub source_id: SourceId,
    /// Acquisition identity supplied at open or by [`WasapiCaptureStream::begin_epoch`].
    pub stream_epoch: StreamEpoch,
    /// Zero-based first source frame in this epoch.
    pub start_sample: u64,
    /// Source frames copied into the caller buffer, or valid silent frames.
    pub frames: u32,
    /// Rate, channel count, and raw channel mask of this stream.
    pub format: NativeF32Format,
    /// WASAPI 100 ns QPC domain.
    pub clock_domain: CaptureClockDomain,
    /// Device and owner timing for this packet.
    pub timing: CaptureTiming,
    /// Device position when the timestamp is usable.
    pub device_position: Option<u64>,
    /// Timeline state immediately before `start_sample`.
    pub discontinuity: CaptureDiscontinuity,
    /// The device marked the whole packet as digital silence.
    pub silent: bool,
}

/// Result of one non-waiting native capture read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCaptureRead {
    /// The capture client had no packet waiting.
    Idle,
    /// The stream is stopped. Call `start` before the next read.
    Stopped,
    /// `destination` holds `frames * channels` interleaved samples.
    Packet(NativeCapturePacket),
}

/// Diagnostics recorded while a capture stream is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureStreamDiagnostics {
    /// Shared-engine mix format observed before initialize.
    pub mix_format: Option<WasapiFormat>,
    /// Format passed to WASAPI initialization.
    ///
    /// Fixed 48 kHz opens request engine float. Native opens request the accepted mix format.
    pub engine_format: WasapiFormat,
    /// Accepted mix format when this stream was opened natively.
    pub native_format: Option<NativeF32Format>,
    /// Whether Windows performed rate/channel conversion.
    pub autoconvert: bool,
    /// Whether `IAudioClient3::InitializeSharedAudioStream` succeeded.
    pub iaudioclient3: bool,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    /// Initial IAudioClient3 period request, or the classic-path estimate.
    pub requested_period_frames: u32,
    /// Requested shared callback period, or estimated classic period in frames.
    pub period_frames: u32,
    /// Classic default shared-mode device period in 100 ns units.
    pub default_period_hns: i64,
    /// Classic minimum exclusive-mode device period in 100 ns units.
    pub minimum_period_hns: i64,
    /// Complete shared-engine period range for the requested stream format.
    pub shared_engine_periods: Option<SharedEnginePeriods>,
    /// Failure to obtain the shared-engine period range.
    pub shared_engine_period_failure: Option<WasapiCallFailure>,
    /// Failure from opening the requested minimum-period stream, if attempted.
    pub minimum_period_failure: Option<WasapiCallFailure>,
    /// Failure from opening the default-period shared stream, if attempted.
    pub default_period_failure: Option<WasapiCallFailure>,
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
///
/// An opened stream retains its COM initialization and stays on its owner.
///
/// ```compile_fail,E0277
/// use rlb_audio_windows::WasapiCaptureStream;
/// fn requires_send<T: Send>() {}
/// requires_send::<WasapiCaptureStream>();
/// ```
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
    // Declared last: all native clients/services release before CoUninitialize.
    _apartment: ComApartment,
}

impl WasapiCaptureStream {
    /// Opens a capture stream on the current MTA thread.
    ///
    /// The returned stream owns its COM MTA initialization and must be used
    /// and dropped on this thread, including after the enumerator is gone.
    pub fn open(request: CaptureRequest) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_capture(request)
    }

    /// Opens the endpoint's native PCM format without Windows conversion.
    ///
    /// Same-thread ownership follows [`Self::open`]. The default shared period
    /// is requested first, with the classic shared client as the fallback.
    pub fn open_native(request: NativeCaptureRequest) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_native_capture(request)
    }

    /// Opens a native mix-format stream using `preference` for the shared period.
    ///
    /// Render-endpoint loopback stays on classic shared initialization.
    pub fn open_native_with_period(
        request: NativeCaptureRequest,
        preference: SharedPeriodPreference,
    ) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_native_capture_with_period(request, preference)
    }

    /// Mix format accepted for this stream, when it was opened natively.
    #[must_use]
    pub const fn native_format(&self) -> Option<NativeF32Format> {
        self.client.native_format
    }

    /// Maximum frames a caller must prepare before `process`.
    #[must_use]
    pub const fn callback_capacity_frames(&self) -> u32 {
        self.client.buffer_frames
    }

    /// Capture-owner boundary after all previously released native packets.
    /// Call between reads. A future known device gap can move the next packet
    /// forward from this boundary; it can never make an older packet eligible.
    /// This lets a live but silent stream acknowledge publication without
    /// waiting for the first sound and losing that sound to control setup.
    #[must_use]
    pub fn native_boundary(&self) -> Option<(StreamEpoch, u64)> {
        (self.client.native_format.is_some() && self.status != BackendStatus::Faulted)
            .then_some((self.stream_epoch, self.source_sample))
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
            native_format: self.client.native_format,
            autoconvert: self.client.autoconvert,
            iaudioclient3: self.client.iaudioclient3,
            buffer_frames: self.client.buffer_frames,
            requested_period_frames: self.client.requested_period_frames,
            period_frames: self.client.period_frames,
            default_period_hns: self.client.default_period_hns,
            minimum_period_hns: self.client.minimum_period_hns,
            shared_engine_periods: self.client.shared_engine_periods,
            shared_engine_period_failure: self.client.shared_engine_period_failure,
            minimum_period_failure: self.client.minimum_period_failure,
            default_period_failure: self.client.default_period_failure,
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

    /// Waits for audio or a control notification, prioritizing ready control.
    ///
    /// The stream and this wait remain on the stream's creating thread. Other
    /// threads may signal a clone of `control` without touching this stream.
    pub fn wait_with_control(
        &self,
        control: &AudioWake,
        timeout_ms: u32,
    ) -> Result<NativeWake, WindowsAudioError> {
        self.client.wait_with_control(control, timeout_ms)
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

    /// Copies one available native packet into `destination`.
    ///
    /// The slice may be longer than the packet. A delivered packet writes
    /// `frames * channels` interleaved samples at the front and leaves the
    /// remainder unchanged. This method does not wait, resample, or change
    /// channel count. Finite samples keep their bits. A non-finite packet is
    /// released, counted, and refused with the caller buffer unchanged; the
    /// next delivered packet carries those frames as a gap.
    ///
    /// `UnknownDiscontinuity` and a malformed packet fault the stream. Call
    /// `stop`, [`Self::begin_epoch`] with a new acquisition identity, then
    /// `start` before reading further packets.
    pub fn read_native(
        &mut self,
        destination: &mut [f32],
    ) -> Result<NativeCaptureRead, WindowsAudioError> {
        if self
            .client
            .native_format
            .is_some_and(|format| !format.is_f32())
        {
            return Err(AudioError::BackendContractViolation.into());
        }
        self.read_native_into(NativeDestination::F32(destination))
    }

    /// Reads native PCM directly into F64 storage. F32 and signed integer
    /// originals widen exactly; F64 originals retain their finite values.
    pub fn read_native_f64(
        &mut self,
        destination: &mut [f64],
    ) -> Result<NativeCaptureRead, WindowsAudioError> {
        self.read_native_into(NativeDestination::F64(destination))
    }

    fn read_native_into(
        &mut self,
        mut destination: NativeDestination<'_>,
    ) -> Result<NativeCaptureRead, WindowsAudioError> {
        if self.client.native_format.is_none() {
            return Err(WindowsAudioError::LegacyStreamNativeCall);
        }
        match self.status {
            BackendStatus::Stopped => return Ok(NativeCaptureRead::Stopped),
            BackendStatus::Running => {}
            _ => return Err(AudioError::BackendUnavailable.into()),
        }
        match self.with_capture_packet(|stream, data, frames, flags, device_position, qpc| {
            stream.consume_native(&mut destination, data, frames, flags, device_position, qpc)
        }) {
            Ok(Some(packet)) => Ok(NativeCaptureRead::Packet(packet)),
            Ok(None) => Ok(NativeCaptureRead::Idle),
            Err(error) => {
                if native_failure_faults(&error) {
                    self.status = BackendStatus::Faulted;
                }
                Err(error)
            }
        }
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
        let apartment = ComApartment::initialize_mta()?;
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
            _apartment: apartment,
        })
    }

    /// Opens the endpoint's current validated native PCM mix format.
    pub fn open_native_capture(
        &self,
        request: NativeCaptureRequest,
    ) -> Result<WasapiCaptureStream, WindowsAudioError> {
        self.open_native_capture_with_period(request, SharedPeriodPreference::Default)
    }

    /// Opens a native mix-format capture stream using `preference` for the shared period.
    ///
    /// Loopback uses classic shared initialization and does not create a companion render stream.
    pub fn open_native_capture_with_period(
        &self,
        request: NativeCaptureRequest,
        preference: SharedPeriodPreference,
    ) -> Result<WasapiCaptureStream, WindowsAudioError> {
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
        let apartment = ComApartment::initialize_mta()?;
        let client = NativeClient::open_native_pcm(self, info.endpoint_id(), usage, preference)?;
        let native = client
            .native_format
            .ok_or(WindowsAudioError::NativeFormatRejected {
                reason: crate::format::NativeFormatReject::MixFormatUnavailable,
            })?;
        let endpoint = EndpointDescriptor::new(
            info.endpoint_id(),
            info.friendly_name(),
            backend_direction,
            info.default_roles(),
            info.state(),
            native.format_range(),
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
            channels: native.channels,
            clock_domain: wasapi_clock_domain(),
            source_sample: 0,
            pending_gap_frames: 0,
            next_device_position: None,
            saw_packet: false,
            captured_packets: 0,
            capacity_violations: 0,
            discontinuities: 0,
            non_finite_samples: 0,
            _apartment: apartment,
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
        if self.client.native_format.is_some() {
            return Err(AudioError::BackendContractViolation);
        }
        if self.status == BackendStatus::Stopped {
            return Ok(CaptureProcessResult::EndOfStream);
        }
        if self.status != BackendStatus::Running {
            return Err(AudioError::BackendUnavailable);
        }
        match self.with_capture_packet(|stream, data, frames, flags, device_position, qpc| {
            stream.consume_packet(destination, data, frames, flags, device_position, qpc)
        }) {
            Ok(Some(result)) => Ok(result),
            Ok(None) => Ok(CaptureProcessResult::Idle),
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
    fn with_capture_packet<T>(
        &mut self,
        consume: impl FnOnce(&mut Self, *mut u8, u32, u32, u64, u64) -> Result<T, WindowsAudioError>,
    ) -> Result<Option<T>, WindowsAudioError> {
        let available = unsafe { self.capture.GetNextPacketSize() }
            .map_err(|error| crate::hresult("IAudioCaptureClient::GetNextPacketSize", error))?;
        if available == 0 {
            return Ok(None);
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
            return complete_capture_packet(Ok(None), || {
                unsafe { self.capture.ReleaseBuffer(0) }
                    .map_err(|error| crate::hresult("IAudioCaptureClient::ReleaseBuffer", error))
            });
        }

        let result = consume(self, data, frames, flags, device_position, qpc_position);
        complete_capture_packet(result.map(Some), || {
            unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(|error| crate::hresult("IAudioCaptureClient::ReleaseBuffer", error))
        })
    }

    fn consume_native(
        &mut self,
        destination: &mut NativeDestination<'_>,
        data: *mut u8,
        frames: u32,
        flags: u32,
        device_position: u64,
        qpc_position: u64,
    ) -> Result<NativeCapturePacket, WindowsAudioError> {
        let format = self
            .client
            .native_format
            .ok_or(WindowsAudioError::LegacyStreamNativeCall)?;
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
        let timing = observe_wasapi_packet_timing(
            packet_flags.timestamp_error,
            qpc_position,
            wasapi_qpc_100ns_now(),
        );
        let device_position_out = if packet_flags.timestamp_error {
            None
        } else {
            Some(device_position)
        };
        let sample_count = frames_usize
            .checked_mul(usize::from(format.channels))
            .ok_or(AudioError::CapacityOverflow {
                field: "capture_interleaved_samples",
            })?;
        if destination.len() < sample_count {
            self.note_consumed_packet(timeline, device_position_out, false);
            self.capacity_violations = self.capacity_violations.saturating_add(1);
            return Err(WindowsAudioError::CaptureCapacityExceeded);
        }

        let silent = if packet_flags.silent || data.is_null() {
            if !packet_flags.silent && data.is_null() {
                return Err(WindowsAudioError::MalformedNativePacket { frames });
            }
            destination.silence(sample_count);
            true
        } else {
            let bytes = sample_count
                .checked_mul(usize::from(format.bits_per_sample / 8))
                .ok_or(AudioError::CapacityOverflow {
                    field: "capture_interleaved_bytes",
                })?;
            let source = unsafe { std::slice::from_raw_parts(data, bytes) };
            match destination.copy(format, source) {
                Ok(()) => packet_flags.silent,
                Err(NativeCopyFailure::NonFinite { samples }) => {
                    self.non_finite_samples = self.non_finite_samples.saturating_add(samples);
                    self.note_consumed_packet(timeline, device_position_out, false);
                    return Err(WindowsAudioError::NativeSamplesNonFinite {
                        frames,
                        non_finite_samples: samples,
                        start_sample: timeline.start_sample,
                    });
                }
                Err(NativeCopyFailure::TooSmall { .. }) => {
                    self.note_consumed_packet(timeline, device_position_out, false);
                    self.capacity_violations = self.capacity_violations.saturating_add(1);
                    return Err(WindowsAudioError::CaptureCapacityExceeded);
                }
                Err(NativeCopyFailure::MalformedBytes { .. }) => {
                    return Err(WindowsAudioError::MalformedNativePacket { frames });
                }
            }
        };

        let packet = NativeCapturePacket {
            source_id: self.source_id,
            stream_epoch: self.stream_epoch,
            start_sample: timeline.start_sample,
            frames,
            format,
            clock_domain: self.clock_domain,
            timing,
            device_position: device_position_out,
            discontinuity: timeline.discontinuity,
            silent,
        };
        self.note_consumed_packet(timeline, device_position_out, true);
        self.captured_packets = self.captured_packets.saturating_add(1);
        Ok(packet)
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
        // Owner QPC is read after GetBuffer returns and before sample copying.
        let timing = observe_wasapi_packet_timing(
            packet_flags.timestamp_error,
            qpc_position,
            wasapi_qpc_100ns_now(),
        );
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
            timing,
            capture_time: timing.compatibility_capture_time(),
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

/// A failed native release invalidates the stream even when consuming the
/// packet produced a normally recoverable error. Invoke the release once.
fn complete_capture_packet<T>(
    consumed: Result<T, WindowsAudioError>,
    release: impl FnOnce() -> Result<(), WindowsAudioError>,
) -> Result<T, WindowsAudioError> {
    release()?;
    consumed
}

fn native_failure_faults(error: &WindowsAudioError) -> bool {
    !matches!(
        error,
        WindowsAudioError::CaptureCapacityExceeded
            | WindowsAudioError::NativeSamplesNonFinite { .. }
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointRole;
    use rlb_audio::{
        AudioMetrics, BoundedSpsc, CaptureBlockPool, CaptureInput, CaptureReblocker, CaptureTiming,
        PlanarSamples,
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
                timing: CaptureTiming::absent(),
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
    #[ignore = "opens and starts default loopback; select explicitly with --ignored"]
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
    fn native_capture_request_keeps_flow_separate_from_channel_count() {
        let (source_id, stream_epoch) = ids();
        let request = NativeCaptureRequest {
            selection: EndpointSelection::fixed("endpoint"),
            flow: CaptureFlow::Microphone,
            source_id,
            stream_epoch,
        };
        assert_eq!(request.flow, CaptureFlow::Microphone);
        assert_eq!(request.source_id, source_id);
    }

    #[test]
    fn capture_release_failure_overrides_recoverable_consume_error_once() {
        use std::cell::Cell;

        for consumed in [Ok(1), Err(WindowsAudioError::CaptureCapacityExceeded)] {
            let release_calls = Cell::new(0);
            let result = complete_capture_packet(consumed, || {
                release_calls.set(release_calls.get() + 1);
                Err(WindowsAudioError::Hresult {
                    operation: "IAudioCaptureClient::ReleaseBuffer",
                    code: 0x8889_0004,
                })
            });
            assert_eq!(release_calls.get(), 1);
            assert!(matches!(
                result,
                Err(WindowsAudioError::Hresult {
                    operation: "IAudioCaptureClient::ReleaseBuffer",
                    code: 0x8889_0004,
                })
            ));
            assert!(native_failure_faults(&result.unwrap_err()));
        }

        let release_calls = Cell::new(0);
        let result: Result<(), _> = complete_capture_packet(
            Err(WindowsAudioError::NativeSamplesNonFinite {
                frames: 1,
                non_finite_samples: 1,
                start_sample: 0,
            }),
            || {
                release_calls.set(release_calls.get() + 1);
                Ok(())
            },
        );
        assert_eq!(release_calls.get(), 1);
        assert!(matches!(
            result,
            Err(WindowsAudioError::NativeSamplesNonFinite { .. })
        ));
        assert!(!native_failure_faults(&result.unwrap_err()));
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
