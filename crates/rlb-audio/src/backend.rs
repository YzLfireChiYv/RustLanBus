use crate::{AudioError, CaptureMetadata, PlanarSamples};

/// Lifecycle state reported by an already-constructed platform backend.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BackendStatus {
    /// No endpoint resources are held.
    Closed,
    /// Endpoint resources are prepared and callbacks may start.
    Ready,
    /// Realtime callbacks are active.
    Running,
    /// A normal stop retained the endpoint configuration.
    Stopped,
    /// The selected endpoint is currently unavailable.
    Unavailable,
    /// The stream requires supervisor-side reconstruction.
    Faulted,
}

/// Device-flow role of a stable endpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EndpointDirection {
    /// Hardware or virtual capture input.
    Capture,
    /// Hardware or virtual playback output.
    Render,
    /// Capture view of a render endpoint.
    LoopbackCapture,
}

/// Current enumerated operating state of an endpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EndpointState {
    /// Endpoint is available for negotiation.
    Active,
    /// Endpoint is present and disabled.
    Disabled,
    /// Endpoint was unplugged or otherwise became unavailable.
    Unplugged,
    /// A pinned stable identity is currently absent.
    NotPresent,
}

/// OS default roles held by an endpoint at enumeration time.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct DefaultEndpointRoles {
    /// Default general-purpose multimedia endpoint.
    pub multimedia: bool,
    /// Default console endpoint.
    pub console: bool,
    /// Default communications endpoint.
    pub communications: bool,
}

/// Inclusive format bounds advertised during non-realtime enumeration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EndpointFormatRange {
    /// Lowest supported sample rate.
    pub minimum_sample_rate_hz: u32,
    /// Highest supported sample rate.
    pub maximum_sample_rate_hz: u32,
    /// Lowest supported channel count.
    pub minimum_channels: u8,
    /// Highest supported channel count.
    pub maximum_channels: u8,
    /// Whether the adapter can deliver or accept planar engine f32.
    pub supports_engine_f32: bool,
}

impl EndpointFormatRange {
    /// Validates monotonically ordered, nonzero bounds.
    pub fn validate(self) -> Result<(), AudioError> {
        if self.minimum_sample_rate_hz == 0
            || self.minimum_sample_rate_hz > self.maximum_sample_rate_hz
            || self.minimum_channels == 0
            || self.minimum_channels > self.maximum_channels
        {
            return Err(AudioError::InvalidEndpointFormatRange);
        }
        Ok(())
    }
}

/// Stable platform endpoint identity and its current enumeration projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointDescriptor {
    endpoint_id: String,
    friendly_name: String,
    direction: EndpointDirection,
    default_roles: DefaultEndpointRoles,
    state: EndpointState,
    format_range: EndpointFormatRange,
}

impl EndpointDescriptor {
    /// Constructs a descriptor on a non-realtime supervisor thread.
    pub fn new(
        endpoint_id: impl Into<String>,
        friendly_name: impl Into<String>,
        direction: EndpointDirection,
        default_roles: DefaultEndpointRoles,
        state: EndpointState,
        format_range: EndpointFormatRange,
    ) -> Result<Self, AudioError> {
        let endpoint_id = endpoint_id.into();
        let friendly_name = friendly_name.into();
        if endpoint_id.is_empty() {
            return Err(AudioError::EmptyEndpointId);
        }
        if friendly_name.is_empty() {
            return Err(AudioError::EmptyEndpointName);
        }
        format_range.validate()?;
        Ok(Self {
            endpoint_id,
            friendly_name,
            direction,
            default_roles,
            state,
            format_range,
        })
    }

    /// Returns the opaque stable platform endpoint ID used for binding.
    #[must_use]
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// Returns the user-visible label, which carries no binding identity.
    #[must_use]
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Returns the endpoint flow role.
    #[must_use]
    pub const fn direction(&self) -> EndpointDirection {
        self.direction
    }

    /// Returns default-role flags captured during enumeration.
    #[must_use]
    pub const fn default_roles(&self) -> DefaultEndpointRoles {
        self.default_roles
    }

    /// Returns current enumerated endpoint state.
    #[must_use]
    pub const fn state(&self) -> EndpointState {
        self.state
    }

    /// Returns the advertised format bounds.
    #[must_use]
    pub const fn format_range(&self) -> EndpointFormatRange {
        self.format_range
    }
}

/// Preallocated planar destination supplied to a capture backend callback.
#[derive(Debug)]
pub struct CaptureProcessBuffer<'a> {
    samples: &'a mut [f32],
    frame_capacity: usize,
    channel_capacity: u8,
}

