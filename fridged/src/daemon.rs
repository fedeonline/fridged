//! The control loop: read, decide, protect, switch, once per tick.
//!
//! [`Daemon::step`] is one tick and takes its time from a [`Clock`], so a whole
//! day of operation can be simulated in a test. [`Daemon::run`] repeats it in
//! real time until a shutdown is requested, then turns the relay OFF.

use std::time::Duration;

use crate::channel::Subscriber;
use crate::clock::Clock;
use crate::control::{Band, ControlState, Controller, Params, RelayCommand};
use crate::relay::{Limits, Outcome, ProtectedRelay, Relay, RelayError, RelayState};
use crate::sensor::{Celsius, SensorError, TempSensor};
use crate::signals::Shutdown;

/// Opens (or reopens) the sensor. Called again on every tick while it fails,
/// so a sensor missing at startup is picked up when it comes back.
pub type SensorOpener = Box<dyn FnMut() -> Result<Box<dyn TempSensor>, SensorError>>;

/// What happened on one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    pub temperature: Option<Celsius>,
    pub command: RelayCommand,
    pub outcome: Outcome,
    pub relay: RelayState,
    pub mode: ControlState,
    pub overrun_fault: bool,
    /// The band in force on this tick.
    pub band: Band,
}

pub struct Daemon<C> {
    clock: C,
    controller: Controller,
    relay: ProtectedRelay<Box<dyn Relay>>,
    sensor: Option<Box<dyn TempSensor>>,
    open_sensor: SensorOpener,
    /// Whether the previous tick had a reading: log only the transitions.
    sensor_was_ok: bool,
    /// New bands from the remote settings, already validated.
    settings: Option<Subscriber<Band>>,
}

impl<C: Clock> Daemon<C> {
    /// Forces the relay OFF (starting `min_off_time`) and tries to open the
    /// sensor. A missing sensor is not an error: the loop keeps retrying, and
    /// SAFE mode takes over after `sensor_timeout`.
    pub fn new(
        clock: C,
        params: Params,
        limits: Limits,
        relay: Box<dyn Relay>,
        mut open_sensor: SensorOpener,
    ) -> Result<Self, RelayError> {
        let now = clock.now();
        let relay = ProtectedRelay::new(relay, limits, now)?;
        log::info!(
            "relay forced OFF; restart allowed in {} s (min_off_time)",
            limits.min_off_time.as_secs()
        );
        let sensor = match open_sensor() {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("{}", crate::error::chain(&e));
                None
            }
        };
        Ok(Self {
            clock,
            controller: Controller::new(params, now),
            relay,
            sensor_was_ok: sensor.is_some(),
            sensor,
            open_sensor,
            settings: None,
        })
    }

    /// Applies the bands published on `settings`, at the start of a tick.
    pub fn with_settings(mut self, settings: Subscriber<Band>) -> Self {
        self.settings = Some(settings);
        self
    }

    /// One control tick.
    pub fn step(&mut self) -> Result<Tick, RelayError> {
        if let Some(band) = self.settings.as_ref().and_then(Subscriber::take) {
            let old = self.controller.band();
            if band != old {
                log::info!("band changed {old} → {band} (remote settings)");
                self.controller.set_band(band);
            }
        }
        let temperature = self.read();
        let now = self.clock.now();

        let mode_before = self.controller.state();
        let command = self.controller.tick(temperature, now);
        let outcome = self.relay.request(command.wanted, now)?;
        let mode = self.controller.state();

        let fault_before = self.controller.overrun_fault();
        if outcome == Outcome::ForcedOff {
            let stops = self.controller.report_forced_off();
            let limit = self.controller.params().overrun_after_stops;
            if stops >= limit {
                log::warn!(
                    "overrun fault: {stops} rests in a row after max_continuous_on without \
                     reaching target_min (door open? refrigerant?)"
                );
            } else {
                log::info!(
                    "compressor resting after max_continuous_on ({stops}/{limit} in a row); \
                     cooling resumes after min_off_time"
                );
            }
        }
        if fault_before && !self.controller.overrun_fault() {
            log::info!("overrun fault cleared: target_min reached");
        }
        log_mode_change(mode_before, mode);
        match outcome {
            Outcome::Switched(state) => {
                log::info!(
                    "compressor {state} ({:?}, {})",
                    command.reason,
                    fmt_temp(temperature)
                );
            }
            Outcome::Held { hold, until } => log::debug!(
                "{} held by {hold:?} for {} s more",
                command.wanted,
                until.saturating_duration_since(now).as_secs()
            ),
            Outcome::Unchanged | Outcome::ForcedOff => {}
        }
        log::debug!(
            "{} → want {} ({:?}), relay {}",
            fmt_temp(temperature),
            command.wanted,
            command.reason,
            self.relay.state()
        );

        Ok(Tick {
            temperature,
            command,
            outcome,
            relay: self.relay.state(),
            mode,
            overrun_fault: self.controller.overrun_fault(),
            band: self.controller.band(),
        })
    }

    /// Ticks every `interval` until `shutdown` is requested, then turns the
    /// relay OFF. `on_tick` sees every tick: it must not block, it runs on the
    /// control thread.
    pub fn run(
        &mut self,
        interval: Duration,
        shutdown: &Shutdown,
        mut on_tick: impl FnMut(&Tick),
    ) -> Result<(), RelayError> {
        let result = self.run_until(interval, shutdown, &mut on_tick);
        // Even after an error: whatever happened, try to leave the relay OFF.
        let off = self.relay.shutdown(self.clock.now());
        log::info!("relay OFF, exiting");
        result.and(off)
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    fn run_until(
        &mut self,
        interval: Duration,
        shutdown: &Shutdown,
        on_tick: &mut impl FnMut(&Tick),
    ) -> Result<(), RelayError> {
        while !shutdown.requested() {
            let started = self.clock.now();
            let tick = self.step()?;
            on_tick(&tick);
            if shutdown.take_hangup() {
                log::info!("SIGHUP ignored: restart the service to apply configuration changes");
            }
            let spent = self.clock.now().saturating_duration_since(started);
            if shutdown.sleep(interval.saturating_sub(spent)) {
                break;
            }
        }
        log::info!("shutdown requested");
        Ok(())
    }

    /// One reading, reopening the sensor first if it is not open. Logs only
    /// the transitions between working and failing.
    fn read(&mut self) -> Option<Celsius> {
        if self.sensor.is_none() {
            self.sensor = (self.open_sensor)().ok();
        }
        let result = match self.sensor.as_mut() {
            Some(sensor) => sensor
                .read()
                .map_err(|e| format!("sensor read failed: {}", crate::error::chain(&e))),
            None => Err("sensor unavailable".to_owned()),
        };
        match &result {
            Ok(t) if !self.sensor_was_ok => log::info!("sensor back: {t}"),
            Err(e) if self.sensor_was_ok => log::error!("{e}"),
            _ => {}
        }
        self.sensor_was_ok = result.is_ok();
        result.ok()
    }
}

