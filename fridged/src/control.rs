//! Control logic: from a temperature reading and an instant to the relay state
//! the controller wants.
//!
//! Pure on purpose: no hardware, no clock, no I/O. The compressor protections
//! are not here either: they live in the relay module and may delay or
//! override what this asks for.

use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{ControlConfig, check_band};
use crate::relay::RelayState;
use crate::sensor::Celsius;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub target_min: f64,
    pub target_max: f64,
    pub sensor_timeout: Duration,
    pub safe_on: Duration,
    pub safe_off: Duration,
    pub overrun_after_stops: u32,
}

impl From<&ControlConfig> for Params {
    fn from(c: &ControlConfig) -> Self {
        Self {
            target_min: c.target_min,
            target_max: c.target_max,
            sensor_timeout: c.sensor_timeout,
            safe_on: c.safe_on,
            safe_off: c.safe_off,
            overrun_after_stops: c.overrun_after_stops,
        }
    }
}

/// The hysteresis band, in °C. Can change while running (settings in
/// Firestore); always validated before it reaches the controller.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub min: f64,
    pub max: f64,
}

impl Band {
    /// The same rules as `control.target_min`/`target_max` in the config.
    pub fn new(min: f64, max: f64) -> Result<Self, String> {
        let problems = check_band(min, max);
        if problems.is_empty() {
            Ok(Self { min, max })
        } else {
            Err(problems.join("; "))
        }
    }
}

impl fmt::Display for Band {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}–{:.1} °C", self.min, self.max)
    }
}

/// Operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    /// Hysteresis on the measured temperature.
    Normal,
    /// No valid reading for `sensor_timeout`: blind duty cycle. `phase` is the
    /// current half of the cycle, started at `since`.
    Safe { phase: RelayState, since: Instant },
}

/// Why the controller wants what it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    /// Above `target_max`: cool.
    AboveMax,
    /// At or below `target_min`: stop.
    AtOrBelowMin,
    /// Inside the band: keep doing what we were doing.
    InBand,
    /// Reading missing, but not for long enough to give up on the sensor:
    /// keep doing what we were doing.
    Stale,
    /// SAFE mode, ON half of the duty cycle.
    SafeOn,
    /// SAFE mode, OFF half of the duty cycle.
    SafeOff,
}

/// What the controller asks of the relay on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayCommand {
    pub wanted: RelayState,
    pub reason: Reason,
}

/// The control state machine. Feed it one reading per tick.
///
/// - NORMAL, cooling hysteresis: ON above `target_max`, OFF at or below
///   `target_min`, unchanged in between. Exactly `target_max` is not "above".
/// - SAFE: entered after `sensor_timeout` without a valid reading (counted from
///   creation if none ever arrived). Alternates `safe_off` and `safe_on`,
///   starting with OFF. Left on the first valid reading.
/// - Overrun fault: a forced stop after `max_continuous_on` is just a rest,
///   and cooling resumes once `min_off_time` has passed. `overrun_after_stops`
///   of them in a row raise the fault; reaching `target_min` clears it and
///   restarts the count, since it proves the compressor can cool.
#[derive(Debug, Clone)]
pub struct Controller {
    params: Params,
    state: ControlState,
    wanted: RelayState,
    last_valid: Instant,
    last_temperature: Option<Celsius>,
    /// Forced stops since the temperature last reached `target_min`.
    forced_stops: u32,
}

impl Controller {
    pub fn new(params: Params, now: Instant) -> Self {
        Self {
            params,
            state: ControlState::Normal,
            wanted: RelayState::Off,
            last_valid: now,
            last_temperature: None,
            forced_stops: 0,
        }
    }

    /// One control step. `reading` is `None` when the sensor failed.
    pub fn tick(&mut self, reading: Option<Celsius>, now: Instant) -> RelayCommand {
        let reason = match reading {
            Some(t) => self.on_reading(t, now),
            None => self.on_missing_reading(now),
        };
        RelayCommand {
            wanted: self.wanted,
            reason,
        }
    }

