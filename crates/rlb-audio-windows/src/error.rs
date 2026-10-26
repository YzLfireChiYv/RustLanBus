use rlb_audio::AudioError;
use thiserror::Error;

/// Failures produced while enumerating, opening, or running a Windows endpoint.
///
/// HRESULT formatting and user-facing text belong on a supervisor or
/// diagnostics thread. Realtime methods only construct fixed-size variants.
#[derive(Clone, Debug, Error)]
pub enum WindowsAudioError {
    /// A Windows API returned a failing HRESULT.
    #[error("{operation} failed with HRESULT 0x{code:08x}")]
    Hresult {
        /// Stable operation label.
        operation: &'static str,
        /// Raw HRESULT bits.
        code: u32,
    },
    /// A non-realtime `wasapi` helper failed.
    #[error("{operation} failed: {message}")]
    Wasapi {
        /// Stable operation label.
        operation: &'static str,
        /// Helper error text captured outside the realtime path.
        message: String,
    },
    /// A pinned endpoint or current default endpoint is absent.
    #[error("audio endpoint is unavailable: {endpoint_id}")]
    DeviceUnavailable {
        /// Opaque Windows endpoint ID, or a stable default-selection label.
        endpoint_id: String,
    },
    /// A selected endpoint has the wrong capture/render direction.
    #[error("endpoint {endpoint_id} has the wrong data-flow direction")]
    DirectionMismatch {
        /// Opaque Windows endpoint ID.
        endpoint_id: String,
    },
    /// No default endpoint exists for the requested direction and role.
    #[error("no default {direction} endpoint exists for role {role}")]
    NoDefaultEndpoint {
        /// Stable direction label.
        direction: &'static str,
        /// Stable role label.
        role: &'static str,
    },
    /// The active endpoint cannot accept the fixed 48 kHz f32 engine stream.
    #[error("endpoint does not support the requested 48 kHz f32 shared stream")]
    UnsupportedEngineFormat,
    /// The endpoint mix format cannot be opened through the native PCM entry.
    #[error("native PCM mix format was rejected: {reason}")]
    NativeFormatRejected {
        /// Why this mix format stayed closed.
        reason: crate::format::NativeFormatReject,
    },
    /// A fixed 48 kHz stream method was called on a native-format stream.
    #[error("fixed 48 kHz stream method was called on a native PCM stream")]
    NativeStreamLegacyCall,
    /// A native-format stream method was called on a fixed 48 kHz stream.
    #[error("native PCM stream method was called on a fixed 48 kHz stream")]
    LegacyStreamNativeCall,
    /// A native packet contained non-finite samples. Those samples were not copied.
    #[error(
        "native packet at frame {start_sample} ({frames} frames) contained {non_finite_samples} non-finite samples"
    )]
    NativeSamplesNonFinite {
        /// Frames in the refused packet.
        frames: u32,
        /// Non-finite samples found in that packet.
        non_finite_samples: u64,
        /// Source or output frame where the refused packet started.
        start_sample: u64,
    },
    /// The capture client returned a packet whose storage could not be read.
    #[error("native capture packet of {frames} frames was malformed")]
    MalformedNativePacket {
        /// Frames reported for the malformed packet.
        frames: u32,
    },
    /// A packet discontinuity could not be converted to a known source-frame gap.
    #[error("capture discontinuity has no reliable frame duration; start a new epoch")]
    UnknownDiscontinuity,
    /// A device packet exceeded callback storage prepared before stream start.
    #[error("capture packet exceeded the prepared callback capacity")]
    CaptureCapacityExceeded,
    /// A stream method was invoked from a thread other than its owner.
    #[error("WASAPI stream used from a thread other than its creating MTA thread")]
    WrongThread,
    /// An event wait failed.
    #[error("WASAPI event wait failed with Win32 error {code}")]
    WaitFailed {
        /// `GetLastError` value captured after `WAIT_FAILED`.
        code: u32,
    },
    /// The worker thread could not be created.
    #[error("failed to spawn audio worker: {message}")]
    ThreadSpawn {
        /// Operating-system error text.
        message: String,
    },
    /// A worker ended before publishing its startup result.
    #[error("audio worker ended before reporting startup")]
    StartupChannelClosed,
    /// A worker panicked. Realtime code is required to avoid panics.
    #[error("audio worker panicked")]
    ThreadPanicked,
    /// MMCSS `Pro Audio` registration failed.
    #[error("MMCSS Pro Audio registration failed with Win32 error {code}")]
    MmcssFailed {
        /// `GetLastError` after `AvSetMmThreadCharacteristicsW`.
        code: u32,
    },
    /// A platform-neutral audio contract rejected data or capacity.
    #[error(transparent)]
    Core(#[from] AudioError),
}

impl WindowsAudioError {
    /// Returns the stable diagnostics code for this failure class.
    #[must_use]
    pub const fn stable_code(&self) -> &'static str {
        match self {
            Self::Hresult { .. } | Self::WaitFailed { .. } => "wasapi_call_failed",
            Self::Wasapi { .. } => "wasapi_helper_failed",
            Self::DeviceUnavailable { .. } | Self::NoDefaultEndpoint { .. } => "device_unavailable",
            Self::DirectionMismatch { .. } => "device_direction_mismatch",
            Self::UnsupportedEngineFormat => "device_format_unsupported",
            Self::NativeFormatRejected { .. } => "native_format_rejected",
            Self::NativeStreamLegacyCall | Self::LegacyStreamNativeCall => "native_api_mismatch",
            Self::NativeSamplesNonFinite { .. } => "native_samples_non_finite",
            Self::MalformedNativePacket { .. } => "native_packet_malformed",
            Self::UnknownDiscontinuity => "capture_discontinuity_unknown",
            Self::CaptureCapacityExceeded => "capture_capacity_violation",
            Self::WrongThread => "wasapi_thread_violation",
            Self::ThreadSpawn { .. } | Self::StartupChannelClosed | Self::ThreadPanicked => {
                "audio_worker_failed"
            }
            Self::MmcssFailed { .. } => "mmcss_registration_failed",
            Self::Core(_) => "audio_contract_violation",
        }
    }

    /// Maps a stream-processing failure onto the platform-neutral capture error.
    #[must_use]
    pub const fn to_capture_process_error(&self) -> AudioError {
        match self {
            Self::UnknownDiscontinuity => AudioError::UnknownDiscontinuity,
            Self::CaptureCapacityExceeded => AudioError::CaptureCapacityExceeded,
            Self::NativeStreamLegacyCall
            | Self::LegacyStreamNativeCall
            | Self::NativeSamplesNonFinite { .. }
            | Self::MalformedNativePacket { .. } => AudioError::BackendContractViolation,
            Self::Core(error) => *error,
            _ => AudioError::BackendUnavailable,
        }
    }
}

pub(crate) fn hresult(operation: &'static str, error: windows::core::Error) -> WindowsAudioError {
    WindowsAudioError::Hresult {
        operation,
        code: error.code().0 as u32,
    }
}

pub(crate) fn wasapi_error(
    operation: &'static str,
    error: impl std::fmt::Display,
) -> WindowsAudioError {
    WindowsAudioError::Wasapi {
        operation,
        message: error.to_string(),
    }
}
