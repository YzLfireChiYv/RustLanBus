//! Local observations of actual audio returned through the current virtual peer.
//! This bounded metadata ledger holds no PCM and never changes audio delivery.

use std::collections::VecDeque;
use std::time::Instant;

use rlb_audio_windows::NativeCapturePacket;
use rlb_contact::{ReturnMapping, ReturnState};
use rlb_model::{MemberId, RunId, SourceId};
use rlb_peer::{AdmittedMetadata, PreparedFragment};

use crate::participant::snapshot::ReturnLoopState;

const RECENT_FRAGMENTS: usize = 512;

struct SentFragment {
    source: SourceId,
    epoch: u64,
    first: u64,
    end: u64,
    rate: u32,
    sent: Instant,
    first_qpc: Option<u64>,
}

pub(super) struct ReturnTiming {
    member: Option<(MemberId, RunId)>,
    mappings: Vec<ReturnMapping>,
    sent: VecDeque<SentFragment>,
    result: Option<ReturnLoopState>,
}

impl Default for ReturnTiming {
    fn default() -> Self {
        Self {
            member: None,
            mappings: Vec::new(),
            sent: VecDeque::with_capacity(RECENT_FRAGMENTS),
            result: None,
        }
    }
}

impl ReturnTiming {
    pub(super) fn configure(&mut self, state: Option<ReturnState>) {
        let member = state
            .as_ref()
            .and_then(|state| state.member.as_ref())
            .map(|member| (member.member_id, member.run_id));
        if self.member != member {
            self.sent.clear();
            self.result = member.map(|(member_id, run_id)| ReturnLoopState {
                member_id,
                run_id,
                returned_packets: 0,
                packet_rtt_ms: None,
                minimum_packet_rtt_ms: None,
                capture_to_receive_ms: None,
                minimum_capture_to_receive_ms: None,
            });
        }
        self.member = member;
        self.mappings = state.map(|state| state.mappings).unwrap_or_default();
    }

    pub(super) fn snapshot(&self) -> Option<ReturnLoopState> {
        self.result.clone()
    }

    pub(super) fn tracks(&self, member: MemberId) -> bool {
        self.member.is_some_and(|current| current.0 == member)
    }

    pub(super) fn sent(
        &mut self,
        member: MemberId,
        fragment: &PreparedFragment<'_>,
        capture: NativeCapturePacket,
        packet_frame_offset: Option<u64>,
        sent: Instant,
    ) {
        if self.member.map(|member| member.0) != Some(member) {
            return;
        }
        let first = fragment.start_frame();
        let Some(end) = first.checked_add(u64::from(fragment.frames())) else {
            return;
        };
        let rate = fragment.format().sample_rate_hz();
        let first_qpc = (capture.clock_domain == rlb_audio_windows::wasapi_clock_domain())
            .then_some(capture.timing.first_frame)
            .flatten()
            .and_then(|anchor| {
                anchor
                    .at
                    .ticks()
                    .checked_add(frame_ticks(packet_frame_offset?, rate)?)
            });
        if self.sent.len() == RECENT_FRAGMENTS {
            self.sent.pop_front();
        }
        self.sent.push_back(SentFragment {
            source: fragment.source(),
            epoch: fragment.epoch().get(),
            first,
            end,
            rate,
            sent,
            first_qpc,
        });
    }

    pub(super) fn received(
        &mut self,
        local_member: MemberId,
        local_run: RunId,
        metadata: AdmittedMetadata,
        received: Instant,
        received_qpc: Option<u64>,
    ) {
        if self.member != Some((metadata.member_id, metadata.remote_run)) {
            return;
        }
        let Some(mapping) = self.mappings.iter().find(|mapping| {
            mapping.source_id == metadata.entry.id
                && mapping.epoch == metadata.entry.epoch.get()
                && mapping.upstream_member_id == local_member
                && mapping.upstream_run_id == local_run
        }) else {
            return;
        };
        let Some(first) = metadata
            .start_frame
            .checked_sub(mapping.output_first_frame)
            .and_then(|offset| mapping.input_first_frame.checked_add(offset))
        else {
            return;
        };
        let Some(end) = first.checked_add(u64::from(metadata.frames)) else {
            return;
        };
        let Some(sent) = self.sent.iter().rev().find(|sent| {
            sent.source == mapping.upstream_source_id
                && sent.epoch == mapping.upstream_epoch
                && sent.first <= first
                && sent.end >= end
        }) else {
            return;
        };
        let Some(elapsed) = received.checked_duration_since(sent.sent) else {
            return;
        };
        let rtt = elapsed.as_secs_f64() * 1000.;
        let capture_age = sent.first_qpc.and_then(|anchor| {
            let first_qpc = anchor.checked_add(frame_ticks(first - sent.first, sent.rate)?)?;
            Some(received_qpc?.checked_sub(first_qpc)? as f64 / 10_000.)
        });
        let Some(result) = &mut self.result else {
            return;
        };
        result.returned_packets = result.returned_packets.saturating_add(1);
        result.packet_rtt_ms = Some(rtt);
        result.minimum_packet_rtt_ms =
            Some(result.minimum_packet_rtt_ms.map_or(rtt, |old| old.min(rtt)));
        result.capture_to_receive_ms = capture_age;
        if let Some(age) = capture_age {
            result.minimum_capture_to_receive_ms = Some(
                result
                    .minimum_capture_to_receive_ms
                    .map_or(age, |old| old.min(age)),
            );
        }
    }
}

fn frame_ticks(frames: u64, rate: u32) -> Option<u64> {
    u64::try_from(u128::from(frames).checked_mul(10_000_000)? / u128::from(rate)).ok()
}