    /// Tells the controller that the relay layer stopped the compressor after
    /// `max_continuous_on`. Returns how many such stops in a row there have
    /// been without reaching `target_min`.
    pub fn report_forced_off(&mut self) -> u32 {
        self.forced_stops = self.forced_stops.saturating_add(1);
        self.forced_stops
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn band(&self) -> Band {
        Band {
            min: self.params.target_min,
            max: self.params.target_max,
        }
    }

    /// Moves the hysteresis band. Takes effect on the next reading; the
    /// current decision stands until then, so a band change never switches
    /// the relay on its own.
    pub fn set_band(&mut self, band: Band) {
        self.params.target_min = band.min;
        self.params.target_max = band.max;
    }

    pub fn state(&self) -> ControlState {
        self.state
    }

    pub fn overrun_fault(&self) -> bool {
        self.forced_stops >= self.params.overrun_after_stops
    }

    pub fn last_temperature(&self) -> Option<Celsius> {
        self.last_temperature
    }

    fn on_reading(&mut self, t: Celsius, now: Instant) -> Reason {
        self.last_valid = now;
        self.last_temperature = Some(t);
        self.state = ControlState::Normal;

        if t.get() > self.params.target_max {
            self.wanted = RelayState::On;
            Reason::AboveMax
        } else if t.get() <= self.params.target_min {
            self.wanted = RelayState::Off;
            self.forced_stops = 0;
            Reason::AtOrBelowMin
        } else {
            Reason::InBand
        }
    }

    fn on_missing_reading(&mut self, now: Instant) -> Reason {
        let (phase, since) = match self.state {
            ControlState::Normal => {
                if now.saturating_duration_since(self.last_valid) < self.params.sensor_timeout {
                    return Reason::Stale;
                }
                (RelayState::Off, now)
            }
            ControlState::Safe { phase, since } => {
                let length = match phase {
                    RelayState::On => self.params.safe_on,
                    RelayState::Off => self.params.safe_off,
                };
                if now.saturating_duration_since(since) < length {
                    (phase, since)
                } else {
                    let next = match phase {
                        RelayState::On => RelayState::Off,
                        RelayState::Off => RelayState::On,
                    };
                    (next, now)
                }
            }
        };
        self.state = ControlState::Safe { phase, since };
        self.wanted = phase;
        match phase {
            RelayState::On => Reason::SafeOn,
            RelayState::Off => Reason::SafeOff,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::fake::FakeRelay;
    use crate::relay::{Limits, Outcome, ProtectedRelay};
    use RelayState::{Off, On};

    const TIMEOUT: u64 = 60;
    const SAFE_ON: u64 = 600;
    const SAFE_OFF: u64 = 2400;

    fn params() -> Params {
        Params {
            target_min: 11.0,
            target_max: 13.0,
            sensor_timeout: Duration::from_secs(TIMEOUT),
            safe_on: Duration::from_secs(SAFE_ON),
            safe_off: Duration::from_secs(SAFE_OFF),
            overrun_after_stops: 3,
        }
    }

    /// A controller created at t0, and t(s) = t0 + s seconds.
    fn setup() -> (Controller, impl Fn(u64) -> Instant) {
        let t0 = Instant::now();
        (Controller::new(params(), t0), move |s| {
            t0 + Duration::from_secs(s)
        })
    }

    fn wanted(c: &mut Controller, temp: f64, at: Instant) -> RelayState {
        c.tick(Some(Celsius(temp)), at).wanted
    }

    // --- hysteresis -------------------------------------------------------

    #[test]
    fn starts_off_in_band() {
        let (mut c, t) = setup();
        let cmd = c.tick(Some(Celsius(12.0)), t(0));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: Off,
                reason: Reason::InBand
            }
        );
        assert_eq!(c.state(), ControlState::Normal);
    }

    #[test]
    fn hysteresis_rising() {
        let (mut c, t) = setup();
        // Warming up through the band: stays OFF until above max.
        for (i, temp) in [10.5, 11.0, 12.0, 12.9, 13.0].into_iter().enumerate() {
            assert_eq!(wanted(&mut c, temp, t(i as u64 * 15)), Off, "{temp}");
        }
        let cmd = c.tick(Some(Celsius(13.0625)), t(90));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: On,
                reason: Reason::AboveMax
            }
        );
    }

    #[test]
    fn hysteresis_falling() {
        let (mut c, t) = setup();
        assert_eq!(wanted(&mut c, 14.0, t(0)), On);
        // Cooling down through the band: stays ON until min.
        for (i, temp) in [13.0, 12.0, 11.5, 11.0625].into_iter().enumerate() {
            let cmd = c.tick(Some(Celsius(temp)), t(15 + i as u64 * 15));
            assert_eq!(
                cmd,
                RelayCommand {
                    wanted: On,
                    reason: Reason::InBand
                },
                "{temp}"
            );
        }
        let cmd = c.tick(Some(Celsius(11.0)), t(90));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: Off,
                reason: Reason::AtOrBelowMin
            }
        );
    }

    #[test]
    fn exact_thresholds() {
        let (mut c, t) = setup();
        // Exactly max is not above it.
        assert_eq!(wanted(&mut c, 13.0, t(0)), Off);
        assert_eq!(wanted(&mut c, 13.0001, t(15)), On);
        // Exactly min counts as reached.
        assert_eq!(wanted(&mut c, 11.0001, t(30)), On);
        assert_eq!(wanted(&mut c, 11.0, t(45)), Off);
    }

    #[test]
    fn dead_band_keeps_either_state() {
        let (mut c, t) = setup();
        for s in 0..10 {
            assert_eq!(wanted(&mut c, 12.0, t(s * 15)), Off);
        }
        assert_eq!(wanted(&mut c, 13.5, t(150)), On);
        for s in 11..20 {
            assert_eq!(wanted(&mut c, 12.0, t(s * 15)), On);
        }
    }

    #[test]
    fn far_out_of_band() {
        let (mut c, t) = setup();
        assert_eq!(wanted(&mut c, 30.0, t(0)), On);
        assert_eq!(wanted(&mut c, -5.0, t(15)), Off);
    }

    // --- SAFE mode --------------------------------------------------------

    #[test]
    fn short_sensor_gap_keeps_the_last_decision() {
        let (mut c, t) = setup();
        assert_eq!(wanted(&mut c, 14.0, t(0)), On);
        let cmd = c.tick(None, t(TIMEOUT - 1));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: On,
                reason: Reason::Stale
            }
        );
        assert_eq!(c.state(), ControlState::Normal);
    }

    #[test]
    fn enters_safe_exactly_at_the_timeout_starting_off() {
        let (mut c, t) = setup();
        assert_eq!(wanted(&mut c, 14.0, t(0)), On);
        let cmd = c.tick(None, t(TIMEOUT));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: Off,
                reason: Reason::SafeOff
            }
        );
        assert_eq!(
            c.state(),
            ControlState::Safe {
                phase: Off,
                since: t(TIMEOUT)
            }
        );
    }

    #[test]
    fn timeout_counts_from_the_last_valid_reading() {
        let (mut c, t) = setup();
        c.tick(Some(Celsius(12.0)), t(100));
        assert_eq!(c.tick(None, t(100 + TIMEOUT - 1)).reason, Reason::Stale);
        assert_eq!(c.tick(None, t(100 + TIMEOUT)).reason, Reason::SafeOff);
    }

    #[test]
    fn sensor_missing_from_startup_leads_to_safe() {
        let (mut c, t) = setup();
        assert_eq!(c.tick(None, t(15)).reason, Reason::Stale);
        assert_eq!(c.tick(None, t(TIMEOUT)).reason, Reason::SafeOff);
        assert_eq!(c.last_temperature(), None);
    }

    #[test]
    fn safe_duty_cycle() {
        let (mut c, t) = setup();
        let enter = TIMEOUT;
        c.tick(None, t(enter));
        // OFF phase lasts safe_off...
        assert_eq!(c.tick(None, t(enter + SAFE_OFF - 1)).wanted, Off);
        let on_at = enter + SAFE_OFF;
        assert_eq!(
            c.tick(None, t(on_at)),
            RelayCommand {
                wanted: On,
                reason: Reason::SafeOn
            }
        );
        // ...then ON for safe_on...
        assert_eq!(c.tick(None, t(on_at + SAFE_ON - 1)).wanted, On);
        // ...then OFF again.
        assert_eq!(c.tick(None, t(on_at + SAFE_ON)).wanted, Off);
    }

    #[test]
    fn leaves_safe_on_the_first_valid_reading() {
        let (mut c, t) = setup();
        c.tick(None, t(TIMEOUT));
        let on_at = TIMEOUT + SAFE_OFF;
        assert_eq!(c.tick(None, t(on_at)).wanted, On);
        // In band: the hysteresis keeps what SAFE was doing.
        let cmd = c.tick(Some(Celsius(12.0)), t(on_at + 15));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: On,
                reason: Reason::InBand
            }
        );
        assert_eq!(c.state(), ControlState::Normal);
        // And the timeout starts over from that reading.
        assert_eq!(
            c.tick(None, t(on_at + 15 + TIMEOUT - 1)).reason,
            Reason::Stale
        );
    }

    #[test]
    fn a_single_reading_between_failures_resets_safe() {
        let (mut c, t) = setup();
        c.tick(None, t(TIMEOUT));
        c.tick(Some(Celsius(12.0)), t(TIMEOUT + 15));
        assert_eq!(c.tick(None, t(TIMEOUT + 30)).reason, Reason::Stale);
    }

    // --- overrun ----------------------------------------------------------

    #[test]
    fn forced_stops_are_rests_until_too_many_in_a_row() {
        let (mut c, t) = setup();
        c.tick(Some(Celsius(14.0)), t(0));
        assert_eq!(c.report_forced_off(), 1);
        assert_eq!(c.report_forced_off(), 2);
        assert!(!c.overrun_fault());
        // Still too warm: keeps asking for cold through the rests.
        assert_eq!(wanted(&mut c, 12.5, t(15)), On);
        assert_eq!(c.report_forced_off(), 3);
        assert!(c.overrun_fault());
        // Reaching min proves cooling works again, and restarts the count.
        c.tick(Some(Celsius(11.0)), t(30));
        assert!(!c.overrun_fault());
        assert_eq!(c.report_forced_off(), 1);
    }

    #[test]
    fn reaching_min_between_rests_restarts_the_count() {
        // Last night: two rests, then the band reached. No fault.
        let (mut c, t) = setup();
        c.tick(Some(Celsius(14.0)), t(0));
        c.report_forced_off();
        c.report_forced_off();
        c.tick(Some(Celsius(10.9)), t(15));
        c.tick(Some(Celsius(14.0)), t(30));
        c.report_forced_off();
        assert!(!c.overrun_fault());
    }

    // --- band changes -----------------------------------------------------

    #[test]
    fn band_validation_matches_the_config_rules() {
        assert!(Band::new(7.0, 10.0).is_ok());
        assert!(Band::new(10.0, 7.0).unwrap_err().contains("must exceed"));
        assert!(Band::new(7.0, 7.4).unwrap_err().contains("at least 0.5"));
        assert!(Band::new(-3.0, 4.0).unwrap_err().contains("outside"));
        assert!(Band::new(f64::NAN, 4.0).unwrap_err().contains("finite"));
        assert_eq!(Band::new(7.0, 10.5).unwrap().to_string(), "7.0–10.5 °C");
    }

    #[test]
    fn lowering_the_band_starts_cooling_at_the_next_reading() {
        let (mut c, t) = setup();
        // 12 °C is inside 11–13: OFF.
        assert_eq!(wanted(&mut c, 12.0, t(0)), Off);
        c.set_band(Band::new(8.0, 11.0).unwrap());
        assert_eq!(
            c.band(),
            Band {
                min: 8.0,
                max: 11.0
            }
        );
        // Now above the new max.
        let cmd = c.tick(Some(Celsius(12.0)), t(15));
        assert_eq!(
            cmd,
            RelayCommand {
                wanted: On,
                reason: Reason::AboveMax
            }
        );
    }

    #[test]
    fn hysteresis_continues_across_a_band_change() {
        let (mut c, t) = setup();
        assert_eq!(wanted(&mut c, 14.0, t(0)), On);
        // Cooling, still inside the new band: keeps cooling down to its min.
        c.set_band(Band::new(10.0, 13.0).unwrap());
        assert_eq!(wanted(&mut c, 12.0, t(15)), On);
        assert_eq!(wanted(&mut c, 10.0, t(30)), Off);
    }

    #[test]
    fn raising_the_band_stops_cooling_at_the_next_reading() {
        let (mut c, t) = setup();
        assert_eq!(wanted(&mut c, 14.0, t(0)), On);
        c.set_band(Band::new(14.5, 16.0).unwrap());
        assert_eq!(wanted(&mut c, 14.0, t(15)), Off);
    }

    // --- with the real protections --------------------------------------

    /// Controller and ProtectedRelay together, as the loop wires them.
    #[test]
    fn protections_shape_what_the_controller_asks() {
        let (mut c, t) = setup();
        let limits = Limits {
            min_off_time: Duration::from_secs(300),
            min_on_time: Duration::from_secs(120),
            max_continuous_on: Duration::from_secs(7200),
        };
        let mut relay = ProtectedRelay::new(FakeRelay::default(), limits, t(0)).unwrap();

        // Warm at startup: wanted ON at once, but min_off_time holds it.
        let cmd = c.tick(Some(Celsius(14.0)), t(0));
        assert!(matches!(
            relay.request(cmd.wanted, t(0)).unwrap(),
            Outcome::Held { .. }
        ));
        let cmd = c.tick(Some(Celsius(14.0)), t(300));
        assert_eq!(
            relay.request(cmd.wanted, t(300)).unwrap(),
            Outcome::Switched(On)
        );

        // Reaches min quickly: wanted OFF, but min_on_time holds it.
        let cmd = c.tick(Some(Celsius(11.0)), t(330));
        assert!(matches!(
            relay.request(cmd.wanted, t(330)).unwrap(),
            Outcome::Held { .. }
        ));
        let cmd = c.tick(Some(Celsius(10.8)), t(420));
        assert_eq!(
            relay.request(cmd.wanted, t(420)).unwrap(),
            Outcome::Switched(Off)
        );

        // Warm again right away: min_off_time from the stop.
        let cmd = c.tick(Some(Celsius(13.5)), t(435));
        assert!(matches!(
            relay.request(cmd.wanted, t(435)).unwrap(),
            Outcome::Held { .. }
        ));
        assert_eq!(relay.state(), Off);
        assert_eq!(relay.into_inner().transitions(), 2);
    }

    #[test]
    fn one_forced_stop_is_a_rest_not_a_fault() {
        let (mut c, t) = setup();
        let limits = Limits {
            min_off_time: Duration::from_secs(300),
            min_on_time: Duration::from_secs(120),
            max_continuous_on: Duration::from_secs(7200),
        };
        let mut relay = ProtectedRelay::new(FakeRelay::default(), limits, t(0)).unwrap();
        let mut s = 300;
        relay
            .request(c.tick(Some(Celsius(14.0)), t(s)).wanted, t(s))
            .unwrap();
        // The door is open: two hours ON and still warm.
        let mut forced = false;
        while s < 300 + 7200 {
            s += 15;
            let cmd = c.tick(Some(Celsius(13.5)), t(s));
            if relay.request(cmd.wanted, t(s)).unwrap() == Outcome::ForcedOff {
                c.report_forced_off();
                forced = true;
                break;
            }
        }
        assert!(forced);
        assert_eq!(s, 300 + 7200);
        assert!(!c.overrun_fault());
        assert_eq!(relay.state(), Off);
        // Still warm: after min_off_time cooling resumes.
        let s = s + 300;
        let cmd = c.tick(Some(Celsius(13.4)), t(s));
        assert_eq!(
            relay.request(cmd.wanted, t(s)).unwrap(),
            Outcome::Switched(On)
        );
    }
}
