use rlb_audio::{AudioError, BackendStatus, EndpointDescriptor, EndpointDirection, RenderBackend};
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, IAudioRenderClient};

use crate::client::{
    ClientUsage, EventWait, NativeClient, SharedEnginePeriods, SharedPeriodPreference,
    WasapiCallFailure,
};
use crate::endpoint::{EndpointEnumerator, EndpointSelection, resolve_selection};
use crate::{WasapiFormat, WindowsAudioError, hresult};

/// Supervisor request used to open the single system-playback stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderRequest {
    /// Follow-default or pinned render endpoint identity.
    pub selection: EndpointSelection,
}

/// Diagnostics recorded while a render stream is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderStreamDiagnostics {
    /// Shared-engine mix format observed before initialize.
    pub mix_format: Option<WasapiFormat>,
    /// 48 kHz stereo f32 format requested from WASAPI.
    pub engine_format: WasapiFormat,
    /// Whether Windows performed rate/channel conversion.
    pub autoconvert: bool,
    /// Whether `IAudioClient3::InitializeSharedAudioStream` succeeded.
    pub iaudioclient3: bool,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    /// Period asked of `IAudioClient3` before any fallback.
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
    /// Stereo frames written to the endpoint.
    pub submitted_frames: u64,
}

/// Event-driven shared-mode WASAPI render stream.
pub struct WasapiRenderStream {
    client: NativeClient,
    render: IAudioRenderClient,
    endpoint: EndpointDescriptor,
    selection: EndpointSelection,
    status: BackendStatus,
    submitted_callbacks: u64,
    submitted_frames: u64,
}

impl WasapiRenderStream {
    /// Opens a stereo render stream on the current MTA thread.
    ///
    /// The caller must keep a COM MTA initialization alive for the stream's
    /// complete lifetime. Prefer an owned `EndpointEnumerator` on the audio
    /// thread and its `open_render` method, with the stream dropped first.
    pub fn open(request: RenderRequest) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        enumerator.open_render(request)
    }

    /// Opens render using `preference` for the shared-mode period.
    ///
    /// The caller must retain COM MTA initialization as described in [`Self::open`].
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
            submitted_callbacks: self.submitted_callbacks,
            submitted_frames: self.submitted_frames,
        }
    }

    /// Waits for the WASAPI render event.
    pub fn wait(&self, timeout_ms: u32) -> Result<EventWait, WindowsAudioError> {
        self.client.wait(timeout_ms)
    }

    /// Frames currently writable after a wait.
    pub fn available_frames(&self) -> Result<u32, WindowsAudioError> {
        let padding = self.client.padding()?;
        Ok(self.client.buffer_frames.saturating_sub(padding))
    }

    /// Prefills silence and starts the endpoint stream after prefill succeeds.
    pub fn start(&mut self) -> Result<(), WindowsAudioError> {
        self.prefill_silence()?;
        self.client.start()?;
        self.status = BackendStatus::Running;
        Ok(())
    }

    /// Stops rendering and keeps the endpoint configuration.
    pub fn stop(&mut self) -> Result<(), WindowsAudioError> {
        self.client.stop()?;
        self.status = BackendStatus::Stopped;
        Ok(())
    }

    fn prefill_silence(&self) -> Result<(), WindowsAudioError> {
        let frames = self.client.buffer_frames;
        if frames == 0 {
            return Ok(());
        }
        let data = unsafe { self.render.GetBuffer(frames) }
            .map_err(|error| hresult("IAudioRenderClient::GetBuffer", error))?;
        unsafe {
            self.render
                .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
        }
        .map_err(|error| hresult("IAudioRenderClient::ReleaseBuffer", error))?;
        let _ = data;
        Ok(())
    }
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
        let client =
            NativeClient::open(self, info.endpoint_id(), 2, ClientUsage::Render, preference)?;
        let render = client.render_service()?;
        Ok(WasapiRenderStream {
            client,
            render,
            endpoint,
            selection: request.selection,
            status: BackendStatus::Ready,
            submitted_callbacks: 0,
            submitted_frames: 0,
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
        let data = unsafe { self.render.GetBuffer(frames_u32) }.map_err(|_| {
            self.status = BackendStatus::Faulted;
            AudioError::BackendUnavailable
        })?;
        if data.is_null() {
            let _ = unsafe { self.render.ReleaseBuffer(0, 0) };
            self.status = BackendStatus::Faulted;
            return Err(AudioError::BackendUnavailable);
        }
        let dest = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), required) };
        dest.copy_from_slice(interleaved_stereo);
        if unsafe { self.render.ReleaseBuffer(frames_u32, 0) }.is_err() {
            self.status = BackendStatus::Faulted;
            return Err(AudioError::BackendUnavailable);
        }
        self.submitted_callbacks = self.submitted_callbacks.saturating_add(1);
        self.submitted_frames = self.submitted_frames.saturating_add(frames as u64);
        Ok(())
    }
}

// SAFETY: the stream is confined to one audio-worker thread after open. WASAPI
// MTA COM interfaces and the event handle are not shared with another owner.
unsafe impl Send for WasapiRenderStream {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointRole;

    #[test]
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
