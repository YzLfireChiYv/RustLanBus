//! Output routing/control owns endpoint lifetimes; PCM fans out directly.

#[cfg(not(test))]
use super::devices::{BindingView, DeviceHandle, DeviceWatch};
pub(crate) use super::endpoint::OutputStatus;
use super::endpoint::{EndpointHandle, EndpointIngress, EndpointRuntime};
#[cfg(not(test))]
use super::routes::RouteGuard;
#[cfg(test)]
use crate::participant::audio::render::RenderCore;
use crate::participant::audio::render::{self, RenderSource, SourceBlock, TimelineId};
use crate::participant::config::{ParticipantConfig, SourceKind};
use crate::participant::output_routing::{OutputTarget, default_output_targets};
use arc_swap::ArcSwap;
use rlb_audio_windows::AudioWake;
use rlb_model::SourceId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

#[derive(Clone)]
struct Destination {
    endpoint: String,
    ingress: EndpointIngress,
}

struct Input {
    source: RenderSource,
    active: AtomicBool,
    end: AtomicU64,
    destinations: ArcSwap<Vec<Destination>>,
}

#[derive(Clone)]
pub(crate) struct OutputIngress {
    input: Arc<Input>,
}

impl OutputIngress {
    fn new(source: RenderSource) -> Self {
        Self {
            input: Arc::new(Input {
                source,
                active: AtomicBool::new(true),
                end: AtomicU64::new(0),
                destinations: ArcSwap::from_pointee(Vec::new()),
            }),
        }
    }
    pub(crate) fn source(&self) -> RenderSource {
        self.input.source
    }
    fn active(&self) -> bool {
        self.input.active.load(Ordering::Acquire)
    }
    fn retire(&self) {
        self.input.active.store(false, Ordering::Release);
        for target in self.input.destinations.load().iter() {
            target.ingress.retire();
        }
        self.input.destinations.store(Arc::new(Vec::new()));
    }
    pub(crate) fn discard_through(&self, end: u64) {
        if !self.active() {
            return;
        }
        self.input.end.fetch_max(end, Ordering::Release);
        for target in self.input.destinations.load().iter() {
            target.ingress.discard_through(end);
        }
    }
    pub(crate) fn push(&self, block: SourceBlock<'_>) -> Result<(), String> {
        self.push_observed(block, Instant::now())
    }
    pub(crate) fn push_observed(
        &self,
        block: SourceBlock<'_>,
        observed: Instant,
    ) -> Result<(), String> {
        if !self.active() {
            return Err("This output source has been retired.".into());
        }
        let source = self.source();
        if block.timeline != source.timeline
            || block.permission != source.permission
            || block.format != source.format
        {
            return Err("PCM has a superseded output source context.".into());
        }
        let channels = usize::from(block.format.channels());
        if block.samples.is_empty()
            || !block.samples.len().is_multiple_of(channels)
            || block.samples.iter().any(|sample| !sample.is_finite())
        {
            return Err("Output PCM is empty, malformed or non-finite.".into());
        }
        let end = block
            .start_frame
            .checked_add((block.samples.len() / channels) as u64)
            .ok_or("Output source frame coordinate exhausted.")?;
        self.input.end.fetch_max(end, Ordering::Release);
        let targets = self.input.destinations.load();
        let mut error = None;
        let mut accepted = false;
        for target in targets.iter() {
            if !self.active() {
                return Err("This output source has been retired.".into());
            }
            match target.ingress.push_observed(block, observed) {
                Ok(()) => accepted = true,
                Err(failure) => {
                    error.get_or_insert(failure);
                }
            }
        }
        if accepted || targets.is_empty() {
            Ok(())
        } else {
            Err(error.unwrap_or_else(|| "Output routes are unavailable.".into()))
        }
    }
}

struct Control {
    revision: u64,
    config: ParticipantConfig,
    remote: Vec<OutputIngress>,
    local: Vec<OutputIngress>,
    main_key: Option<String>,
    #[cfg(test)]
    software_selected: bool,
}

