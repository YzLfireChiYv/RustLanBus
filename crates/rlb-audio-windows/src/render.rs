use core::num::NonZeroU64;

use rlb_audio::{AudioError, BackendStatus, EndpointDescriptor, EndpointDirection, RenderBackend};
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, IAudioRenderClient};

use crate::client::{
    ClientUsage, EventWait, NativeClient, SharedEnginePeriods, SharedPeriodPreference,
    WasapiCallFailure,
};
use crate::clock::{PreparedRenderClock, RenderClockFailure, RenderClockObservation};
use crate::convert::{non_finite_sample_count, validate_native_f64, write_native_f64};
use crate::endpoint::{ComApartment, EndpointEnumerator, EndpointSelection, resolve_selection};
use crate::format::NativeF32Format;
use crate::wake::{AudioWake, NativeWake};
use crate::{WasapiFormat, WindowsAudioError, hresult};

enum RenderData<'a> {
    F32(&'a [f32]),
    NativeF64 {
        samples: &'a [f64],
        format: NativeF32Format,
    },
}

impl RenderData<'_> {
    fn byte_len(&self) -> Result<usize, AudioError> {
        let (samples, width) = match self {
            Self::F32(samples) => (samples.len(), core::mem::size_of::<f32>()),
            Self::NativeF64 { samples, format } => {
                (samples.len(), usize::from(format.bits_per_sample / 8))
            }
        };
        samples
            .checked_mul(width)
            .ok_or(AudioError::CapacityOverflow {
                field: "render_interleaved_bytes",
            })
    }

    fn write(&self, destination: &mut [u8]) {
        match self {
            Self::F32(samples) => unsafe {
                core::ptr::copy_nonoverlapping(
                    samples.as_ptr().cast::<u8>(),
                    destination.as_mut_ptr(),
                    destination.len(),
                );
            },
            Self::NativeF64 { samples, format } => write_native_f64(*format, samples, destination),
        }
    }
}

/// Supervisor request used to open the single system-playback stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderRequest {
    /// Follow-default or pinned render endpoint identity.
    pub selection: EndpointSelection,
}

/// Opens the selected render endpoint at its current 32-bit float mix format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRenderRequest {
    /// Follow-default or pinned render endpoint identity.
    pub selection: EndpointSelection,
}

/// A successful native submission in this opened stream's frame coordinate.
///
/// Explicit priming and ordinary submissions share this coordinate. The render
/// owner associates it with its output generation. A zero-frame submission
/// leaves the coordinate unchanged and does not acquire a device buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeRenderReceipt {
    /// Inclusive first frame committed by this call.
    pub start_frame: u64,
    /// Exclusive end of the committed span.
    pub end_frame: u64,
    /// Frames committed by this call.
    pub frames: u32,
}

/// Diagnostics recorded while a render stream is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderStreamDiagnostics {
    /// Shared-engine mix format observed before initialize.
    pub mix_format: Option<WasapiFormat>,
    /// Format passed to WASAPI initialization.
    ///
    /// Fixed 48 kHz opens request stereo engine float. Native opens request the accepted mix format.
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
    /// Successful render submissions.
    pub submitted_callbacks: u64,
    /// Ordinary PCM frames successfully submitted, excluding explicit prefill.
    pub submitted_frames: u64,
    /// Frames successfully submitted as explicit startup PCM or silence priming.
    pub prefilled_frames: u64,
    /// All successfully submitted frames in this stream coordinate, including prefill.
    pub committed_frames: u64,
    /// Nominal render-clock units per second, when clock preparation succeeded.
    pub clock_ticks_per_second: Option<NonZeroU64>,
    /// Why the optional clock could not be prepared; ordinary render remains available.
    pub clock_preparation_failure: Option<RenderClockFailure>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RenderCommit {
    submitted_callbacks: u64,
    submitted_frames: u64,
    prefilled_frames: u64,
    committed_frames: u64,
}

