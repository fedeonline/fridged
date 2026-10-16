//! Compressor protections, enforced between the control logic and the relay.
//!
//! - `min_off_time`: after stopping, wait before restarting. Restarting against
//!   residual system pressure damages the compressor.
//! - `min_on_time`: once started, keep running a minimum time (short-cycling).
//! - `max_continuous_on`: stop after running this long without a break.
//!
//! The control logic only *requests* a state; [`ProtectedRelay`] decides what
//! the relay actually does. Time comes in as a parameter, so this is
//! deterministic and tested without waiting.

use std::time::{Duration, Instant};

use super::{Relay, RelayError, RelayState};
use crate::config::RelayConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub min_off_time: Duration,
    pub min_on_time: Duration,
    pub max_continuous_on: Duration,
}

impl From<&RelayConfig> for Limits {
    fn from(c: &RelayConfig) -> Self {
        Self {
            min_off_time: c.min_off_time,
            min_on_time: c.min_on_time,
            max_continuous_on: c.max_continuous_on,
        }
    }
}

/// Which protection refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    MinOffTime,
    MinOnTime,
}

/// What happened to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The relay already was in the requested state.
    Unchanged,
    /// The relay switched to the requested state.
    Switched(RelayState),
    /// A protection kept the current state; it can change from `until` on.
    Held { hold: Hold, until: Instant },
    /// The compressor ran for `max_continuous_on` and was stopped, whatever
    /// was requested.
    ForcedOff,
}

pub struct ProtectedRelay<R> {
    relay: R,
    limits: Limits,
    state: RelayState,
    /// When the current state began.
    since: Instant,
}

impl<R: Relay> ProtectedRelay<R> {
    /// Forces the relay OFF and starts a full `min_off_time` from `now`.
    ///
    /// Nothing says how long ago the compressor last ran (the daemon may have
    /// just crashed with it ON), so the safe assumption is that it just stopped.
    pub fn new(mut relay: R, limits: Limits, now: Instant) -> Result<Self, RelayError> {
        relay.set(RelayState::Off)?;
        Ok(Self {
            relay,
            limits,
            state: RelayState::Off,
            since: now,
        })
    }

    pub fn state(&self) -> RelayState {
        self.state
    }

    /// When the current state began.
    pub fn since(&self) -> Instant {
        self.since
    }

    /// Asks for `wanted`; the protections decide what actually happens.
    ///
    /// Call it on every control tick, even when nothing changes: that is also
    /// when `max_continuous_on` is checked, and the output level is re-asserted.
    pub fn request(&mut self, wanted: RelayState, now: Instant) -> Result<Outcome, RelayError> {
        let elapsed = now.saturating_duration_since(self.since);

        let outcome = match (self.state, wanted) {
            (RelayState::On, _) if elapsed >= self.limits.max_continuous_on => {
                self.switch(RelayState::Off, now)?;
                return Ok(Outcome::ForcedOff);
            }
            (current, wanted) if current == wanted => Outcome::Unchanged,
            (RelayState::Off, RelayState::On) if elapsed < self.limits.min_off_time => {
                Outcome::Held {
                    hold: Hold::MinOffTime,
                    until: self.since + self.limits.min_off_time,
                }
            }
            (RelayState::On, RelayState::Off) if elapsed < self.limits.min_on_time => {
                Outcome::Held {
                    hold: Hold::MinOnTime,
                    until: self.since + self.limits.min_on_time,
                }
            }
            (_, wanted) => {
                self.switch(wanted, now)?;
                return Ok(Outcome::Switched(wanted));
            }
        };
        // Nothing changed: drive the current level again anyway, in case
        // something else touched the pin.
        self.relay.set(self.state)?;
        Ok(outcome)
    }

    /// Turns the compressor OFF immediately, ignoring `min_on_time`: for a
    /// clean exit, after which nothing would be left to turn it off.
    pub fn shutdown(&mut self, now: Instant) -> Result<(), RelayError> {
        self.switch(RelayState::Off, now)
    }

    pub fn into_inner(self) -> R {
        self.relay
    }

