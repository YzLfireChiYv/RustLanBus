use super::*;
use rlb_model::{DirectSessionId, MemberId, RunId, StreamEpoch};
use rlb_peer::{ChannelLayout, PcmEncoding, PcmFormat, PeerInstance, PublicationEpoch};
use std::time::Duration;

fn config() -> ParticipantConfig {
    ParticipantConfig::create("Output route fixture", "127.0.0.1:0".parse().unwrap()).unwrap()
}

fn local(config: &ParticipantConfig) -> RenderSource {
    RenderSource {
        timeline: TimelineId::Local {
            run: RunId::from_bytes([1; 16]).unwrap(),
            source: config.sources[0].id,
            acquisition: StreamEpoch::from_bytes([2; 16]).unwrap(),
        },
        permission: render::Permission::Local {
            monitor_revision: 1,
            capture_instance: 1,
        },
        activity: render::SourceActivity::Active,
        format: PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap(),
        kind: Some(rlb_peer::SourceKind::System),
    }
}

fn remote() -> RenderSource {
    RenderSource {
        timeline: TimelineId::Remote {
            member: MemberId::from_bytes([3; 16]).unwrap(),
            run: RunId::from_bytes([4; 16]).unwrap(),
            source: SourceId::from_bytes([5; 16]).unwrap(),
            epoch: PublicationEpoch::new(1).unwrap(),
        },
        permission: render::Permission::Remote {
            peer_instance: PeerInstance::new(1).unwrap(),
            session: DirectSessionId::from_bytes([6; 16]).unwrap(),
            fence: 1,
        },
        activity: render::SourceActivity::Active,
        format: PcmFormat::new(48_000, ChannelLayout::Mono, PcmEncoding::F32Le, 32).unwrap(),
        kind: Some(rlb_peer::SourceKind::System),
    }
}

