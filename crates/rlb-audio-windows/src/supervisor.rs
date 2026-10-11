use std::sync::Arc;

use rlb_audio::{
    AudioMetrics, EndpointDirection, MixGraphConsumer, MixGraphPublisher, MixGraphReclaimer,
    mix_graph_channel,
};
use rlb_model::{CaptureMethod, SourceId, StreamEpoch};

use crate::capture::{CaptureFlow, CaptureRequest};
use crate::endpoint::{
    BindingAction, EndpointCatalog, EndpointEnumerator, EndpointSelection, poll_binding,
};
use crate::worker::{CaptureWorker, CaptureWorkerReport, RenderWorker, RenderWorkerReport};
use crate::{RenderRequest, WindowsAudioError};

/// Follow/pin selections for the three first-release Windows audio slots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceBindings {
    /// Render-endpoint loopback used as `system_audio`.
    pub system_audio: EndpointSelection,
    /// Capture-endpoint microphone.
    pub microphone: EndpointSelection,
    /// Shared-mode system playback.
    pub render: EndpointSelection,
}

/// Stable source identities that survive endpoint rebind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorSources {
    /// Product `system_audio` source.
    pub system_audio: SourceId,
    /// Product `microphone` source.
    pub microphone: SourceId,
}

/// Current bind result for one audio slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlotStatus {
    /// A worker is open on this endpoint.
    Running {
        /// Opaque Windows endpoint ID.
        endpoint_id: String,
        /// User-visible endpoint label.
        friendly_name: String,
    },
    /// The configured selection is absent or failed to open.
    Unavailable {
        /// Fixed ID or stable follow-default selection label.
        endpoint_id: String,
    },
}

/// Latest status of the three isolated audio slots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorPoll {
    /// System-audio loopback slot.
    pub system_audio: SlotStatus,
    /// Microphone slot.
    pub microphone: SlotStatus,
    /// System-playback slot.
    pub render: SlotStatus,
    /// Mix-graph generation. Increments when the render worker is rebound.
    pub mix_epoch: u64,
}

/// Device-supervisor owner for WASAPI follow/pin open and rebind.
///
/// Enumeration and worker lifecycle stay on this thread. Capture and render
/// callbacks run on the workers this type starts. A failure in one slot leaves
/// the other slots running. Mix-graph commands are published from this thread
/// and applied on the render worker with `try_recv`.
pub struct DeviceSupervisor {
    enumerator: EndpointEnumerator,
    metrics: Arc<AudioMetrics>,
    bindings: DeviceBindings,
    sources: SupervisorSources,
    system_capture_method: CaptureMethod,
    system_audio: CaptureSlot,
    microphone: CaptureSlot,
    render: RenderSlot,
    mix: MixGraphState,
}

struct MixGraphState {
    publisher: MixGraphPublisher,
    reclaimer: MixGraphReclaimer,
    epoch: u64,
}

struct CaptureSlot {
    worker: Option<CaptureWorker>,
    epoch: StreamEpoch,
    status: SlotStatus,
    open_flow: Option<CaptureFlow>,
}

struct RenderSlot {
    worker: Option<RenderWorker>,
    status: SlotStatus,
    pending_mix: Option<MixGraphConsumer>,
}

impl DeviceSupervisor {
    /// Enumerates once and opens every slot that currently resolves.
    pub fn start(
        bindings: DeviceBindings,
        sources: SupervisorSources,
        metrics: Arc<AudioMetrics>,
    ) -> Result<Self, WindowsAudioError> {
        let enumerator = EndpointEnumerator::new()?;
        let (publisher, consumer, reclaimer) = mix_graph_channel()?;
        let mut supervisor = Self {
            enumerator,
            metrics,
            bindings,
            sources,
            system_capture_method: CaptureMethod::WasapiLoopback,
            system_audio: CaptureSlot {
                worker: None,
                epoch: new_epoch()?,
                status: SlotStatus::Unavailable {
                    endpoint_id: String::new(),
                },
                open_flow: None,
            },
            microphone: CaptureSlot {
                worker: None,
                epoch: new_epoch()?,
                status: SlotStatus::Unavailable {
                    endpoint_id: String::new(),
                },
                open_flow: None,
            },
            render: RenderSlot {
                worker: None,
                status: SlotStatus::Unavailable {
                    endpoint_id: String::new(),
                },
                pending_mix: Some(consumer),
            },
            mix: MixGraphState {
                publisher,
                reclaimer,
                epoch: 0,
            },
        };
        supervisor.poll()?;
        Ok(supervisor)
    }

