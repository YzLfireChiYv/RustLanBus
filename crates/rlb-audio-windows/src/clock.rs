use core::num::NonZeroU64;

use rlb_audio::{CaptureClockDomain, CaptureTiming, FrameTimeQuality, FrameTimestamp};
use rlb_clock::MonotonicInstant;
use windows::Win32::Media::Audio::IAudioClock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{HRESULT, Interface};

use crate::client::WasapiCallFailure;

/// A failed preparation or observation of the optional render clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RenderClockFailure {
    /// An exact native call failure, without substituting a position.
    #[error("output clock call failed: {0:?}")]
    Call(WasapiCallFailure),
    /// The clock reported an unusable zero frequency.
    #[error("output clock reported zero ticks per second")]
    ZeroFrequency,
}

/// One output-clock position and its Windows-correlated QPC reading.
///
/// Position ticks use `ticks_per_second`, rather than the client sample rate.
/// QPC values use [`wasapi_clock_domain`]. The two owner readings bracket the
/// clock call; they do not make a separate padding query an atomic snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderClockObservation {
    /// Raw position in this stream's clock units, including a measured zero.
    pub position_ticks: u64,
    /// Nominal clock units per second returned at stream open.
    pub ticks_per_second: NonZeroU64,
    /// QPC time correlated by Windows with `position_ticks`, in 100 ns units.
    pub correlated_qpc_100ns: u64,
    /// Unmodified successful HRESULT; degraded success remains distinguishable.
    pub result_code: u32,
    /// Owner QPC just before the call, or absent if the query failed.
    pub observed_before_qpc_100ns: Option<u64>,
    /// Owner QPC just after the call, or absent if the query failed.
    pub observed_after_qpc_100ns: Option<u64>,
}

impl RenderClockObservation {
    /// Whether Windows returned a successful status other than `S_OK`.
    ///
    /// This includes `S_FALSE` and `AUDCLNT_S_POSITION_STALLED`. Such an
    /// observation alone does not establish a new output timeline.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        self.result_code != 0
    }
}

pub(crate) struct PreparedRenderClock {
    clock: IAudioClock,
    frequency: NonZeroU64,
}

impl PreparedRenderClock {
    pub(crate) fn new(clock: IAudioClock) -> Result<Self, RenderClockFailure> {
        let frequency = unsafe { clock.GetFrequency() }.map_err(|error| {
            RenderClockFailure::Call(WasapiCallFailure {
                operation: "IAudioClock::GetFrequency",
                code: error.code().0 as u32,
            })
        })?;
        let frequency = NonZeroU64::new(frequency).ok_or(RenderClockFailure::ZeroFrequency)?;
        Ok(Self { clock, frequency })
    }

    pub(crate) const fn frequency(&self) -> NonZeroU64 {
        self.frequency
    }

    pub(crate) fn observe(&self) -> Result<RenderClockObservation, RenderClockFailure> {
        let mut position = 0_u64;
        let mut qpc = 0_u64;
        let before = wasapi_qpc_100ns_now();
        // The generated GetPosition wrapper converts every successful HRESULT
        // to (), losing degraded statuses. Keep the native status via its vtable.
        let result = unsafe {
            (Interface::vtable(&self.clock).GetPosition)(
                Interface::as_raw(&self.clock),
                &mut position,
                &mut qpc,
            )
        };
        let after = wasapi_qpc_100ns_now();
        render_clock_observation(result, position, self.frequency, qpc, before, after)
    }
}

fn render_clock_observation(
    result: HRESULT,
    position_ticks: u64,
    ticks_per_second: NonZeroU64,
    correlated_qpc_100ns: u64,
    observed_before_qpc_100ns: Option<u64>,
    observed_after_qpc_100ns: Option<u64>,
) -> Result<RenderClockObservation, RenderClockFailure> {
    if result.is_err() {
        return Err(RenderClockFailure::Call(WasapiCallFailure {
            operation: "IAudioClock::GetPosition",
            code: result.0 as u32,
        }));
    }
    Ok(RenderClockObservation {
        position_ticks,
        ticks_per_second,
        correlated_qpc_100ns,
        result_code: result.0 as u32,
        observed_before_qpc_100ns,
        observed_after_qpc_100ns,
    })
}

/// WASAPI `GetBuffer` QPC timestamps are already scaled to 100 ns units.
pub const WASAPI_QPC_TICKS_PER_SECOND: u64 = 10_000_000;