impl RenderCommit {
    // Compute the complete next ledger before acquiring a native buffer. The
    // owner installs it only after successful ReleaseBuffer, never on failure.
    fn after_submission(self, frames: u32, prefill: bool) -> Result<Self, AudioError> {
        if frames == 0 {
            return Ok(self);
        }
        let frames = u64::from(frames);
        let mut next = self;
        next.committed_frames =
            self.committed_frames
                .checked_add(frames)
                .ok_or(AudioError::CapacityOverflow {
                    field: "render_committed_frames",
                })?;
        if prefill {
            next.prefilled_frames =
                self.prefilled_frames
                    .checked_add(frames)
                    .ok_or(AudioError::CapacityOverflow {
                        field: "render_prefilled_frames",
                    })?;
        } else {
            next.submitted_frames =
                self.submitted_frames
                    .checked_add(frames)
                    .ok_or(AudioError::CapacityOverflow {
                        field: "render_submitted_frames",
                    })?;
            next.submitted_callbacks =
                self.submitted_callbacks
                    .checked_add(1)
                    .ok_or(AudioError::CapacityOverflow {
                        field: "render_submitted_callbacks",
                    })?;
        }
        Ok(next)
    }
}

/// Event-driven shared-mode WASAPI render stream.
///
/// An opened stream retains its COM initialization and stays on its owner.
///
/// ```compile_fail,E0277
/// use rlb_audio_windows::WasapiRenderStream;
/// fn requires_send<T: Send>() {}
/// requires_send::<WasapiRenderStream>();
/// ```
pub struct WasapiRenderStream {
    client: NativeClient,
    render: IAudioRenderClient,
    clock: Result<PreparedRenderClock, RenderClockFailure>,
    endpoint: EndpointDescriptor,
    selection: EndpointSelection,
    status: BackendStatus,
    commit: RenderCommit,
    // Declared last: all native clients/services release before CoUninitialize.
    _apartment: ComApartment,
}

