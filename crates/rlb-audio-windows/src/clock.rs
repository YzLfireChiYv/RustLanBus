use core::num::NonZeroU64;

use rlb_audio::CaptureClockDomain;
use rlb_clock::MonotonicInstant;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

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

pub(crate) fn capture_instant(qpc_100ns: Option<u64>) -> MonotonicInstant {
    let ticks = qpc_100ns.or_else(wasapi_qpc_100ns_now).unwrap_or(0);
    MonotonicInstant::from_ticks(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
