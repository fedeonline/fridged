//! Daemon configuration, loaded from a TOML file.
//!
//! Every field has a default, so an empty file is a valid configuration: the
//! file only needs to list what differs from the defaults. Unknown keys are
//! rejected, so a typo cannot silently fall back to a default.
//!
//! Durations are written as whole seconds in keys ending in `_secs` and exposed
//! as [`Duration`]. Temperatures are in degrees Celsius.
//!
//! Semantic checks (e.g. `target_min < target_max`) live in [`validate`];
//! [`Config::load`] runs them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod validate;

pub use validate::{MIN_DEAD_BAND, SETPOINT_RANGE, ValidationError, check_band};

/// Default location of the configuration file on the Pi.
pub const DEFAULT_PATH: &str = "/etc/fridged/config.toml";

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub sensor: SensorConfig,
    pub relay: RelayConfig,
    pub control: ControlConfig,
    pub spool: SpoolConfig,
    pub firestore: FirestoreConfig,
    pub healthcheck: HealthcheckConfig,
}

/// MCP9808 on I²C.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SensorConfig {
    /// `/dev/i2c-<bus>`. Bus 1 is the one on the GPIO header.
    pub i2c_bus: u8,
    /// 7-bit address; TOML accepts hex, e.g. `0x18`.
    pub address: u16,
    /// Time between two readings: one control-loop tick.
    #[serde(rename = "read_interval_secs", with = "secs")]
    pub read_interval: Duration,
}

impl Default for SensorConfig {
    fn default() -> Self {
        Self {
            i2c_bus: 1,
            address: 0x18,
            read_interval: Duration::from_secs(15),
        }
    }
}

/// Relay driving the compressor, and the protections that always apply to it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    /// BCM GPIO number.
    pub gpio_pin: u8,
    /// `false` when the relay switches on with its input HIGH, as solid-state
    /// relays with a DC input do; many electromechanical relay boards for the
    /// Pi are the other way round.
    pub active_low: bool,
    /// Minimum OFF time before a restart: lets the system pressure equalize.
    #[serde(rename = "min_off_time_secs", with = "secs")]
    pub min_off_time: Duration,
    /// Minimum ON time once started: avoids short-cycling.
    #[serde(rename = "min_on_time_secs", with = "secs")]
    pub min_on_time: Duration,
    /// Longest continuous run before a forced stop. Reaching it usually means a
    /// fault (door left open, refrigerant loss) rather than a warm fridge.
    #[serde(rename = "max_continuous_on_secs", with = "secs")]
    pub max_continuous_on: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            // Fotek SSR-25 DA: + on BCM 26 (pin 37), - on GND (pin 39).
            gpio_pin: 26,
            active_low: false,
            min_off_time: Duration::from_secs(5 * 60),
            min_on_time: Duration::from_secs(2 * 60),
            max_continuous_on: Duration::from_secs(2 * 60 * 60),
        }
    }
}

/// Hysteresis band and the fallback used when the sensor is unavailable.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlConfig {
    /// Compressor turns OFF at or below this temperature (°C).
    pub target_min: f64,
    /// Compressor turns ON above this temperature (°C).
    pub target_max: f64,
    /// How long without a valid reading before switching to SAFE mode.
    #[serde(rename = "sensor_timeout_secs", with = "secs")]
    pub sensor_timeout: Duration,
    /// SAFE mode duty cycle: ON phase.
    #[serde(rename = "safe_on_secs", with = "secs")]
    pub safe_on: Duration,
    /// SAFE mode duty cycle: OFF phase.
    #[serde(rename = "safe_off_secs", with = "secs")]
    pub safe_off: Duration,
    /// A stop after `relay.max_continuous_on` is a rest, after which cooling
    /// resumes. This many in a row without reaching `target_min` is an overrun
    /// fault (door left open, refrigerant loss).
    pub overrun_after_stops: u32,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            // Beer storage, the user's setting: cooler than a cellar for ales,
            // warmer than a food fridge.
            target_min: 8.0,
            target_max: 11.0,
            sensor_timeout: Duration::from_secs(60),
            // Blind duty cycle, deliberately on the low side (20%): at a
            // cellar setpoint the compressor needs little run time, and
            // overcooling is worse than a few degrees too warm, since frozen
            // bottles burst. To be calibrated against the real fridge.
            safe_on: Duration::from_secs(10 * 60),
            safe_off: Duration::from_secs(40 * 60),
            // About 6 h of nearly continuous running without result: the long
            // pull-down after a band change took 2 rests, not 3.
            overrun_after_stops: 3,
        }
    }
}

/// Local JSONL spool that buffers samples while the network is down.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpoolConfig {
    pub path: PathBuf,
    /// Samples older than this are dropped, uploaded or not.
    pub max_spool_days: u32,
}