#[derive(Default)]
struct Published {
    revision: u64,
    main_key: Option<String>,
    outputs: Vec<(String, EndpointHandle)>,
    errors: BTreeMap<String, String>,
    pending_main: bool,
    main_error: Option<String>,
}

#[derive(Clone)]
pub(crate) struct OutputHandle {
    control: Arc<Mutex<Control>>,
    published: Arc<ArcSwap<Published>>,
    wake: AudioWake,
    stop: Arc<AtomicBool>,
    #[cfg(not(test))]
    devices: DeviceHandle,
    #[cfg(not(test))]
    routes: RouteGuard,
    #[cfg(test)]
    inspect: SyncSender<Inspection>,
}

pub(crate) struct OutputRuntime {
    pub(crate) handle: OutputHandle,
    owner: Option<JoinHandle<()>>,
    #[cfg(not(test))]
    devices: DeviceWatch,
}

#[cfg(test)]
type Inspection = Box<dyn FnOnce(&EndpointHandle) + Send>;

impl OutputRuntime {
    pub(crate) fn start(config: ParticipantConfig) -> Result<Self, String> {
        let wake = AudioWake::new().map_err(|error| error.to_string())?;
        #[cfg(not(test))]
        let devices = DeviceWatch::start(config.output_endpoint_id.clone(), wake.clone())?;
        #[cfg(test)]
        let (inspect, inspections) = mpsc::sync_channel(16);
        let handle = OutputHandle {
            control: Arc::new(Mutex::new(Control {
                revision: 1,
                config,
                remote: Vec::new(),
                local: Vec::new(),
                main_key: None,
                #[cfg(test)]
                software_selected: false,
            })),
            published: Arc::new(ArcSwap::from_pointee(Published::default())),
            wake,
            stop: Arc::new(AtomicBool::new(false)),
            #[cfg(not(test))]
            devices: devices.handle.clone(),
            #[cfg(not(test))]
            routes: RouteGuard::default(),
            #[cfg(test)]
            inspect,
        };
        let owner_handle = handle.clone();
        let (ready, boot) = std::sync::mpsc::sync_channel(1);
        let owner = thread::Builder::new()
            .name("rlb-output-routes".into())
            .spawn(move || {
                let mut owner = Owner::new(owner_handle);
                let result = owner.reconcile();
                let _ = ready.send(result);
                while !owner.handle.stop.load(Ordering::Acquire) {
                    let _ = owner.reconcile();
                    #[cfg(test)]
                    while let Ok(inspect) = inspections.try_recv() {
                        if let Some(main) = owner.main_handle() {
                            inspect(&main);
                        }
                    }
                    if owner.handle.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let wait = if owner.handle.published.load().errors.is_empty() {
                        u32::MAX
                    } else {
                        500
                    };
                    if owner.handle.wake.wait(wait).is_err() {
                        break;
                    }
                }
                for runtime in owner.outputs.values_mut() {
                    runtime.stop();
                }
            })
            .map_err(|error| format!("The output route owner could not start: {error}"))?;
        match boot.recv() {
            Ok(Ok(())) => Ok(Self {
                handle,
                owner: Some(owner),
                #[cfg(not(test))]
                devices,
            }),
            result => {
                handle.stop.store(true, Ordering::Release);
                let _ = handle.wake.signal();
                let _ = owner.join();
                Err(result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| "The output route owner stopped at startup.".into()))
            }
        }
    }
    pub(crate) fn stop(&mut self) {
        self.handle.stop.store(true, Ordering::Release);
        let _ = self.handle.wake.signal();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
        #[cfg(not(test))]
        self.devices.stop();
    }
}
impl Drop for OutputRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

