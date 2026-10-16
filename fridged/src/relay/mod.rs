//! Compressor relay.
//!
//! [`Relay`] is the bare switch. The compressor protections (`min_off_time`,
//! `min_on_time`, `max_continuous_on`) are layered on top of it in this module
//! and always apply, whatever the control logic asks for.

use std::fmt;

pub mod dry_run;
#[cfg(test)]
pub mod fake;
#[cfg(target_os = "linux")]
mod gpio;
mod protection;

#[cfg(target_os = "linux")]
pub use gpio::GpioRelay;
pub use protection::{Hold, Limits, Outcome, ProtectedRelay};

/// Whether the compressor is powered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RelayState {
    Off,
    On,
}

impl fmt::Display for RelayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "OFF",
            Self::On => "ON",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("GPIO error")]
    Gpio(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The GPIO needs Linux; elsewhere only `--dry-run` works.
    #[error("the relay GPIO is only reachable on Linux: use --dry-run")]
    Unsupported,
}

/// A switch that powers the compressor on or off. No protections: see the
/// protected wrapper for those.
pub trait Relay {
    fn set(&mut self, state: RelayState) -> Result<(), RelayError>;
}

impl<T: Relay + ?Sized> Relay for Box<T> {
    fn set(&mut self, state: RelayState) -> Result<(), RelayError> {
        (**self).set(state)
    }
}

/// Whether the GPIO must be driven high for `state`.
///
/// An active-low relay switches on when its input is pulled LOW; a DC-input
/// solid-state relay, like the one on this fridge, is active high.
pub fn pin_high(state: RelayState, active_low: bool) -> bool {
    (state == RelayState::On) != active_low
}

/// Opens the relay GPIO, forcing it OFF.
#[cfg(target_os = "linux")]
pub fn open_gpio(config: &crate::config::RelayConfig) -> Result<Box<dyn Relay>, RelayError> {
    Ok(Box::new(GpioRelay::open(
        config.gpio_pin,
        config.active_low,
    )?))
}

/// Opens the relay GPIO, forcing it OFF.
#[cfg(not(target_os = "linux"))]
pub fn open_gpio(_config: &crate::config::RelayConfig) -> Result<Box<dyn Relay>, RelayError> {
    Err(RelayError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_low_levels() {
        assert!(!pin_high(RelayState::On, true));
        assert!(pin_high(RelayState::Off, true));
    }

    #[test]
    fn active_high_levels() {
        assert!(pin_high(RelayState::On, false));
        assert!(!pin_high(RelayState::Off, false));
    }

    #[test]
    fn display() {
        assert_eq!(RelayState::On.to_string(), "ON");
        assert_eq!(RelayState::Off.to_string(), "OFF");
    }
}
