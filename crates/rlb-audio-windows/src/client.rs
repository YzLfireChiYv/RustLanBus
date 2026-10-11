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

struct EnginePeriods {
    default_period: u32,
    min_period: u32,
}

pub(crate) struct NativeClient {
    client: IAudioClient,
    event: HANDLE,
    pub mix_format: Option<WasapiFormat>,
    pub engine_format: WasapiFormat,
    pub autoconvert: bool,
    pub iaudioclient3: bool,
    pub buffer_frames: u32,
    pub period_frames: u32,
    pub requested_period_frames: u32,
    pub default_period_hns: i64,
    pub minimum_period_hns: i64,
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
        let mix_ptr = unsafe { client.GetMixFormat() }.ok();
        let mix_format = mix_ptr.and_then(|ptr| unsafe { parse_mix_format(ptr) }.ok());
        let (default_period_hns, minimum_period_hns) = read_periods(&client);
        let engine_wave = crate::format::WasapiFormat::engine_wave_format(channels)?;
        let engine_format = WasapiFormat::from_wave_format(&engine_wave);

        let mut iaudioclient3 = false;
        let mut autoconvert = false;
        let mut period_frames = hns_to_frames(default_period_hns, 48_000).max(1);
        let mut requested_period_frames = period_frames;
        let mut initialized = false;
        let engine_periods = client
            .cast::<IAudioClient3>()
            .ok()
            .and_then(|client3| {
                shared_engine_periods(&client3, engine_wave.as_waveformatex_ref()).ok()
            });

        if preference == SharedPeriodPreference::Minimum {
            if let Some(periods) = engine_periods {
                requested_period_frames = periods.min_period.max(1);
                if let Some(opened) =
                    try_shared_stream(&device, usage, &engine_wave, requested_period_frames)
                {
                    client = opened;
                    period_frames = requested_period_frames;
                    iaudioclient3 = true;
                    initialized = true;
                } else if let Some(opened) =
                    try_shared_stream(&device, usage, &engine_wave, periods.default_period.max(1))
                {
                    client = opened;
                    period_frames = periods.default_period.max(1);
                    iaudioclient3 = true;
                    initialized = true;
                } else {
                    client = activate_client(&device)?;
                    if usage != ClientUsage::LoopbackCapture {
                        let _ = set_media_category(&client);
                    }
                }
            }
        } else if usage != ClientUsage::LoopbackCapture {
            if let Some(periods) = engine_periods {
                period_frames = periods.default_period.max(1);
                requested_period_frames = period_frames;
                if let Some(opened) =
                    try_shared_stream(&device, usage, &engine_wave, period_frames)
                {
                    client = opened;
                    iaudioclient3 = true;
                    initialized = true;
                }
            }
            if !initialized {
                client = activate_client(&device)?;
                let _ = set_media_category(&client);
            }
        }

        if !initialized {
            let mix_is_engine = mix_format.is_some_and(|format| format.is_engine_f32(channels));
            autoconvert = !mix_is_engine;
            let format: *const WAVEFORMATEX = if mix_is_engine {
                mix_ptr.expect("mix format pointer")
            } else {
                engine_wave.as_waveformatex_ref()
            };
            try_initialize(&client, format, usage, autoconvert)?;
        }

        if let Some(ptr) = mix_ptr {
            unsafe { CoTaskMemFree(Some(ptr.cast())) };
        }

        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| hresult("CreateEventW", error))?;
        if let Err(error) = unsafe { client.SetEventHandle(event) } {
            let _ = unsafe { CloseHandle(event) };
            return Err(hresult("IAudioClient::SetEventHandle", error));
        }

        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|error| hresult("IAudioClient::GetBufferSize", error))?;
        if buffer_frames == 0 {
            let _ = unsafe { CloseHandle(event) };
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
        })
    }

    pub(crate) fn wait(&self, timeout_ms: u32) -> Result<EventWait, WindowsAudioError> {
        let result = unsafe { WaitForSingleObject(self.event, timeout_ms) };
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
        if !self.event.is_invalid() {
            let _ = unsafe { CloseHandle(self.event) };
            self.event = HANDLE::default();
        }
    }
}

fn activate_client(device: &IMMDevice) -> Result<IAudioClient, WindowsAudioError> {
    unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|error| hresult("IMMDevice::Activate(IAudioClient)", error))
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
) -> Result<EnginePeriods, WindowsAudioError> {
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
    .map_err(|error| hresult("IAudioClient3::GetSharedModeEnginePeriod", error))?;
    Ok(EnginePeriods {
        default_period,
        min_period,
    })
}

fn try_shared_stream(
    device: &IMMDevice,
    usage: ClientUsage,
    engine_wave: &wasapi::WaveFormat,
    period_frames: u32,
) -> Option<IAudioClient> {
    let client = activate_client(device).ok()?;
    if usage != ClientUsage::LoopbackCapture {
        let _ = set_media_category(&client);
    }
    let client3 = client.cast::<IAudioClient3>().ok()?;
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
    .ok()?;
    Some(client)
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
