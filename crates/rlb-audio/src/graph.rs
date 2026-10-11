//! Lock-free mix-graph publish path used by the shared render callback.
//!
//! The coordinator thread allocates readers and publishes [`MixGraphCommand`]
//! values through a preallocated `sync_channel`. The render thread drains with
//! `try_recv` before mixing. Removed readers travel back through a second
//! bounded channel so their final drop happens on the coordinator thread.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use rlb_model::{MemberId, MemberMix, SourceId, SourceMix};

use crate::{AudioError, SharedRenderMixer, SourceReader, SourceRenderConfig};

/// Default command-queue depth: one slot per first-release mix source.
pub const MIX_GRAPH_COMMAND_CAPACITY: usize = 64;
/// Default retire-queue depth matching the prepared mixer capacity.
pub const MIX_GRAPH_RETIRE_CAPACITY: usize = 64;

/// One graph mutation prepared off the render thread.
pub enum MixGraphCommand {
    /// Installs a prepared reader on a free mixer slot.
    AddSource {
        /// Mix controls and routing for the new source.
        config: SourceRenderConfig,
        /// Independently advancing timeline reader.
        reader: Box<dyn SourceReader>,
    },
    /// Uninstalls a source so its reader can be retired.
    RemoveSource {
        /// Stable source identity.
        source_id: SourceId,
    },
    /// Updates source-level gain, pan, and mute.
    SetSourceMix {
        /// Stable source identity.
        source_id: SourceId,
        /// Validated persistent source mix.
        source_mix: SourceMix,
    },
    /// Updates member-level gain, mute, and solo.
    SetMemberMix {
        /// Stable member identity.
        member_id: MemberId,
        /// Validated persistent member mix.
        member_mix: MemberMix,
    },
    /// Changes whether a source is routed to the shared output.
    SetSourceRouted {
        /// Stable source identity.
        source_id: SourceId,
        /// Whether the source sums into system playback.
        routed: bool,
    },
    /// Changes whether an online member participates in the solo set.
    SetMemberSoloEligible {
        /// Stable member identity.
        member_id: MemberId,
        /// Whether this member currently activates solo.
        eligible: bool,
    },
}

impl core::fmt::Debug for MixGraphCommand {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AddSource { config, .. } => formatter
                .debug_struct("AddSource")
                .field("source_id", &config.source_id)
                .field("member_id", &config.member_id)
                .finish_non_exhaustive(),
            Self::RemoveSource { source_id } => formatter
                .debug_struct("RemoveSource")
                .field("source_id", source_id)
                .finish(),
            Self::SetSourceMix {
                source_id,
                source_mix,
            } => formatter
                .debug_struct("SetSourceMix")
                .field("source_id", source_id)
                .field("source_mix", source_mix)
                .finish(),
            Self::SetMemberMix {
                member_id,
                member_mix,
            } => formatter
                .debug_struct("SetMemberMix")
                .field("member_id", member_id)
                .field("member_mix", member_mix)
                .finish(),
            Self::SetSourceRouted { source_id, routed } => formatter
                .debug_struct("SetSourceRouted")
                .field("source_id", source_id)
                .field("routed", routed)
                .finish(),
            Self::SetMemberSoloEligible {
                member_id,
                eligible,
            } => formatter
                .debug_struct("SetMemberSoloEligible")
                .field("member_id", member_id)
                .field("eligible", eligible)
                .finish(),
        }
    }
}

/// A command that could not enter the bounded publish queue.
#[derive(Debug)]
pub struct MixGraphPublishError(pub MixGraphCommand);

/// Coordinator-owned publisher for mix-graph commands.
#[derive(Clone)]
pub struct MixGraphPublisher {
    commands: SyncSender<MixGraphCommand>,
}

impl MixGraphPublisher {
    /// Pushes one command immediately. The render thread never waits here.
    #[allow(clippy::result_large_err)]
    pub fn try_publish(&self, command: MixGraphCommand) -> Result<(), MixGraphPublishError> {
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(command) | TrySendError::Disconnected(command)) => {
                Err(MixGraphPublishError(command))
            }
        }
    }
}

/// Render-thread consumer that applies queued graph mutations with `try_recv`.
pub struct MixGraphConsumer {
    commands: Receiver<MixGraphCommand>,
    retire: SyncSender<Box<dyn SourceReader>>,
}

impl MixGraphConsumer {
    /// Applied every currently queued command before the mixer callback.
    ///
    /// Failed applies increment the mixer's callback-contract counter. A reader
    /// that cannot enter the retire queue is leaked with `mem::forget` so the
    /// last drop does not run inside the WASAPI callback.
    pub fn try_apply_all(&mut self, mixer: &mut SharedRenderMixer) -> usize {
        let mut applied = 0_usize;
        while let Ok(command) = self.commands.try_recv() {
            if self.apply_one(mixer, command) {
                applied = applied.saturating_add(1);
            } else {
                mixer.record_callback_contract_violation();
            }
        }
        applied
    }

    fn apply_one(&mut self, mixer: &mut SharedRenderMixer, command: MixGraphCommand) -> bool {
        match command {
            MixGraphCommand::AddSource { config, reader } => {
                match mixer.try_add_source(config, reader) {
                    Ok(()) => true,
                    Err((_, reader)) => {
                        self.retire_reader(reader);
                        false
                    }
                }
            }
            MixGraphCommand::RemoveSource { source_id } => match mixer.take_source(source_id) {
                Ok(reader) => self.retire_reader(reader),
                Err(AudioError::SourceNotFound) => true,
                Err(_) => false,
            },
            MixGraphCommand::SetSourceMix {
                source_id,
                source_mix,
            } => mixer.set_source_mix(source_id, source_mix).is_ok(),
            MixGraphCommand::SetMemberMix {
                member_id,
                member_mix,
            } => mixer.set_member_mix(member_id, member_mix).is_ok(),
            MixGraphCommand::SetSourceRouted { source_id, routed } => {
                mixer.set_source_routed(source_id, routed).is_ok()
            }
            MixGraphCommand::SetMemberSoloEligible {
                member_id,
                eligible,
            } => mixer.set_member_solo_eligible(member_id, eligible).is_ok(),
        }
    }