impl<'a> CaptureProcessBuffer<'a> {
    /// Creates a mutable planar view over caller-owned fixed storage.
    pub fn new(
        samples: &'a mut [f32],
        frame_capacity: usize,
        channel_capacity: u8,
    ) -> Result<Self, AudioError> {
        if frame_capacity == 0 {
            return Err(AudioError::ZeroCapacity {
                field: "capture_process_frames",
            });
        }
        if !matches!(channel_capacity, 1 | 2) {
            return Err(AudioError::UnsupportedChannels {
                channels: channel_capacity,
            });
        }
        let required = frame_capacity
            .checked_mul(usize::from(channel_capacity))
            .ok_or(AudioError::CapacityOverflow {
                field: "capture_process_samples",
            })?;
        if samples.len() < required {
            return Err(AudioError::PlanarStorageTooShort {
                required,
                actual: samples.len(),
            });
        }
        Ok(Self {
            samples,
            frame_capacity,
            channel_capacity,
        })
    }

    /// Returns one complete writable channel plane.
    pub fn channel_mut(&mut self, channel: u8) -> Option<&mut [f32]> {
        if channel >= self.channel_capacity {
            return None;
        }
        let start = usize::from(channel) * self.frame_capacity;
        self.samples.get_mut(start..start + self.frame_capacity)
    }

    /// Returns a read-only view over the first `frames` in each channel.
    pub fn captured(&self, frames: usize, channels: u8) -> Result<PlanarSamples<'_>, AudioError> {
        if frames > self.frame_capacity || channels > self.channel_capacity {
            return Err(AudioError::CaptureCapacityExceeded);
        }
        PlanarSamples::new(self.samples, frames, channels, self.frame_capacity)
    }

    /// Returns fixed frame capacity per channel.
    #[must_use]
    pub const fn frame_capacity(&self) -> usize {
        self.frame_capacity
    }

    /// Returns fixed channel-plane capacity.
    #[must_use]
    pub const fn channel_capacity(&self) -> u8 {
        self.channel_capacity
    }
}

/// Outcome of one non-waiting capture process call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureProcessResult {
    /// No device frames were available at this callback boundary.
    Idle,
    /// Planar storage was filled for the declared metadata range.
    Captured(CaptureMetadata),
    /// A finite fake stream or a normally stopped source has ended.
    EndOfStream,
}

/// Already-opened platform capture stream used by its realtime owner.
///
/// Enumeration, device opening, start, stop, and close remain supervisor-side
/// operations. `process` only copies or converts an available callback buffer
/// into supplied preallocated storage.
pub trait CaptureBackend: Send {
    /// Returns the latest atomically published lifecycle state.
    fn status(&self) -> BackendStatus;

    /// Returns the stable selected endpoint descriptor.
    fn endpoint(&self) -> &EndpointDescriptor;

    /// Processes one callback without allocation, waiting, logging, or I/O
    /// beyond the already-open audio endpoint.
    fn process(
        &mut self,
        destination: &mut CaptureProcessBuffer<'_>,
    ) -> Result<CaptureProcessResult, AudioError>;
}

/// Already-opened platform render stream used by its realtime owner.
pub trait RenderBackend: Send {
    /// Returns the latest atomically published lifecycle state.
    fn status(&self) -> BackendStatus;

    /// Returns the stable selected endpoint descriptor.
    fn endpoint(&self) -> &EndpointDescriptor;

    /// Submits one complete interleaved stereo callback buffer.
    ///
    /// The slice length must equal `frames * 2`. Implementations use only
    /// pre-opened endpoint resources and must not allocate, wait, or log.
    fn render(&mut self, interleaved_stereo: &[f32], frames: usize) -> Result<(), AudioError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format_range() -> EndpointFormatRange {
        EndpointFormatRange {
            minimum_sample_rate_hz: 44_100,
            maximum_sample_rate_hz: 96_000,
            minimum_channels: 1,
            maximum_channels: 2,
            supports_engine_f32: true,
        }
    }

    #[test]
    fn endpoint_binding_uses_nonempty_stable_id() {
        let descriptor = EndpointDescriptor::new(
            "platform-id",
            "Friendly name",
            EndpointDirection::Render,
            DefaultEndpointRoles {
                multimedia: true,
                ..DefaultEndpointRoles::default()
            },
            EndpointState::Active,
            format_range(),
        )
        .expect("descriptor");
        assert_eq!(descriptor.endpoint_id(), "platform-id");
        assert_eq!(descriptor.friendly_name(), "Friendly name");
        assert!(descriptor.default_roles().multimedia);
        assert_eq!(
            EndpointDescriptor::new(
                "",
                "Friendly name",
                EndpointDirection::Render,
                DefaultEndpointRoles::default(),
                EndpointState::Active,
                format_range()
            ),
            Err(AudioError::EmptyEndpointId)
        );
    }
}
