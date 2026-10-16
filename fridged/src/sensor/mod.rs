//! Temperature sensing.
//!
//! The control loop only sees the [`TempSensor`] trait, so it runs the same
//! against the real MCP9808 and against a fake on the development machine.

use std::fmt;

pub mod mcp9808;

/// A temperature in degrees Celsius.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Celsius(pub f64);

impl Celsius {
    pub fn get(self) -> f64 {
        self.0
    }
}

impl fmt::Display for Celsius {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The MCP9808 resolves 0.0625 °C: two decimals show every step that
        // matters without pretending to more precision.
        write!(f, "{:.2} °C", self.0)
    }
}

/// Why a reading could not be obtained.
///
/// Every variant is recoverable from the daemon's point of view: the control
/// loop keeps going, and falls back to SAFE mode if readings stay missing.
#[derive(Debug, thiserror::Error)]
pub enum SensorError {
    /// The I²C transaction failed: wiring, bus not enabled, sensor unpowered.
    #[error("I²C bus error")]
    Bus(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Something answered at the address, but it is not an MCP9808.
    #[error("not an MCP9808: manufacturer {manufacturer:#06x}, device {device:#04x}")]
    WrongDevice { manufacturer: u16, device: u8 },
    /// Outside what the sensor can measure: a corrupted transfer, not a temperature.
    #[error("implausible reading {0}")]
    Implausible(Celsius),
    /// The real sensor needs Linux I²C; elsewhere only `--simulate` works.
    #[error("the MCP9808 is only reachable on Linux: use --simulate")]
    Unsupported,
}

/// Something that measures the fridge temperature.
pub trait TempSensor {
    fn read(&mut self) -> Result<Celsius, SensorError>;
}

/// Opens and initializes the MCP9808 described by `config`.
#[cfg(target_os = "linux")]
pub fn open_mcp9808(
    config: &crate::config::SensorConfig,
) -> Result<Box<dyn TempSensor>, SensorError> {
    let sensor = mcp9808::Mcp9808::open(config.i2c_bus, config.address)?;
    Ok(Box::new(sensor))
}

/// Opens and initializes the MCP9808 described by `config`.
#[cfg(not(target_os = "linux"))]
pub fn open_mcp9808(
    _config: &crate::config::SensorConfig,
) -> Result<Box<dyn TempSensor>, SensorError> {
    Err(SensorError::Unsupported)
}

impl<T: TempSensor + ?Sized> TempSensor for Box<T> {
    fn read(&mut self) -> Result<Celsius, SensorError> {
        (**self).read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_has_two_decimals() {
        assert_eq!(Celsius(12.0625).to_string(), "12.06 °C");
        assert_eq!(Celsius(-0.0625).to_string(), "-0.06 °C");
    }

    #[test]
    fn ordering() {
        assert!(Celsius(11.0) < Celsius(13.0));
        assert!(Celsius(-1.0) < Celsius(0.0));
    }

    #[test]
    fn errors_describe_themselves() {
        let e = SensorError::WrongDevice {
            manufacturer: 0x1234,
            device: 0x05,
        };
        assert_eq!(
            e.to_string(),
            "not an MCP9808: manufacturer 0x1234, device 0x05"
        );
        let e = SensorError::Implausible(Celsius(200.0));
        assert_eq!(e.to_string(), "implausible reading 200.00 °C");
    }
}