fn log_mode_change(before: ControlState, after: ControlState) {
    match (before, after) {
        (ControlState::Normal, ControlState::Safe { .. }) => {
            log::warn!("no valid reading for sensor_timeout: SAFE mode (blind duty cycle)");
        }
        (ControlState::Safe { .. }, ControlState::Normal) => {
            log::info!("valid reading again: back to NORMAL");
        }
        _ => {}
    }
}

fn fmt_temp(t: Option<Celsius>) -> String {
    t.map_or_else(|| "no reading".to_owned(), |t| t.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Instant;

    use super::*;
    use crate::clock::FakeClock;
    use crate::control::Reason;
    use crate::sim::SimFridge;

    const TICK: Duration = Duration::from_secs(15);

    fn params() -> Params {
        Params {
            target_min: 11.0,
            target_max: 13.0,
            sensor_timeout: Duration::from_secs(60),
            safe_on: Duration::from_secs(600),
            safe_off: Duration::from_secs(2400),
            overrun_after_stops: 3,
        }
    }

    fn limits() -> Limits {
        Limits {
            min_off_time: Duration::from_secs(300),
            min_on_time: Duration::from_secs(120),
            max_continuous_on: Duration::from_secs(7200),
        }
    }

    /// A daemon closed on a simulated fridge, on a fake clock. The sensor
    /// fails while `sensor_down` is set.
    fn simulated(start: f64) -> (Daemon<Rc<FakeClock>>, SimFridge<FakeClock>, Rc<Cell<bool>>) {
        let clock = Rc::new(FakeClock::new());
        let fridge = SimFridge::new(Celsius(start), Rc::clone(&clock));
        let down = Rc::new(Cell::new(false));
        let opener: SensorOpener = {
            let fridge = fridge.clone();
            let down = Rc::clone(&down);
            Box::new(move || {
                Ok(Box::new(FlakySensor {
                    inner: fridge.clone(),
                    down: Rc::clone(&down),
                }) as Box<dyn TempSensor>)
            })
        };
        let daemon =
            Daemon::new(clock, params(), limits(), Box::new(fridge.clone()), opener).unwrap();
        (daemon, fridge, down)
    }

    /// Always reads the same temperature, whatever the compressor does.
    struct Constant(f64);

    impl TempSensor for Constant {
        fn read(&mut self) -> Result<Celsius, SensorError> {
            Ok(Celsius(self.0))
        }
    }

    struct FlakySensor {
        inner: SimFridge<FakeClock>,
        down: Rc<Cell<bool>>,
    }

    impl TempSensor for FlakySensor {
        fn read(&mut self) -> Result<Celsius, SensorError> {
            if self.down.get() {
                return Err(SensorError::Implausible(Celsius(999.0)));
            }
            self.inner.read()
        }
    }

    /// Runs `ticks` steps, returning each tick with its time from the start.
    fn run(daemon: &mut Daemon<Rc<FakeClock>>, ticks: usize) -> Vec<(Duration, Tick)> {
        (0..ticks)
            .map(|_| {
                let tick = daemon.step().unwrap();
                let at = daemon.clock.elapsed();
                daemon.clock.advance(TICK);
                (at, tick)
            })
            .collect()
    }

    /// (time, new state) for every relay switch.
    fn switches(ticks: &[(Duration, Tick)]) -> Vec<(Duration, RelayState)> {
        ticks
            .iter()
            .filter_map(|(at, t)| match t.outcome {
                Outcome::Switched(s) => Some((*at, s)),
                Outcome::ForcedOff => Some((*at, RelayState::Off)),
                _ => None,
            })
            .collect()
    }

    fn assert_protections_respected(ticks: &[(Duration, Tick)]) {
        let sw = switches(ticks);
        // The first start counts from startup (t = 0), when the relay was forced OFF.
        let mut previous = (Duration::ZERO, RelayState::Off);
        for &(at, state) in &sw {
            let gap = at - previous.0;
            match state {
                RelayState::On => assert!(
                    gap >= limits().min_off_time,
                    "restart after {gap:?} at {at:?}"
                ),
                RelayState::Off => {
                    assert!(gap >= limits().min_on_time, "stop after {gap:?} at {at:?}")
                }
            }
            assert_ne!(state, previous.1, "switch to the same state at {at:?}");
            previous = (at, state);
        }
    }

    #[test]
    fn a_simulated_day_stays_in_band() {
        let (mut daemon, fridge, _) = simulated(18.0); // a warm fridge just plugged in
        let ticks = run(&mut daemon, 24 * 3600 / 15);

        // Once in band, the true temperature never leaves it by more than a
        // tick's worth of drift.
        let first_in_band = ticks
            .iter()
            .position(|(_, t)| t.temperature.is_some_and(|c| c.get() <= 13.0))
            .unwrap();
        for (at, t) in &ticks[first_in_band..] {
            let c = t.temperature.unwrap().get();
            assert!((10.85..=13.15).contains(&c), "{c} °C at {at:?}");
        }
        assert_protections_respected(&ticks);

        // About 18 min per cycle with these rates: ~78 starts a day.
        let starts = switches(&ticks)
            .iter()
            .filter(|(_, s)| *s == RelayState::On)
            .count();
        assert!((60..=100).contains(&starts), "{starts} starts");
        assert!(ticks.iter().all(|(_, t)| t.mode == ControlState::Normal));
        assert!(!daemon.controller().overrun_fault());
        let _ = fridge;
    }

    #[test]
    fn an_hour_without_sensor_runs_the_safe_cycle_and_recovers() {
        let (mut daemon, _fridge, down) = simulated(12.0);
        let mut ticks = run(&mut daemon, 4 * 60 * 4); // 4 h normal
        down.set(true);
        ticks.extend(run(&mut daemon, 60 * 4)); // 1 h without sensor
        down.set(false);
        ticks.extend(run(&mut daemon, 2 * 60 * 4)); // 2 h normal

        assert_protections_respected(&ticks);
        let safe: Vec<_> = ticks
            .iter()
            .filter(|(_, t)| matches!(t.mode, ControlState::Safe { .. }))
            .collect();
        // The timeout counts from the last valid reading, one tick before the
        // outage: 3 stale ticks, SAFE from the 4th failed one.
        assert_eq!(safe.len(), 60 * 4 - 3);
        assert!(
            safe.iter()
                .any(|(_, t)| t.command.reason == Reason::SafeOff)
        );
        assert!(safe.iter().any(|(_, t)| t.command.reason == Reason::SafeOn));
        // Back in NORMAL, and back in band.
        let (_, last) = ticks.last().unwrap();
        assert_eq!(last.mode, ControlState::Normal);
        assert!((10.85..=13.15).contains(&last.temperature.unwrap().get()));
    }

    #[test]
    fn a_door_left_open_trips_the_overrun_fault() {
        // Cooling cannot keep up: the fridge warms even with the compressor on.
        let clock = Rc::new(FakeClock::new());
        let opener: SensorOpener = Box::new(|| Ok(Box::new(Constant(14.0)) as Box<dyn TempSensor>));
        let relay = Box::new(crate::relay::fake::FakeRelay::default());
        let mut daemon = Daemon::new(Rc::clone(&clock), params(), limits(), relay, opener).unwrap();
        let forced = |ticks: &[(Duration, Tick)]| {
            ticks
                .iter()
                .filter(|(_, t)| t.outcome == Outcome::ForcedOff)
                .count()
        };
        // First 2 h 5 min ON, then a rest, then cooling resumes: not a fault yet.
        let mut ticks = run(&mut daemon, 3 * 3600 / 15);
        assert_eq!(forced(&ticks), 1);
        assert!(!daemon.controller().overrun_fault());
        // The third rest in a row, about 6 h in, is.
        ticks.extend(run(&mut daemon, 4 * 3600 / 15));
        assert_eq!(forced(&ticks), 3);
        assert!(daemon.controller().overrun_fault());
        assert_protections_respected(&ticks);
    }

    #[test]
    fn a_sensor_missing_at_startup_is_retried() {
        let clock = Rc::new(FakeClock::new());
        let attempts = Rc::new(Cell::new(0));
        let opener: SensorOpener = {
            let attempts = Rc::clone(&attempts);
            Box::new(move || {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 10 {
                    Err(SensorError::Unsupported)
                } else {
                    Ok(Box::new(Constant(12.0)) as Box<dyn TempSensor>)
                }
            })
        };
        let relay = Box::new(crate::relay::fake::FakeRelay::default());
        let mut daemon = Daemon::new(Rc::clone(&clock), params(), limits(), relay, opener).unwrap();
        let ticks = run(&mut daemon, 20);
        // 1 attempt at startup + one per tick: the 9th tick opens it.
        assert!(ticks[..8].iter().all(|(_, t)| t.temperature.is_none()));
        assert!(matches!(ticks[7].1.mode, ControlState::Safe { .. }));
        assert_eq!(ticks[8].1.temperature, Some(Celsius(12.0)));
        assert_eq!(ticks[8].1.mode, ControlState::Normal);
    }

    #[test]
    fn a_published_band_applies_at_the_next_tick() {
        let clock = Rc::new(FakeClock::new());
        let opener: SensorOpener = Box::new(|| Ok(Box::new(Constant(12.0)) as Box<dyn TempSensor>));
        let relay = Box::new(crate::relay::fake::FakeRelay::default());
        let (tx, rx) = crate::channel::latest();
        let mut daemon = Daemon::new(Rc::clone(&clock), params(), limits(), relay, opener)
            .unwrap()
            .with_settings(rx);

        // 12 °C inside 11–13.
        let tick = daemon.step().unwrap();
        assert_eq!(tick.command.wanted, RelayState::Off);
        assert_eq!(
            tick.band,
            Band {
                min: 11.0,
                max: 13.0
            }
        );

        tx.publish(Band::new(8.0, 11.0).unwrap());
        let tick = daemon.step().unwrap();
        assert_eq!(
            tick.band,
            Band {
                min: 8.0,
                max: 11.0
            }
        );
        assert_eq!(tick.command.wanted, RelayState::On);
        // Taken once: nothing new afterwards.
        assert_eq!(
            daemon.step().unwrap().band,
            Band {
                min: 8.0,
                max: 11.0
            }
        );
    }

    #[test]
    fn run_turns_the_relay_off_on_shutdown() {
        let clock = Rc::new(FakeClock::new());
        let opener: SensorOpener = Box::new(|| Ok(Box::new(Constant(14.0)) as Box<dyn TempSensor>));
        let fridge = SimFridge::new(Celsius(14.0), Rc::clone(&clock));
        let mut daemon = Daemon::new(
            Rc::clone(&clock),
            params(),
            limits(),
            Box::new(fridge.clone()),
            opener,
        )
        .unwrap();
        clock.advance(Duration::from_secs(300));
        daemon.step().unwrap();
        assert_eq!(fridge.compressor(), RelayState::On);

        let shutdown = Shutdown::default();
        shutdown.request();
        let started = Instant::now();
        daemon.run(TICK, &shutdown, |_| {}).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        // Turned OFF although min_on_time had not elapsed.
        assert_eq!(fridge.compressor(), RelayState::Off);
    }
}
