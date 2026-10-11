use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMOPTIONS_NONE,
    AudioCategory_Media, AudioClientProperties, IAudioCaptureClient, IAudioClient, IAudioClient2,
    IAudioClient3, IAudioRenderClient, IMMDevice, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoTaskMemFree};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

use crate::endpoint::EndpointEnumerator;
use crate::format::{hns_to_frames, parse_mix_format};
use crate::{WasapiFormat, WindowsAudioError, hresult};

const AUDCLNT_E_UNSUPPORTED_FORMAT: u32 = 0x8889_0008;

/// Result of waiting on the WASAPI stream event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventWait {
    /// The endpoint signaled that a callback buffer is ready.
    Signaled,
    /// The wait reached its timeout without a callback.
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientUsage {
    Capture,
    LoopbackCapture,
    Render,
}

/// Which shared-mode engine period a new stream asks Windows for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SharedPeriodPreference {
    /// The engine's default shared period. Existing callers keep this.
    #[default]
    Default,
    /// The shortest shared period `IAudioClient3` reports for the engine format.
    Minimum,
}

/// Shared-engine periods reported for the requested stream format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedEnginePeriods {
    /// Default shared-engine period in frames.
    pub default_period_frames: u32,
    /// Valid requested periods are multiples of this frame count.
    pub fundamental_period_frames: u32,
    /// Smallest supported shared-engine period in frames.
    pub minimum_period_frames: u32,
    /// Largest supported shared-engine period in frames.
    pub maximum_period_frames: u32,
}

/// A retained Windows API failure from shared-period negotiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasapiCallFailure {
    /// Exact operation that failed, including activation or interface lookup.
    pub operation: &'static str,
    /// Unmodified HRESULT bits.
    pub code: u32,
}

impl WasapiCallFailure {
    fn new(operation: &'static str, error: windows::core::Error) -> Self {
        Self {
            operation,
            code: error.code().0 as u32,
        }
    }
}

impl From<WasapiCallFailure> for WindowsAudioError {
    fn from(failure: WasapiCallFailure) -> Self {
        Self::Hresult {
            operation: failure.operation,
            code: failure.code,
        }
    }
}

/// Owns the allocation returned by `IAudioClient::GetMixFormat` on every exit.
struct MixFormatAllocation(*mut WAVEFORMATEX);

impl Drop for MixFormatAllocation {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

/// Keeps a successfully created stream event alive until all open steps finish.
struct StreamEvent(HANDLE);

impl StreamEvent {
    fn new() -> Result<Self, WindowsAudioError> {
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(|error| hresult("CreateEventW", error))
    }
}

impl Drop for StreamEvent {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

pub(crate) struct NativeClient {
    client: IAudioClient,
    event: StreamEvent,
    pub mix_format: Option<WasapiFormat>,
    pub engine_format: WasapiFormat,
    pub autoconvert: bool,
    pub iaudioclient3: bool,
    pub buffer_frames: u32,
    pub period_frames: u32,
    pub requested_period_frames: u32,
    pub default_period_hns: i64,
    pub minimum_period_hns: i64,
    pub shared_engine_periods: Option<SharedEnginePeriods>,
    pub shared_engine_period_failure: Option<WasapiCallFailure>,
    pub minimum_period_failure: Option<WasapiCallFailure>,
    pub default_period_failure: Option<WasapiCallFailure>,
}

impl NativeClient {
    pub(crate) fn open(
        enumerator: &EndpointEnumerator,
        endpoint_id: &str,
        channels: u8,
        usage: ClientUsage,
        preference: SharedPeriodPreference,
    ) -> Result<Self, WindowsAudioError> {
        let device = enumerator.get_device(endpoint_id)?;
        Self::initialize(device, channels, usage, preference)
    }

