#![forbid(unsafe_code)]

//! Local monotonic time for media deadlines, leases, and diagnostics.
//!
//! This crate deliberately exposes no wall-clock API. Source sample indexes
//! remain the media ordering authority; monotonic values only measure local
//! intervals and deadlines.

use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use std::error::Error;
use std::fmt;

/// Nanoseconds in one SI second.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// An instant in one clock's opaque tick domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct MonotonicInstant(u64);

impl MonotonicInstant {
    /// Creates an instant from a clock-domain tick count.
    #[must_use]
    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    /// Returns the underlying clock-domain ticks.
    #[must_use]
    pub const fn ticks(self) -> u64 {
        self.0
    }

    /// Computes a duration when `earlier` belongs to the same clock domain.
    #[must_use]
    pub const fn checked_duration_since(self, earlier: Self) -> Option<MonotonicDuration> {
        match self.0.checked_sub(earlier.0) {
            Some(ticks) => Some(MonotonicDuration::from_ticks(ticks)),
            None => None,
        }
    }

    /// Adds a clock-domain duration without wrapping.
    #[must_use]
    pub const fn checked_add(self, duration: MonotonicDuration) -> Option<Self> {
        match self.0.checked_add(duration.ticks()) {
            Some(ticks) => Some(Self(ticks)),
            None => None,
        }
    }
}

/// A nonnegative duration in one clock's tick domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct MonotonicDuration(u64);

impl MonotonicDuration {
    /// Creates a duration from clock-domain ticks.
    #[must_use]
    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    /// Returns the underlying tick count.
    #[must_use]
    pub const fn ticks(self) -> u64 {
        self.0
    }
}

/// Failure to represent a scaled time value in `u64`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeConversionOverflow;

impl fmt::Display for TimeConversionOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("scaled monotonic time exceeds u64")
    }
}

impl Error for TimeConversionOverflow {}

/// Local monotonic clock used for intervals and deadlines.
///
/// Values from distinct clock instances or hosts must never be directly
/// compared. Media ordering uses source sample indexes.
pub trait MonotonicClock: Send + Sync {
    /// Returns the current instant in this clock's tick domain.
    fn now(&self) -> MonotonicInstant;

    /// Returns this clock domain's number of ticks per second.
    fn ticks_per_second(&self) -> NonZeroU64;

    /// Returns the current local monotonic time converted with floor rounding.
    fn now_ns(&self) -> Result<u64, TimeConversionOverflow> {
        ticks_to_ns(self.now().ticks(), self.ticks_per_second())
    }
}

/// Converts clock ticks to nanoseconds with floor rounding and checked math.
pub fn ticks_to_ns(
    ticks: u64,
    ticks_per_second: NonZeroU64,
) -> Result<u64, TimeConversionOverflow> {
    scale_floor(ticks, NANOS_PER_SECOND, ticks_per_second.get())
}

/// Converts clock ticks to nanoseconds with ceiling rounding and checked math.
pub fn ticks_to_ns_ceil(
    ticks: u64,
    ticks_per_second: NonZeroU64,
) -> Result<u64, TimeConversionOverflow> {
    scale_ceil(ticks, NANOS_PER_SECOND, ticks_per_second.get())
}

/// Converts nanoseconds to clock ticks with floor rounding and checked math.
pub fn ns_to_ticks(
    nanoseconds: u64,
    ticks_per_second: NonZeroU64,
) -> Result<u64, TimeConversionOverflow> {
    scale_floor(nanoseconds, ticks_per_second.get(), NANOS_PER_SECOND)
}

/// Converts nanoseconds to clock ticks with ceiling rounding and checked math.
pub fn ns_to_ticks_ceil(
    nanoseconds: u64,
    ticks_per_second: NonZeroU64,
) -> Result<u64, TimeConversionOverflow> {
    scale_ceil(nanoseconds, ticks_per_second.get(), NANOS_PER_SECOND)
}

fn scale_floor(
    value: u64,
    numerator: u64,
    denominator: u64,
) -> Result<u64, TimeConversionOverflow> {
    let scaled = u128::from(value) * u128::from(numerator) / u128::from(denominator);
    u64::try_from(scaled).map_err(|_| TimeConversionOverflow)
}

fn scale_ceil(value: u64, numerator: u64, denominator: u64) -> Result<u64, TimeConversionOverflow> {
    if value == 0 {
        return Ok(0);
    }
    let product = u128::from(value) * u128::from(numerator);
    let scaled = product.div_ceil(u128::from(denominator));
    u64::try_from(scaled).map_err(|_| TimeConversionOverflow)
}