impl OutputHandle {
    #[cfg(not(test))]
    pub(crate) fn routes(&self) -> RouteGuard {
        self.routes.clone()
    }
    #[cfg(not(test))]
    pub(crate) fn devices(&self) -> DeviceHandle {
        self.devices.clone()
    }
    pub(crate) fn catalogue(&self) -> Vec<crate::participant::snapshot::AudioEndpoint> {
        #[cfg(not(test))]
        {
            self.devices.catalogue()
        }
        #[cfg(test)]
        {
            Vec::new()
        }
    }
    pub(crate) fn configure(&self, config: &ParticipantConfig) -> Result<(), String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.config != *config {
            let revision = control
                .revision
                .checked_add(1)
                .ok_or("Output route revision exhausted.")?;
            let main = main_for_config(config, control.main_key.as_deref(), &control.config);
            for input in control.remote.iter().chain(&control.local) {
                let wanted = resolve_targets(&targets_for(config, input.source()), main.as_deref());
                let old = input.input.destinations.load();
                let next = old
                    .iter()
                    .filter_map(|target| {
                        if wanted.contains(&target.endpoint) && target.ingress.is_active() {
                            Some(target.clone())
                        } else {
                            target.ingress.retire();
                            None
                        }
                    })
                    .collect();
                input.input.destinations.store(Arc::new(next));
            }
            control.config = config.clone();
            control.revision = revision;
        }
        drop(control);
        #[cfg(not(test))]
        self.devices
            .configure(config.output_endpoint_id.as_deref(), false)?;
        self.wake.signal().map_err(|error| error.to_string())
    }
    pub(crate) fn set_local_source(
        &self,
        id: SourceId,
        source: Option<RenderSource>,
    ) -> Result<Option<OutputIngress>, String> {
        if source.is_some_and(
            |entry| !matches!(entry.timeline,TimelineId::Local{source,..} if source==id),
        ) {
            return Err("Local monitor update has a different source identity.".into());
        }
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let at = control.local.iter().position(
            |input| matches!(input.source().timeline,TimelineId::Local{source,..} if source==id),
        );
        if let Some(at) = at {
            if source == Some(control.local[at].source()) {
                return Ok(Some(control.local[at].clone()));
            }
        } else if source.is_none() {
            return Ok(None);
        }
        let revision = control
            .revision
            .checked_add(1)
            .ok_or("Output route revision exhausted.")?;
        if let Some(at) = at {
            control.local.remove(at).retire();
        }
        let input = source.map(OutputIngress::new);
        if let Some(input) = &input {
            control.local.push(input.clone());
        }
        control.revision = revision;
        drop(control);
        self.wake.signal().map_err(|error| error.to_string())?;
        Ok(input)
    }
    pub(crate) fn replace_remote(
        &self,
        sources: Vec<RenderSource>,
    ) -> Result<Vec<OutputIngress>, String> {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = &control.remote;
        if sources
            .iter()
            .eq(previous.iter().map(|input| &input.input.source))
        {
            return Ok(previous.clone());
        }
        if sources.len() + control.local.len() > render::ABSOLUTE_MAX_SOURCES {
            return Err("Output admission exceeds the source budget.".into());
        }
        let revision = control
            .revision
            .checked_add(1)
            .ok_or("Output route revision exhausted.")?;
        let next: Vec<_> = sources
            .into_iter()
            .map(|source| {
                previous
                    .iter()
                    .find(|input| input.source() == source)
                    .cloned()
                    .unwrap_or_else(|| OutputIngress::new(source))
            })
            .collect();
        for old in previous {
            if !next
                .iter()
                .any(|current| Arc::ptr_eq(&current.input, &old.input))
            {
                old.retire();
            }
        }
        control.remote = next.clone();
        control.revision = revision;
        drop(control);
        self.wake.signal().map_err(|error| error.to_string())?;
        Ok(next)
    }
    pub(crate) fn refresh(&self) -> Result<(), String> {
        #[cfg(not(test))]
        {
            let config = self
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .config
                .clone();
            self.devices
                .configure(config.output_endpoint_id.as_deref(), true)?;
        }
        let current = self.published.load();
        for (_, output) in &current.outputs {
            output.refresh()?;
        }
        self.wake.signal().map_err(|error| error.to_string())
    }
    pub(crate) fn status(&self) -> OutputStatus {
        let current = self.published.load();
        let mut outputs: Vec<_> = current
            .outputs
            .iter()
            .map(|(id, handle)| {
                let mut state = handle.status();
                if let Some(error) = current.errors.get(id) {
                    state.state = "unavailable";
                    state.last_error = Some(error.clone());
                }
                (id.clone(), state)
            })
            .collect();
        for (id, error) in &current.errors {
            if !outputs.iter().any(|(key, _)| key == id) {
                outputs.push((
                    id.clone(),
                    OutputStatus {
                        state: "unavailable",
                        last_error: Some(error.clone()),
                        ..OutputStatus::default()
                    },
                ));
            }
        }
        let representative = current
            .main_key
            .as_ref()
            .and_then(|id| {
                outputs
                    .iter()
                    .find(|(key, state)| key == id && state.state == "running")
            })
            .or_else(|| outputs.iter().find(|(_, state)| state.state == "running"))
            .or_else(|| {
                current
                    .main_key
                    .as_ref()
                    .and_then(|id| outputs.iter().find(|(key, _)| key == id))
            })
            .or_else(|| outputs.first());
        let mut summary = representative
            .map(|(_, state)| state.clone())
            .unwrap_or_else(|| OutputStatus {
                state: "idle",
                ..OutputStatus::default()
            });
        if current.pending_main && outputs.iter().all(|(_, state)| state.state != "running") {
            summary.state = "awaiting-device";
        }
        summary.admission_revision = current.revision;
        // Amplitude is comparable across destinations even when their clocks
        // differ. The main meter displays the largest current output peak.
        let peak = outputs.iter().filter_map(|(_, state)| {
            (state.state == "running")
                .then_some((state.peak_abs?, state.peak_observed?))
                .filter(|(_, observed)| observed.elapsed() <= std::time::Duration::from_secs(2))
        }).max_by(|(left, _), (right, _)| left.total_cmp(right));
        summary.peak_abs = peak.map(|(value, _)| value);
        summary.peak_observed = peak.map(|(_, observed)| observed);
        summary.overloaded = if outputs
            .iter()
            .any(|(_, state)| state.overloaded == Some(true))
        {
            Some(true)
        } else if outputs.iter().any(|(_, state)| state.overloaded.is_some()) {
            Some(false)
        } else {
            None
        };
        if summary.last_error.is_none() {
            summary.last_error = outputs
                .iter()
                .find_map(|(_, state)| state.last_error.clone());
        }
        if summary.last_error.is_none() {
            summary.last_error = current.main_error.clone();
        }
        summary.main_endpoint_id = current.main_key.clone();
        summary.outputs = outputs;
        if self.stop.load(Ordering::Acquire) {
            summary.state = "stopped";
        }
        summary
    }
    #[cfg(test)]
    pub(crate) fn select_software(&self) {
        let mut control = self.control.lock().unwrap();
        control.software_selected = true;
        control.revision += 1;
        drop(control);
        self.wake.signal().unwrap();
    }
    #[cfg(test)]
    pub(crate) fn inspect<T: Send + 'static>(
        &self,
        inspect: impl FnOnce(&RenderCore) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::sync_channel(1);
        self.inspect
            .send(Box::new(move |handle| {
                let result = handle.inspect(inspect);
                let _ = tx.send(result);
            }))
            .unwrap();
        self.wake.signal().unwrap();
        rx.recv().unwrap()
    }
}

