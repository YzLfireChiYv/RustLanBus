//! Explicit render-core contract.
//!
//! `SourceAdmission` carries a full `SourceEntry`, source fence and stream grant.
//! `AdmissionSnapshot` adds the peer instance, session and run. Local monitor
//! blocks use a separate permission and do not consult that snapshot.

use rlb_model::{DirectSessionId, MemberId, RunId, SourceId, StreamEpoch};
use rlb_peer::{
    AdmissionSnapshot, AdmittedPcm, ChannelLayout, PcmFormat, PeerInstance, PublicationEpoch,
    PublicationState, SourceKind,
};

/// Runtime ceiling: 64 remote catalogues plus two local originals. The wire's
/// 25-entry bound applies to each peer, not the complete render registry.
pub const ABSOLUTE_MAX_SOURCES: usize = 64 * rlb_peer::MAX_SOURCES + 2;
pub const ABSOLUTE_MAX_CHANNELS: usize = rlb_model::pcm::MAX_CHANNELS;
pub const ABSOLUTE_MAX_BLOCK_FRAMES: usize = rlb_peer::MAX_ORIGINAL_FRAMES;
pub const ABSOLUTE_MAX_WINDOW_FRAMES: usize = 16_384;
pub const ABSOLUTE_MAX_QUANTUM_FRAMES: usize = 1_024;
pub const SINC_TAPS: usize = 256;
pub const SINC_PHASES: usize = 128;

/// Provisional reserve controller and shortage policy.
///
/// The numbers are software starting values. They are not latency targets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimingPolicy {
    pub brief_missing_frames: u32,
    pub delivery_guard_frames: u64,
    /// Reserve above the steady target that starts discard and re-prime.
    pub excess_discard_frames: u64,
    pub filter_seconds: f64,
    pub kp_per_second: f64,
    pub ki_per_second_squared: f64,
    pub correction_ppm: f64,
    /// Constructor bound for the output/input ratio, relative to nominal.
    pub max_ratio_relative: f64,
}