impl WasapiRenderStream {
    /// Opens a stereo render stream on the current MTA thread.
    ///
    /// The returned stream owns its COM MTA initialization and must be used
    /// and dropped on this thread, including after the enumerator is gone.
    pub fn open(request: RenderRequest) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_render(request)
    }

    /// Opens the endpoint's native PCM format without Windows conversion.
    ///
    /// Same-thread ownership follows [`Self::open`]. The owner explicitly primes
    /// this stream before starting it; event waits and its output clock stay here.
    pub fn open_native(request: NativeRenderRequest) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_native_render(request)
    }

    /// Opens a native mix-format render stream using `preference` for the shared period.
    pub fn open_native_with_period(
        request: NativeRenderRequest,
        preference: SharedPeriodPreference,
    ) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_native_render_with_period(request, preference)
    }

    /// Mix format accepted for this stream, when it was opened natively.
    #[must_use]
    pub const fn native_format(&self) -> Option<NativeF32Format> {
        self.client.native_format
    }

    /// Opens render using `preference` for the shared-mode period.
    ///
    /// Same-thread ownership follows [`Self::open`].
    pub fn open_with_period(
        request: RenderRequest,
        preference: SharedPeriodPreference,
    ) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_render_with_period(request, preference)
    }

    /// Follow/pin selection used to open this stream.
    #[must_use]
    pub const fn selection(&self) -> &EndpointSelection {
        &self.selection
    }

    /// Maximum frames a caller must prepare before `render`.
    #[must_use]
    pub const fn callback_capacity_frames(&self) -> u32 {
        self.client.buffer_frames
    }

    /// Snapshot of format, period, and submit counters.
    #[must_use]
    pub const fn diagnostics(&self) -> RenderStreamDiagnostics {
        RenderStreamDiagnostics {
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
            submitted_callbacks: self.commit.submitted_callbacks,
            submitted_frames: self.commit.submitted_frames,
            prefilled_frames: self.commit.prefilled_frames,
            committed_frames: self.commit.committed_frames,
            clock_ticks_per_second: match &self.clock {
                Ok(clock) => Some(clock.frequency()),
                Err(_) => None,
            },
            clock_preparation_failure: match &self.clock {
                Ok(_) => None,
                Err(error) => Some(*error),
            },
        }
    }

    /// Reads the optional output clock on this stream's owner thread.
    ///
    /// Raw clock units and the correlated QPC pair retain their native status.
    /// A missing or degraded observation does not reset output continuity.
    pub fn clock_observation(&self) -> Result<RenderClockObservation, RenderClockFailure> {
        match &self.clock {
            Ok(clock) => clock.observe(),
            Err(error) => Err(*error),
        }
    }

    /// Waits for the WASAPI render event.
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

    /// Frames currently writable after a wait.
    pub fn available_frames(&self) -> Result<u32, WindowsAudioError> {
        let padding = self.padding_frames()?;
        Ok(self.client.buffer_frames.saturating_sub(padding))
    }

    /// Frames queued in this opened render stream, including explicit priming.
    pub fn padding_frames(&self) -> Result<u32, WindowsAudioError> {
        self.client.padding()
    }

    /// Starts a fresh stream, or resumes the same stopped stream.
    ///
    /// Native streams use only the owner's explicit priming. The fixed 48 kHz
    /// entry retains its initial full-buffer silence prefill.
    /// Repeated calls while running leave its queued audio and counters intact.
    /// Resuming does not reset its clock coordinate or repeat startup prefill.
    pub fn start(&mut self) -> Result<(), WindowsAudioError> {
        match self.status {
            BackendStatus::Ready
                if implicit_start_prefill(self.client.native_format.is_some(), self.status) =>
            {
                if let Err(error) = self.prefill_silence() {
                    self.status = BackendStatus::Faulted;
                    return Err(error);
                }
            }
            BackendStatus::Ready | BackendStatus::Stopped => {}
            BackendStatus::Running => return Ok(()),
            _ => return Err(AudioError::BackendUnavailable.into()),
        }
        if let Err(error) = self.client.start() {
            self.status = BackendStatus::Faulted;
            return Err(error);
        }
        self.status = BackendStatus::Running;
        Ok(())
    }

    /// Submits interleaved 32-bit float at this stream's mix-format channel count.
    ///
    /// The slice length is `frames * channels`. Finite samples keep their bits.
    /// Non-finite input is refused before a device buffer is acquired, and the
    /// commit counters stay unchanged. A failed `ReleaseBuffer` also leaves the
    /// previous commit in place.
    pub fn render_native(
        &mut self,
        interleaved: &[f32],
        frames: usize,
    ) -> Result<(), WindowsAudioError> {
        self.submit_native(interleaved, frames).map(|_| ())
    }

    /// Commits native PCM while running and returns its successful output span.
    pub fn submit_native(
        &mut self,
        interleaved: &[f32],
        frames: usize,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        self.submit_native_pcm(interleaved, frames, false)
    }

    /// Queues exactly `frames` of native PCM before this fresh stream starts.
    ///
    /// Several calls may prime a ready stream up to its actual writable capacity.
    /// Stopped streams retain their existing queue and cannot be primed again.
    pub fn prime_native(
        &mut self,
        interleaved: &[f32],
        frames: usize,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        self.submit_native_pcm(interleaved, frames, true)
    }

    /// Commits canonical F64 PCM to the native device container. Quantization
    /// occurs once here, following source conversion and F64 accumulation.
    pub fn submit_native_f64(
        &mut self,
        interleaved: &[f64],
        frames: usize,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        self.submit_native_pcm_f64(interleaved, frames, false)
    }

    /// Primes a fresh native stream from canonical F64 PCM.
    pub fn prime_native_f64(
        &mut self,
        interleaved: &[f64],
        frames: usize,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        self.submit_native_pcm_f64(interleaved, frames, true)
    }

    fn submit_native_pcm_f64(
        &mut self,
        interleaved: &[f64],
        frames: usize,
        priming: bool,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        let frames_u32 = self.prepare_native_submission(frames, priming)?;
        let format = self
            .client
            .native_format
            .ok_or(WindowsAudioError::LegacyStreamNativeCall)?;
        let required = frames.checked_mul(usize::from(format.channels)).ok_or(
            AudioError::CapacityOverflow {
                field: "render_interleaved_samples",
            },
        )?;
        if interleaved.len() != required {
            return Err(AudioError::RenderStorageLength {
                required,
                actual: interleaved.len(),
            }
            .into());
        }
        let non_finite = interleaved
            .iter()
            .filter(|sample| !sample.is_finite())
            .count() as u64;
        if non_finite != 0 {
            return Err(WindowsAudioError::NativeSamplesNonFinite {
                frames: frames_u32,
                non_finite_samples: non_finite,
                start_sample: self.commit.committed_frames,
            });
        }
        validate_native_f64(format, interleaved)?;
        self.submit_render_data(
            RenderData::NativeF64 {
                samples: interleaved,
                format,
            },
            frames_u32,
            priming,
        )
        .map_err(Into::into)
    }

    /// Queues exactly `frames` of silence before this fresh native stream starts.
    pub fn prime_native_silence(
        &mut self,
        frames: usize,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        let frames = self.prepare_native_submission(frames, true)?;
        self.submit_silence(frames)
    }

    fn prepare_native_submission(
        &mut self,
        frames: usize,
        priming: bool,
    ) -> Result<u32, WindowsAudioError> {
        if self.client.native_format.is_none() {
            return Err(WindowsAudioError::LegacyStreamNativeCall);
        }
        let required_status = if priming {
            BackendStatus::Ready
        } else {
            BackendStatus::Running
        };
        if self.status != required_status {
            return Err(AudioError::BackendUnavailable.into());
        }
        if frames > usize::try_from(self.client.buffer_frames).unwrap_or(usize::MAX) {
            return Err(AudioError::RenderCapacityExceeded.into());
        }
        let frames = u32::try_from(frames).map_err(|_| AudioError::CapacityOverflow {
            field: "render_frames",
        })?;
        if priming && frames > 0 {
            let available = self.available_frames().inspect_err(|_| {
                self.status = BackendStatus::Faulted;
            })?;
            if frames > available {
                return Err(AudioError::RenderCapacityExceeded.into());
            }
        }
        Ok(frames)
    }

    fn submit_native_pcm(
        &mut self,
        interleaved: &[f32],
        frames: usize,
        priming: bool,
    ) -> Result<NativeRenderReceipt, WindowsAudioError> {
        let frames_u32 = self.prepare_native_submission(frames, priming)?;
        let format = self
            .client
            .native_format
            .ok_or(WindowsAudioError::LegacyStreamNativeCall)?;
        if !format.is_f32() {
            return Err(AudioError::BackendContractViolation.into());
        }
        let required = frames.checked_mul(usize::from(format.channels)).ok_or(
            AudioError::CapacityOverflow {
                field: "render_interleaved_samples",
            },
        )?;
        if interleaved.len() != required {
            return Err(AudioError::RenderStorageLength {
                required,
                actual: interleaved.len(),
            }
            .into());
        }
        let non_finite = non_finite_sample_count(interleaved);
        if non_finite > 0 {
            return Err(WindowsAudioError::NativeSamplesNonFinite {
                frames: frames_u32,
                non_finite_samples: non_finite,
                start_sample: self.commit.committed_frames,
            });
        }
        self.submit_interleaved(interleaved, frames_u32, priming)
            .map_err(Into::into)
    }

    /// Stops rendering and keeps the endpoint configuration.
    pub fn stop(&mut self) -> Result<(), WindowsAudioError> {
        match self.status {
            BackendStatus::Running => {}
            BackendStatus::Ready | BackendStatus::Stopped => return Ok(()),
            _ => return Err(AudioError::BackendUnavailable.into()),
        }
        if let Err(error) = self.client.stop() {
            self.status = BackendStatus::Faulted;
            return Err(error);
        }
        self.status = BackendStatus::Stopped;
        Ok(())
    }

    fn prefill_silence(&mut self) -> Result<(), WindowsAudioError> {
        self.submit_silence(self.client.buffer_frames).map(|_| ())
    }

    fn submit_silence(&mut self, frames: u32) -> Result<NativeRenderReceipt, WindowsAudioError> {
        let start_frame = self.commit.committed_frames;
        if frames == 0 {
            return Ok(commit_receipt(start_frame, self.commit, frames));
        }
        let next_commit = self
            .commit
            .after_submission(frames, true)
            .inspect_err(|_| {
                self.status = BackendStatus::Faulted;
            })?;
        let _data = unsafe { self.render.GetBuffer(frames) }
            .map_err(|error| hresult("IAudioRenderClient::GetBuffer", error))
            .inspect_err(|_| {
                self.status = BackendStatus::Faulted;
            })?;
        self.commit = gate_commit(
            unsafe {
                self.render
                    .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            }
            .map_err(|error| hresult("IAudioRenderClient::ReleaseBuffer", error)),
            next_commit,
        )
        .inspect_err(|_| {
            self.status = BackendStatus::Faulted;
        })?;
        Ok(commit_receipt(start_frame, self.commit, frames))
    }

    fn submit_interleaved(
        &mut self,
        interleaved: &[f32],
        frames: u32,
        prefill: bool,
    ) -> Result<NativeRenderReceipt, AudioError> {
        self.submit_render_data(RenderData::F32(interleaved), frames, prefill)
    }

    fn submit_render_data(
        &mut self,
        data_source: RenderData<'_>,
        frames: u32,
        prefill: bool,
    ) -> Result<NativeRenderReceipt, AudioError> {
        let start_frame = self.commit.committed_frames;
        if frames == 0 {
            return Ok(commit_receipt(start_frame, self.commit, frames));
        }
        let prepared = self
            .commit
            .after_submission(frames, prefill)
            .inspect_err(|_| {
                self.status = BackendStatus::Faulted;
            })?;
        let bytes = data_source.byte_len()?;
        let data = unsafe { self.render.GetBuffer(frames) }.map_err(|_| {
            self.status = BackendStatus::Faulted;
            AudioError::BackendUnavailable
        })?;
        if data.is_null() {
            let _ = unsafe { self.render.ReleaseBuffer(0, 0) };
            self.status = BackendStatus::Faulted;
            return Err(AudioError::BackendUnavailable);
        }
        data_source.write(unsafe { std::slice::from_raw_parts_mut(data, bytes) });
        match gate_commit(
            unsafe { self.render.ReleaseBuffer(frames, 0) }
                .map_err(|_| AudioError::BackendUnavailable),
            prepared,
        ) {
            Ok(commit) => {
                self.commit = commit;
                Ok(commit_receipt(start_frame, commit, frames))
            }
            Err(error) => {
                self.status = BackendStatus::Faulted;
                Err(error)
            }
        }
    }
}

