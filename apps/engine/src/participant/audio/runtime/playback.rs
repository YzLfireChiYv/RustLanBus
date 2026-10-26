//! Media-to-output bridge. DSP and native submissions belong to rlb-output.
//! Current admission travels independently of the bounded PCM queue.

use rlb_model::MemberId;
use std::time::Instant;
use rlb_peer::{AdmissionProjection, AdmissionSnapshot, AdmittedMetadata, Peer};

use super::output::{OutputHandle, OutputIngress, OutputRuntime, OutputStatus};
use crate::participant::audio::render;
#[cfg(test)]
use crate::participant::audio::render::TimelineId;
use crate::participant::config::ParticipantConfig;

const MAX_PEERS: usize = 64;
const SOURCE_SLOTS: usize = render::ABSOLUTE_MAX_SOURCES;

struct CachedPeer {
    member: MemberId,
    projection: AdmissionProjection,
    snapshot: AdmissionSnapshot,
    peaks: [Option<(rlb_model::SourceId, f64, Instant)>; rlb_peer::MAX_SOURCES],
}

pub struct PlaybackOwner {
    output: OutputRuntime,
    peers: Vec<CachedPeer>,
    ingresses: Vec<OutputIngress>,
    joined: bool,
    #[cfg(test)]
    admitted: Vec<TimelineId>,
    last_error: Option<String>,
}

impl PlaybackOwner {
    pub fn new(config: ParticipantConfig) -> Result<Self, String> {
        Ok(Self {
            output: OutputRuntime::start(config)?,
            peers: Vec::with_capacity(MAX_PEERS),
            ingresses: Vec::with_capacity(SOURCE_SLOTS),
            joined: false,
            #[cfg(test)]
            admitted: Vec::with_capacity(SOURCE_SLOTS),
            last_error: None,
        })
    }

    pub(crate) fn configure(&self, config: &ParticipantConfig) -> Result<(), String> {
        self.output.handle.configure(config)
    }

    pub(crate) fn refresh(&self) -> Result<(), String> {
        self.output.handle.refresh()
    }

    pub(crate) fn status(&self) -> OutputStatus {
        self.output.handle.status()
    }

    pub(crate) fn handle(&self) -> OutputHandle {
        self.output.handle.clone()
    }

    pub(crate) fn stop(&mut self) {
        self.output.stop();
    }

