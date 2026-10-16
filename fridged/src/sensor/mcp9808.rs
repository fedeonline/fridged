//! Microchip MCP9808 digital temperature sensor (I²C).
//!
//! Datasheet: DS25095. All registers are big-endian (MSB first).
//!
//! The driver talks to a [`RegisterBus`] rather than to `rppal` directly, so
//! its logic is tested on the development machine; on Linux the bus is
//! `rppal::i2c::I2c`.

use std::ops::RangeInclusive;
use std::thread;
use std::time::Duration;

use super::{Celsius, SensorError, TempSensor};

/// Configuration register; bit 8 is SHDN (low-power shutdown).
pub const REG_CONFIG: u8 = 0x01;
/// Ambient temperature register (T_A).
pub const REG_AMBIENT: u8 = 0x05;
pub const REG_MANUFACTURER_ID: u8 = 0x06;
/// Upper byte: device id; lower byte: silicon revision.
pub const REG_DEVICE_ID: u8 = 0x07;
/// Resolution register (one byte).
pub const REG_RESOLUTION: u8 = 0x08;

const CONFIG_SHUTDOWN: u16 = 1 << 8;

pub const MANUFACTURER_ID: u16 = 0x0054;
pub const DEVICE_ID: u8 = 0x04;

/// One LSB of the T_A register.
pub const LSB: f64 = 0.0625;

/// Resolution register value for 0.0625 °C (the power-up default; set
/// explicitly in case something changed it).
pub const RESOLUTION_0_0625: u8 = 0x03;
/// Conversion time at 0.0625 °C resolution.
pub const CONVERSION_TIME: Duration = Duration::from_millis(250);

/// The sensor's operating range. Anything outside is a corrupted transfer.
pub const OPERATING_RANGE: RangeInclusive<f64> = -40.0..=125.0;

/// Register-level access to the sensor.
pub trait RegisterBus {
    type Error: std::error::Error + Send + Sync + 'static;

    fn read_u16(&mut self, reg: u8) -> Result<u16, Self::Error>;
    fn read_u8(&mut self, reg: u8) -> Result<u8, Self::Error>;
    fn write_u16(&mut self, reg: u8, value: u16) -> Result<(), Self::Error>;
    fn write_u8(&mut self, reg: u8, value: u8) -> Result<(), Self::Error>;
}

pub struct Mcp9808<B> {
    bus: B,
}

impl<B: RegisterBus> Mcp9808<B> {
    /// Checks the sensor's identity and prepares it for continuous reading:
    /// wakes it from shutdown if needed and sets 0.0625 °C resolution.
    pub fn new(mut bus: B) -> Result<Self, SensorError> {
        let manufacturer = bus.read_u16(REG_MANUFACTURER_ID).map_err(bus_error)?;
        let [device, _revision] = bus
            .read_u16(REG_DEVICE_ID)
            .map_err(bus_error)?
            .to_be_bytes();
        if manufacturer != MANUFACTURER_ID || device != DEVICE_ID {
            return Err(SensorError::WrongDevice {
                manufacturer,
                device,
            });
        }

        let config = bus.read_u16(REG_CONFIG).map_err(bus_error)?;
        let was_shut_down = config & CONFIG_SHUTDOWN != 0;
        if was_shut_down {
            bus.write_u16(REG_CONFIG, config & !CONFIG_SHUTDOWN)
                .map_err(bus_error)?;
        }

        bus.write_u8(REG_RESOLUTION, RESOLUTION_0_0625)
            .map_err(bus_error)?;
        let resolution = bus.read_u8(REG_RESOLUTION).map_err(bus_error)?;
        if resolution & 0x03 != RESOLUTION_0_0625 {
            return Err(bus_error(ResolutionNotApplied(resolution)));
        }

        if was_shut_down {
            // T_A holds no valid conversion until the first one completes.
            thread::sleep(CONVERSION_TIME);
        }
        Ok(Self { bus })
    }

    pub fn into_bus(self) -> B {
        self.bus
    }
}

impl<B: RegisterBus> TempSensor for Mcp9808<B> {
    fn read(&mut self) -> Result<Celsius, SensorError> {
        let raw = self.bus.read_u16(REG_AMBIENT).map_err(bus_error)?;
        let t = decode_ambient(raw);
        if !OPERATING_RANGE.contains(&t.get()) {
            return Err(SensorError::Implausible(t));
        }
        Ok(t)
    }
}

fn bus_error(e: impl std::error::Error + Send + Sync + 'static) -> SensorError {
    SensorError::Bus(Box::new(e))
}

/// The resolution register did not keep the value written to it.
#[derive(Debug, thiserror::Error)]
#[error("resolution register reads {0:#04x} after writing {RESOLUTION_0_0625:#04x}")]
struct ResolutionNotApplied(u8);

#[cfg(target_os = "linux")]
mod linux {
    use rppal::i2c::{Error, I2c};

    use super::{Mcp9808, RegisterBus, SensorError, bus_error};

    impl RegisterBus for I2c {
        type Error = Error;