    fn retire_reader(&self, reader: Box<dyn SourceReader>) -> bool {
        match self.retire.try_send(reader) {
            Ok(()) => true,
            Err(TrySendError::Full(reader) | TrySendError::Disconnected(reader)) => {
                core::mem::forget(reader);
                false
            }
        }
    }
}

/// Coordinator-owned receiver for readers leaving the render graph.
pub struct MixGraphReclaimer {
    retired: Receiver<Box<dyn SourceReader>>,
}

impl MixGraphReclaimer {
    /// Drops every currently retired reader on the calling thread.
    pub fn drain(&mut self) {
        while self.retired.try_recv().is_ok() {}
    }
}

/// Preallocates the command and retire queues used by one shared render mixer.
pub fn mix_graph_channel()
-> Result<(MixGraphPublisher, MixGraphConsumer, MixGraphReclaimer), AudioError> {
    mix_graph_channel_with_capacity(MIX_GRAPH_COMMAND_CAPACITY, MIX_GRAPH_RETIRE_CAPACITY)
}

/// Preallocates mix-graph queues with caller-chosen bounds.
pub fn mix_graph_channel_with_capacity(
    command_capacity: usize,
    retire_capacity: usize,
) -> Result<(MixGraphPublisher, MixGraphConsumer, MixGraphReclaimer), AudioError> {
    if command_capacity == 0 {
        return Err(AudioError::ZeroCapacity {
            field: "mix_graph_command_capacity",
        });
    }
    if retire_capacity == 0 {
        return Err(AudioError::ZeroCapacity {
            field: "mix_graph_retire_capacity",
        });
    }
    let (command_tx, command_rx) = sync_channel(command_capacity);
    let (retire_tx, retire_rx) = sync_channel(retire_capacity);
    Ok((
        MixGraphPublisher {
            commands: command_tx,
        },
        MixGraphConsumer {
            commands: command_rx,
            retire: retire_tx,
        },
        MixGraphReclaimer { retired: retire_rx },
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rlb_model::{MemberId, MemberMix, SourceId, SourceMix};

    use super::*;
    use crate::{AudioMetrics, ConstantSourceReader, SoftLimiter};

    fn member(byte: u8) -> MemberId {
        MemberId::from_bytes([byte; 16]).expect("nonzero member")
    }

    fn source(byte: u8) -> SourceId {
        SourceId::from_bytes([byte; 16]).expect("nonzero source")
    }

    fn config(member_id: MemberId, source_id: SourceId) -> SourceRenderConfig {
        SourceRenderConfig {
            member_id,
            source_id,
            channels: 2,
            routed: true,
            member_solo_eligible: true,
            source_mix: SourceMix::default(),
            member_mix: MemberMix::default(),
        }
    }

    #[test]
    fn render_thread_applies_add_and_remove_without_blocking() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer = SharedRenderMixer::new(2, 8, SoftLimiter::default(), Arc::clone(&metrics))
            .expect("mixer");
        let (publisher, mut consumer, mut reclaimer) = mix_graph_channel().expect("channel");
        let source_id = source(2);
        publisher
            .try_publish(MixGraphCommand::AddSource {
                config: config(member(1), source_id),
                reader: Box::new(ConstantSourceReader::new(2, [0.25, -0.25]).expect("reader")),
            })
            .expect("publish add");
        assert_eq!(consumer.try_apply_all(&mut mixer), 1);

        let mut output = vec![0.0; 16];
        mixer.render(&mut output, 8).expect("render");
        assert!(output.iter().any(|sample| *sample != 0.0));

        publisher
            .try_publish(MixGraphCommand::RemoveSource { source_id })
            .expect("publish remove");
        assert_eq!(consumer.try_apply_all(&mut mixer), 1);
        reclaimer.drain();
        mixer.render(&mut output, 8).expect("silent render");
        assert!(output.iter().all(|sample| *sample == 0.0));
        assert_eq!(metrics.snapshot().callback_contract_violations_total, 0);
    }

    #[test]
    fn duplicate_add_is_retired_and_counted() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut mixer = SharedRenderMixer::new(2, 4, SoftLimiter::default(), Arc::clone(&metrics))
            .expect("mixer");
        let (publisher, mut consumer, mut reclaimer) = mix_graph_channel().expect("channel");
        let source_id = source(2);
        publisher
            .try_publish(MixGraphCommand::AddSource {
                config: config(member(1), source_id),
                reader: Box::new(ConstantSourceReader::new(2, [0.1, 0.1]).expect("reader")),
            })
            .expect("first");
        publisher
            .try_publish(MixGraphCommand::AddSource {
                config: config(member(1), source_id),
                reader: Box::new(ConstantSourceReader::new(2, [0.2, 0.2]).expect("reader")),
            })
            .expect("duplicate");
        assert_eq!(consumer.try_apply_all(&mut mixer), 1);
        reclaimer.drain();
        assert_eq!(metrics.snapshot().callback_contract_violations_total, 1);
        assert_eq!(metrics.snapshot().active_render_sources, 1);
    }
}