    /// Follow/pin selections currently owned by this supervisor.
    #[must_use]
    pub const fn bindings(&self) -> &DeviceBindings {
        &self.bindings
    }

    /// Stable source identities.
    #[must_use]
    pub const fn sources(&self) -> SupervisorSources {
        self.sources
    }

    /// Current system-audio capture method.
    #[must_use]
    pub const fn system_capture_method(&self) -> CaptureMethod {
        self.system_capture_method
    }

    /// Changes the system-audio capture method. The next `poll` rebinds the slot.
    pub fn set_system_capture_method(&mut self, method: CaptureMethod) {
        if self.system_capture_method != method {
            self.system_capture_method = method;
            let _ = stop_capture(&mut self.system_audio);
            self.system_audio.open_flow = None;
        }
    }

    /// Replaces follow/pin selections. The next `poll` rebinds changed slots.
    pub fn set_bindings(&mut self, bindings: DeviceBindings) {
        self.bindings = bindings;
    }

    /// Resolves how the system-audio slot should open for one capture method.
    ///
    /// `TestSignedVirtualOutput` has no WASAPI path until the driver is audited
    /// and loaded. Callers must leave that slot unavailable.
    #[must_use]
    pub const fn system_audio_capture_plan(
        method: CaptureMethod,
    ) -> Option<(EndpointDirection, CaptureFlow, u8)> {
        match method {
            CaptureMethod::WasapiLoopback => {
                Some((EndpointDirection::Render, CaptureFlow::Loopback, 2))
            }
            CaptureMethod::InstalledCableCapture => {
                Some((EndpointDirection::Capture, CaptureFlow::Microphone, 2))
            }
            CaptureMethod::TestSignedVirtualOutput => None,
        }
    }

    /// Current system-audio capture epoch.
    #[must_use]
    pub const fn system_audio_epoch(&self) -> StreamEpoch {
        self.system_audio.epoch
    }

    /// Current microphone capture epoch.
    #[must_use]
    pub const fn microphone_epoch(&self) -> StreamEpoch {
        self.microphone.epoch
    }

    /// System-audio worker when that slot is running.
    pub fn system_audio(&mut self) -> Option<&mut CaptureWorker> {
        self.system_audio.worker.as_mut()
    }

    /// Microphone worker when that slot is running.
    pub fn microphone(&mut self) -> Option<&mut CaptureWorker> {
        self.microphone.worker.as_mut()
    }

    /// Render worker when that slot is running.
    #[must_use]
    pub fn render(&self) -> Option<&RenderWorker> {
        self.render.worker.as_ref()
    }

    /// Publisher used by the engine coordinator to mutate the live mix graph.
    #[must_use]
    pub fn mix_publisher(&self) -> MixGraphPublisher {
        self.mix.publisher.clone()
    }

    /// Mix-graph generation. Engine republishes the last plan when this changes.
    #[must_use]
    pub const fn mix_epoch(&self) -> u64 {
        self.mix.epoch
    }

    /// Drops retired mix readers on the supervisor thread.
    pub fn reclaim_mix_readers(&mut self) {
        self.mix.reclaimer.drain();
    }

