//! Shared non-audio endpoint observer for output and both original sources.
//!
//! Each observation uses one fresh catalog on this worker's COM apartment.
//! Audio owners receive prepared bindings; the observer never opens streams.

use crate::participant::snapshot::AudioEndpoint;
use rlb_audio::{EndpointDirection, EndpointState};
use rlb_audio_windows::{
    AudioWake, EndpointCatalog, EndpointEnumerator, EndpointInfo, EndpointSelection,
    NativeF32Format, resolve_selection,
};
use rlb_model::SourceId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EndpointBinding {
    pub(crate) endpoint_id: String,
    /// Actual endpoint flow: render is acquired through its loopback view.
    pub(crate) direction: EndpointDirection,
    pub(crate) format: NativeF32Format,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BindingView {
    /// Requested endpoint, or the output owner's follow-default selection.
    pub(crate) selection: Option<String>,
    /// Zero means that the observer has not published its first result.
    pub(crate) revision: u64,
    pub(crate) endpoint: Option<EndpointBinding>,
    pub(crate) error: Option<String>,
}

#[derive(Clone)]
struct Requested {
    selection: Option<String>,
    enabled: bool,
    revision: u64,
    force_revision: u64,
}

#[derive(Default)]
struct Published {
    view: BindingView,
    force_revision: u64,
}

struct BindingState {
    requested: Mutex<Requested>,
    published: Mutex<Published>,
}

impl BindingState {
    fn new(selection: Option<String>, enabled: bool) -> Self {
        Self {
            requested: Mutex::new(Requested {
                selection,
                enabled,
                revision: 1,
                force_revision: 0,
            }),
            published: Mutex::new(Published::default()),
        }
    }

    fn request(&self) -> Requested {
        self.requested
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn configure(
        &self,
        endpoint_id: Option<&str>,
        enabled: bool,
        force: bool,
        control: &AudioWake,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        if stop.load(Ordering::Acquire) {
            return Err("The audio device observer has stopped.".into());
        }
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if requested.selection.as_deref() == endpoint_id && requested.enabled == enabled && !force {
            return Ok(());
        }
        let revision = requested
            .revision
            .checked_add(1)
            .ok_or("Audio device observation request counter exhausted.")?;
        requested.selection = endpoint_id.map(str::to_owned);
        requested.enabled = enabled;
        requested.revision = revision;
        if force {
            requested.force_revision = revision;
        }
        drop(requested);
        control.signal().map_err(|error| error.to_string())
    }

    fn snapshot_after(&self, revision: u64) -> Option<BindingView> {
        let published = self
            .published
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (published.view.revision != revision).then(|| published.view.clone())
    }
}

#[derive(Clone)]
struct CaptureRegistration {
    source: SourceId,
    system: bool,
    state: Arc<BindingState>,
    notify: AudioWake,
}

struct OutputRegistration {
    state: BindingState,
    notify: AudioWake,
}

#[derive(Clone)]
pub(crate) struct DeviceHandle {
    output: Arc<BindingState>,
    outputs: Arc<Mutex<Vec<Weak<OutputRegistration>>>>,
    captures: Arc<Mutex<Vec<CaptureRegistration>>>,
    catalogue: Arc<Mutex<Vec<AudioEndpoint>>>,
    control: AudioWake,
    stop: Arc<AtomicBool>,
}

impl DeviceHandle {
    /// Requests fresh observation after a selection change or explicit refresh.
    pub(crate) fn configure(&self, endpoint_id: Option<&str>, force: bool) -> Result<(), String> {
        self.output
            .configure(endpoint_id, true, force, &self.control, &self.stop)
    }

    pub(crate) fn snapshot_after(&self, revision: u64) -> Option<BindingView> {
        self.output.snapshot_after(revision)
    }

    /// Reuses the observer's latest catalog; callers never enumerate devices.
    pub(crate) fn catalogue(&self) -> Vec<AudioEndpoint> {
        self.catalogue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Observes one fixed output on the existing catalog worker. The output
    /// set groups aliases by resolved endpoint ID before opening native owners.
    /// Dropping the last handle releases this registration after an in-flight
    /// observation; the observer retains only a weak registration between turns.
    pub(crate) fn register_output(
        &self,
        endpoint_id: String,
        notify: AudioWake,
    ) -> Result<OutputDeviceHandle, String> {
        if self.stop.load(Ordering::Acquire) {
            return Err("The audio device observer has stopped.".into());
        }
        if endpoint_id.is_empty() {
            return Err("A fixed audio output requires an endpoint ID.".into());
        }
        let registration = Arc::new(OutputRegistration {
            state: BindingState::new(Some(endpoint_id), true),
            notify,
        });
        let mut outputs = self
            .outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        outputs.retain(|output| output.strong_count() > 0);
        outputs.push(Arc::downgrade(&registration));
        if let Err(error) = self.control.signal() {
            outputs.pop();
            return Err(error.to_string());
        }
        Ok(OutputDeviceHandle {
            registration,
            control: self.control.clone(),
            stop: self.stop.clone(),
        })
    }

    /// Registers one of the two run-wide original sources on this observer.
    /// Registration persists until DeviceWatch stops; configure disables it.
    pub(crate) fn register_capture(
        &self,
        source: SourceId,
        system: bool,
        endpoint: Option<String>,
        enabled: bool,
        notify: AudioWake,
    ) -> Result<CaptureDeviceHandle, String> {
        if self.stop.load(Ordering::Acquire) {
            return Err("The audio device observer has stopped.".into());
        }
        let mut captures = self
            .captures
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if captures.iter().any(|capture| capture.source == source) {
            return Err(
                "This original source already has a device observation registration.".into(),
            );
        }
        if captures.len() >= 2 {
            return Err(
                "Both original source device observation slots are already registered.".into(),
            );
        }
        let state = Arc::new(BindingState::new(endpoint, enabled));
        captures.push(CaptureRegistration {
            source,
            system,
            state: state.clone(),
            notify,
        });
        if let Err(error) = self.control.signal() {
            captures.pop();
            return Err(error.to_string());
        }
        Ok(CaptureDeviceHandle {
            state,
            control: self.control.clone(),
            stop: self.stop.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct OutputDeviceHandle {
    registration: Arc<OutputRegistration>,
    control: AudioWake,
    stop: Arc<AtomicBool>,
}

impl OutputDeviceHandle {
    pub(crate) fn snapshot_after(&self, revision: u64) -> Option<BindingView> {
        self.registration.state.snapshot_after(revision)
    }

    /// Force re-observation of this fixed endpoint without changing selection.
    pub(crate) fn refresh(&self) -> Result<(), String> {
        let requested = self.registration.state.request();
        self.registration.state.configure(
            requested.selection.as_deref(),
            true,
            true,
            &self.control,
            &self.stop,
        )
    }
}

#[derive(Clone)]
pub(crate) struct CaptureDeviceHandle {
    state: Arc<BindingState>,
    control: AudioWake,
    stop: Arc<AtomicBool>,
}

impl CaptureDeviceHandle {
    /// Disabled sources have no resolved endpoint. Enabled sources require an
    /// explicit endpoint selection; None is the UI's unconfigured state.
    pub(crate) fn configure(
        &self,
        endpoint: Option<&str>,
        enabled: bool,
        force: bool,
    ) -> Result<(), String> {
        self.state
            .configure(endpoint, enabled, force, &self.control, &self.stop)
    }

    pub(crate) fn snapshot_after(&self, revision: u64) -> Option<BindingView> {
        self.state.snapshot_after(revision)
    }
}

pub(crate) struct DeviceWatch {
    pub(crate) handle: DeviceHandle,
    owner: Option<JoinHandle<()>>,
}

impl DeviceWatch {
    pub(crate) fn start(endpoint_id: Option<String>, notify: AudioWake) -> Result<Self, String> {
        let handle = DeviceHandle {
            output: Arc::new(BindingState::new(endpoint_id, true)),
            outputs: Arc::new(Mutex::new(Vec::new())),
            captures: Arc::new(Mutex::new(Vec::with_capacity(2))),
            catalogue: Arc::new(Mutex::new(Vec::new())),
            control: AudioWake::new().map_err(|error| error.to_string())?,
            stop: Arc::new(AtomicBool::new(false)),
        };
        let owner_handle = handle.clone();
        let owner = thread::Builder::new()
            .name("rlb-audio-devices".into())
            .spawn(move || observe(owner_handle, notify))
            .map_err(|error| format!("The audio device observer could not start: {error}"))?;
        Ok(Self {
            handle,
            owner: Some(owner),
        })
    }

    pub(crate) fn stop(&mut self) {
        self.handle.stop.store(true, Ordering::Release);
        let _ = self.handle.control.signal();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

fn observe(handle: DeviceHandle, notify: AudioWake) {
    // Constructed, used and dropped on this worker only. Failed construction
    // stays visible and is retried at the same bounded observation cadence.
    let mut enumerator = None;
    while !handle.stop.load(Ordering::Acquire) {
        let requested = handle.output.request();
        let outputs: Vec<_> = output_registrations(&handle.outputs)
            .into_iter()
            .map(|output| {
                let requested = output.state.request();
                (output, requested)
            })
            .collect();
        let captures: Vec<_> = handle
            .captures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|capture| (capture.clone(), capture.state.request()))
            .collect();
        let catalog = observe_catalog(&mut enumerator);
        if handle.stop.load(Ordering::Acquire) {
            break;
        }
        let endpoints = catalog
            .as_ref()
            .map(|catalog| catalog.endpoints().iter().map(endpoint_view).collect())
            .unwrap_or_default();
        {
            let mut current = handle
                .catalogue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *current != endpoints {
                *current = endpoints;
            }
        }
        let result = catalog
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|catalog| resolve_binding(catalog, &requested, BindingKind::Output));
        let mut outcome = publish(&handle.output, &notify, &requested, result);
        for (output, requested) in &outputs {
            let result = catalog
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|catalog| resolve_binding(catalog, requested, BindingKind::Output));
            outcome = outcome.combine(publish(&output.state, &output.notify, requested, result));
        }
        for (capture, requested) in &captures {
            let result = if !requested.enabled {
                Ok(None)
            } else if requested.selection.is_none() {
                Err("Select an audio endpoint for this original source.".into())
            } else {
                let kind = if capture.system {
                    BindingKind::System
                } else {
                    BindingKind::Microphone
                };
                catalog
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|catalog| resolve_binding(catalog, requested, kind))
            };
            outcome = outcome.combine(publish(&capture.state, &capture.notify, requested, result));
        }
        match outcome {
            PublishOutcome::Stopped => break,
            // A blocking Windows query finished for an obsolete selection.
            // Resolve current requests before publishing their replacements.
            PublishOutcome::Obsolete => continue,
            PublishOutcome::Published => {}
        }
        // The private wake is distinct from every audio owner's notification.
        // A signal published during enumeration remains ready for this wait.
        if let Err(error) = handle.control.wait(1_000) {
            let error = error.to_string();
            let _ = publish(
                &handle.output,
                &notify,
                &handle.output.request(),
                Err(error.clone()),
            );
            for output in output_registrations(&handle.outputs) {
                let _ = publish(
                    &output.state,
                    &output.notify,
                    &output.state.request(),
                    Err(error.clone()),
                );
            }
            let captures = handle
                .captures
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            for capture in captures.iter() {
                let _ = publish(
                    &capture.state,
                    &capture.notify,
                    &capture.state.request(),
                    Err(error.clone()),
                );
            }
            break;
        }
    }
    handle.stop.store(true, Ordering::Release);
}

fn output_registrations(
    outputs: &Mutex<Vec<Weak<OutputRegistration>>>,
) -> Vec<Arc<OutputRegistration>> {
    let mut live = Vec::new();
    outputs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|output| {
            if let Some(output) = output.upgrade() {
                live.push(output);
                true
            } else {
                false
            }
        });
    live
}

fn observe_catalog(enumerator: &mut Option<EndpointEnumerator>) -> Result<EndpointCatalog, String> {
    if enumerator.is_none() {
        *enumerator = Some(EndpointEnumerator::new().map_err(|error| error.to_string())?);
    }
    enumerator
        .as_ref()
        .expect("prepared device enumerator")
        .enumerate()
        .map_err(|error| error.to_string())
}

fn endpoint_view(endpoint: &EndpointInfo) -> AudioEndpoint {
    let available = endpoint.state() == EndpointState::Active;
    let reason = match endpoint.state() {
        EndpointState::Active => native_format(endpoint).err(),
        EndpointState::Disabled => Some("This audio endpoint is disabled in Windows.".into()),
        EndpointState::NotPresent => Some("This audio endpoint is not currently present.".into()),
        EndpointState::Unplugged => Some("This audio endpoint is unplugged.".into()),
    };
    AudioEndpoint {
        id: endpoint.endpoint_id().to_owned(),
        name: endpoint.friendly_name().to_owned(),
        direction: match endpoint.direction() {
            EndpointDirection::Capture => "capture",
            EndpointDirection::Render | EndpointDirection::LoopbackCapture => "render",
        },
        available,
        supported: available && reason.is_none(),
        reason,
    }
}

#[derive(Clone, Copy)]
enum BindingKind {
    Output,
    System,
    Microphone,
}

fn resolve_binding(
    catalog: &EndpointCatalog,
    requested: &Requested,
    kind: BindingKind,
) -> Result<Option<EndpointBinding>, String> {
    let selection = requested
        .selection
        .as_deref()
        .map(EndpointSelection::fixed)
        .unwrap_or_else(EndpointSelection::follow_default);
    let direction = match kind {
        BindingKind::Output | BindingKind::System => EndpointDirection::Render,
        BindingKind::Microphone => EndpointDirection::Capture,
    };
    let endpoint =
        resolve_selection(&selection, direction, catalog).map_err(|error| error.to_string())?;
    let format = native_format(&endpoint)?;
    Ok(Some(EndpointBinding {
        endpoint_id: endpoint.endpoint_id().to_owned(),
        direction: endpoint.direction(),
        format,
    }))
}

fn native_format(endpoint: &EndpointInfo) -> Result<NativeF32Format, String> {
    let mix = endpoint
        .mix_format()
        .ok_or("The selected audio endpoint did not report its current mix format.")?;
    let format = NativeF32Format::from_mix(mix).map_err(|error| error.to_string())?;
    Ok(format)
}

enum PublishOutcome {
    Published,
    Obsolete,
    Stopped,
}

impl PublishOutcome {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Stopped, _) | (_, Self::Stopped) => Self::Stopped,
            (Self::Obsolete, _) | (_, Self::Obsolete) => Self::Obsolete,
            (Self::Published, Self::Published) => Self::Published,
        }
    }
}

fn publish(
    state: &BindingState,
    notify: &AudioWake,
    requested: &Requested,
    result: Result<Option<EndpointBinding>, String>,
) -> PublishOutcome {
    let current = state
        .requested
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if current.revision != requested.revision {
        return PublishOutcome::Obsolete;
    }
    let (endpoint, error) = match result {
        Ok(endpoint) => (endpoint, None),
        Err(error) => (None, Some(error)),
    };
    let mut published = state
        .published
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let view = &published.view;
    if view.revision != 0
        && published.force_revision == requested.force_revision
        && view.selection == requested.selection
        && view.endpoint == endpoint
        && view.error == error
    {
        return PublishOutcome::Published;
    }
    let Some(revision) = view.revision.checked_add(1) else {
        published.view.endpoint = None;
        published.view.error = Some("Audio device observation revision exhausted.".into());
        drop(published);
        let _ = notify.signal();
        return PublishOutcome::Stopped;
    };
    *published = Published {
        view: BindingView {
            selection: requested.selection.clone(),
            revision,
            endpoint,
            error,
        },
        force_revision: requested.force_revision,
    };
    drop(published);
    if notify.signal().is_ok() {
        PublishOutcome::Published
    } else {
        PublishOutcome::Stopped
    }
}
