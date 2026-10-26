#![forbid(unsafe_code)]

//! Platform-neutral capture, reblocking, shared-render, and mix-graph primitives.
//!
//! All storage used by [`CaptureReblocker::process`], [`SharedRenderMixer::render`],
//! and [`MixGraphConsumer::try_apply_all`] is prepared by constructors. Those
//! realtime entry points perform bounded work and contain no device, logging, or
//! blocking operations. Mix-graph commands are published from a coordinator
//! thread through [`mix_graph_channel`].

mod backend;
mod capture;
mod error;
mod fake;
mod graph;
mod metrics;
mod mixer;
mod playout;

pub use backend::{
    BackendStatus, CaptureBackend, CaptureProcessBuffer, CaptureProcessResult,
    DefaultEndpointRoles, EndpointDescriptor, EndpointDirection, EndpointFormatRange,
    EndpointState, RenderBackend,
};
pub use capture::{
    BoundedSpsc, CaptureBlock, CaptureBlockConsumer, CaptureBlockPool, CaptureBlockProducer,
    CaptureClockDomain, CaptureDiscontinuity, CaptureInput, CaptureMetadata, CaptureReblocker,
    CaptureTiming, FrameTimeQuality, FrameTimestamp, PlanarSamples, ReblockReport, TryPushError,
};
pub use error::AudioError;
pub use fake::{ConstantSourceReader, FakeCapture, FakeRender};
pub use graph::{
    MIX_GRAPH_COMMAND_CAPACITY, MIX_GRAPH_RETIRE_CAPACITY, MixGraphCommand, MixGraphConsumer,
    MixGraphPublishError, MixGraphPublisher, MixGraphReclaimer, mix_graph_channel,
    mix_graph_channel_with_capacity,
};
pub use metrics::{AudioMetrics, AudioMetricsSnapshot};
pub use mixer::{
    DEFAULT_LIMITER_KNEE, LIMITER_ALGORITHM_ID, RAMP_FRAMES, RenderReport, SharedRenderMixer,
    SoftLimiter, SourceReadOutcome, SourceReader, SourceRenderConfig, SourceRenderMetricsSnapshot,
};
pub use playout::{
    DirectPcmPlayout, DirectPlayoutConfig, PcmPacket, PlayoutError, PlayoutPushReport,
    PlayoutRenderReport, PlayoutStats,
};
