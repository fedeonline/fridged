//! Single-slot channel from the control thread to the uploader: a bounded
//! channel of capacity one that drops the oldest value, so the newest always
//! wins.
//!
//! The control thread publishes its state on every tick and never blocks
//! beyond a brief lock. The uploader takes the newest value once per round;
//! everything published in between is overwritten, by design.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

struct State<T> {
    value: Option<T>,
    closed: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    closed: Condvar,
}

impl<T> Shared<T> {
    /// A panic elsewhere must not take the channel down with it.
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Publisher<T>(Arc<Shared<T>>);
pub struct Subscriber<T>(Arc<Shared<T>>);

pub fn latest<T>() -> (Publisher<T>, Subscriber<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            value: None,
            closed: false,
        }),
        closed: Condvar::new(),
    });
    (Publisher(Arc::clone(&shared)), Subscriber(shared))
}

impl<T> Publisher<T> {
    /// Replaces the value not taken yet, if any.
    pub fn publish(&self, value: T) {
        self.0.lock().value = Some(value);
    }
}

impl<T> Drop for Publisher<T> {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.closed.notify_all();
    }
}

impl<T> Subscriber<T> {
    /// The newest value published since the last `take`, if any.
    pub fn take(&self) -> Option<T> {
        self.0.lock().value.take()
    }

    /// Waits up to `timeout` for the publisher to go away. Returns whether it
    /// did. Publishing does not wake it up.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.0.lock();
        loop {
            if s.closed {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            s = self
                .0
                .closed
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    const SHORT: Duration = Duration::from_millis(20);

    #[test]
    fn newest_value_wins() {
        let (tx, rx) = latest();
        assert_eq!(rx.take(), None);
        tx.publish(1);
        tx.publish(2);
        tx.publish(3);
        assert_eq!(rx.take(), Some(3));
        assert_eq!(rx.take(), None);
    }

    #[test]
    fn publish_never_blocks_even_without_a_reader() {
        let (tx, _rx) = latest();
        let start = Instant::now();
        for i in 0..100_000 {
            tx.publish(i);
        }
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_closed_times_out_while_the_publisher_lives() {
        let (tx, rx) = latest::<u8>();
        tx.publish(1);
        let start = Instant::now();
        assert!(!rx.wait_closed(Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(tx);
    }

    #[test]
    fn dropping_the_publisher_wakes_the_subscriber() {
        let (tx, rx) = latest::<u8>();
        let t = thread::spawn(move || rx.wait_closed(Duration::from_secs(10)));
        thread::sleep(SHORT);
        drop(tx);
        assert!(t.join().unwrap());
    }

    #[test]
    fn the_last_value_survives_closing() {
        let (tx, rx) = latest();
        tx.publish(7);
        drop(tx);
        assert!(rx.wait_closed(SHORT));
        assert_eq!(rx.take(), Some(7));
    }
}