fn wait_until(handle: &OutputHandle, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // Also wakes the routing owner and places a barrier in the main core.
        handle.inspect(|_| ());
        if condition() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Output route did not settle: {:?}",
            handle.status()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn settle(handle: &OutputHandle) {
    wait_until(handle, || {
        handle.published.load().revision == handle.control.lock().unwrap().revision
    });
}

fn endpoint(handle: &OutputHandle, id: &str) -> EndpointHandle {
    handle
        .published
        .load()
        .outputs
        .iter()
        .find(|(key, _)| key == id)
        .unwrap_or_else(|| panic!("Missing output route {id}"))
        .1
        .clone()
}

fn send(ingress: &OutputIngress, start: u64, frames: usize) {
    let source = ingress.source();
    let samples = vec![0.25; frames * usize::from(source.format.channels())];
    ingress
        .push_observed(
            SourceBlock {
                timeline: source.timeline,
                permission: source.permission,
                format: source.format,
                start_frame: start,
                arrived_at_ms: 0,
                samples: &samples,
            },
            Instant::now(),
        )
        .unwrap();
}

#[test]
fn one_original_fans_out_to_independent_output_formats_and_receipts() {
    let mut config = config();
    config.output_endpoint_id = Some("headphones-mono-48000".into());
    config.sources[0].output_targets = vec![
        OutputTarget::Main,
        OutputTarget::Endpoint("virtual-stereo-44100".into()),
    ];
    let source = local(&config);
    let runtime = OutputRuntime::start(config.clone()).unwrap();
    runtime.handle.select_software();
    let ingress = runtime
        .handle
        .set_local_source(config.sources[0].id, Some(source))
        .unwrap()
        .unwrap();
    settle(&runtime.handle);
    let mono = endpoint(&runtime.handle, "headphones-mono-48000");
    let stereo = endpoint(&runtime.handle, "virtual-stereo-44100");
    for target in [&mono, &stereo] {
        target.inspect(move |core| assert!(core.source_status(source.timeline).unwrap().active));
    }
    wait_until(&runtime.handle, || {
        mono.status().state == "running" && stereo.status().state == "running"
    });
    let before_mono = mono.inspect(RenderCore::submitted_frames);
    let before_stereo = stereo.inspect(RenderCore::submitted_frames);
    send(&ingress, 0, 1_024);
    wait_until(&runtime.handle, || {
        [&mono, &stereo].iter().all(|target| {
            target.inspect(move |core| core.source_status(source.timeline).unwrap().high_water)
                >= 1_024
        }) && mono.inspect(RenderCore::submitted_frames) > before_mono
            && stereo.inspect(RenderCore::submitted_frames) > before_stereo
    });
    let mono_status = mono.status();
    let stereo_status = stereo.status();
    assert_eq!(
        (
            mono_status.rate_hz,
            mono_status.channels,
            mono_status.period_frames
        ),
        (Some(48_000), Some(1), Some(64))
    );
    assert_eq!(
        (
            stereo_status.rate_hz,
            stereo_status.channels,
            stereo_status.period_frames
        ),
        (Some(44_100), Some(2), Some(96))
    );
    assert_eq!(ingress.input.destinations.load().len(), 2);
}

#[test]
fn main_alias_is_one_lane_and_moving_main_preserves_the_fixed_lane() {
    let mut config = config();
    config.output_endpoint_id = Some("fixed-mono".into());
    config.sources[0].output_targets = vec![
        OutputTarget::Main,
        OutputTarget::Endpoint("fixed-mono".into()),
    ];
    let source = local(&config);
    let runtime = OutputRuntime::start(config.clone()).unwrap();
    runtime.handle.select_software();
    let ingress = runtime
        .handle
        .set_local_source(config.sources[0].id, Some(source))
        .unwrap()
        .unwrap();
    settle(&runtime.handle);
    assert_eq!(runtime.handle.published.load().outputs.len(), 1);
    assert_eq!(ingress.input.destinations.load().len(), 1);
    let fixed = ingress.input.destinations.load()[0].ingress.clone();
    let output = endpoint(&runtime.handle, "fixed-mono");
    output.inspect(move |core| assert!(core.source_status(source.timeline).unwrap().active));
    send(&ingress, 0, 512);
    let high_water =
        output.inspect(move |core| core.source_status(source.timeline).unwrap().high_water);
    assert_eq!(high_water, 512);

    config.output_endpoint_id = Some("new-main-stereo-44100".into());
    runtime.handle.configure(&config).unwrap();
    settle(&runtime.handle);
    assert_eq!(
        runtime.handle.status().main_endpoint_id.as_deref(),
        Some("new-main-stereo-44100")
    );
    assert_eq!(runtime.handle.published.load().outputs.len(), 2);
    let destinations = ingress.input.destinations.load();
    assert_eq!(destinations.len(), 2);
    assert!(
        destinations
            .iter()
            .any(|target| target.endpoint == "fixed-mono" && target.ingress.same_binding(&fixed))
    );
    assert!(fixed.is_active());
    drop(destinations);
    let fixed_status = output.inspect(move |core| core.source_status(source.timeline).unwrap());
    assert!(fixed_status.active);
    assert_eq!(fixed_status.high_water, high_water);
    send(&ingress, 512, 512);
    for target in [output, endpoint(&runtime.handle, "new-main-stereo-44100")] {
        assert_eq!(
            target.inspect(move |core| core.source_status(source.timeline).unwrap().high_water),
            1_024
        );
    }
}

#[test]
fn a_missing_fixed_output_keeps_its_choice_while_healthy_output_continues() {
    let mut config = config();
    let targets = vec![
        OutputTarget::Endpoint("missing:headphones".into()),
        OutputTarget::Endpoint("healthy-stereo".into()),
    ];
    config.sources[0].output_targets = targets.clone();
    let source = local(&config);
    let runtime = OutputRuntime::start(config.clone()).unwrap();
    runtime.handle.select_software();
    let ingress = runtime
        .handle
        .set_local_source(config.sources[0].id, Some(source))
        .unwrap()
        .unwrap();
    settle(&runtime.handle);
    let healthy = endpoint(&runtime.handle, "healthy-stereo");
    let missing = endpoint(&runtime.handle, "missing:headphones");
    wait_until(&runtime.handle, || {
        healthy.status().state == "running" && missing.status().state == "unavailable"
    });
    assert_eq!(
        runtime.handle.control.lock().unwrap().config.sources[0].output_targets,
        targets
    );
    runtime
        .handle
        .inspect(move |core| assert!(core.source_status(source.timeline).is_none()));
    let before = healthy.inspect(RenderCore::submitted_frames);
    send(&ingress, 0, 512);
    wait_until(&runtime.handle, || {
        healthy.inspect(move |core| {
            core.source_status(source.timeline).unwrap().high_water == 512
                && core.submitted_frames() > before
        })
    });
    assert!(missing.status().last_error.is_some());
    assert_eq!(
        endpoint(&runtime.handle, "software-fixture").status().state,
        "idle"
    );
}

#[test]
fn removing_remote_admission_preserves_the_local_monitor_binding() {
    let config = config();
    let local = local(&config);
    let remote = remote();
    let runtime = OutputRuntime::start(config.clone()).unwrap();
    runtime.handle.select_software();
    let ingress = runtime
        .handle
        .set_local_source(config.sources[0].id, Some(local))
        .unwrap()
        .unwrap();
    let remote_ingress = runtime
        .handle
        .replace_remote(vec![remote])
        .unwrap()
        .remove(0);
    settle(&runtime.handle);
    let prior = ingress.input.destinations.load()[0].ingress.clone();
    runtime.handle.inspect(move |core| {
        assert!(core.source_status(local.timeline).unwrap().active);
        assert!(core.source_status(remote.timeline).unwrap().active);
    });
    runtime.handle.replace_remote(vec![]).unwrap();
    settle(&runtime.handle);
    assert!(ingress.active());
    assert!(!remote_ingress.active());
    assert!(
        ingress.input.destinations.load()[0]
            .ingress
            .same_binding(&prior)
    );
    runtime.handle.inspect(move |core| {
        assert!(core.source_status(local.timeline).unwrap().active);
        assert!(
            core.source_status(remote.timeline)
                .is_none_or(|source| !source.active)
        );
    });
    send(&ingress, 0, 512);
    runtime.handle.inspect(move |core| {
        assert_eq!(core.source_status(local.timeline).unwrap().high_water, 512)
    });
}

#[test]
fn empty_output_targets_keep_the_input_active_without_enabling_playback() {
    let mut config = config();
    config.sources[0].output_targets.clear();
    let source = local(&config);
    let runtime = OutputRuntime::start(config.clone()).unwrap();
    runtime.handle.select_software();
    let ingress = runtime
        .handle
        .set_local_source(config.sources[0].id, Some(source))
        .unwrap()
        .unwrap();
    settle(&runtime.handle);
    send(&ingress, 0, 512);
    assert!(ingress.active());
    assert!(ingress.input.destinations.load().is_empty());
    assert_eq!(ingress.input.end.load(Ordering::Acquire), 512);
    runtime
        .handle
        .inspect(move |core| assert!(core.source_status(source.timeline).is_none()));
    assert_eq!(
        runtime.handle.control.lock().unwrap().config.sources,
        config.sources
    );
    assert_eq!(runtime.handle.published.load().outputs.len(), 1);
    assert_eq!(runtime.handle.status().state, "idle");
}