impl Default for SpoolConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("/var/lib/fridged/spool.jsonl"),
            max_spool_days: 7,
        }
    }
}

/// Cloud Firestore upload.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FirestoreConfig {
    /// GCP project. `None` disables uploading: the daemon still regulates and
    /// spools locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Firestore database id; projects can have several.
    pub database: String,
    /// Service-account JSON key. Never committed to the repository. The
    /// default is where systemd's `LoadCredential=` puts it for the service.
    pub credentials_path: PathBuf,
    /// Document id under which this fridge publishes.
    pub device_id: String,
    /// Every this often one sample of the controller's state is recorded and
    /// the spool is uploaded. The compressor is still controlled every
    /// `sensor.read_interval_secs`.
    #[serde(rename = "upload_interval_secs", with = "secs")]
    pub upload_interval: Duration,
}

impl Default for FirestoreConfig {
    fn default() -> Self {
        Self {
            project_id: None,
            database: String::from("(default)"),
            credentials_path: PathBuf::from(
                "/run/credentials/fridged.service/service-account.json",
            ),
            device_id: String::from("fridge"),
            upload_interval: Duration::from_secs(5 * 60),
        }
    }
}

/// healthchecks.io dead man's switch.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthcheckConfig {
    /// Ping URL of the check, pinged after every upload round (`/fail` appended
    /// on failure). `None` disables it. Anyone with the URL can ping the check:
    /// keep it out of version control.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ping_url: Option<String>,
}

impl Config {
    /// Reads, parses and validates the configuration file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config = Self::from_toml(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config.validate().map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        })?;
        Ok(config)
    }

    /// Parses a configuration from TOML text, without validating it.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// Why a configuration file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("cannot parse {}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid configuration in {}", path.display())]
    Invalid {
        path: PathBuf,
        source: ValidationError,
    },
}

/// (De)serializes a [`Duration`] as a whole number of seconds.
mod secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../deploy/config.example.toml");

    #[test]
    fn example_is_valid() {
        let cfg = Config::from_toml(EXAMPLE).unwrap();
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn example_matches_the_defaults() {
        // The example documents the defaults: changing one means updating both.
        assert_eq!(Config::from_toml(EXAMPLE).unwrap(), Config::default());
    }

    #[test]
    fn example_lists_every_key() {
        // A field added to the structs but not to the example fails here.
        let example: toml::Table = toml::from_str(EXAMPLE).unwrap();
        let defaults = toml::Table::try_from(Config::default()).unwrap();
        assert_eq!(keys(&example), keys(&defaults));
    }

    /// `section.key` for every key, sorted.
    fn keys(table: &toml::Table) -> Vec<String> {
        let mut out: Vec<String> = table
            .iter()
            .flat_map(|(section, value)| {
                let inner = value.as_table().expect("top level holds only sections");
                inner.keys().map(move |k| format!("{section}.{k}"))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_overrides_only_listed_keys() {
        let cfg = Config::from_toml(
            r#"
            [control]
            target_max = 14.5

            [relay]
            min_off_time_secs = 600
            "#,
        )
        .unwrap();
        assert_eq!(cfg.control.target_max, 14.5);
        assert_eq!(cfg.control.target_min, ControlConfig::default().target_min);
        assert_eq!(cfg.relay.min_off_time, Duration::from_secs(600));
        assert_eq!(cfg.relay.min_on_time, RelayConfig::default().min_on_time);
    }

    #[test]
    fn hex_address_is_accepted() {
        let cfg = Config::from_toml("[sensor]\naddress = 0x19").unwrap();
        assert_eq!(cfg.sensor.address, 0x19);
    }

    #[test]
    fn unknown_key_is_rejected() {
        // A typo must not silently fall back to the default.
        assert!(Config::from_toml("[relay]\nmin_off_time = 600").is_err());
        assert!(Config::from_toml("[contrl]\ntarget_max = 6.0").is_err());
    }

    #[test]
    fn negative_duration_is_rejected() {
        assert!(Config::from_toml("[relay]\nmin_on_time_secs = -1").is_err());
    }

    #[test]
    fn missing_file_reports_the_path() {
        let err = Config::load(Path::new("/nonexistent/fridged.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
        assert!(err.to_string().contains("/nonexistent/fridged.toml"));
    }

    #[test]
    fn load_rejects_an_invalid_file() {
        let path = std::env::temp_dir().join(format!("fridged-test-{}.toml", std::process::id()));
        fs::write(&path, "[control]\ntarget_min = 13.0\ntarget_max = 11.0\n").unwrap();
        let result = Config::load(&path);
        fs::remove_file(&path).unwrap();
        assert!(
            matches!(result, Err(ConfigError::Invalid { .. })),
            "{result:?}"
        );
    }
}
