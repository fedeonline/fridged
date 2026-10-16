//! Monotonic time source for the control loop.
//!
//! The control logic itself never reads a clock: it is handed an `Instant`.
//! The loop gets that instant from a [`Clock`], so tests can drive time.

#[cfg(test)]
use std::cell::Cell;
use std::rc::Rc;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

pub trait Clock {
    fn now(&self) -> Instant;
}

impl<C: Clock + ?Sized> Clock for Rc<C> {
    fn now(&self) -> Instant {
        (**self).now()
    }
}

/// The real monotonic clock. Never `SystemTime`: wall-clock jumps (NTP, a Pi
/// without RTC booting in 1970) must not touch the compressor timers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that only moves when told to.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FakeClock {
    start: Instant,
    elapsed: Cell<Duration>,
}

#[cfg(test)]
impl FakeClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
        }
    }

    pub fn advance(&self, d: Duration) {
        self.elapsed.set(self.elapsed.get() + d);
    }

    /// Time since the clock was created.
    pub fn elapsed(&self) -> Duration {
        self.elapsed.get()
    }
}

#[cfg(test)]
impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + self.elapsed.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_moves_only_when_advanced() {
        let clock = FakeClock::new();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0);
        clock.advance(Duration::from_secs(15));
        assert_eq!(clock.now() - t0, Duration::from_secs(15));
        assert_eq!(clock.elapsed(), Duration::from_secs(15));
    }

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock;
        let a = c.now();
        assert!(c.now() >= a);
    }
}
