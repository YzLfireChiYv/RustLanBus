use std::fmt;
use std::io;

use rlb_audio::AudioError;
use rlb_audio_windows::WindowsAudioError;
use rlb_jitter::JitterError;
use rlb_media::PacketizeError;
use rlb_model::{EndpointError, IdGenerationError, IdentityRecordError};
use rlb_net::BindError;
use rlb_session::CoordinatorError;

/// Failures from the persistent engine composition root.
#[derive(Debug)]
pub enum EngineError {
    /// WASAPI adapter or audio-worker failure.
    Audio(WindowsAudioError),
    /// Platform-neutral audio contract.
    Core(AudioError),
    /// Member-session reducer rejected an event.
    Session(CoordinatorError),
    /// Capture unit could not be packetized.
    Packetize(PacketizeError),
    /// Operating-system CSPRNG was unavailable.
    Identity(IdGenerationError),
    /// Fixed source slots were not distinct.
    IdentityRecord(IdentityRecordError),
    /// Encoder or helper thread could not start.
    ThreadSpawn(String),
    /// Encoder thread panicked.
    ThreadPanicked,
    /// Identity file exists and failed validation; no replacement was written.
    IdentityCorrupt,
    /// Member config file exists and failed validation; no replacement was written.
    ConfigCorrupt,
    /// Filesystem persistence failed.
    Persist(&'static str),
    /// Filesystem IO failed.
    Io(io::Error),
    /// JSON encode/decode failed.
    Json(serde_json::Error),
    /// Media UDP socket could not bind.
    Bind(BindError),
    /// A publishable endpoint could not be constructed.
    Endpoint(EndpointError),
    /// Jitter timeline construction failed.
    Jitter(JitterError),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Audio(error) => write!(formatter, "{error}"),
            Self::Core(error) => write!(formatter, "{error}"),
            Self::Session(error) => write!(formatter, "{error}"),
            Self::Packetize(error) => write!(formatter, "{error}"),
            Self::Identity(error) => write!(formatter, "{error}"),
            Self::IdentityRecord(error) => write!(formatter, "{error}"),
            Self::ThreadSpawn(message) => {
                write!(formatter, "failed to spawn engine worker: {message}")
            }
            Self::ThreadPanicked => write!(formatter, "engine worker panicked"),
            Self::IdentityCorrupt => write!(
                formatter,
                "identity.json exists and is unreadable; refusing to mint a replacement identity"
            ),
            Self::ConfigCorrupt => write!(
                formatter,
                "config.json exists and is unreadable; refusing to overwrite it with defaults"
            ),
            Self::Persist(message) => write!(formatter, "{message}"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
            Self::Bind(error) => write!(formatter, "{error}"),
            Self::Endpoint(error) => write!(formatter, "{error}"),
            Self::Jitter(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<WindowsAudioError> for EngineError {
    fn from(error: WindowsAudioError) -> Self {
        Self::Audio(error)
    }
}

impl From<AudioError> for EngineError {
    fn from(error: AudioError) -> Self {
        Self::Core(error)
    }
}

impl From<CoordinatorError> for EngineError {
    fn from(error: CoordinatorError) -> Self {
        Self::Session(error)
    }
}

impl From<PacketizeError> for EngineError {
    fn from(error: PacketizeError) -> Self {
        Self::Packetize(error)
    }
}

impl From<IdGenerationError> for EngineError {
    fn from(error: IdGenerationError) -> Self {
        Self::Identity(error)
    }
}

impl From<IdentityRecordError> for EngineError {
    fn from(error: IdentityRecordError) -> Self {
        Self::IdentityRecord(error)
    }
}

impl From<io::Error> for EngineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for EngineError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<BindError> for EngineError {
    fn from(error: BindError) -> Self {
        Self::Bind(error)
    }
}

impl From<EndpointError> for EngineError {
    fn from(error: EndpointError) -> Self {
        Self::Endpoint(error)
    }
}

impl From<JitterError> for EngineError {
    fn from(error: JitterError) -> Self {
        Self::Jitter(error)
    }
}
