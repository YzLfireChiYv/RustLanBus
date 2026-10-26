//! Persistent render core.
//!
//! The core turns admitted original PCM into one prepared output quantum.
//! It does not open an endpoint, own a socket, or call WASAPI. The media
//! owner supplies admission and PCM. The output owner supplies service
//! observations and the successful submission receipt.
//!
//! `RenderAdmission` is the seam. [`remote_render_sources`] reads a coherent
//! `AdmissionSnapshot`, whose source records already carry their descriptors.
//! [`block_from_admitted_pcm`] borrows caller-owned PCM for insertion.

mod contract;
mod control;
mod convert;
mod core;
mod ledger;
mod matrix;
mod mix;
mod window;

pub use contract::{
    ABSOLUTE_MAX_BLOCK_FRAMES, ABSOLUTE_MAX_CHANNELS, ABSOLUTE_MAX_QUANTUM_FRAMES,
    ABSOLUTE_MAX_SOURCES, ABSOLUTE_MAX_WINDOW_FRAMES, ChannelMatrix, ChannelRoute,
    CompletionReport, OutputSetup, Permission, Phase, PrepareOutcome, ProgressObservation,
    PushReport, QuantumHandle, RenderAdmission, RenderCapacities, RenderError, RenderSource,
    SINC_PHASES, SINC_TAPS, ServiceObservation, SourceActivity, SourceBlock, SourceGain,
    SourceStatus, SubmissionReceipt, TimelineId, TimingPolicy, block_from_admitted_pcm,
    remote_render_sources,
};
pub use core::{Preset, RenderCore};
pub use matrix::default_channel_matrix;

#[cfg(test)]
mod tests;
