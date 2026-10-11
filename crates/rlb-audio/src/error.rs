use thiserror::Error;

/// Stable failures produced by platform-neutral audio preparation or callbacks.
///
/// Realtime code may construct these values. Formatting them belongs on a
/// non-realtime diagnostics thread.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum AudioError {
    /// A constructor received zero for a required bounded capacity.
    #[error("{field} must be greater than zero")]
    ZeroCapacity {
        /// Stable parameter name.
        field: &'static str,
    },
    /// A frame or channel capacity cannot be represented safely.
    #[error("{field} capacity is too large")]
    CapacityOverflow {
        /// Stable parameter name.
        field: &'static str,
    },
    /// The first-release core only accepts mono or stereo.
    #[error("unsupported channel count {channels}; expected 1 or 2")]
    UnsupportedChannels {
        /// Rejected channel count.
        channels: u8,
    },
    /// The first-release engine rate is fixed at 48 kHz.
    #[error("unsupported sample rate {sample_rate_hz}; expected 48000 Hz")]
    UnsupportedSampleRate {
        /// Rejected sample rate.
        sample_rate_hz: u32,
    },
    /// A planar slice did not cover the declared frames, channels, and stride.
    #[error("planar sample storage is too short: need {required}, got {actual}")]
    PlanarStorageTooShort {
        /// Minimum number of samples required.
        required: usize,
        /// Number of supplied samples.
        actual: usize,
    },
    /// Channel planes would overlap because the stride is too small.
    #[error("planar channel stride {stride} is smaller than {frames} frames")]
    InvalidPlanarStride {
        /// Supplied channel stride.
        stride: usize,
        /// Declared frame count.
        frames: usize,
    },
    /// Capture metadata and its planar view declared different frame counts.
    #[error("capture metadata and sample view frame counts differ")]
    CaptureFrameCountMismatch,
    /// A frame range overflowed its `u64` source-sample domain.
    #[error("capture sample range overflows u64")]
    SampleRangeOverflow,
    /// A discontinuity declared an empty gap.
    #[error("a known capture discontinuity must contain at least one frame")]
    EmptyDiscontinuity,
    /// A block was prepared for a different source than its reblocker.
    #[error("capture source does not match the prepared reblocker")]
    SourceMismatch,
    /// A block was prepared for a different epoch than its reblocker.
    #[error("capture epoch does not match the prepared reblocker")]
    EpochMismatch,
    /// Input source time moved backwards or overlapped a prior callback.
    #[error("capture sample ranges overlap or move backwards")]
    NonMonotonicCapture,
    /// A known discontinuity did not agree with the observed sample gap.
    #[error("declared capture discontinuity does not match the sample timeline")]
    DiscontinuityMismatch,
    /// Capture metadata exceeds the destination's prepared storage.
    #[error("capture block exceeds its prepared frame or channel capacity")]
    CaptureCapacityExceeded,
    /// A block returned to a pool was prepared by an incompatible pool.
    #[error("capture block does not belong to this pool shape")]
    ForeignCaptureBlock,
    /// More blocks were returned than the pool owns.
    #[error("capture pool already contains all prepared blocks")]
    CapturePoolFull,
    /// An endpoint descriptor omitted its stable platform identifier.
    #[error("endpoint identifier must not be empty")]
    EmptyEndpointId,
    /// An endpoint descriptor omitted its user-facing label.
    #[error("endpoint friendly name must not be empty")]
    EmptyEndpointName,
    /// An endpoint format range was internally inconsistent.
    #[error("endpoint format range is invalid")]
    InvalidEndpointFormatRange,
    /// A callback backend has no further capture frames.
    #[error("capture backend reached the end of its prepared input")]
    CaptureEnded,
    /// A backend cannot process audio in its current state.
    #[error("audio backend is unavailable")]
    BackendUnavailable,
    /// The backend produced metadata outside the supplied process buffer.
    #[error("capture backend exceeded the supplied process buffer")]
    BackendContractViolation,
    /// A render request exceeds the mixer's prepared callback size.
    #[error("render request exceeds prepared frame capacity")]
    RenderCapacityExceeded,
    /// Interleaved render storage did not match `frames * 2`.
    #[error("render storage length is invalid: need {required}, got {actual}")]
    RenderStorageLength {
        /// Number of stereo samples required.
        required: usize,
        /// Number of supplied samples.
        actual: usize,
    },
    /// A reader was registered twice for one stable source identity.
    #[error("source is already registered in the render mixer")]
    DuplicateSource,
    /// Every preallocated render source slot is in use.
    #[error("render mixer has no free source slot")]
    SourceCapacityExceeded,
    /// A control update referred to a source absent from the prepared graph.
    #[error("render source was not found")]
    SourceNotFound,
    /// A control update referred to a member absent from the prepared graph.
    #[error("render member was not found")]
    MemberNotFound,
    /// A limiter knee was NaN, infinite, or outside its useful range.
    #[error("soft limiter knee must be finite and in 0.0..1.0")]
    InvalidLimiterKnee,
    /// A capture discontinuity did not include a measurable sample gap.
    #[error("capture discontinuity has no reliable frame duration")]
    UnknownDiscontinuity,
}
