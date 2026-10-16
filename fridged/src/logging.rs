//! Minimal `log` backend writing to stderr.
//!
//! Under systemd, stderr goes to the journal: each line then starts with a
//! `<N>` syslog priority prefix (see `sd-daemon(3)`), so `journalctl -p warning`
//! works and the journal adds the timestamps. Run by hand, lines carry a
//! plain level name instead.

use std::io::Write;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct StderrLogger {
    journald: bool,
    /// Whether other crates (ureq, rustls, ...) log below info too.
    verbose_dependencies: bool,
}

/// Installs the logger. `verbosity`: 0 = info, 1 = debug for `fridged` only,
/// 2+ = trace for everything, dependencies included.
///
/// Call once, at startup, before spawning threads.
pub fn init(verbosity: u8) {
    // systemd sets JOURNAL_STREAM when stderr is connected to the journal.
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let logger: &'static StderrLogger = Box::leak(Box::new(StderrLogger {
        journald,
        verbose_dependencies: verbosity >= 2,
    }));
    // Ignored if a logger is already installed (e.g. called twice in tests).
    let _ = log::set_logger(logger);
    log::set_max_level(level_filter(verbosity));
}

fn level_filter(verbosity: u8) -> LevelFilter {
    match verbosity {
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let ours = metadata.target() == "fridged" || metadata.target().starts_with("fridged::");
        metadata.level() <= log::max_level()
            && (ours || self.verbose_dependencies || metadata.level() <= Level::Info)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format_line(
            record.level(),
            record.target(),
            record.args(),
            self.journald,
        );
        // Nothing sensible to do if stderr is gone.
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

fn format_line(level: Level, target: &str, msg: &std::fmt::Arguments, journald: bool) -> String {
    // `fridged::config` → `config`; the binary itself logs as `fridged`.
    let target = target.strip_prefix("fridged::").unwrap_or(target);
    if journald {
        format!("<{}>{target}: {msg}", syslog_priority(level))
    } else {
        format!("{level:<5} {target}: {msg}")
    }
}

/// Syslog priorities as understood by journald.
fn syslog_priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_format() {
        let line = format_line(
            Level::Warn,
            "fridged::config",
            &format_args!("x = {}", 1),
            false,
        );
        assert_eq!(line, "WARN  config: x = 1");
    }

    #[test]
    fn journald_format_has_priority_prefix() {
        let line = format_line(Level::Error, "fridged", &format_args!("boom"), true);
        assert_eq!(line, "<3>fridged: boom");
        let line = format_line(Level::Info, "fridged::relay", &format_args!("on"), true);
        assert_eq!(line, "<6>relay: on");
    }

    #[test]
    fn dependencies_stay_at_info_unless_very_verbose() {
        let meta = |target, level| Metadata::builder().target(target).level(level).build();
        log::set_max_level(LevelFilter::Trace);
        let quiet = StderrLogger {
            journald: false,
            verbose_dependencies: false,
        };
        assert!(quiet.enabled(&meta("fridged::uploader", Level::Debug)));
        assert!(quiet.enabled(&meta("fridged", Level::Debug)));
        assert!(!quiet.enabled(&meta("ureq::run", Level::Debug)));
        assert!(!quiet.enabled(&meta("fridgedx", Level::Debug)));
        assert!(quiet.enabled(&meta("ureq::run", Level::Warn)));
        let loud = StderrLogger {
            journald: false,
            verbose_dependencies: true,
        };
        assert!(loud.enabled(&meta("rustls::client", Level::Trace)));
    }

    #[test]
    fn verbosity_levels() {
        assert_eq!(level_filter(0), LevelFilter::Info);
        assert_eq!(level_filter(1), LevelFilter::Debug);
        assert_eq!(level_filter(5), LevelFilter::Trace);
    }
}
