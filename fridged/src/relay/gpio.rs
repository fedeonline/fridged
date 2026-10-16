//! Relay on a BCM2835 GPIO, through `rppal`.

use std::panic;
use std::sync::{Mutex, Once, TryLockError};

use rppal::gpio::{Gpio, OutputPin};

use super::{Relay, RelayError, RelayState, pin_high};

/// The relay pin and its `active_low` flag.
///
/// Global so the panic hook can reach it: the release profile aborts on panic,
/// so no destructor runs, and without the hook the compressor would stay in
/// whatever state it was until systemd restarts the daemon (or forever, if it
/// gives up).
static PIN: Mutex<Option<(OutputPin, bool)>> = Mutex::new(None);
static PANIC_HOOK: Once = Once::new();

/// Handle to the relay pin, driven as an output for the whole life of the process.
///
/// `rppal` maps the GPIO registers directly: after a crash the pin keeps its
/// last level, the kernel does not reset it. Hence the pin is forced OFF when
/// opened, when dropped, and on panic.
pub struct GpioRelay {
    _private: (),
}

impl GpioRelay {
    /// Takes BCM pin `pin` as an output, already at the OFF level.
    pub fn open(pin: u8, active_low: bool) -> Result<Self, RelayError> {
        let mut slot = PIN.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(RelayError::Gpio("the relay GPIO is already open".into()));
        }
        let pin = Gpio::new().and_then(|g| g.get(pin)).map_err(gpio_error)?;
        // The level is written before the pin switches to output, so it never
        // drives ON, not even for an instant.
        let mut pin = if pin_high(RelayState::Off, active_low) {
            pin.into_output_high()
        } else {
            pin.into_output_low()
        };
        // On drop rppal would turn the pin back into an input. A floating input
        // can energize some active-low modules: keep it an output, driven OFF.
        pin.set_reset_on_drop(false);
        *slot = Some((pin, active_low));

        PANIC_HOOK.call_once(|| {
            let previous = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                force_off_on_panic();
                previous(info);
            }));
        });
        Ok(Self { _private: () })
    }
}

impl Relay for GpioRelay {
    fn set(&mut self, state: RelayState) -> Result<(), RelayError> {
        drive(&mut PIN.lock().unwrap_or_else(|e| e.into_inner()), state);
        Ok(())
    }
}

impl Drop for GpioRelay {
    fn drop(&mut self) {
        let mut slot = PIN.lock().unwrap_or_else(|e| e.into_inner());
        drive(&mut slot, RelayState::Off);
        // The pin stays an output at the OFF level (reset_on_drop is off).
        *slot = None;
    }
}

fn drive(slot: &mut Option<(OutputPin, bool)>, state: RelayState) {
    if let Some((pin, active_low)) = slot {
        if pin_high(state, *active_low) {
            pin.set_high();
        } else {
            pin.set_low();
        }
    }
}

/// Never blocks: a panic while this thread holds the lock must not deadlock.
fn force_off_on_panic() {
    let mut slot = match PIN.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    drive(&mut slot, RelayState::Off);
}

fn gpio_error(e: rppal::gpio::Error) -> RelayError {
    RelayError::Gpio(Box::new(e))
}
