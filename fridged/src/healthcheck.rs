//! Dead man's switch on healthchecks.io: a ping after every upload round.
//!
//! Success pings the check URL, failure pings `<url>/fail`; both carry a short
//! text that healthchecks.io shows in its log. If pings stop altogether (the Pi
//! is off, the network is down, the daemon is dead), healthchecks.io alerts on
//! its own once the grace time runs out.
//!
//! Pinging must never get in the way: short timeout, failures only logged.
//! Called from the uploader thread only.

use std::time::Duration;

use ureq::Agent;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Where upload rounds are reported.
pub trait Heartbeat: Send {
    fn success(&mut self, message: &str);
    fn failure(&mut self, message: &str);
}

pub struct HealthchecksIo {
    agent: Agent,
    url: String,
    /// The last ping failed: log the recovery once, not every success.
    unreachable: bool,
}

impl HealthchecksIo {
    pub fn new(url: &str) -> Self {
        let config = Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build();
        Self {
            agent: Agent::new_with_config(config),
            url: url.trim_end_matches('/').to_owned(),
            unreachable: false,
        }
    }

    fn ping(&mut self, url: &str, body: &str) {
        match self.agent.post(url).send(body) {
            Ok(_) => {
                if self.unreachable {
                    log::info!("healthchecks.io reachable again");
                    self.unreachable = false;
                }
            }
            Err(e) => {
                if !self.unreachable {
                    log::warn!("healthchecks.io ping failed: {e}");
                    self.unreachable = true;
                }
            }
        }
    }
}

impl Heartbeat for HealthchecksIo {
    fn success(&mut self, message: &str) {
        let url = self.url.clone();
        self.ping(&url, message);
    }

    fn failure(&mut self, message: &str) {
        let url = format!("{}/fail", self.url);
        self.ping(&url, message);
    }
}