    /// Re-enumerates and rebinds only the slots whose endpoint identity changed.
    pub fn poll(&mut self) -> Result<SupervisorPoll, WindowsAudioError> {
        let catalog = self.enumerator.enumerate()?;
        match Self::system_audio_capture_plan(self.system_capture_method) {
            Some((direction, flow, channels)) => {
                if self.system_audio.open_flow.is_some_and(|open| open != flow) {
                    let _ = stop_capture(&mut self.system_audio);
                    self.system_audio.open_flow = None;
                }
                reconcile_capture(
                    &catalog,
                    &self.bindings.system_audio,
                    direction,
                    flow,
                    channels,
                    self.sources.system_audio,
                    &self.metrics,
                    &mut self.system_audio,
                );
                if self.system_audio.worker.is_some() {
                    self.system_audio.open_flow = Some(flow);
                }
            }
            None => {
                let _ = stop_capture(&mut self.system_audio);
                self.system_audio.open_flow = None;
                self.system_audio.status = SlotStatus::Unavailable {
                    endpoint_id: "test_signed_virtual_output".to_owned(),
                };
            }
        }
        reconcile_capture(
            &catalog,
            &self.bindings.microphone,
            EndpointDirection::Capture,
            CaptureFlow::Microphone,
            1,
            self.sources.microphone,
            &self.metrics,
            &mut self.microphone,
        );
        reconcile_render(
            &catalog,
            &self.bindings.render,
            &self.metrics,
            &mut self.mix,
            &mut self.render,
        );
        Ok(self.snapshot())
    }

    /// Latest slot statuses without enumerating.
    #[must_use]
    pub fn snapshot(&self) -> SupervisorPoll {
        SupervisorPoll {
            system_audio: self.system_audio.status.clone(),
            microphone: self.microphone.status.clone(),
            render: self.render.status.clone(),
            mix_epoch: self.mix.epoch,
        }
    }

    /// Stops every worker and returns their terminal diagnostics.
    pub fn shutdown(mut self) -> SupervisorShutdown {
        SupervisorShutdown {
            system_audio: stop_capture(&mut self.system_audio),
            microphone: stop_capture(&mut self.microphone),
            render: stop_render(&mut self.render),
        }
    }
}

impl Drop for DeviceSupervisor {
    fn drop(&mut self) {
        let _ = stop_capture(&mut self.system_audio);
        let _ = stop_capture(&mut self.microphone);
        let _ = stop_render(&mut self.render);
    }
}

/// Terminal results collected when the supervisor stops its workers.
#[derive(Debug)]
pub struct SupervisorShutdown {
    /// Loopback worker stop result, if a worker was running.
    pub system_audio: Option<Result<CaptureWorkerReport, WindowsAudioError>>,
    /// Microphone worker stop result, if a worker was running.
    pub microphone: Option<Result<CaptureWorkerReport, WindowsAudioError>>,
    /// Render worker stop result, if a worker was running.
    pub render: Option<Result<RenderWorkerReport, WindowsAudioError>>,
}

#[allow(clippy::too_many_arguments)]
fn reconcile_capture(
    catalog: &EndpointCatalog,
    selection: &EndpointSelection,
    bind_direction: EndpointDirection,
    flow: CaptureFlow,
    channels: u8,
    source_id: SourceId,
    metrics: &Arc<AudioMetrics>,
    slot: &mut CaptureSlot,
) {
    let active_id = slot.worker.as_ref().map(CaptureWorker::endpoint_id);
    match poll_binding(selection, bind_direction, active_id, catalog) {
        BindingAction::Unchanged => {}
        BindingAction::Rebind(_) => {
            let _ = stop_capture(slot);
            match new_epoch() {
                Ok(epoch) => slot.epoch = epoch,
                Err(_) => {
                    slot.status = SlotStatus::Unavailable {
                        endpoint_id: selection_label(selection, bind_direction),
                    };
                    return;
                }
            }
            match CaptureWorker::spawn(
                CaptureRequest {
                    selection: selection.clone(),
                    flow,
                    source_id,
                    stream_epoch: slot.epoch,
                    channels,
                },
                Arc::clone(metrics),
            ) {
                Ok(worker) => {
                    slot.status = SlotStatus::Running {
                        endpoint_id: worker.endpoint_id().to_owned(),
                        friendly_name: worker.friendly_name().to_owned(),
                    };
                    slot.worker = Some(worker);
                }
                Err(error) => {
                    slot.status = SlotStatus::Unavailable {
                        endpoint_id: unavailable_id(&error, selection, bind_direction),
                    };
                }
            }
        }
        BindingAction::Unavailable { endpoint_id } => {
            let _ = stop_capture(slot);
            slot.status = SlotStatus::Unavailable { endpoint_id };
        }
    }
}

