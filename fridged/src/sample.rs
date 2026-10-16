//! One telemetry record: what the controller saw and did on a tick.
//!
//! Serialized as one JSON object per line in the spool, with camelCase keys,
//! the shape the uploader turns into Firestore documents.

use serde::{Deserialize, Serialize};

use crate::control::{ControlState, Reason};
use crate::daemon::Tick;
use crate::relay::RelayState;
use crate::timestamp::Stamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Mode {
    Normal,
    Safe,
}

impl From<ControlState> for Mode {
    fn from(s: ControlState) -> Self {
        match s {
            ControlState::Normal => Self::Normal,
            ControlState::Safe { .. } => Self::Safe,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    /// Wall clock, ms since the Unix epoch. Trust it only with `clock_synced`.
    pub time_ms: i64,
    pub clock_synced: bool,
    pub boot_id: String,
    pub uptime_ms: u64,
    /// `None` when the sensor gave no reading on this tick.
    pub temperature: Option<f64>,
    /// What the relay actually is, after the compressor protections.
    pub relay: RelayState,
    /// What the controller asked for, and why.
    pub wanted: RelayState,
    pub reason: Reason,
    pub mode: Mode,
    pub overrun_fault: bool,
    /// The hysteresis band in force. `None` only in samples spooled before
    /// the band was recorded.
    #[serde(default)]
    pub target_min: Option<f64>,
    #[serde(default)]
    pub target_max: Option<f64>,
    /// `time_ms` was re-dated after NTP synchronized (see [`Sample::correct_time`]).
    #[serde(default)]
    pub time_corrected: bool,
}

impl Sample {
    pub fn new(tick: &Tick, stamp: Stamp) -> Self {
        Self {
            time_ms: stamp.time_ms,
            clock_synced: stamp.synced,
            boot_id: stamp.boot_id,
            uptime_ms: stamp.uptime_ms,
            temperature: tick.temperature.map(|t| t.get()),
            relay: tick.relay,
            wanted: tick.command.wanted,
            reason: tick.command.reason,
            mode: tick.mode.into(),
            overrun_fault: tick.overrun_fault,
            target_min: Some(tick.band.min),
            target_max: Some(tick.band.max),
            time_corrected: false,
        }
    }

    /// If this sample was taken before NTP synchronized, re-dates it from `now`
    /// (a synchronized stamp from the same boot). Returns whether it changed.
    pub fn correct_time(&mut self, now: &Stamp) -> bool {
        match self.stamp().correct(now) {
            Some(time_ms) => {
                self.time_ms = time_ms;
                self.clock_synced = true;
                self.time_corrected = true;
                true
            }
            None => false,
        }
    }

    pub fn stamp(&self) -> Stamp {
        Stamp {
            time_ms: self.time_ms,
            synced: self.clock_synced,
            boot_id: self.boot_id.clone(),
            uptime_ms: self.uptime_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::control::RelayCommand;
    use crate::relay::Outcome;
    use crate::sensor::Celsius;

    pub(crate) fn sample() -> Sample {
        let tick = Tick {
            temperature: Some(Celsius(12.0625)),
            command: RelayCommand {
                wanted: RelayState::On,
                reason: Reason::AboveMax,
            },
            outcome: Outcome::Held {
                hold: crate::relay::Hold::MinOffTime,
                until: Instant::now(),
            },
            relay: RelayState::Off,
            mode: ControlState::Normal,
            overrun_fault: false,
            band: crate::control::Band {
                min: 8.0,
                max: 11.0,
            },
        };
        let stamp = Stamp {
            time_ms: 1_760_000_000_000,
            synced: true,
            boot_id: "b".to_owned(),
            uptime_ms: 42,
        };
        Sample::new(&tick, stamp)
    }

    #[test]
    fn json_shape() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert_eq!(
            json,
            r#"{"timeMs":1760000000000,"clockSynced":true,"bootId":"b","uptimeMs":42,"temperature":12.0625,"relay":"OFF","wanted":"ON","reason":"aboveMax","mode":"NORMAL","overrunFault":false,"targetMin":8.0,"targetMax":11.0,"timeCorrected":false}"#
        );
    }

    #[test]
    fn round_trip() {
        let s = sample();
        let back: Sample = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn correct_time_redates_an_unsynced_sample() {
        let mut s = sample();
        s.clock_synced = false;
        s.time_ms = 1_000; // the clock restored at boot, far in the past
        s.uptime_ms = 10_000;
        let now = Stamp {
            time_ms: 1_760_000_600_000,
            synced: true,
            boot_id: "b".to_owned(),
            uptime_ms: 70_000,
        };
        assert!(s.correct_time(&now));
        assert_eq!(s.time_ms, 1_760_000_540_000);
        assert!(s.clock_synced && s.time_corrected);
        // Idempotent: a synced sample is left alone.
        assert!(!s.correct_time(&now));
    }

    #[test]
    fn older_lines_without_time_corrected_still_parse() {
        let json = serde_json::to_string(&sample())
            .unwrap()
            .replace(",\"timeCorrected\":false", "");
        assert!(!json.contains("timeCorrected"));
        assert!(
            !serde_json::from_str::<Sample>(&json)
                .unwrap()
                .time_corrected
        );
    }

    #[test]
    fn missing_reading_is_null() {
        let mut s = sample();
        s.temperature = None;
        assert!(
            serde_json::to_string(&s)
                .unwrap()
                .contains(r#""temperature":null"#)
        );
    }
}
