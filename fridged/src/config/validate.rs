//! Semantic checks on a parsed [`Config`].
//!
//! Parsing only guarantees the right types; these checks reject values that
//! would make the controller misbehave. Every problem is reported, not just the
//! first, so a broken file can be fixed in one pass.

use std::time::Duration;

use super::Config;

/// Narrowest allowed hysteresis band (°C). The MCP9808 resolves 0.0625 °C and
/// is accurate to ±0.25 °C: a narrower band makes the compressor chase noise.
pub const MIN_DEAD_BAND: f64 = 0.5;

/// Plausible setpoint range (°C). Below 0 the beer freezes; above 25 a fridge
/// is pointless, and the value is most likely in Fahrenheit.
pub const SETPOINT_RANGE: (f64, f64) = (0.0, 25.0);

/// MCP9808 addresses, selected by the A0–A2 pins.
const MCP9808_ADDRESSES: std::ops::RangeInclusive<u16> = 0x18..=0x1F;

/// Highest BCM GPIO on the Pi header.
const MAX_GPIO: u8 = 27;
/// BCM 2 and 3 are SDA/SCL of I²C bus 1, which the sensor uses.
const I2C1_PINS: [u8; 2] = [2, 3];

/// Every problem found in a configuration, in a single error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{}", problems.join("; "))]
pub struct ValidationError {
    pub problems: Vec<String>,
}

/// Problems with a hysteresis band, wherever it comes from (the config file
/// or the settings document in Firestore): one rule set for both.
pub fn check_band(min: f64, max: f64) -> Vec<String> {
    let mut p = Vec::new();
    if !min.is_finite() || !max.is_finite() {
        p.push("target_min and target_max must be finite numbers".to_owned());
        return p;
    }
    let (lo, hi) = SETPOINT_RANGE;
    for (name, t) in [("target_min", min), ("target_max", max)] {
        if !(lo..=hi).contains(&t) {
            p.push(format!("{name} = {t} °C is outside {lo}..={hi} °C"));
        }
    }
    if max - min < MIN_DEAD_BAND {
        p.push(format!(
            "target_max ({max}) must exceed target_min ({min}) by at least {MIN_DEAD_BAND} °C"
        ));
    }
    p
}

impl Config {
    /// Checks that the configuration is safe to run with.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut p = Vec::new();

        let band = check_band(self.control.target_min, self.control.target_max);
        p.extend(band.into_iter().map(|problem| format!("control.{problem}")));

        let durations = [
            ("sensor.read_interval_secs", self.sensor.read_interval),
            ("relay.min_off_time_secs", self.relay.min_off_time),
            ("relay.min_on_time_secs", self.relay.min_on_time),
            ("relay.max_continuous_on_secs", self.relay.max_continuous_on),
            ("control.sensor_timeout_secs", self.control.sensor_timeout),
            ("control.safe_on_secs", self.control.safe_on),
            ("control.safe_off_secs", self.control.safe_off),
            (
                "firestore.upload_interval_secs",
                self.firestore.upload_interval,
            ),
        ];
        for (name, d) in durations {
            if d.is_zero() {
                p.push(format!("{name} must be greater than 0"));
            }
        }

        let r = &self.relay;
        if r.max_continuous_on <= r.min_on_time {
            p.push(format!(
                "relay.max_continuous_on_secs ({}) must be greater than min_on_time_secs ({})",
                secs(r.max_continuous_on),
                secs(r.min_on_time)
            ));
        }
        // The relay protections would silently stretch these phases, so the
        // configured duty cycle would not be the one actually applied.
        let c = &self.control;
        if c.safe_on < r.min_on_time {
            p.push(format!(
                "control.safe_on_secs ({}) is shorter than relay.min_on_time_secs ({})",
                secs(c.safe_on),
                secs(r.min_on_time)
            ));
        }
        if c.safe_off < r.min_off_time {
            p.push(format!(
                "control.safe_off_secs ({}) is shorter than relay.min_off_time_secs ({})",
                secs(c.safe_off),
                secs(r.min_off_time)
            ));
        }
        // A single late or failed reading must not trigger SAFE mode.
        if c.sensor_timeout < self.sensor.read_interval * 2 {
            p.push(format!(
                "control.sensor_timeout_secs ({}) must be at least twice sensor.read_interval_secs ({})",
                secs(c.sensor_timeout),
                secs(self.sensor.read_interval)
            ));
        }

        if !MCP9808_ADDRESSES.contains(&self.sensor.address) {
            p.push(format!(
                "sensor.address {:#04x} is not an MCP9808 address ({:#04x}..={:#04x})",
                self.sensor.address,
                MCP9808_ADDRESSES.start(),
                MCP9808_ADDRESSES.end()
            ));
        }
        if r.gpio_pin > MAX_GPIO {
            p.push(format!(
                "relay.gpio_pin {} does not exist (0..={MAX_GPIO})",
                r.gpio_pin
            ));
        } else if self.sensor.i2c_bus == 1 && I2C1_PINS.contains(&r.gpio_pin) {
            p.push(format!(
                "relay.gpio_pin {} is an I²C bus 1 pin, used by the sensor",
                r.gpio_pin
            ));
        }

        if c.overrun_after_stops == 0 {
            p.push("control.overrun_after_stops must be at least 1".to_owned());
        }
        // A sample is the state of a control tick: it cannot come more often.
        if self.firestore.upload_interval < self.sensor.read_interval {
            p.push(format!(
                "firestore.upload_interval_secs ({}) is shorter than sensor.read_interval_secs ({})",
                secs(self.firestore.upload_interval),
                secs(self.sensor.read_interval)
            ));
        }
        if self.spool.max_spool_days == 0 {
            p.push("spool.max_spool_days must be at least 1".to_owned());
        }