fn reconcile_render(
    catalog: &EndpointCatalog,
    selection: &EndpointSelection,
    metrics: &Arc<AudioMetrics>,
    mix: &mut MixGraphState,
    slot: &mut RenderSlot,
) {
    let active_id = slot.worker.as_ref().map(RenderWorker::endpoint_id);
    match poll_binding(selection, EndpointDirection::Render, active_id, catalog) {
        BindingAction::Unchanged => {}
        BindingAction::Rebind(_) => {
            let _ = stop_render(slot);
            match next_mix_consumer(mix, slot) {
                Ok(consumer) => match RenderWorker::spawn(
                    RenderRequest {
                        selection: selection.clone(),
                    },
                    Arc::clone(metrics),
                    consumer,
                ) {
                    Ok(worker) => {
                        slot.status = SlotStatus::Running {
                            endpoint_id: worker.endpoint_id().to_owned(),
                            friendly_name: worker.friendly_name().to_owned(),
                        };
                        slot.worker = Some(worker);
                    }
                    Err(error) => {
                        slot.status = SlotStatus::Unavailable {
                            endpoint_id: unavailable_id(
                                &error,
                                selection,
                                EndpointDirection::Render,
                            ),
                        };
                    }
                },
                Err(_) => {
                    slot.status = SlotStatus::Unavailable {
                        endpoint_id: selection_label(selection, EndpointDirection::Render),
                    };
                }
            }
        }
        BindingAction::Unavailable { endpoint_id } => {
            let _ = stop_render(slot);
            slot.status = SlotStatus::Unavailable { endpoint_id };
        }
    }
}

fn next_mix_consumer(
    mix: &mut MixGraphState,
    slot: &mut RenderSlot,
) -> Result<MixGraphConsumer, rlb_audio::AudioError> {
    if let Some(consumer) = slot.pending_mix.take() {
        return Ok(consumer);
    }
    let (publisher, consumer, reclaimer) = mix_graph_channel()?;
    mix.publisher = publisher;
    mix.reclaimer = reclaimer;
    mix.epoch = mix.epoch.saturating_add(1);
    Ok(consumer)
}

fn stop_capture(slot: &mut CaptureSlot) -> Option<Result<CaptureWorkerReport, WindowsAudioError>> {
    slot.worker.take().map(CaptureWorker::stop)
}

fn stop_render(slot: &mut RenderSlot) -> Option<Result<RenderWorkerReport, WindowsAudioError>> {
    slot.worker.take().map(RenderWorker::stop)
}

fn new_epoch() -> Result<StreamEpoch, WindowsAudioError> {
    StreamEpoch::generate().map_err(|_| WindowsAudioError::Wasapi {
        operation: "StreamEpoch::generate",
        message: "operating-system random source unavailable".to_owned(),
    })
}

fn unavailable_id(
    error: &WindowsAudioError,
    selection: &EndpointSelection,
    direction: EndpointDirection,
) -> String {
    match error {
        WindowsAudioError::DeviceUnavailable { endpoint_id } => endpoint_id.clone(),
        WindowsAudioError::NoDefaultEndpoint { direction, role } => {
            format!("default:{direction}:{role}")
        }
        _ => selection_label(selection, direction),
    }
}