fn targets_for(config: &ParticipantConfig, source: RenderSource) -> Vec<OutputTarget> {
    match source.timeline {
        TimelineId::Local { source, .. } => config
            .source(source)
            .map(|source| source.output_targets.clone())
            .unwrap_or_default(),
        TimelineId::Remote { member, .. } => config
            .member(member)
            .and_then(|member| {
                member.source_output_targets.iter().find(|choice| {
                    Some(match choice.kind {
                        SourceKind::System => rlb_peer::SourceKind::System,
                        SourceKind::Microphone => rlb_peer::SourceKind::Microphone,
                    }) == source.kind
                })
            })
            .map(|choice| choice.targets.clone())
            .unwrap_or_else(default_output_targets),
    }
}

fn resolve_targets(targets: &[OutputTarget], main: Option<&str>) -> BTreeSet<String> {
    targets
        .iter()
        .filter_map(|target| match target {
            OutputTarget::Main => main.map(str::to_owned),
            OutputTarget::Endpoint(id) => Some(id.clone()),
        })
        .collect()
}

fn main_for_config(
    config: &ParticipantConfig,
    current: Option<&str>,
    previous: &ParticipantConfig,
) -> Option<String> {
    config.output_endpoint_id.clone().or_else(|| {
        (config.output_endpoint_id == previous.output_endpoint_id)
            .then(|| current.map(str::to_owned))
            .flatten()
    })
}

