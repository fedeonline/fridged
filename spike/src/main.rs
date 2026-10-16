//! Phase 0 spike: validates the ARMv6 toolchain before we write the application.
//!
//! It checks the three things that, if broken, would force a redesign:
//!   1. TLS  — `ureq` + `rustls` (hence `ring`) cross-compile and run on ARMv6
//!   2. GPIO — `rppal` can reach the BCM2835
//!   3. I2C  — the bus responds and the MCP9808 identifies itself
//!
//! Exits non-zero if a mandatory check fails.

use std::error::Error;
use std::process::ExitCode;

/// A Google endpoint: exercises the same certificate chain we will use for Firestore.
const TLS_PROBE_URL: &str = "https://www.googleapis.com/discovery/v1/apis?name=firestore";

// The hardware constants below are only referenced by the Linux-only probes.
/// MCP9808 default address on the Adafruit breakout.
#[cfg(target_os = "linux")]
const MCP9808_ADDR: u16 = 0x18;
#[cfg(target_os = "linux")]
const REG_MANUFACTURER_ID: u8 = 0x06;
#[cfg(target_os = "linux")]
const REG_DEVICE_ID: u8 = 0x07;
#[cfg(target_os = "linux")]
const EXPECTED_MANUFACTURER_ID: u16 = 0x0054;

/// Default relay GPIO (BCM numbering).
#[cfg(target_os = "linux")]
const RELAY_PIN: u8 = 26;

fn main() -> ExitCode {
    println!("=== fridged spike — toolchain validation ===");
    println!("target: {}", std::env::consts::ARCH);
    println!();

    // TLS is the only blocking check: without it, the plan changes.
    let tls_ok = report("TLS (ureq + rustls)", probe_tls());

    // GPIO and I2C only make sense on the Pi: elsewhere they are skipped
    // without failing the spike.
    let (gpio_ok, i2c_ok) = probe_hardware();

    println!();
    println!("=== result ===");
    println!("  TLS : {}", verdict(Some(tls_ok)));
    println!("  GPIO: {}", verdict(gpio_ok));
    println!("  I2C : {}", verdict(i2c_ok));

    if tls_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn verdict(ok: Option<bool>) -> &'static str {
    match ok {
        Some(true) => "OK",
        Some(false) => "FAILED",
        None => "skipped (not Linux/Pi)",
    }
}

/// Prints the outcome of a check and reduces it to a boolean.
fn report(label: &str, result: Result<String, Box<dyn Error>>) -> bool {
    match result {
        Ok(detail) => {
            println!("[ OK ] {label}: {detail}");
            true
        }
        Err(e) => {
            println!("[FAIL] {label}: {e}");
            false
        }
    }
}

/// A real HTTPS request to Google. If this passes on ARMv6, `ring` works.
fn probe_tls() -> Result<String, Box<dyn Error>> {
    let mut res = ureq::get(TLS_PROBE_URL).call()?;
    let status = res.status();
    let body = res.body_mut().read_to_string()?;
    Ok(format!("HTTP {status}, {} bytes received", body.len()))
}

#[cfg(target_os = "linux")]
fn probe_hardware() -> (Option<bool>, Option<bool>) {
    let gpio = report(&format!("GPIO (rppal, pin {RELAY_PIN})"), probe_gpio());
    let i2c = report(
        &format!("I2C (MCP9808 @ 0x{MCP9808_ADDR:02x})"),
        probe_i2c(),
    );
    (Some(gpio), Some(i2c))
}

#[cfg(not(target_os = "linux"))]
fn probe_hardware() -> (Option<bool>, Option<bool>) {
    println!("[skip] GPIO and I2C: only available on Linux/Raspberry Pi");
    (None, None)
}

/// Reads the pin's logic level without changing its mode: does not move the relay.
#[cfg(target_os = "linux")]
fn probe_gpio() -> Result<String, Box<dyn Error>> {
    let gpio = rppal::gpio::Gpio::new()?;
    let pin = gpio.get(RELAY_PIN)?;
    Ok(format!("current level {:?}", pin.read()))
}

/// Queries the MCP9808 IDs: confirms bus, address and sensor presence.
#[cfg(target_os = "linux")]
fn probe_i2c() -> Result<String, Box<dyn Error>> {
    let mut i2c = rppal::i2c::I2c::with_bus(1)?;
    i2c.set_slave_address(MCP9808_ADDR)?;

    let mut buf = [0u8; 2];
    i2c.write_read(&[REG_MANUFACTURER_ID], &mut buf)?;
    let manufacturer = u16::from_be_bytes(buf);

    i2c.write_read(&[REG_DEVICE_ID], &mut buf)?;
    let device = u16::from_be_bytes(buf);

    if manufacturer != EXPECTED_MANUFACTURER_ID {
        return Err(format!(
            "manufacturer ID 0x{manufacturer:04x}, expected 0x{EXPECTED_MANUFACTURER_ID:04x} \
             (wrong address, or a different device?)"
        )
        .into());
    }

    Ok(format!(
        "manufacturer 0x{manufacturer:04x}, device 0x{device:04x}"
    ))
}