/// Shared local domain for every WASAPI stream in this process.
#[must_use]
pub fn wasapi_clock_domain() -> CaptureClockDomain {
    CaptureClockDomain::new(
        NonZeroU64::new(1).expect("1 is nonzero"),
        NonZeroU64::new(WASAPI_QPC_TICKS_PER_SECOND).expect("10 MHz is nonzero"),
    )
}

/// Returns the current local QPC time scaled to WASAPI's 100 ns tick domain.
///
/// Compare this only with capture timestamps whose clock domain is
/// [`wasapi_clock_domain`]. These absolute ticks have a different origin and
/// scale from Rust `Instant` elapsed durations and from clocks on other hosts.
#[must_use]
pub fn wasapi_qpc_100ns_now() -> Option<u64> {
    let mut counter = 0_i64;
    let mut frequency = 0_i64;
    unsafe { QueryPerformanceCounter(&mut counter) }.ok()?;
    unsafe { QueryPerformanceFrequency(&mut frequency) }.ok()?;
    if counter < 0 || frequency <= 0 {
        return None;
    }
    let ticks = (i128::from(counter) * 10_000_000) / i128::from(frequency);
    u64::try_from(ticks).ok()
}

/// Separates a WASAPI packet's device QPC pair from the owner's own QPC reading.
///
/// A packet without `TIMESTAMP_ERROR` keeps `device_qpc_100ns` as a reported
/// first-frame instant, including a measured zero. `TIMESTAMP_ERROR` leaves the
/// first-frame anchor absent and does not substitute the owner reading.
/// A failed owner query stays absent rather than becoming tick zero.
#[must_use]
pub(crate) fn observe_wasapi_packet_timing(
    timestamp_error: bool,
    device_qpc_100ns: u64,
    owner_qpc_100ns: Option<u64>,
) -> CaptureTiming {
    let first_frame = if timestamp_error {
        None
    } else {
        Some(FrameTimestamp {
            at: MonotonicInstant::from_ticks(device_qpc_100ns),
            quality: FrameTimeQuality::ReportedCorrelated,
        })
    };
    CaptureTiming {
        first_frame,
        observed_on_owner: owner_qpc_100ns.map(MonotonicInstant::from_ticks),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_clock_preserves_success_quality_and_absent_failure() {
        let frequency = NonZeroU64::new(192_000).unwrap();
        for code in [0, 1, 0x0889_0003] {
            let observation =
                render_clock_observation(HRESULT(code), 0, frequency, 0, None, Some(10)).unwrap();
            assert_eq!(observation.position_ticks, 0);
            assert_eq!(observation.correlated_qpc_100ns, 0);
            assert_eq!(observation.result_code, code as u32);
            assert_eq!(observation.is_degraded(), code != 0);
            assert_eq!(observation.observed_before_qpc_100ns, None);
        }
        assert_eq!(
            render_clock_observation(
                HRESULT(0x8889_0004_u32 as i32),
                0,
                frequency,
                0,
                Some(10),
                Some(20),
            ),
            Err(RenderClockFailure::Call(WasapiCallFailure {
                operation: "IAudioClock::GetPosition",
                code: 0x8889_0004,
            }))
        );
    }

    #[test]
    fn qpc_domain_is_100ns_ticks() {
        assert_eq!(
            wasapi_clock_domain().ticks_per_second().get(),
            WASAPI_QPC_TICKS_PER_SECOND
        );
        let first = wasapi_qpc_100ns_now().expect("qpc");
        let second = wasapi_qpc_100ns_now().expect("qpc");
        assert!(second >= first);
    }

    #[test]
    fn packet_timing_keeps_measured_values_and_absent_failures() {
        let zero = observe_wasapi_packet_timing(false, 0, Some(25));
        assert_eq!(zero.first_frame.unwrap().at.ticks(), 0);
        assert_eq!(
            zero.first_frame.unwrap().quality,
            FrameTimeQuality::ReportedCorrelated
        );
        assert_eq!(zero.observed_on_owner.unwrap().ticks(), 25);

        let nonzero = observe_wasapi_packet_timing(false, 80, None);
        assert_eq!(nonzero.first_frame.unwrap().at.ticks(), 80);
        assert!(nonzero.observed_on_owner.is_none());

        let timestamp_error = observe_wasapi_packet_timing(true, 80, Some(90));
        assert!(timestamp_error.first_frame.is_none());
        assert_eq!(timestamp_error.observed_on_owner.unwrap().ticks(), 90);

        let no_owner = observe_wasapi_packet_timing(true, 0, None);
        assert!(no_owner.first_frame.is_none());
        assert!(no_owner.observed_on_owner.is_none());
        assert_eq!(no_owner.compatibility_capture_time().ticks(), 0);
    }
}