fn implicit_start_prefill(native: bool, status: BackendStatus) -> bool {
    !native && status == BackendStatus::Ready
}

fn commit_receipt(start_frame: u64, committed: RenderCommit, frames: u32) -> NativeRenderReceipt {
    NativeRenderReceipt {
        start_frame,
        end_frame: committed.committed_frames,
        frames,
    }
}

fn gate_commit<T>(release: Result<(), T>, prepared: RenderCommit) -> Result<RenderCommit, T> {
    release?;
    Ok(prepared)
}

impl EndpointEnumerator {
    /// Resolves `request` against the current catalog and opens a render stream.
    pub fn open_render(
        &self,
        request: RenderRequest,
    ) -> Result<WasapiRenderStream, WindowsAudioError> {
        self.open_render_with_period(request, SharedPeriodPreference::Default)
    }

    /// Opens render using `preference` for the shared-mode period.
    pub fn open_render_with_period(
        &self,
        request: RenderRequest,
        preference: SharedPeriodPreference,
    ) -> Result<WasapiRenderStream, WindowsAudioError> {
        let catalog = self.enumerate()?;
        let info = resolve_selection(&request.selection, EndpointDirection::Render, &catalog)?;
        let endpoint = info.to_descriptor(EndpointDirection::Render, 2)?;
        let apartment = ComApartment::initialize_mta()?;
        let client =
            NativeClient::open(self, info.endpoint_id(), 2, ClientUsage::Render, preference)?;
        let render = client.render_service()?;
        let clock = client
            .clock_service()
            .map_err(RenderClockFailure::Call)
            .and_then(PreparedRenderClock::new);
        Ok(WasapiRenderStream {
            client,
            render,
            clock,
            endpoint,
            selection: request.selection,
            status: BackendStatus::Ready,
            commit: RenderCommit::default(),
            _apartment: apartment,
        })
    }

