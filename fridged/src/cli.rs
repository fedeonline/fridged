//! Command-line arguments.
//!
//! Parsed with `lexopt` rather than `clap`: a handful of flags does not justify
//! `clap`'s build time and binary size on the Pi Zero.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config;

pub const USAGE: &str = "\
Usage: fridged [OPTIONS]

Keeps a refrigerator inside a temperature band and publishes telemetry.

Options:
  -c, --config <PATH>  configuration file [default: /etc/fridged/config.toml]
      --simulate       use a simulated sensor instead of the MCP9808
      --dry-run        log relay commands without driving the GPIO
      --once           take one reading, print it and exit
      --check          validate the configuration and exit
      --relay-off      force the relay OFF and exit (systemd ExecStopPost)
  -v, --verbose        debug logging (repeat for trace)
  -h, --help           print this help
  -V, --version        print the version";

/// What the user asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Run(Args),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub config: PathBuf,
    /// Simulated sensor: lets the daemon run on a machine without the MCP9808.
    pub simulate: bool,
    /// Relay commands are logged, the GPIO is never touched.
    pub dry_run: bool,
    /// One reading, then exit: for bring-up on the Pi.
    pub once: bool,
    /// Validate the configuration, then exit.
    pub check: bool,
    /// Force the relay OFF, then exit: run by systemd after every stop, so the
    /// relay is OFF even after a SIGKILL.
    pub relay_off: bool,
    /// 0 = info, 1 = debug, 2 or more = trace.
    pub verbosity: u8,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            config: PathBuf::from(config::DEFAULT_PATH),
            simulate: false,
            dry_run: false,
            once: false,
            check: false,
            relay_off: false,
            verbosity: 0,
        }
    }
}

impl Command {
    /// Parses the process arguments.
    pub fn from_env() -> Result<Self, lexopt::Error> {
        Self::parse(lexopt::Parser::from_env())
    }

    /// Parses `args`, which must not include the program name.
    pub fn from_args<I>(args: I) -> Result<Self, lexopt::Error>
    where
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        Self::parse(lexopt::Parser::from_args(args))
    }

    fn parse(mut parser: lexopt::Parser) -> Result<Self, lexopt::Error> {
        use lexopt::prelude::*;

        let mut args = Args::default();
        while let Some(arg) = parser.next()? {
            match arg {
                Short('c') | Long("config") => args.config = parser.value()?.into(),
                Long("simulate") => args.simulate = true,
                Long("dry-run") => args.dry_run = true,
                Long("once") => args.once = true,
                Long("check") => args.check = true,
                Long("relay-off") => args.relay_off = true,
                Short('v') | Long("verbose") => args.verbosity = args.verbosity.saturating_add(1),
                Short('h') | Long("help") => return Ok(Self::Help),
                Short('V') | Long("version") => return Ok(Self::Version),
                _ => return Err(arg.unexpected()),
            }
        }
        let modes = [args.once, args.check, args.relay_off];
        if modes.iter().filter(|&&m| m).count() > 1 {
            return Err("--once, --check and --relay-off exclude each other".into());
        }
        Ok(Self::Run(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Args {
        match Command::from_args(args).unwrap() {
            Command::Run(a) => a,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_gives_defaults() {
        assert_eq!(run(&[]), Args::default());
        assert_eq!(
            Args::default().config,
            PathBuf::from("/etc/fridged/config.toml")
        );
    }

    #[test]
    fn all_flags() {
        let a = run(&[
            "--config",
            "x.toml",
            "--simulate",
            "--dry-run",
            "--once",
            "-vv",
        ]);
        assert_eq!(
            a,
            Args {
                config: "x.toml".into(),
                simulate: true,
                dry_run: true,
                once: true,
                check: false,
                relay_off: false,
                verbosity: 2,
            }
        );
    }

    #[test]
    fn exit_modes() {
        assert!(run(&["--check"]).check);
        assert!(run(&["--relay-off"]).relay_off);
        assert!(Command::from_args(["--check", "--once"]).is_err());
        assert!(Command::from_args(["--relay-off", "--check"]).is_err());
    }

    #[test]
    fn config_value_forms() {
        assert_eq!(run(&["-c", "a.toml"]).config, PathBuf::from("a.toml"));
        assert_eq!(run(&["--config=b.toml"]).config, PathBuf::from("b.toml"));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(Command::from_args(["--help"]).unwrap(), Command::Help);
        assert_eq!(Command::from_args(["-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn errors() {
        assert!(Command::from_args(["--bogus"]).is_err());
        assert!(Command::from_args(["stray"]).is_err());
        // --config without a value.
        assert!(Command::from_args(["--config"]).is_err());
    }
}