    fn switch(&mut self, state: RelayState, now: Instant) -> Result<(), RelayError> {
        // Only record the change once the hardware accepted it.
        self.relay.set(state)?;
        if state != self.state {
            self.state = state;
            self.since = now;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::fake::FakeRelay;
    use RelayState::{Off, On};

    const MIN_OFF: u64 = 300;
    const MIN_ON: u64 = 120;
    const MAX_ON: u64 = 7200;

    fn limits() -> Limits {
        Limits {
            min_off_time: Duration::from_secs(MIN_OFF),
            min_on_time: Duration::from_secs(MIN_ON),
            max_continuous_on: Duration::from_secs(MAX_ON),
        }
    }

    /// A relay created at t0, and a function giving t0 + `s` seconds.
    fn setup() -> (ProtectedRelay<FakeRelay>, impl Fn(u64) -> Instant) {
        let t0 = Instant::now();
        let relay = ProtectedRelay::new(FakeRelay::default(), limits(), t0).unwrap();
        (relay, move |s| t0 + Duration::from_secs(s))
    }

    /// Turned ON at t(MIN_OFF), the first moment it is allowed.
    fn running() -> (ProtectedRelay<FakeRelay>, impl Fn(u64) -> Instant) {
        let (mut r, t) = setup();
        assert_eq!(r.request(On, t(MIN_OFF)).unwrap(), Outcome::Switched(On));
        (r, t)
    }

    #[test]
    fn startup_forces_off() {
        let (r, _) = setup();
        assert_eq!(r.state(), Off);
        assert_eq!(r.into_inner().history, [Off]);
    }

    #[test]
    fn startup_failure_is_reported() {
        let relay = FakeRelay {
            broken: true,
            ..FakeRelay::default()
        };
        assert!(ProtectedRelay::new(relay, limits(), Instant::now()).is_err());
    }

    #[test]
    fn min_off_time_applies_from_startup() {
        let (mut r, t) = setup();
        assert_eq!(
            r.request(On, t(0)).unwrap(),
            Outcome::Held {
                hold: Hold::MinOffTime,
                until: t(MIN_OFF)
            }
        );
        assert!(matches!(
            r.request(On, t(MIN_OFF - 1)).unwrap(),
            Outcome::Held { .. }
        ));
        assert_eq!(r.request(On, t(MIN_OFF)).unwrap(), Outcome::Switched(On));
        assert_eq!(r.state(), On);
    }

    #[test]
    fn min_on_time() {
        let (mut r, t) = running();
        let on_at = MIN_OFF;
        assert_eq!(
            r.request(Off, t(on_at + 1)).unwrap(),
            Outcome::Held {
                hold: Hold::MinOnTime,
                until: t(on_at + MIN_ON)
            }
        );
        assert!(matches!(
            r.request(Off, t(on_at + MIN_ON - 1)).unwrap(),
            Outcome::Held { .. }
        ));
        assert_eq!(
            r.request(Off, t(on_at + MIN_ON)).unwrap(),
            Outcome::Switched(Off)
        );
    }

    #[test]
    fn min_off_time_applies_after_every_stop() {
        let (mut r, t) = running();
        let off_at = MIN_OFF + MIN_ON;
        r.request(Off, t(off_at)).unwrap();
        assert!(matches!(
            r.request(On, t(off_at + MIN_OFF - 1)).unwrap(),
            Outcome::Held {
                hold: Hold::MinOffTime,
                ..
            }
        ));
        assert_eq!(
            r.request(On, t(off_at + MIN_OFF)).unwrap(),
            Outcome::Switched(On)
        );
    }

    #[test]
    fn max_continuous_on_forces_off_then_min_off_applies() {
        let (mut r, t) = running();
        let on_at = MIN_OFF;
        assert_eq!(
            r.request(On, t(on_at + MAX_ON - 1)).unwrap(),
            Outcome::Unchanged
        );
        assert_eq!(
            r.request(On, t(on_at + MAX_ON)).unwrap(),
            Outcome::ForcedOff
        );
        assert_eq!(r.state(), Off);
        // Still wanted ON, but the stop counts as a stop.
        let stop = on_at + MAX_ON;
        assert!(matches!(
            r.request(On, t(stop + 1)).unwrap(),
            Outcome::Held {
                hold: Hold::MinOffTime,
                ..
            }
        ));
        assert_eq!(
            r.request(On, t(stop + MIN_OFF)).unwrap(),
            Outcome::Switched(On)
        );
    }

    #[test]
    fn max_continuous_on_wins_over_min_on() {
        // A pathological config where min_on exceeds max_on: the stop wins.
        let t0 = Instant::now();
        let t = |s| t0 + Duration::from_secs(s);
        let lim = Limits {
            min_off_time: Duration::ZERO,
            min_on_time: Duration::from_secs(100),
            max_continuous_on: Duration::from_secs(50),
        };
        let mut r = ProtectedRelay::new(FakeRelay::default(), lim, t0).unwrap();
        r.request(On, t(0)).unwrap();
        assert_eq!(r.request(Off, t(50)).unwrap(), Outcome::ForcedOff);
    }

    #[test]
    fn repeated_requests_reassert_without_transitions() {
        let (mut r, t) = running();
        for s in 0..5 {
            assert_eq!(r.request(On, t(MIN_OFF + s)).unwrap(), Outcome::Unchanged);
        }
        let relay = r.into_inner();
        assert_eq!(relay.history.len(), 7); // startup OFF, switch ON, 5 re-asserts
        assert_eq!(relay.transitions(), 1);
    }

    #[test]
    fn a_held_request_does_not_restart_the_timer() {
        let (mut r, t) = setup();
        r.request(On, t(100)).unwrap();
        r.request(On, t(200)).unwrap();
        // Still measured from startup, not from the refused requests.
        assert_eq!(r.request(On, t(MIN_OFF)).unwrap(), Outcome::Switched(On));
    }

    #[test]
    fn failed_switch_keeps_the_previous_state() {
        let (mut r, t) = setup();
        r.relay.broken = true;
        assert!(r.request(On, t(MIN_OFF)).is_err());
        assert_eq!(r.state(), Off);
        r.relay.broken = false;
        assert_eq!(
            r.request(On, t(MIN_OFF + 1)).unwrap(),
            Outcome::Switched(On)
        );
    }

    #[test]
    fn shutdown_ignores_min_on_time() {
        let (mut r, t) = running();
        r.shutdown(t(MIN_OFF + 1)).unwrap();
        assert_eq!(r.state(), Off);
        assert_eq!(r.into_inner().state(), Some(Off));
    }

    #[test]
    fn clock_going_backwards_does_not_bypass_protections() {
        // Instant is monotonic, but be defensive: an earlier `now` must count
        // as no time elapsed, never as a huge one.
        let (mut r, t) = setup();
        let before = t(0) - Duration::from_secs(1);
        assert!(matches!(
            r.request(On, before).unwrap(),
            Outcome::Held { .. }
        ));
    }
}