    /// Opens the render endpoint's current validated native PCM mix format.
    pub fn open_native_render(
        &self,
        request: NativeRenderRequest,
    ) -> Result<WasapiRenderStream, WindowsAudioError> {
        self.open_native_render_with_period(request, SharedPeriodPreference::Default)
    }

    /// Opens a native mix-format render stream using `preference` for the shared period.
    pub fn open_native_render_with_period(
        &self,
        request: NativeRenderRequest,
        preference: SharedPeriodPreference,
    ) -> Result<WasapiRenderStream, WindowsAudioError> {
        let catalog = self.enumerate()?;
        let info = resolve_selection(&request.selection, EndpointDirection::Render, &catalog)?;
        let apartment = ComApartment::initialize_mta()?;
        let client = NativeClient::open_native_pcm(
            self,
            info.endpoint_id(),
            ClientUsage::Render,
            preference,
        )?;
        let native = client
            .native_format
            .ok_or(WindowsAudioError::NativeFormatRejected {
                reason: crate::format::NativeFormatReject::MixFormatUnavailable,
            })?;
        let endpoint = EndpointDescriptor::new(
            info.endpoint_id(),
            info.friendly_name(),
            EndpointDirection::Render,
            info.default_roles(),
            info.state(),
            native.format_range(),
        )?;
        let render = client.render_service()?;
        let clock = client
            .clock_service()
            .map_err(RenderClockFailure::Call)
            .and_then(PreparedRenderClock::new);
        Ok(WasapiRenderStream {
            client,
            render,
            clock,
            endpoint,
            selection: request.selection,
            status: BackendStatus::Ready,
            commit: RenderCommit::default(),
            _apartment: apartment,
        })
    }
}