fn selection_label(selection: &EndpointSelection, direction: EndpointDirection) -> String {
    match selection {
        EndpointSelection::FollowDefault { role } => {
            let flow = match direction {
                EndpointDirection::Capture => "capture",
                EndpointDirection::Render | EndpointDirection::LoopbackCapture => "render",
            };
            format!("default:{flow}:{}", role.as_str())
        }
        EndpointSelection::Fixed { endpoint_id } => endpoint_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointRole;
    use rlb_audio::EndpointDirection;

    fn sources() -> SupervisorSources {
        SupervisorSources {
            system_audio: SourceId::generate().expect("system source"),
            microphone: SourceId::generate().expect("mic source"),
        }
    }

    fn has_default(direction: EndpointDirection) -> bool {
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        enumerator
            .enumerate()
            .expect("catalog")
            .default_endpoint(direction, EndpointRole::Console)
            .is_some()
    }

    #[test]
    fn follow_default_opens_available_slots_and_second_poll_is_unchanged() {
        if !has_default(EndpointDirection::Render) {
            return;
        }
        let metrics = Arc::new(AudioMetrics::new());
        let mut supervisor = DeviceSupervisor::start(
            DeviceBindings {
                system_audio: EndpointSelection::follow_default(),
                microphone: EndpointSelection::follow_default(),
                render: EndpointSelection::follow_default(),
            },
            sources(),
            metrics,
        )
        .expect("start supervisor");
        let first = supervisor.snapshot();
        assert!(matches!(first.system_audio, SlotStatus::Running { .. }));
        assert!(matches!(first.render, SlotStatus::Running { .. }));
        if has_default(EndpointDirection::Capture) {
            assert!(matches!(first.microphone, SlotStatus::Running { .. }));
        }
        let system_id = supervisor
            .system_audio()
            .expect("system worker")
            .endpoint_id()
            .to_owned();
        let second = supervisor.poll().expect("poll");
        match second.system_audio {
            SlotStatus::Running { endpoint_id, .. } => assert_eq!(endpoint_id, system_id),
            SlotStatus::Unavailable { .. } => panic!("follow-default loopback became unavailable"),
        }
        supervisor.shutdown();
    }

    #[test]
    fn missing_microphone_pin_leaves_system_audio_and_render_running() {
        if !has_default(EndpointDirection::Render) {
            return;
        }
        let metrics = Arc::new(AudioMetrics::new());
        let mut supervisor = DeviceSupervisor::start(
            DeviceBindings {
                system_audio: EndpointSelection::follow_default(),
                microphone: EndpointSelection::fixed("missing-microphone"),
                render: EndpointSelection::follow_default(),
            },
            sources(),
            metrics,
        )
        .expect("start supervisor");
        let poll = supervisor.snapshot();
        assert!(matches!(poll.system_audio, SlotStatus::Running { .. }));
        assert!(matches!(poll.render, SlotStatus::Running { .. }));
        assert_eq!(
            poll.microphone,
            SlotStatus::Unavailable {
                endpoint_id: "missing-microphone".to_owned()
            }
        );
        assert!(supervisor.microphone().is_none());
        assert!(supervisor.system_audio().is_some());
        supervisor.shutdown();
    }

    #[test]
    fn system_audio_capture_plan_keeps_cable_on_capture_endpoints() {
        assert_eq!(
            DeviceSupervisor::system_audio_capture_plan(CaptureMethod::WasapiLoopback),
            Some((EndpointDirection::Render, CaptureFlow::Loopback, 2))
        );
        assert_eq!(
            DeviceSupervisor::system_audio_capture_plan(CaptureMethod::InstalledCableCapture),
            Some((EndpointDirection::Capture, CaptureFlow::Microphone, 2))
        );
        assert_eq!(
            DeviceSupervisor::system_audio_capture_plan(CaptureMethod::TestSignedVirtualOutput),
            None
        );
    }

    #[test]
    fn test_signed_virtual_output_leaves_system_audio_unavailable() {
        let metrics = Arc::new(AudioMetrics::new());
        let mut supervisor = DeviceSupervisor::start(
            DeviceBindings {
                system_audio: EndpointSelection::follow_default(),
                microphone: EndpointSelection::follow_default(),
                render: EndpointSelection::follow_default(),
            },
            sources(),
            metrics,
        )
        .expect("start supervisor");
        supervisor.set_system_capture_method(CaptureMethod::TestSignedVirtualOutput);
        let poll = supervisor.poll().expect("poll");
        assert_eq!(
            poll.system_audio,
            SlotStatus::Unavailable {
                endpoint_id: "test_signed_virtual_output".to_owned()
            }
        );
        supervisor.shutdown();
    }
}