    pub fn synchronize<'a, I>(&mut self, joined: bool, peers: I) -> Result<(), String>
    where
        I: IntoIterator<Item = (MemberId, &'a Peer)>,
    {
        let result = self.synchronize_inner(joined, peers);
        self.remember_error(result)
    }

    fn synchronize_inner<'a, I>(&mut self, joined: bool, peers: I) -> Result<(), String>
    where
        I: IntoIterator<Item = (MemberId, &'a Peer)>,
    {
        let mut current: [Option<(MemberId, &'a Peer)>; MAX_PEERS] = [None; MAX_PEERS];
        let mut count = 0;
        if joined {
            for item in peers {
                if count == MAX_PEERS {
                    return Err("Playback admission exceeds the 64-peer budget.".into());
                }
                current[count] = Some(item);
                count += 1;
            }
        }
        current[..count].sort_unstable_by(|left, right| {
            left.expect("occupied peer slot")
                .0
                .as_bytes()
                .cmp(right.expect("occupied peer slot").0.as_bytes())
        });
        for pair in current[..count].windows(2) {
            if pair[0].expect("occupied peer slot").0 == pair[1].expect("occupied peer slot").0 {
                return Err("Playback admission contains a duplicate member.".into());
            }
        }
        let changed = self.joined != joined
            || self.peers.len() != count
            || current[..count].iter().enumerate().any(|(index, item)| {
                let (member, peer) = item.expect("occupied peer slot");
                let cached = &self.peers[index];
                if cached.member != member {
                    return true;
                }
                let mut projection = AdmissionProjection::default();
                peer.project_admission(&mut projection);
                cached.projection != projection
            });
        if !changed {
            return Ok(());
        }
        let mut next = Vec::with_capacity(count);
        let mut sources = Vec::new();
        for (member, peer) in current[..count].iter().flatten().copied() {
            let mut projection = AdmissionProjection::default();
            peer.project_admission(&mut projection);
            let snapshot = peer.admission_snapshot();
            sources.extend(
                render::remote_render_sources(member, &snapshot)
                    .map_err(|error| error.to_string())?,
            );
            next.push(CachedPeer {
                member,
                projection,
                snapshot,
                peaks: [None; rlb_peer::MAX_SOURCES],
            });
        }
        self.ingresses = self.output.handle.replace_remote(sources)?;
        self.peers = next;
        self.joined = joined;
        self.last_error = None;
        #[cfg(test)]
        {
            self.admitted.retain(|timeline| {
                self.ingresses
                    .iter()
                    .any(|ingress| ingress.source().timeline == *timeline)
            });
            // Inspection is a test-only barrier on the actual output owner.
            self.output.handle.inspect(|_| ());
        }
        Ok(())
    }

    pub fn accept_remote(
        &mut self,
        metadata: AdmittedMetadata,
        samples: &[f64],
    ) -> Result<(), String> {
        let result = self.accept_remote_inner(metadata, samples);
        self.remember_error(result)
    }

    fn accept_remote_inner(
        &mut self,
        metadata: AdmittedMetadata,
        samples: &[f64],
    ) -> Result<(), String> {
        let cached = self
            .peers
            .iter_mut()
            .find(|cached| cached.member == metadata.member_id)
            .ok_or_else(|| "PCM has no synchronized current member.".to_owned())?;
        let snapshot = &cached.snapshot;
        if !self.joined
            || !snapshot.connected
            || !snapshot.receive_ready
            || snapshot.quiesced
            || snapshot.peer_instance != metadata.peer_instance
            || snapshot.remote_run != Some(metadata.remote_run)
            || snapshot.session != Some(metadata.session)
            || metadata.admission_revision > snapshot.revision
            || !snapshot.sources.iter().any(|source| {
                source.receive_ready
                    && source.entry == metadata.entry
                    && source.fence == metadata.source_fence
            })
        {
            return Err("PCM does not match current synchronized admission.".into());
        }
        let block = render::block_from_admitted_pcm(rlb_peer::AdmittedPcm { metadata, samples })
            .map_err(|error| error.to_string())?;
        if samples.len() != usize::from(metadata.frames) * usize::from(block.format.channels()) {
            return Err("PCM length does not match its admitted frame count.".into());
        }
        let ingress = self
            .ingresses
            .iter()
            .find(|ingress| {
                ingress.source().timeline == block.timeline
                    && ingress.source().permission == block.permission
            })
            .ok_or("PCM has no current output ingress.")?;
        if let Some(index) = cached.snapshot.sources.iter()
            .position(|source| source.entry.id == metadata.entry.id)
        {
            let peak = samples.iter().fold(0.0_f64, |peak, sample| peak.max(sample.abs()));
            cached.peaks[index] = Some((metadata.entry.id, peak, Instant::now()));
        }
        ingress.push(block)?;
        #[cfg(test)]
        {
            if let Some(existing) = self
                .admitted
                .iter_mut()
                .find(|timeline| timeline.same_logical_source(block.timeline))
            {
                *existing = block.timeline;
            } else {
                if self.admitted.len() == SOURCE_SLOTS {
                    return Err("Diagnostic admitted-source list is full.".into());
                }
                self.admitted.push(block.timeline);
            }
        }
        Ok(())
    }

    fn remember_error<T>(&mut self, result: Result<T, String>) -> Result<T, String> {
        if let Err(error) = &result {
            self.last_error = Some(error.clone());
        } else {
            self.last_error = None;
        }
        result
    }

    /// Pre-gain observations of current, admitted original sources.
    pub(crate) fn source_peaks(
        &self,
        member: MemberId,
    ) -> [Option<(rlb_model::SourceId, f64, Instant)>; rlb_peer::MAX_SOURCES] {
        self.peers.iter().find(|peer| peer.member == member)
            .map(|peer| peer.peaks).unwrap_or([None; rlb_peer::MAX_SOURCES])
    }

    #[cfg(test)]
    pub fn select_software_route(&mut self) {
        self.output.handle.select_software();
    }

    /// Native service is independent of UDP and control-loop scheduling.
    pub fn service_software(&mut self) -> Result<(), String> {
        Ok(())
    }

    pub fn submitted_frames(&self) -> u64 {
        self.status().submitted_frames
    }
    pub fn admission_revision(&self) -> u64 {
        self.status().admission_revision
    }
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[cfg(test)]
    pub fn admitted_timelines(&self) -> &[TimelineId] {
        &self.admitted
    }

    #[cfg(test)]
    pub fn admitted_closed(&self) -> Vec<u64> {
        let timelines = self.admitted.clone();
        self.output.handle.inspect(move |core| {
            timelines
                .iter()
                .filter_map(|timeline| {
                    core.source_status(*timeline)
                        .map(|status| status.closed_before)
                })
                .collect()
        })
    }

    #[cfg(test)]
    pub fn source_closed(&self, timeline: TimelineId) -> Option<u64> {
        self.output.handle.inspect(move |core| {
            core.source_status(timeline)
                .map(|status| status.closed_before)
        })
    }

    #[cfg(test)]
    pub fn source_active(&self, timeline: TimelineId) -> Option<bool> {
        self.output
            .handle
            .inspect(move |core| core.source_status(timeline).map(|status| status.active))
    }
}
