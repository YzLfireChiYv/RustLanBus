//! Windows WASAPI shared-mode adapter for RustLanBus V2.
//!
//! Enumeration, binding, and stream open/start/stop run on a supervisor or
//! audio-worker thread. [`WasapiCaptureStream::process`] and
//! [`WasapiRenderStream::render`] convert or submit one already-available
//! callback without waiting, allocating, or logging.

mod capture;
mod client;
mod clock;
mod convert;
mod endpoint;
mod error;
mod format;
mod mmcss;
mod render;
mod supervisor;
mod worker;

pub use capture::{CaptureFlow, CaptureRequest, CaptureStreamDiagnostics, WasapiCaptureStream};
pub use client::{EventWait, SharedEnginePeriods, SharedPeriodPreference, WasapiCallFailure};
pub use clock::{WASAPI_QPC_TICKS_PER_SECOND, wasapi_clock_domain, wasapi_qpc_100ns_now};
pub use endpoint::{
    BindingAction, EndpointCatalog, EndpointEnumerator, EndpointInfo, EndpointRole,
    EndpointSelection, poll_binding, resolve_selection,
};
pub use error::WindowsAudioError;
pub use format::{CapturePacketFlags, SampleEncoding, WasapiFormat};
pub use mmcss::MmcssRegistration;
pub use render::{RenderRequest, RenderStreamDiagnostics, WasapiRenderStream};
pub use rlb_audio::{
    MixGraphCommand, MixGraphConsumer, MixGraphPublishError, MixGraphPublisher, MixGraphReclaimer,
};
pub use supervisor::{
    DeviceBindings, DeviceSupervisor, SlotStatus, SupervisorPoll, SupervisorShutdown,
    SupervisorSources,
};
pub use worker::{
    CaptureWorker, CaptureWorkerReady, CaptureWorkerReport, RENDER_SOURCE_CAPACITY, RenderWorker,
    RenderWorkerReady, RenderWorkerReport,
};

pub(crate) use error::{hresult, wasapi_error};

#[cfg(test)]
mod tests {
    use super::*;
    use rlb_audio::{EndpointDirection, EndpointState};

    #[test]
    fn enumerator_reports_stable_ids_and_default_roles() {
        let enumerator = EndpointEnumerator::new().expect("enumerator");
        let catalog = enumerator.enumerate().expect("catalog");
        assert!(
            !catalog.endpoints().is_empty(),
            "this Windows session should expose at least one audio endpoint"
        );
        for endpoint in catalog.endpoints() {
            assert!(!endpoint.endpoint_id().is_empty());
            assert!(!endpoint.friendly_name().is_empty());
            assert!(
                matches!(
                    endpoint.direction(),
                    EndpointDirection::Capture | EndpointDirection::Render
                ),
                "unexpected direction {:?}",
                endpoint.direction()
            );
            if endpoint.state() == EndpointState::Active {
                if let Some(format) = endpoint.mix_format() {
                    assert!(format.sample_rate_hz > 0);
                    assert!(format.channels > 0);
                }
            }
        }
        let render_default =
            catalog.default_endpoint(EndpointDirection::Render, EndpointRole::Console);
        let capture_default =
            catalog.default_endpoint(EndpointDirection::Capture, EndpointRole::Console);
        if let Some(render) = render_default {
            assert_eq!(render.direction(), EndpointDirection::Render);
            assert!(render.default_roles().console);
        }
        if let Some(capture) = capture_default {
            assert_eq!(capture.direction(), EndpointDirection::Capture);
            assert!(capture.default_roles().console);
        }
    }
}