/// `std::time::Instant` clock with nanoseconds as its local tick unit.
#[derive(Clone, Debug)]
pub struct StdMonotonic {
    origin: Instant,
}

impl StdMonotonic {
    /// Starts a new local monotonic domain at tick zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for StdMonotonic {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for StdMonotonic {
    fn now(&self) -> MonotonicInstant {
        let elapsed = self.origin.elapsed().as_nanos();
        let ticks = u64::try_from(elapsed).unwrap_or(u64::MAX);
        MonotonicInstant::from_ticks(ticks)
    }

    fn ticks_per_second(&self) -> NonZeroU64 {
        NonZeroU64::new(NANOS_PER_SECOND).expect("one billion is nonzero")
    }
}

/// Deterministic, thread-safe monotonic clock for tests and simulations.
#[derive(Debug)]
pub struct FakeClock {
    ticks: AtomicU64,
    ticks_per_second: NonZeroU64,
}

impl FakeClock {
    /// Creates a fake clock at `initial_ticks`.
    #[must_use]
    pub const fn new(initial_ticks: u64, ticks_per_second: NonZeroU64) -> Self {
        Self {
            ticks: AtomicU64::new(initial_ticks),
            ticks_per_second,
        }
    }

    /// Sets the current tick value.
    pub fn set(&self, ticks: u64) {
        self.ticks.store(ticks, Ordering::SeqCst);
    }

    /// Advances without wrapping and returns the resulting instant.
    pub fn advance(&self, ticks: u64) -> Result<MonotonicInstant, TimeConversionOverflow> {
        self.ticks
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(ticks)
            })
            .map(|previous| MonotonicInstant::from_ticks(previous + ticks))
            .map_err(|_| TimeConversionOverflow)
    }
}

impl MonotonicClock for FakeClock {
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_ticks(self.ticks.load(Ordering::SeqCst))
    }

    fn ticks_per_second(&self) -> NonZeroU64 {
        self.ticks_per_second
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonzero(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    #[test]
    fn qpc_style_ticks_convert_without_intermediate_overflow() {
        let frequency = nonzero(10_000_000);
        assert_eq!(ticks_to_ns(12_345_678, frequency), Ok(1_234_567_800));
        assert_eq!(ns_to_ticks(1_234_567_800, frequency), Ok(12_345_678));
        assert_eq!(
            ticks_to_ns(100_000_000_000_000_000, frequency),
            Ok(10_000_000_000_000_000_000)
        );
    }

    #[test]
    fn floor_and_ceiling_rounding_are_explicit() {
        let frequency = nonzero(3);
        assert_eq!(ticks_to_ns(1, frequency), Ok(333_333_333));
        assert_eq!(ticks_to_ns_ceil(1, frequency), Ok(333_333_334));
        assert_eq!(ns_to_ticks(1, frequency), Ok(0));
        assert_eq!(ns_to_ticks_ceil(1, frequency), Ok(1));
    }

    #[test]
    fn unrepresentable_conversions_are_rejected() {
        assert_eq!(
            ticks_to_ns(u64::MAX, nonzero(1)),
            Err(TimeConversionOverflow)
        );
        assert_eq!(
            ns_to_ticks(u64::MAX, nonzero(u64::MAX)),
            Err(TimeConversionOverflow)
        );
    }

    #[test]
    fn fake_clock_controls_deadlines_deterministically() {
        let clock = FakeClock::new(100, nonzero(1_000));
        let start = clock.now();
        assert_eq!(clock.advance(25), Ok(MonotonicInstant::from_ticks(125)));
        assert_eq!(
            clock.now().checked_duration_since(start),
            Some(MonotonicDuration::from_ticks(25))
        );
        clock.set(500);
        assert_eq!(clock.now(), MonotonicInstant::from_ticks(500));
    }

    #[test]
    fn fake_clock_rejects_wraparound() {
        let clock = FakeClock::new(u64::MAX, nonzero(1));
        assert_eq!(clock.advance(1), Err(TimeConversionOverflow));
        assert_eq!(clock.now().ticks(), u64::MAX);
    }

    #[test]
    fn standard_clock_does_not_move_backwards() {
        let clock = StdMonotonic::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
        assert_eq!(clock.ticks_per_second().get(), NANOS_PER_SECOND);
    }
}