impl RenderBackend for WasapiRenderStream {
    fn status(&self) -> BackendStatus {
        self.status
    }

    fn endpoint(&self) -> &EndpointDescriptor {
        &self.endpoint
    }

    fn render(&mut self, interleaved_stereo: &[f32], frames: usize) -> Result<(), AudioError> {
        if self.client.native_format.is_some() {
            return Err(AudioError::BackendContractViolation);
        }
        if self.status != BackendStatus::Running {
            return Err(AudioError::BackendUnavailable);
        }
        if frames == 0 {
            return Ok(());
        }
        if frames > self.client.buffer_frames as usize {
            return Err(AudioError::RenderCapacityExceeded);
        }
        let required = frames.checked_mul(2).ok_or(AudioError::CapacityOverflow {
            field: "render_interleaved_samples",
        })?;
        if interleaved_stereo.len() != required {
            return Err(AudioError::RenderStorageLength {
                required,
                actual: interleaved_stereo.len(),
            });
        }
        let frames_u32 = u32::try_from(frames).map_err(|_| AudioError::CapacityOverflow {
            field: "render_frames",
        })?;
        self.submit_interleaved(interleaved_stereo, frames_u32, false)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointRole;

    #[test]
    fn commit_ledger_counts_prefill_and_checks_before_installing() {
        let initial = RenderCommit::default();
        let prefilled = initial.after_submission(1_056, true).unwrap();
        assert_eq!(initial.committed_frames, 0);
        assert_eq!(prefilled.committed_frames, 1_056);
        assert_eq!(prefilled.prefilled_frames, 1_056);
        assert_eq!(prefilled.submitted_frames, 0);
        let submitted = prefilled.after_submission(480, false).unwrap();
        assert_eq!(submitted.committed_frames, 1_536);
        assert_eq!(submitted.submitted_frames, 480);
        assert_eq!(submitted.submitted_callbacks, 1);
        assert_eq!(submitted.prefilled_frames, 1_056);
        assert_eq!(submitted.after_submission(0, false).unwrap(), submitted);
        let full = RenderCommit {
            committed_frames: u64::MAX,
            ..submitted
        };
        assert!(full.after_submission(1, false).is_err());
        assert_eq!(full.committed_frames, u64::MAX);
        let full_callbacks = RenderCommit {
            submitted_callbacks: u64::MAX,
            ..submitted
        };
        assert!(full_callbacks.after_submission(1, false).is_err());
        assert_eq!(full_callbacks.committed_frames, 1_536);
    }

    #[test]
    fn failed_release_does_not_replace_the_installed_commit() {
        let current = RenderCommit {
            submitted_callbacks: 2,
            submitted_frames: 480,
            prefilled_frames: 1_056,
            committed_frames: 1_536,
        };
        let prepared = current.after_submission(128, false).unwrap();
        assert_eq!(current.committed_frames, 1_536);
        assert!(gate_commit(Err(AudioError::BackendUnavailable), prepared).is_err());
        assert_eq!(current.committed_frames, 1_536);
        let installed = gate_commit::<AudioError>(Ok(()), prepared).unwrap();
        assert_eq!(installed.committed_frames, 1_664);
        assert_eq!(installed.submitted_frames, 608);
        assert_eq!(current.committed_frames, 1_536);
        assert_eq!(
            commit_receipt(current.committed_frames, installed, 128),
            NativeRenderReceipt {
                start_frame: 1_536,
                end_frame: 1_664,
                frames: 128,
            }
        );
    }

    #[test]
    fn native_start_leaves_priming_to_owner_and_receipts_share_commit_coordinate() {
        for status in [
            BackendStatus::Ready,
            BackendStatus::Stopped,
            BackendStatus::Running,
        ] {
            assert!(!implicit_start_prefill(true, status));
            assert_eq!(
                implicit_start_prefill(false, status),
                status == BackendStatus::Ready
            );
        }
        let initial = RenderCommit::default();
        let primed = initial.after_submission(96, true).unwrap();
        assert_eq!(
            commit_receipt(initial.committed_frames, primed, 96),
            NativeRenderReceipt {
                start_frame: 0,
                end_frame: 96,
                frames: 96,
            }
        );
        let submitted = primed.after_submission(48, false).unwrap();
        assert_eq!(
            commit_receipt(primed.committed_frames, submitted, 48),
            NativeRenderReceipt {
                start_frame: 96,
                end_frame: 144,
                frames: 48,
            }
        );
        assert_eq!(submitted.prefilled_frames, 96);
        assert_eq!(submitted.submitted_frames, 48);
        assert_eq!(submitted.committed_frames, 144);
        assert_eq!(
            commit_receipt(submitted.committed_frames, submitted, 0),
            NativeRenderReceipt {
                start_frame: 144,
                end_frame: 144,
                frames: 0,
            }
        );
    }

    #[test]
    #[ignore = "opens and starts default output; select explicitly with --ignored"]
    fn default_render_accepts_silence_callbacks() {
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        let catalog = enumerator.enumerate().expect("catalog");
        if catalog
            .default_endpoint(EndpointDirection::Render, EndpointRole::Console)
            .is_none()
        {
            return;
        }
        let mut stream = enumerator
            .open_render(RenderRequest {
                selection: EndpointSelection::follow_default(),
            })
            .expect("open render");
        stream.start().expect("start");
        let mut submitted = 0_u32;
        for _ in 0..10 {
            match stream.wait(1_000) {
                Ok(EventWait::Signaled) => {}
                Ok(EventWait::Timeout) => continue,
                Err(error) => panic!("wait failed: {error}"),
            }
            let available = stream.available_frames().unwrap_or(0);
            if available == 0 {
                continue;
            }
            let frames = available.min(stream.diagnostics().period_frames.max(1));
            let silence = vec![0.0_f32; frames as usize * 2];
            stream
                .render(&silence, frames as usize)
                .expect("render silence");
            submitted = submitted.saturating_add(frames);
            if submitted >= 480 {
                break;
            }
        }
        stream.stop().expect("stop");
        assert!(
            submitted > 0,
            "shared render should accept at least one silence callback"
        );
    }
}
