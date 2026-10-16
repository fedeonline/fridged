//! Top-level error type.
//!
//! Each subsystem has its own error type next to its code (`ConfigError`,
//! and later the sensor, relay, spool and Firestore errors); `Error` gathers
//! the ones that can stop the daemon. Errors the daemon recovers from, such as
//! a failed sensor reading that leads to SAFE mode, are handled where they
//! occur and do not need to reach this type.

use std::error::Error as StdError;
use std::fmt::Write;

use crate::config::ConfigError;
use crate::relay::RelayError;
use crate::sensor::SensorError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Only fatal where a reading is the whole point, e.g. `--once`.
    #[error("sensor unavailable")]
    Sensor(#[from] SensorError),
    /// Without the relay there is nothing to control.
    #[error("relay unavailable")]
    Relay(#[from] RelayError),
    #[error("cannot install signal handlers")]
    Signals(#[source] std::io::Error),
    #[error("cannot start the uploader thread")]
    Uploader(#[source] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `sysexits.h` codes. A distinct code for a bad configuration lets the systemd
/// unit stop restarting (`RestartPreventExitStatus=`): retrying cannot fix it.
pub const EX_USAGE: u8 = 64;
pub const EX_UNAVAILABLE: u8 = 69;
pub const EX_OSERR: u8 = 71;
pub const EX_CONFIG: u8 = 78;

impl Error {
    /// Process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => EX_CONFIG,
            Self::Sensor(_) | Self::Relay(_) => EX_UNAVAILABLE,
            Self::Signals(_) | Self::Uploader(_) => EX_OSERR,
        }
    }
}

/// Formats an error with its whole `source()` chain on one line, e.g.
/// `invalid configuration in /etc/fridged/config.toml: control.target_max ...`.
///
/// `Display` of each type only describes its own level, so logging an error
/// without this loses the cause.
pub fn chain(err: &dyn StdError) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let _ = write!(out, ": {cause}");
        source = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::Config;

    #[test]
    fn chain_includes_every_level() {
        let err: Error = Config::load(Path::new("/nonexistent/fridged.toml"))
            .unwrap_err()
            .into();
        let line = chain(&err);
        assert!(
            line.starts_with("cannot read /nonexistent/fridged.toml: "),
            "{line}"
        );
        // The io::Error at the bottom of the chain.
        assert!(line.contains("No such file"), "{line}");
    }

    #[test]
    fn transparent_variant_does_not_repeat_its_source() {
        let err: Error = Config::load(Path::new("/nonexistent/fridged.toml"))
            .unwrap_err()
            .into();
        assert_eq!(chain(&err).matches("cannot read").count(), 1);
    }
}