        let f = &self.firestore;
        if f.project_id
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            p.push("firestore.project_id is empty: remove it to disable uploading".to_owned());
        }
        if f.database.trim().is_empty() || f.database.contains('/') {
            p.push(format!(
                "firestore.database {:?} must be non-empty and contain no '/'",
                f.database
            ));
        }
        if f.device_id.trim().is_empty() || f.device_id.contains('/') {
            p.push(format!(
                "firestore.device_id {:?} must be non-empty and contain no '/'",
                f.device_id
            ));
        }

        if let Some(url) = &self.healthcheck.ping_url {
            if !url.starts_with("https://") {
                p.push(format!(
                    "healthcheck.ping_url {url:?} must be an https:// URL"
                ));
            }
        }

        if p.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { problems: p })
        }
    }
}

fn secs(d: Duration) -> u64 {
    d.as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `toml` and returns the validation problems.
    fn problems(toml: &str) -> Vec<String> {
        match Config::from_toml(toml).unwrap().validate() {
            Ok(()) => Vec::new(),
            Err(e) => e.problems,
        }
    }

    fn assert_rejected(toml: &str, needle: &str) {
        let p = problems(toml);
        assert!(
            p.iter().any(|m| m.contains(needle)),
            "expected a problem mentioning {needle:?}, got {p:?}"
        );
    }

    #[test]
    fn defaults_are_valid() {
        assert_eq!(Config::default().validate(), Ok(()));
    }

    #[test]
    fn inverted_band_is_rejected() {
        assert_rejected(
            "[control]\ntarget_min = 13.0\ntarget_max = 11.0",
            "must exceed",
        );
    }

    #[test]
    fn dead_band_limit() {
        assert_rejected(
            "[control]\ntarget_min = 12.0\ntarget_max = 12.4",
            "at least 0.5",
        );
        // Exactly at the limit is allowed.
        assert!(problems("[control]\ntarget_min = 12.0\ntarget_max = 12.5").is_empty());
    }

    #[test]
    fn implausible_setpoints_are_rejected() {
        assert_rejected(
            "[control]\ntarget_min = -2.0\ntarget_max = 1.0",
            "target_min = -2",
        );
        // A Fahrenheit value typed by mistake.
        assert_rejected(
            "[control]\ntarget_min = 52.0\ntarget_max = 55.0",
            "target_max = 55",
        );
    }

    #[test]
    fn non_finite_setpoint_is_rejected() {
        assert_rejected("[control]\ntarget_max = nan", "finite");
    }

    #[test]
    fn zero_durations_are_rejected() {
        assert_rejected(
            "[relay]\nmin_off_time_secs = 0",
            "relay.min_off_time_secs must be",
        );
        assert_rejected(
            "[sensor]\nread_interval_secs = 0",
            "sensor.read_interval_secs must be",
        );
        assert_rejected(
            "[firestore]\nupload_interval_secs = 0",
            "firestore.upload_interval_secs must be",
        );
    }

    #[test]
    fn max_continuous_on_must_exceed_min_on() {
        assert_rejected(
            "[relay]\nmin_on_time_secs = 600\nmax_continuous_on_secs = 600",
            "max_continuous_on_secs",
        );
    }

    #[test]
    fn safe_phases_must_respect_relay_protections() {
        assert_rejected(
            "[control]\nsafe_off_secs = 60",
            "shorter than relay.min_off_time_secs",
        );
        assert_rejected(
            "[control]\nsafe_on_secs = 60",
            "shorter than relay.min_on_time_secs",
        );
    }

    #[test]
    fn sensor_timeout_must_cover_two_readings() {
        assert_rejected("[control]\nsensor_timeout_secs = 20", "twice");
        assert!(problems("[control]\nsensor_timeout_secs = 30").is_empty());
    }

    #[test]
    fn upload_interval_cannot_beat_the_control_tick() {
        assert_rejected(
            "[firestore]\nupload_interval_secs = 10",
            "shorter than sensor.read_interval_secs",
        );
        assert!(problems("[firestore]\nupload_interval_secs = 15").is_empty());
    }

    #[test]
    fn hardware_addresses_are_checked() {
        assert_rejected("[sensor]\naddress = 0x48", "not an MCP9808 address");
        assert_rejected("[relay]\ngpio_pin = 40", "does not exist");
        assert_rejected("[relay]\ngpio_pin = 2", "I²C bus 1 pin");
    }

    #[test]
    fn firestore_ids_are_checked() {
        assert_rejected("[firestore]\nproject_id = \" \"", "project_id is empty");
        assert_rejected("[firestore]\ndevice_id = \"a/b\"", "device_id");
    }

    #[test]
    fn healthcheck_url_must_be_https() {
        assert_rejected(
            "[healthcheck]\nping_url = \"http://hc-ping.com/x\"",
            "https://",
        );
        assert!(problems("[healthcheck]\nping_url = \"https://hc-ping.com/x\"").is_empty());
    }

    #[test]
    fn all_problems_are_reported_at_once() {
        let p = problems(
            "[control]\ntarget_min = 13.0\ntarget_max = 11.0\n[spool]\nmax_spool_days = 0",
        );
        assert_eq!(p.len(), 2, "{p:?}");
    }
}
