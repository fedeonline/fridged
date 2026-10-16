//! Relay for `--dry-run`: logs what it would do, never touches the GPIO.

use super::{Relay, RelayError, RelayState};

#[derive(Debug, Default)]
pub struct DryRunRelay {
    state: Option<RelayState>,
}

impl Relay for DryRunRelay {
    fn set(&mut self, state: RelayState) -> Result<(), RelayError> {
        if self.state != Some(state) {
            log::info!("dry run: relay would be {state}");
            self.state = Some(state);
        }
        Ok(())
    }
}