        fn read_u16(&mut self, reg: u8) -> Result<u16, Error> {
            let mut buf = [0u8; 2];
            self.write_read(&[reg], &mut buf)?;
            Ok(u16::from_be_bytes(buf))
        }

        fn read_u8(&mut self, reg: u8) -> Result<u8, Error> {
            let mut buf = [0u8; 1];
            self.write_read(&[reg], &mut buf)?;
            Ok(buf[0])
        }

        fn write_u16(&mut self, reg: u8, value: u16) -> Result<(), Error> {
            let [hi, lo] = value.to_be_bytes();
            self.write(&[reg, hi, lo]).map(drop)
        }

        fn write_u8(&mut self, reg: u8, value: u8) -> Result<(), Error> {
            self.write(&[reg, value]).map(drop)
        }
    }

    impl Mcp9808<I2c> {
        /// Opens `/dev/i2c-<bus>` and initializes the sensor at `address`.
        pub fn open(bus: u8, address: u16) -> Result<Self, SensorError> {
            let mut i2c = I2c::with_bus(bus).map_err(bus_error)?;
            i2c.set_slave_address(address).map_err(bus_error)?;
            Self::new(i2c)
        }
    }
}

/// Decodes the T_A register into a temperature.
///
/// Layout: bits 15–13 are alert flags (T_crit, T_upper, T_lower) and are
/// ignored; bits 12–0 are the temperature as a 13-bit two's complement number
/// of 0.0625 °C steps (bit 12 is the sign).
///
/// The datasheet's sample code computes negatives as `256 - (UB * 16 + LB / 16)`,
/// which is a known sign error: it yields +0.0625 for −0.0625 °C. Treating the
/// 13 bits as two's complement avoids it.
pub fn decode_ambient(raw: u16) -> Celsius {
    let bits = raw & 0x1FFF;
    let steps = if bits & 0x1000 != 0 {
        i32::from(bits) - 0x2000
    } else {
        i32::from(bits)
    };
    Celsius(f64::from(steps) * LSB)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;

    use super::*;

    /// Register map standing in for the chip. Unknown registers read as 0.
    #[derive(Default)]
    struct FakeBus {
        regs: HashMap<u8, u16>,
        writes: Vec<(u8, u16)>,
        /// Every transaction fails.
        broken: bool,
        /// Writes to the resolution register are dropped.
        sticky_resolution: bool,
    }

    impl FakeBus {
        fn mcp9808() -> Self {
            let mut bus = Self::default();
            bus.regs.insert(REG_MANUFACTURER_ID, 0x0054);
            bus.regs.insert(REG_DEVICE_ID, 0x0400);
            bus.regs.insert(REG_RESOLUTION, 0x03);
            bus.regs.insert(REG_AMBIENT, 0x00C0); // 12.0 °C
            bus
        }

        fn check(&self) -> io::Result<()> {
            if self.broken {
                Err(io::Error::other("remote I/O error"))
            } else {
                Ok(())
            }
        }
    }

    impl RegisterBus for FakeBus {
        type Error = io::Error;

        fn read_u16(&mut self, reg: u8) -> io::Result<u16> {
            self.check()?;
            Ok(self.regs.get(&reg).copied().unwrap_or(0))
        }

        fn read_u8(&mut self, reg: u8) -> io::Result<u8> {
            self.read_u16(reg).map(|v| v as u8)
        }

        fn write_u16(&mut self, reg: u8, value: u16) -> io::Result<()> {
            self.check()?;
            self.writes.push((reg, value));
            self.regs.insert(reg, value);
            Ok(())
        }

        fn write_u8(&mut self, reg: u8, value: u8) -> io::Result<()> {
            self.check()?;
            self.writes.push((reg, u16::from(value)));
            if !(reg == REG_RESOLUTION && self.sticky_resolution) {
                self.regs.insert(reg, u16::from(value));
            }
            Ok(())
        }
    }

    #[test]
    fn init_sets_resolution_and_reads() {
        let mut sensor = Mcp9808::new(FakeBus::mcp9808()).unwrap();
        assert_eq!(sensor.read().unwrap(), Celsius(12.0));
        let bus = sensor.into_bus();
        assert_eq!(bus.writes, vec![(REG_RESOLUTION, 0x03)]);
    }

    #[test]
    fn init_rejects_another_chip() {
        let mut bus = FakeBus::mcp9808();
        bus.regs.insert(REG_MANUFACTURER_ID, 0x1234);
        let err = Mcp9808::new(bus).err().unwrap();
        assert!(matches!(
            err,
            SensorError::WrongDevice {
                manufacturer: 0x1234,
                ..
            }
        ));

        let mut bus = FakeBus::mcp9808();
        bus.regs.insert(REG_DEVICE_ID, 0x0500);
        let err = Mcp9808::new(bus).err().unwrap();
        assert!(matches!(err, SensorError::WrongDevice { device: 0x05, .. }));
    }

    #[test]
    fn revision_byte_does_not_matter() {
        let mut bus = FakeBus::mcp9808();
        bus.regs.insert(REG_DEVICE_ID, 0x04FF);
        assert!(Mcp9808::new(bus).is_ok());
    }

    #[test]
    fn init_wakes_a_shut_down_sensor() {
        let mut bus = FakeBus::mcp9808();
        bus.regs.insert(REG_CONFIG, CONFIG_SHUTDOWN | 0x0008);
        let bus = Mcp9808::new(bus).unwrap().into_bus();
        // SHDN cleared, the other config bits preserved.
        assert_eq!(bus.regs[&REG_CONFIG], 0x0008);
    }

    #[test]
    fn init_leaves_an_awake_sensor_config_alone() {
        let bus = Mcp9808::new(FakeBus::mcp9808()).unwrap().into_bus();
        assert!(bus.writes.iter().all(|&(reg, _)| reg != REG_CONFIG));
    }

    #[test]
    fn init_detects_a_resolution_that_did_not_stick() {
        let mut bus = FakeBus::mcp9808();
        bus.regs.insert(REG_RESOLUTION, 0x00);
        bus.sticky_resolution = true;
        let err = Mcp9808::new(bus).err().unwrap();
        assert!(matches!(err, SensorError::Bus(_)), "{err:?}");
    }

    #[test]
    fn bus_failure_is_a_bus_error() {
        let mut bus = FakeBus::mcp9808();
        bus.broken = true;
        assert!(matches!(
            Mcp9808::new(bus).err().unwrap(),
            SensorError::Bus(_)
        ));

        let mut sensor = Mcp9808::new(FakeBus::mcp9808()).unwrap();
        sensor.bus.broken = true;
        assert!(matches!(sensor.read(), Err(SensorError::Bus(_))));
    }

    #[test]
    fn negative_readings_pass_through() {
        let mut sensor = Mcp9808::new(FakeBus::mcp9808()).unwrap();
        sensor.bus.regs.insert(REG_AMBIENT, 0x1FF0);
        assert_eq!(sensor.read().unwrap(), Celsius(-1.0));
    }

    #[test]
    fn readings_outside_the_operating_range_are_implausible() {
        let mut sensor = Mcp9808::new(FakeBus::mcp9808()).unwrap();
        sensor.bus.regs.insert(REG_AMBIENT, 0x0FFF); // 255.94 °C
        assert!(matches!(sensor.read(), Err(SensorError::Implausible(_))));
        sensor.bus.regs.insert(REG_AMBIENT, 0x1D7F); // -40.06 °C
        assert!(matches!(sensor.read(), Err(SensorError::Implausible(_))));
        sensor.bus.regs.insert(REG_AMBIENT, 0x1D80); // -40.0 °C, still in range
        assert!(sensor.read().is_ok());
    }

    /// (register value, expected °C). Built from the datasheet encoding:
    /// steps of 0.0625 °C, 13-bit two's complement, flags in the top 3 bits.
    const VECTORS: &[(u16, f64)] = &[
        (0x0000, 0.0),
        (0x0001, 0.0625),
        (0x0008, 0.5),
        (0x00C0, 12.0), // cellar setpoint
        (0x00B4, 11.25),
        (0x00D0, 13.0),
        (0x0190, 25.0),
        (0x0194, 25.25),
        (0x07D0, 125.0),    // top of the sensor's range
        (0x0FFF, 255.9375), // largest positive encoding
        (0x1FFF, -0.0625),  // where the datasheet formula goes wrong
        (0x1FF8, -0.5),
        (0x1FF0, -1.0),
        (0x1F00, -16.0),
        (0x1E70, -25.0),
        (0x1D80, -40.0),  // bottom of the sensor's range
        (0x1000, -256.0), // most negative encoding
    ];

    #[test]
    fn datasheet_vectors() {
        for &(raw, expected) in VECTORS {
            assert_eq!(decode_ambient(raw).get(), expected, "raw {raw:#06x}");
        }
    }

    #[test]
    fn alert_flags_are_ignored() {
        for &(raw, expected) in VECTORS {
            for flags in [0x2000, 0x4000, 0x8000, 0xE000] {
                assert_eq!(
                    decode_ambient(raw | flags).get(),
                    expected,
                    "raw {:#06x}",
                    raw | flags
                );
            }
        }
    }

    #[test]
    fn every_encoding_is_a_whole_number_of_steps() {
        for raw in 0..=0x1FFF_u16 {
            let t = decode_ambient(raw).get();
            assert_eq!((t / LSB).fract(), 0.0, "raw {raw:#06x}");
            assert!((-256.0..256.0).contains(&t), "raw {raw:#06x} → {t}");
        }
    }

    #[test]
    fn decoding_is_monotonic_across_the_sign_boundary() {
        // Walk the 13-bit range from most negative to most positive.
        let ordered = (0x1000..=0x1FFF_u16).chain(0x0000..=0x0FFF);
        let temps: Vec<f64> = ordered.map(|r| decode_ambient(r).get()).collect();
        assert!(temps.windows(2).all(|w| w[1] - w[0] == LSB));
    }
}
