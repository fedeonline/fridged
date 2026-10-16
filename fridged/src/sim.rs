//! A crude fridge model for `--simulate`: the temperature falls at a fixed
//! rate while the compressor runs and rises while it is off.
//!
//! The simulated sensor and relay share the model, so the control loop closes
//! on itself without hardware. Linear rates are enough to exercise the
//! control logic; this is not a thermal simulation.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::clock::Clock;
use crate::relay::{Relay, RelayError, RelayState};
use crate::sensor::mcp9808::LSB;
use crate::sensor::{Celsius, SensorError, TempSensor};

/// °C per second with the compressor off.
pub const WARMING_RATE: f64 = 0.15 / 60.0;
/// °C per second with the compressor on.
pub const COOLING_RATE: f64 = 0.4 / 60.0;

struct State {
    temperature: f64,
    compressor: RelayState,
    updated: Instant,
}

impl State {
    /// Integrates the temperature up to `now` with the current compressor state.
    fn advance(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.updated).as_secs_f64();
        let rate = match self.compressor {
            RelayState::On => -COOLING_RATE,
            RelayState::Off => WARMING_RATE,
        };
        self.temperature += rate * dt;
        self.updated = self.updated.max(now);
    }
}

pub struct SimFridge<C> {
    state: Rc<RefCell<State>>,
    clock: Rc<C>,
}

impl<C> Clone for SimFridge<C> {
    fn clone(&self) -> Self {
        Self {
            state: Rc::clone(&self.state),
            clock: Rc::clone(&self.clock),
        }
    }
}

impl<C: Clock> SimFridge<C> {
    pub fn new(start: Celsius, clock: Rc<C>) -> Self {
        let state = State {
            temperature: start.get(),
            compressor: RelayState::Off,
            updated: clock.now(),
        };
        Self {
            state: Rc::new(RefCell::new(state)),
            clock,
        }
    }

    /// The true temperature, unquantized.
    pub fn temperature(&self) -> Celsius {
        let mut s = self.state.borrow_mut();
        s.advance(self.clock.now());
        Celsius(s.temperature)
    }

    pub fn compressor(&self) -> RelayState {
        self.state.borrow().compressor
    }
}

impl<C: Clock> TempSensor for SimFridge<C> {
    /// Quantized to the MCP9808's 0.0625 °C steps.
    fn read(&mut self) -> Result<Celsius, SensorError> {
        let t = self.temperature().get();
        Ok(Celsius((t / LSB).round() * LSB))
    }
}

impl<C: Clock> Relay for SimFridge<C> {
    fn set(&mut self, state: RelayState) -> Result<(), RelayError> {
        let mut s = self.state.borrow_mut();
        s.advance(self.clock.now());
        s.compressor = state;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::clock::FakeClock;

    #[test]
    fn warms_while_off_cools_while_on() {
        let clock = Rc::new(FakeClock::new());
        let mut fridge = SimFridge::new(Celsius(12.0), Rc::clone(&clock));
        clock.advance(Duration::from_secs(600));
        assert!((fridge.temperature().get() - 13.5).abs() < 1e-9);
        fridge.set(RelayState::On).unwrap();
        clock.advance(Duration::from_secs(300));
        assert!((fridge.temperature().get() - 11.5).abs() < 1e-9);
    }

    #[test]
    fn readings_are_quantized() {
        let clock = Rc::new(FakeClock::new());
        let mut fridge = SimFridge::new(Celsius(12.03), clock);
        assert_eq!(fridge.read().unwrap(), Celsius(12.0));
    }
}
