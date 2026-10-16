//! systemd notifications (`sd_notify(3)` protocol), without libsystemd: one
//! datagram per message to the socket named in `$NOTIFY_SOCKET`.
//!
//! - `READY=1` once the relay is forced OFF and the loop is about to start
//!   (`Type=notify`: dependent units wait for it).
//! - `WATCHDOG=1` from the control loop on every tick: if the loop hangs,
//!   systemd kills and restarts the service after `WatchdogSec`.
//! - `STATUS=...`, shown by `systemctl status`.
//!
//! Outside systemd every call is a no-op.

use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

pub struct Notifier {
    socket: Option<(UnixDatagram, SocketAddr)>,
    watchdog: Option<Duration>,
}

impl Notifier {
    /// From `$NOTIFY_SOCKET`, `$WATCHDOG_USEC` and `$WATCHDOG_PID`.
    pub fn from_env() -> Self {
        let var = |name| std::env::var(name).ok();
        Self::new(
            var("NOTIFY_SOCKET").as_deref(),
            var("WATCHDOG_USEC").as_deref(),
            var("WATCHDOG_PID").as_deref(),
            std::process::id(),
        )
    }

    pub fn new(
        notify_socket: Option<&str>,
        watchdog_usec: Option<&str>,
        watchdog_pid: Option<&str>,
        pid: u32,
    ) -> Self {
        let socket = notify_socket.and_then(|s| match open(s) {
            Ok(pair) => Some(pair),
            Err(e) => {
                log::warn!("NOTIFY_SOCKET {s:?} unusable: {e}");
                None
            }
        });
        // The watchdog is for us only if no PID is given or it is ours.
        let ours = watchdog_pid.is_none_or(|p| p.parse() == Ok(pid));
        let watchdog = watchdog_usec
            .and_then(|u| u.parse::<u64>().ok())
            .filter(|&u| u > 0 && ours)
            .map(Duration::from_micros);
        Self { socket, watchdog }
    }

    /// The `WatchdogSec` systemd expects pings within, if any.
    pub fn watchdog_interval(&self) -> Option<Duration> {
        self.watchdog
    }

    pub fn ready(&self) {
        self.send("READY=1");
    }

    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }

    /// Pings the watchdog, if one is configured.
    pub fn watchdog(&self) {
        if self.watchdog.is_some() {
            self.send("WATCHDOG=1");
        }
    }

    pub fn status(&self, status: &str) {
        // One line: a newline would start another assignment.
        self.send(&format!("STATUS={}", status.replace('\n', " ")));
    }

    fn send(&self, message: &str) {
        if let Some((socket, addr)) = &self.socket {
            // Non-blocking: never stall the control loop on systemd.
            if let Err(e) = socket.send_to_addr(message.as_bytes(), addr) {
                log::debug!("sd_notify {message:?} failed: {e}");
            }
        }
    }
}

fn open(name: &str) -> std::io::Result<(UnixDatagram, SocketAddr)> {
    let addr = if let Some(abstract_name) = name.strip_prefix('@') {
        abstract_addr(abstract_name)?
    } else {
        SocketAddr::from_pathname(name)?
    };
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    Ok((socket, addr))
}

#[cfg(target_os = "linux")]
fn abstract_addr(name: &str) -> std::io::Result<SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    SocketAddr::from_abstract_name(name.as_bytes())
}

#[cfg(not(target_os = "linux"))]
fn abstract_addr(_name: &str) -> std::io::Result<SocketAddr> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "abstract sockets are Linux-only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::tests::TempDir;

    fn listen(dir: &TempDir) -> (UnixDatagram, String) {
        let path = dir.0.join("notify.sock");
        let socket = UnixDatagram::bind(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        (socket, path.to_string_lossy().into_owned())
    }

    fn recv(socket: &UnixDatagram) -> String {
        let mut buf = [0u8; 256];
        let n = socket.recv(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn messages_reach_the_socket() {
        let dir = TempDir::new("notify");
        let (server, path) = listen(&dir);
        let n = Notifier::new(Some(&path), Some("60000000"), None, 1);
        n.ready();
        n.watchdog();
        n.status("12.06 °C\ncompressor OFF");
        n.stopping();
        assert_eq!(recv(&server), "READY=1");
        assert_eq!(recv(&server), "WATCHDOG=1");
        assert_eq!(recv(&server), "STATUS=12.06 °C compressor OFF");
        assert_eq!(recv(&server), "STOPPING=1");
    }

    #[test]
    fn watchdog_settings() {
        let n = Notifier::new(None, Some("60000000"), None, 42);
        assert_eq!(n.watchdog_interval(), Some(Duration::from_secs(60)));
        // Meant for another process.
        let n = Notifier::new(None, Some("60000000"), Some("7"), 42);
        assert_eq!(n.watchdog_interval(), None);
        let n = Notifier::new(None, Some("60000000"), Some("42"), 42);
        assert!(n.watchdog_interval().is_some());
        assert_eq!(
            Notifier::new(None, Some("0"), None, 1).watchdog_interval(),
            None
        );
        assert_eq!(
            Notifier::new(None, Some("x"), None, 1).watchdog_interval(),
            None
        );
    }

    #[test]
    fn without_systemd_everything_is_a_no_op() {
        let n = Notifier::new(None, None, None, 1);
        n.ready();
        n.watchdog();
        n.status("x");
        assert_eq!(n.watchdog_interval(), None);
    }

    #[test]
    fn no_watchdog_pings_without_a_watchdog() {
        let dir = TempDir::new("notify-nowd");
        let (server, path) = listen(&dir);
        let n = Notifier::new(Some(&path), None, None, 1);
        n.watchdog();
        n.ready();
        assert_eq!(recv(&server), "READY=1");
    }
}