impl TimingPolicy {
    #[must_use]
    pub const fn provisional() -> Self {
        Self {
            brief_missing_frames: 8,
            delivery_guard_frames: 480,
            excess_discard_frames: 4_096,
            filter_seconds: 0.5,
            kp_per_second: 0.2,
            ki_per_second_squared: 0.01,
            correction_ppm: 1_000.0,
            max_ratio_relative: 1.002,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderCapacities {
    pub sources: usize,
    pub window_frames: usize,
    pub max_block_frames: usize,
    pub max_quantum_frames: usize,
    pub output_channels: u16,
    pub output_layout: ChannelLayout,
    pub output_rate_hz: u32,
    pub output_generation: u64,
    pub quantum_frames: u32,
}

impl RenderCapacities {
    pub fn validate(self, policy: &TimingPolicy) -> Result<(), RenderError> {
        if self.sources == 0
            || self.sources > ABSOLUTE_MAX_SOURCES
            || self.window_frames == 0
            || self.window_frames > ABSOLUTE_MAX_WINDOW_FRAMES
            || self.max_block_frames == 0
            || self.max_block_frames > ABSOLUTE_MAX_BLOCK_FRAMES
            || self.max_quantum_frames == 0
            || self.max_quantum_frames > ABSOLUTE_MAX_QUANTUM_FRAMES
            || self.output_channels == 0
            || usize::from(self.output_channels) > ABSOLUTE_MAX_CHANNELS
            || self.output_channels != self.output_layout.channels()
            || !(8_000..=384_000).contains(&self.output_rate_hz)
            || self.quantum_frames == 0
            || self.quantum_frames as usize > self.max_quantum_frames
            || self.output_generation == 0
        {
            return Err(RenderError::InvalidConfig(
                "render capacities are outside the prepared bounds",
            ));
        }
        if !policy.filter_seconds.is_finite()
            || policy.filter_seconds <= 0.0
            || !policy.kp_per_second.is_finite()
            || policy.kp_per_second < 0.0
            || !policy.ki_per_second_squared.is_finite()
            || policy.ki_per_second_squared < 0.0
            || !policy.correction_ppm.is_finite()
            || policy.correction_ppm < 0.0
            || !policy.max_ratio_relative.is_finite()
            || policy.max_ratio_relative < 1.0
            || policy.delivery_guard_frames >= self.window_frames as u64
            || policy.excess_discard_frames > i64::MAX as u64
        {
            return Err(RenderError::InvalidConfig(
                "timing policy contains a non-finite or inverted coefficient",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineId {
    Remote {
        member: MemberId,
        run: RunId,
        source: SourceId,
        epoch: PublicationEpoch,
    },
    Local {
        run: RunId,
        source: SourceId,
        acquisition: StreamEpoch,
    },
}

impl TimelineId {
    pub const fn publication(reference: rlb_peer::PublicationRef) -> Self {
        Self::Remote {
            member: reference.owner.member,
            run: reference.owner.run,
            source: reference.source,
            epoch: reference.epoch,
        }
    }

    #[must_use]
    pub fn same_logical_source(self, other: Self) -> bool {
        match (self, other) {
            (
                Self::Remote { member, source, .. },
                Self::Remote {
                    member: other_member,
                    source: other_source,
                    ..
                },
            ) => member == other_member && source == other_source,
            (
                Self::Local { source, .. },
                Self::Local {
                    source: other_source,
                    ..
                },
            ) => source == other_source,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Permission {
    Remote {
        peer_instance: PeerInstance,
        session: DirectSessionId,
        fence: u64,
    },
    Local {
        monitor_revision: u64,
        capture_instance: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceActivity {
    Active,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderSource {
    pub timeline: TimelineId,
    pub permission: Permission,
    pub activity: SourceActivity,
    pub format: PcmFormat,
    /// Retained for the owner. Channel count comes from `format`.
    pub kind: Option<SourceKind>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderAdmission {
    /// Monotonic aggregate revision. Sources contain the complete current
    /// union of all peers and local monitors, rather than one peer's delta.
    pub revision: u64,
    pub sources: Vec<RenderSource>,
}

#[derive(Clone, Copy, Debug)]
pub struct SourceBlock<'a> {
    pub timeline: TimelineId,
    pub permission: Permission,
    pub format: PcmFormat,
    pub start_frame: u64,
    /// Original receive observation. The upstream owner must expire queued
    /// blocks before insertion; this core does not claim per-span age expiry.
    pub arrived_at_ms: u64,
    pub samples: &'a [f64],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgressObservation {
    pub timeline: TimelineId,
    pub permission: Permission,
    pub format: PcmFormat,
    pub end_frame: u64,
    pub discard_through: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PushReport {
    pub stored_frames: u64,
    pub duplicate_frames: u64,
    pub stale_frames: u64,
    pub ahead_frames: u64,
    pub high_water: u64,
}

/// Output setup supplied by the future endpoint owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputSetup {
    pub generation: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub layout: ChannelLayout,
    pub quantum_frames: u32,
}

/// One selected service observation. `padding_frames == 0` is not an underrun.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceObservation {
    pub generation: u64,
    pub selected: bool,
    pub quantum_frames: u32,
    /// Endpoint reserve target P, independent of this call's bounded chunk Q.
    pub target_padding_frames: u32,
    pub writable_frames: u32,
    pub padding_frames: u32,
    pub missed_service: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantumHandle {
    pub id: u64,
    pub generation: u64,
    pub frames: u32,
    pub channels: u16,
    pub peak_abs: f64,
    pub over_unity: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrepareOutcome {
    Waiting { generation: u64 },
    Prepared(QuantumHandle),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmissionReceipt {
    pub generation: u64,
    pub quantum_id: u64,
    pub success: bool,
    pub start_frame: u64,
    pub end_frame: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionReport {
    /// Authoritative exclusive native frame end in the current generation.
    pub submitted_output_frames: u64,
    pub accepted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceGain {
    pub timeline: TimelineId,
    pub gain: f64,
}

/// Explicit source-channel to output-channel pairs. Unqualified routes are rejected
/// for that source and produce silence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelRoute {
    pub timeline: TimelineId,
    pub output_for_source: [Option<u8>; ABSOLUTE_MAX_CHANNELS],
}

/// Explicit source-channel to output-channel gains.
///
/// Rows select output channels and columns select source channels. Entries
/// outside the current source/output shape must be zero. This matrix takes
/// precedence over a legacy channel route and the equal-channel identity map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelMatrix {
    pub timeline: TimelineId,
    pub source_layout: ChannelLayout,
    pub output_layout: ChannelLayout,
    pub gains: [[f64; ABSOLUTE_MAX_CHANNELS]; ABSOLUTE_MAX_CHANNELS],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Priming,
    Running,
    AwaitCurrent,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceStatus {
    pub closed_before: u64,
    pub high_water: u64,
    pub phase: Phase,
    pub ratio: f64,
    pub cutoff: Option<f32>,
    pub active: bool,
    pub delivery_guard_frames: u64,
    /// Converted frames accepted by the current native output generation.
    pub submitted_frames: u64,
    pub segment_restarts: u64,
    pub last_error: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub enum RenderError {
    InvalidConfig(&'static str),
    MalformedRange,
    NonFinite,
    NumericalOverflow,
    Capacity(&'static str),
    StaleCompletion,
    CounterExhausted,
    OutputUnavailable,
    UnsupportedLayout {
        source: ChannelLayout,
        output: ChannelLayout,
    },
    MissingDescriptor(SourceId),
    IncoherentAdmission(&'static str),
    Resampler(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(message)
            | Self::Capacity(message)
            | Self::IncoherentAdmission(message) => formatter.write_str(message),
            Self::MalformedRange => {
                formatter.write_str("the source frame range is empty or overflows")
            }
            Self::NonFinite => formatter.write_str("original PCM contains a non-finite sample"),
            Self::NumericalOverflow => {
                formatter.write_str("the mix is non-finite or does not fit in the endpoint sample")
            }
            Self::StaleCompletion => {
                formatter.write_str("the submission receipt does not match the pending quantum")
            }
            Self::CounterExhausted => formatter.write_str("the render quantum counter exhausted"),
            Self::OutputUnavailable => formatter
                .write_str("a fresh output generation is required after submission failure"),
            Self::UnsupportedLayout { source, output } => write!(
                formatter,
                "no documented channel matrix from mask {:#x} to {:#x}",
                source.channel_mask(),
                output.channel_mask()
            ),
            Self::MissingDescriptor(source) => {
                write!(
                    formatter,
                    "admission for {source} has no matching source entry"
                )
            }
            Self::Resampler(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RenderError {}

/// Build remote render sources from one coherent admission snapshot.
///
/// Active, receive-ready sources use the descriptor on `SourceAdmission::entry`.
/// A peer that is disconnected, not receive-ready, quiesced, or missing its
/// instance, run, or session yields an empty list. Omitted sources close at the
/// next boundary. Local sources are composed by the caller.
pub fn remote_render_sources(
    member: MemberId,
    snapshot: &AdmissionSnapshot,
) -> Result<Vec<RenderSource>, RenderError> {
    if !snapshot.connected || !snapshot.receive_ready || snapshot.quiesced {
        return Ok(Vec::new());
    }
    let (Some(peer_instance), Some(session), Some(remote_run)) = (
        snapshot.peer_instance,
        snapshot.session,
        snapshot.remote_run,
    ) else {
        return Ok(Vec::new());
    };
    if snapshot.sources.len() > rlb_peer::MAX_SOURCES {
        return Err(RenderError::Capacity(
            "a catalogue snapshot contains more than 25 sources",
        ));
    }
    let mut built = Vec::with_capacity(snapshot.sources.len());
    for admission in &snapshot.sources {
        let entry = admission.entry;
        if !admission.receive_ready || entry.state != PublicationState::Active {
            continue;
        }
        entry.validate().map_err(|_| {
            RenderError::IncoherentAdmission("an active admission entry is internally inconsistent")
        })?;
        let Some(format) = entry.format else {
            return Err(RenderError::MissingDescriptor(entry.id));
        };
        built.push(RenderSource {
            timeline: TimelineId::publication(rlb_peer::PublicationRef {
                owner: rlb_peer::MemberRun {
                    member,
                    run: remote_run,
                },
                source: entry.id,
                epoch: entry.epoch,
            }),
            permission: Permission::Remote {
                peer_instance,
                session,
                fence: admission.fence,
            },
            activity: SourceActivity::Active,
            format,
            kind: Some(entry.kind),
        });
    }
    Ok(built)
}

/// Borrow admitted PCM without copying it into a second queue.
pub fn block_from_admitted_pcm(pcm: AdmittedPcm<'_>) -> Result<SourceBlock<'_>, RenderError> {
    let metadata = pcm.metadata;
    let Some(peer_instance) = metadata.peer_instance else {
        return Err(RenderError::IncoherentAdmission(
            "admitted PCM has no peer instance",
        ));
    };
    let Some(format) = metadata.entry.format else {
        return Err(RenderError::MissingDescriptor(metadata.entry.id));
    };
    Ok(SourceBlock {
        timeline: TimelineId::publication(rlb_peer::PublicationRef {
            owner: rlb_peer::MemberRun {
                member: metadata.member_id,
                run: metadata.remote_run,
            },
            source: metadata.entry.id,
            epoch: metadata.entry.epoch,
        }),
        permission: Permission::Remote {
            peer_instance,
            session: metadata.session,
            fence: metadata.source_fence,
        },
        format,
        start_frame: metadata.start_frame,
        arrived_at_ms: metadata.arrived_at_ms,
        samples: pcm.samples,
    })
}

#[must_use]
pub fn due_frames(service: &ServiceObservation, prepared_quantum: u32) -> u32 {
    let quantum = service.quantum_frames.min(prepared_quantum);
    let room = service
        .target_padding_frames
        .saturating_sub(service.padding_frames);
    quantum.min(service.writable_frames).min(room)
}

pub(crate) fn frame_end(start: u64, frames: u64) -> Result<u64, RenderError> {
    start
        .checked_add(frames)
        .filter(|end| *end >= start)
        .ok_or(RenderError::MalformedRange)
}

pub(crate) fn block_frames(format: PcmFormat, samples: &[f64]) -> Result<u64, RenderError> {
    let channels = usize::from(format.channels());
    if channels == 0 || !samples.len().is_multiple_of(channels) {
        return Err(RenderError::MalformedRange);
    }
    let frames = samples.len() / channels;
    if frames == 0 {
        return Err(RenderError::MalformedRange);
    }
    u64::try_from(frames).map_err(|_| RenderError::MalformedRange)
}
