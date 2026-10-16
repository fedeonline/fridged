//! Relay that records what it is told, for tests.

use std::io;

use super::{Relay, RelayError, RelayState};

#[derive(Debug, Clone, Default)]
pub struct FakeRelay {
    /// Every `set` that succeeded, in order.
    pub history: Vec<RelayState>,
    /// While true, `set` fails and changes nothing.
    pub broken: bool,
}

impl FakeRelay {
    /// The last state successfully set, if any.
    pub fn state(&self) -> Option<RelayState> {
        self.history.last().copied()
    }

    /// The number of times the output actually changed.
    pub fn transitions(&self) -> usize {
        self.history.windows(2).filter(|w| w[0] != w[1]).count()
    }
}

impl Relay for FakeRelay {
    fn set(&mut self, state: RelayState) -> Result<(), RelayError> {
        if self.broken {
            return Err(RelayError::Gpio(Box::new(io::Error::other(
                "injected GPIO failure",
            ))));
        }
        self.history.push(state);
        Ok(())
    }
}
