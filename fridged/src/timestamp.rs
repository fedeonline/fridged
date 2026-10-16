//! Timestamps for telemetry samples.
//!
//! The Pi Zero has no real-time clock: after a reboot without network its wall
//! clock is wrong (restored from the last shutdown) until NTP synchronizes.
//! So every sample records the wall clock *and* whether it was synchronized,
//! plus the monotonic time since boot and the boot id. Once the clock is
//! synchronized, samples from the same boot can be re-dated exactly (see
//! [`Stamp::correct`]).
//!
//! The compressor timers never use any of this: they run on `Instant`.

use std::time::{SystemTime, UNIX_EPOCH};

/// When something happened, as far as this machine can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// Wall clock, milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Whether the wall clock was synchronized (NTP) at that moment.
    pub synced: bool,
    /// Identifies the boot: uptimes are only comparable within one.
    pub boot_id: String,
    /// Monotonic milliseconds since boot.
    pub uptime_ms: u64,
}

impl Stamp {
    /// Re-dates an unsynchronized stamp from a synchronized one taken later in
    /// the same boot: the uptime difference is exact, whatever the wall clock
    /// said back then. Returns `None` when that is not possible.
    pub fn correct(&self, now: &Stamp) -> Option<i64> {
        if self.synced || !now.synced || self.boot_id != now.boot_id {
            return None;
        }
        let ago = now.uptime_ms.checked_sub(self.uptime_ms)?;
        Some(now.time_ms - i64::try_from(ago).ok()?)
    }
}

/// The machine's clocks.
#[derive(Debug, Clone)]
pub struct SystemTimeSource {
    boot_id: String,
}

impl SystemTimeSource {
    pub fn new() -> Self {
        Self { boot_id: boot_id() }
    }
}

impl Default for SystemTimeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemTimeSource {
    pub fn stamp(&self) -> Stamp {
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Stamp {
            time_ms,
            synced: clock_synced(),
            boot_id: self.boot_id.clone(),
            uptime_ms: uptime_ms(),
        }
    }
}

/// `CLOCK_MONOTONIC`: on Linux, time since boot (the Pi never suspends).
fn uptime_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC always exists.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1000 + nanos / 1_000_000
}

/// What `timedatectl` reports as "System clock synchronized": the kernel's
/// clock state is not `TIME_ERROR` (unsynchronized) once NTP has disciplined it.
#[cfg(target_os = "linux")]
fn clock_synced() -> bool {
    // SAFETY: a zeroed timex with `modes = 0` only reads the clock state.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut tx) };
    state >= 0 && state != libc::TIME_ERROR
}

/// Development machines keep their clocks synchronized.
#[cfg(not(target_os = "linux"))]
fn clock_synced() -> bool {
    true
}

#[cfg(target_os = "linux")]
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| fallback_boot_id())
}

#[cfg(not(target_os = "linux"))]
fn boot_id() -> String {
    fallback_boot_id()
}

/// Unique enough to never match another boot's id.
fn fallback_boot_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("pid{}-{now}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(time_ms: i64, synced: bool, boot: &str, uptime_ms: u64) -> Stamp {
        Stamp {
            time_ms,
            synced,
            boot_id: boot.to_owned(),
            uptime_ms,
        }
    }

    #[test]
    fn unsynced_stamp_is_redated_from_a_later_synced_one() {
        // Booted with the clock restored to 08:00; NTP later says it is 10:00.
        let then = stamp(8 * 3_600_000, false, "a", 60_000);
        let now = stamp(10 * 3_600_000, true, "a", 60_000 + 900_000);
        assert_eq!(then.correct(&now), Some(10 * 3_600_000 - 900_000));
    }

    #[test]
    fn no_correction_across_boots_or_without_sync() {
        let then = stamp(1_000, false, "a", 5_000);
        assert_eq!(then.correct(&stamp(9_000, true, "b", 6_000)), None);
        assert_eq!(then.correct(&stamp(9_000, false, "a", 6_000)), None);
        // Already synced: nothing to correct.
        let synced = stamp(1_000, true, "a", 5_000);
        assert_eq!(synced.correct(&stamp(9_000, true, "a", 6_000)), None);
        // Uptime going backwards cannot happen within a boot; refuse anyway.
        assert_eq!(then.correct(&stamp(9_000, true, "a", 4_000)), None);
    }

    #[test]
    fn system_source_is_sane() {
        let src = SystemTimeSource::new();
        let a = src.stamp();
        let b = src.stamp();
        assert!(a.time_ms > 1_600_000_000_000, "wall clock after 2020");
        assert!(b.uptime_ms >= a.uptime_ms);
        assert_eq!(a.boot_id, b.boot_id);
        assert!(!a.boot_id.is_empty());
    }
}