    fn initialize(
        device: IMMDevice,
        channels: u8,
        usage: ClientUsage,
        preference: SharedPeriodPreference,
    ) -> Result<Self, WindowsAudioError> {
        let mut client = activate_client(&device)?;
        if usage != ClientUsage::LoopbackCapture {
            let _ = set_media_category(&client);
        }
        let mix_allocation = unsafe { client.GetMixFormat() }
            .ok()
            .map(MixFormatAllocation);
        let mix_ptr = mix_allocation.as_ref().map(|allocation| allocation.0);
        let mix_format = mix_ptr.and_then(|ptr| unsafe { parse_mix_format(ptr) }.ok());
        let (default_period_hns, minimum_period_hns) = read_periods(&client);
        let engine_wave = crate::format::WasapiFormat::engine_wave_format(channels)?;
        let engine_format = WasapiFormat::from_wave_format(&engine_wave);

        let mut iaudioclient3 = false;
        let mut autoconvert = false;
        let mut period_frames = hns_to_frames(default_period_hns, 48_000).max(1);
        let mut requested_period_frames = period_frames;
        let mut initialized = false;
        let engine_period_result = client
            .cast::<IAudioClient3>()
            .map_err(|error| WasapiCallFailure::new("IAudioClient::cast(IAudioClient3)", error))
            .and_then(|client3| shared_engine_periods(&client3, engine_wave.as_waveformatex_ref()));
        let (engine_periods, shared_engine_period_failure) = match engine_period_result {
            Ok(periods) => (Some(periods), None),
            Err(failure) => (None, Some(failure)),
        };
        let mut minimum_period_failure = None;
        let mut default_period_failure = None;

        if preference == SharedPeriodPreference::Minimum {
            if let Some(periods) = engine_periods {
                requested_period_frames = periods.minimum_period_frames.max(1);
                match try_shared_stream(&device, usage, &engine_wave, requested_period_frames) {
                    Ok(opened) => {
                        client = opened;
                        period_frames = requested_period_frames;
                        iaudioclient3 = true;
                        initialized = true;
                    }
                    Err(failure) => minimum_period_failure = Some(failure),
                }
                if !initialized {
                    match try_shared_stream(
                        &device,
                        usage,
                        &engine_wave,
                        periods.default_period_frames.max(1),
                    ) {
                        Ok(opened) => {
                            client = opened;
                            period_frames = periods.default_period_frames.max(1);
                            iaudioclient3 = true;
                            initialized = true;
                        }
                        Err(failure) => default_period_failure = Some(failure),
                    }
                }
                if !initialized {
                    client = activate_client(&device)?;
                    if usage != ClientUsage::LoopbackCapture {
                        let _ = set_media_category(&client);
                    }
                }
            }
        } else if usage != ClientUsage::LoopbackCapture {
            if let Some(periods) = engine_periods {
                period_frames = periods.default_period_frames.max(1);
                requested_period_frames = period_frames;
                match try_shared_stream(&device, usage, &engine_wave, period_frames) {
                    Ok(opened) => {
                        client = opened;
                        iaudioclient3 = true;
                        initialized = true;
                    }
                    Err(failure) => default_period_failure = Some(failure),
                }
            }
            if !initialized {
                client = activate_client(&device)?;
                let _ = set_media_category(&client);
            }
        }

        if !initialized {
            period_frames = hns_to_frames(default_period_hns, engine_format.sample_rate_hz).max(1);
            let mix_is_engine = mix_format.is_some_and(|format| format.is_engine_f32(channels));
            autoconvert = !mix_is_engine;
            let format: *const WAVEFORMATEX = if mix_is_engine {
                mix_ptr.expect("mix format pointer")
            } else {
                engine_wave.as_waveformatex_ref()
            };
            try_initialize(&client, format, usage, autoconvert)?;
        }

        drop(mix_allocation);

        let event = StreamEvent::new()?;
        unsafe { client.SetEventHandle(event.0) }
            .map_err(|error| hresult("IAudioClient::SetEventHandle", error))?;

        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|error| hresult("IAudioClient::GetBufferSize", error))?;
        if buffer_frames == 0 {
            return Err(WindowsAudioError::UnsupportedEngineFormat);
        }

        Ok(Self {
            client,
            event,
            mix_format,
            engine_format,
            autoconvert,
            iaudioclient3,
            buffer_frames,
            period_frames,
            requested_period_frames,
            default_period_hns,
            minimum_period_hns,
            shared_engine_periods: engine_periods,
            shared_engine_period_failure,
            minimum_period_failure,
            default_period_failure,
        })
    }

    pub(crate) fn wait(&self, timeout_ms: u32) -> Result<EventWait, WindowsAudioError> {
        let result = unsafe { WaitForSingleObject(self.event.0, timeout_ms) };
        if result == WAIT_OBJECT_0 {
            Ok(EventWait::Signaled)
        } else if result == WAIT_TIMEOUT {
            Ok(EventWait::Timeout)
        } else if result == WAIT_FAILED {
            Err(WindowsAudioError::WaitFailed {
                code: unsafe { windows::Win32::Foundation::GetLastError().0 },
            })
        } else {
            Err(WindowsAudioError::WaitFailed { code: result.0 })
        }
    }

    pub(crate) fn start(&self) -> Result<(), WindowsAudioError> {
        unsafe { self.client.Start() }.map_err(|error| hresult("IAudioClient::Start", error))
    }

    pub(crate) fn stop(&self) -> Result<(), WindowsAudioError> {
        unsafe { self.client.Stop() }.map_err(|error| hresult("IAudioClient::Stop", error))
    }

    pub(crate) fn padding(&self) -> Result<u32, WindowsAudioError> {
        unsafe { self.client.GetCurrentPadding() }
            .map_err(|error| hresult("IAudioClient::GetCurrentPadding", error))
    }

