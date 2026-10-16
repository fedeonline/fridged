//! Termination signals.
//!
//! SIGTERM (systemd stop) and SIGINT (Ctrl-C) only set a flag: the control loop
//! sees it, turns the relay OFF and exits. Nothing is done inside the handler
//! itself, where almost nothing is async-signal-safe.
//!
//! SIGHUP is caught too, and ignored: its default action would terminate the
//! process on the spot, leaving the relay in whatever state it was.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

/// How often [`Shutdown::sleep`] checks the flag: the longest a stop request
/// waits before the loop notices it.
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Default)]
pub struct Shutdown {
    stop: Arc<AtomicBool>,
    hangup: Arc<AtomicBool>,
}

impl Shutdown {
    /// Registers the SIGTERM, SIGINT and SIGHUP handlers.
    pub fn install() -> io::Result<Self> {
        let shutdown = Self::default();
        for signal in [SIGTERM, SIGINT] {
            signal_hook::flag::register(signal, Arc::clone(&shutdown.stop))?;
        }
        signal_hook::flag::register(SIGHUP, Arc::clone(&shutdown.hangup))?;
        Ok(shutdown)
    }

    pub fn requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Requests a shutdown from inside the process.
    pub fn request(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether a SIGHUP arrived since the last call.
    pub fn take_hangup(&self) -> bool {
        self.hangup.swap(false, Ordering::SeqCst)
    }

    /// Sleeps for `duration`, waking early if a shutdown is requested.
    /// Returns `true` if it was.
    pub fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.requested() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(POLL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_set_their_flags() {
        let shutdown = Shutdown::install().unwrap();
        assert!(!shutdown.requested());
        signal_hook::low_level::raise(SIGHUP).unwrap();
        // Still alive, and not stopping.
        assert!(shutdown.take_hangup());
        assert!(!shutdown.take_hangup());
        assert!(!shutdown.requested());
        signal_hook::low_level::raise(SIGTERM).unwrap();
        assert!(shutdown.requested());
    }

    #[test]
    fn sleep_runs_to_the_end_without_a_request() {
        let shutdown = Shutdown::default();
        let start = Instant::now();
        assert!(!shutdown.sleep(Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn sleep_wakes_early_on_request() {
        let shutdown = Shutdown::default();
        let other = shutdown.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            other.request();
        });
        let start = Instant::now();
        assert!(shutdown.sleep(Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_secs(2));
        t.join().unwrap();
    }
}