struct Owner {
    handle: OutputHandle,
    outputs: BTreeMap<String, EndpointRuntime>,
    revision: u64,
    #[cfg(not(test))]
    main_binding: BindingView,
}
impl Owner {
    fn new(handle: OutputHandle) -> Self {
        Self {
            handle,
            outputs: BTreeMap::new(),
            revision: 0,
            #[cfg(not(test))]
            main_binding: BindingView::default(),
        }
    }
    fn reconcile(&mut self) -> Result<(), String> {
        let control = self
            .handle
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let config = control.config.clone();
        let revision = control.revision;
        let inputs: Vec<_> = control
            .remote
            .iter()
            .chain(&control.local)
            .cloned()
            .collect();
        #[cfg(test)]
        let software_selected = control.software_selected;
        drop(control);
        #[cfg(not(test))]
        let binding_changed = if let Some(binding) = self
            .handle
            .devices
            .snapshot_after(self.main_binding.revision)
        {
            self.main_binding = binding;
            true
        } else {
            false
        };
        #[cfg(test)]
        let binding_changed = false;
        if revision == self.revision
            && !binding_changed
            && self.handle.published.load().errors.is_empty()
        {
            return Ok(());
        }
        #[cfg(not(test))]
        let main = config.output_endpoint_id.clone().or_else(|| {
            (self.main_binding.selection == config.output_endpoint_id)
                .then(|| {
                    self.main_binding
                        .endpoint
                        .as_ref()
                        .map(|endpoint| endpoint.endpoint_id.clone())
                })
                .flatten()
        });
        #[cfg(test)]
        let main = Some(
            config
                .output_endpoint_id
                .clone()
                .unwrap_or_else(|| "software-fixture".into()),
        );
        let mut groups: BTreeMap<String, (Vec<RenderSource>, Vec<RenderSource>)> = BTreeMap::new();
        if let Some(main) = &main {
            groups.entry(main.clone()).or_default();
        }
        let plans: Vec<_> = inputs
            .iter()
            .map(|input| {
                let targets = targets_for(&config, input.source());
                let keys = resolve_targets(&targets, main.as_deref());
                for key in &keys {
                    let group = groups.entry(key.clone()).or_default();
                    if matches!(input.source().timeline, TimelineId::Local { .. }) {
                        group.1.push(input.source());
                    } else {
                        group.0.push(input.source());
                    }
                }
                (
                    input.clone(),
                    keys,
                    targets.contains(&OutputTarget::Main) && main.is_none(),
                )
            })
            .collect();
        let mut errors = BTreeMap::new();
        for key in groups.keys() {
            if !self.outputs.contains_key(key) {
                let mut selected = config.clone();
                selected.output_endpoint_id = Some(key.clone());
                match EndpointRuntime::start(
                    selected,
                    #[cfg(not(test))]
                    &self.handle.devices,
                    #[cfg(not(test))]
                    self.handle.routes.clone(),
                ) {
                    Ok(output) => {
                        self.outputs.insert(key.clone(), output);
                    }
                    Err(error) => {
                        errors.insert(key.clone(), error);
                    }
                }
            }
        }
        let mut destinations: BTreeMap<String, Vec<EndpointIngress>> = BTreeMap::new();
        for (key, output) in &self.outputs {
            let mut selected = config.clone();
            selected.output_endpoint_id = Some(key.clone());
            let result = (|| {
                output.handle.configure(&selected)?;
                #[cfg(test)]
                if software_selected {
                    output.handle.select_software();
                }
                let (remote, local) = groups.get(key).cloned().unwrap_or_default();
                let mut all = output.handle.replace_remote(remote)?;
                all.extend(output.handle.replace_local(local)?);
                Ok::<_, String>(all)
            })();
            match result {
                Ok(all) => {
                    destinations.insert(key.clone(), all);
                }
                Err(error) => {
                    errors.insert(key.clone(), error);
                }
            }
        }
        let mut control = self
            .handle
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if control.revision != revision {
            for (key, all) in &destinations {
                for ingress in all {
                    let known = plans.iter().any(|(input, _, _)| {
                        input.input.destinations.load().iter().any(|target| {
                            target.endpoint == *key && target.ingress.same_binding(ingress)
                        })
                    });
                    if !known {
                        ingress.retire();
                    }
                }
            }
            drop(control);
            self.handle
                .wake
                .signal()
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        for (input, keys, _) in &plans {
            let old = input.input.destinations.load();
            for target in old.iter() {
                if !keys.contains(&target.endpoint) {
                    target.ingress.retire();
                }
            }
            let next: Vec<_> = keys
                .iter()
                .filter_map(|key| {
                    destinations
                        .get(key)
                        .and_then(|all| {
                            all.iter()
                                .find(|ingress| ingress.source() == input.source())
                        })
                        .map(|ingress| {
                            if !old.iter().any(|target| {
                                target.endpoint == *key && target.ingress.same_binding(ingress)
                            }) {
                                ingress.begin_at(input.input.end.load(Ordering::Acquire));
                            }
                            Destination {
                                endpoint: key.clone(),
                                ingress: ingress.clone(),
                            }
                        })
                })
                .collect();
            input.input.destinations.store(Arc::new(next));
        }
        control.main_key = main.clone();
        let published = Published {
            revision,
            main_key: main,
            outputs: groups
                .keys()
                .filter_map(|id| {
                    self.outputs
                        .get(id)
                        .map(|output| (id.clone(), output.handle.clone()))
                })
                .collect(),
            errors,
            pending_main: plans.iter().any(|(_, _, pending)| *pending),
            #[cfg(not(test))]
            main_error: plans
                .iter()
                .any(|(input, _, _)| {
                    targets_for(&config, input.source()).contains(&OutputTarget::Main)
                })
                .then(|| self.main_binding.error.clone())
                .flatten(),
            #[cfg(test)]
            main_error: None,
        };
        self.handle.published.store(Arc::new(published));
        self.revision = revision;
        drop(control);
        // Current preferences are the lifetime authority. A selected missing
        // endpoint remains in groups; an unselected endpoint releases its
        // owner, observation registration and prepared source storage here.
        self.outputs.retain(|key, _| groups.contains_key(key));
        Ok(())
    }
    #[cfg(test)]
    fn main_handle(&self) -> Option<EndpointHandle> {
        let graph = self.handle.published.load();
        graph
            .main_key
            .as_ref()
            .and_then(|id| self.outputs.get(id))
            .or_else(|| self.outputs.values().next())
            .map(|output| output.handle.clone())
    }
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