    pub(crate) fn capture_service(&self) -> Result<IAudioCaptureClient, WindowsAudioError> {
        unsafe { self.client.GetService() }
            .map_err(|error| hresult("IAudioClient::GetService(IAudioCaptureClient)", error))
    }

    pub(crate) fn render_service(&self) -> Result<IAudioRenderClient, WindowsAudioError> {
        unsafe { self.client.GetService() }
            .map_err(|error| hresult("IAudioClient::GetService(IAudioRenderClient)", error))
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

fn activate_client(device: &IMMDevice) -> Result<IAudioClient, WasapiCallFailure> {
    unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|error| WasapiCallFailure::new("IMMDevice::Activate(IAudioClient)", error))
}

fn set_media_category(client: &IAudioClient) -> Result<(), WindowsAudioError> {
    let client2 = client
        .cast::<IAudioClient2>()
        .map_err(|error| hresult("IAudioClient::cast(IAudioClient2)", error))?;
    let properties = AudioClientProperties {
        cbSize: size_of::<AudioClientProperties>() as u32,
        bIsOffload: false.into(),
        eCategory: AudioCategory_Media,
        Options: AUDCLNT_STREAMOPTIONS_NONE,
    };
    unsafe { client2.SetClientProperties(&properties) }
        .map_err(|error| hresult("IAudioClient2::SetClientProperties", error))
}

fn read_periods(client: &IAudioClient) -> (i64, i64) {
    let mut default_period = 0_i64;
    let mut minimum_period = 0_i64;
    let _ = unsafe { client.GetDevicePeriod(Some(&mut default_period), Some(&mut minimum_period)) };
    (default_period, minimum_period)
}

fn shared_engine_periods(
    client3: &IAudioClient3,
    format: &windows::Win32::Media::Audio::WAVEFORMATEX,
) -> Result<SharedEnginePeriods, WasapiCallFailure> {
    let mut default_period = 0_u32;
    let mut fundamental = 0_u32;
    let mut min_period = 0_u32;
    let mut max_period = 0_u32;
    unsafe {
        client3.GetSharedModeEnginePeriod(
            format,
            &mut default_period,
            &mut fundamental,
            &mut min_period,
            &mut max_period,
        )
    }
    .map_err(|error| WasapiCallFailure::new("IAudioClient3::GetSharedModeEnginePeriod", error))?;
    Ok(SharedEnginePeriods {
        default_period_frames: default_period,
        fundamental_period_frames: fundamental,
        minimum_period_frames: min_period,
        maximum_period_frames: max_period,
    })
}

fn try_shared_stream(
    device: &IMMDevice,
    usage: ClientUsage,
    engine_wave: &wasapi::WaveFormat,
    period_frames: u32,
) -> Result<IAudioClient, WasapiCallFailure> {
    let client = activate_client(device)?;
    if usage != ClientUsage::LoopbackCapture {
        let _ = set_media_category(&client);
    }
    let client3 = client
        .cast::<IAudioClient3>()
        .map_err(|error| WasapiCallFailure::new("IAudioClient::cast(IAudioClient3)", error))?;
    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    if usage == ClientUsage::LoopbackCapture {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    unsafe {
        client3.InitializeSharedAudioStream(
            flags,
            period_frames,
            engine_wave.as_waveformatex_ref(),
            None,
        )
    }
    .map_err(|error| WasapiCallFailure::new("IAudioClient3::InitializeSharedAudioStream", error))?;
    Ok(client)
}

fn try_initialize(
    client: &IAudioClient,
    format: *const WAVEFORMATEX,
    usage: ClientUsage,
    autoconvert: bool,
) -> Result<(), WindowsAudioError> {
    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    if usage == ClientUsage::LoopbackCapture {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    if autoconvert {
        flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    }
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, format, None) }
        .map_err(map_init_error)
}

fn map_init_error(error: windows::core::Error) -> WindowsAudioError {
    if error.code().0 as u32 == AUDCLNT_E_UNSUPPORTED_FORMAT {
        WindowsAudioError::UnsupportedEngineFormat
    } else {
        hresult("IAudioClient::Initialize", error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::GetHandleInformation;

    #[test]
    fn stream_event_closes_on_later_open_failure() {
        let mut raw = HANDLE::default();
        let result = (|| -> Result<(), WindowsAudioError> {
            let event = StreamEvent::new()?;
            raw = event.0;
            let mut flags = 0_u32;
            unsafe { GetHandleInformation(raw, &mut flags) }
                .map_err(|error| hresult("GetHandleInformation", error))?;
            Err(WindowsAudioError::UnsupportedEngineFormat)
        })();
        assert!(matches!(
            result,
            Err(WindowsAudioError::UnsupportedEngineFormat)
        ));
        let mut flags = 0_u32;
        let error = unsafe { GetHandleInformation(raw, &mut flags) }
            .expect_err("failed stream open must close its event");
        assert_eq!(error.code().0 as u32, 0x8007_0006);
    }
}
