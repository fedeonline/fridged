//! The uploader thread: once per `interval`, records the controller's latest
//! state and ships the spool to Firestore.
//!
//! All disk and network I/O for telemetry happens here, never on the control
//! thread, which only publishes its state into a single-slot channel. Each
//! round, on a fixed grid (no drift):
//!
//! 1. the newest state is appended (and fsynced) to the spool: exactly one
//!    sample per round, so they survive a crash or a network outage;
//! 2. the spool is trimmed to its retention limits;
//! 3. if the wall clock is synchronized, the spool is uploaded in batches,
//!    oldest first; a batch leaves the spool only once Firestore accepted it;
//! 4. the remote settings (the band) are read; a new valid band is saved and
//!    published to the control thread;
//! 5. the round is reported to the heartbeat (healthchecks.io).
//!
//! A failed upload is simply retried at the next round, with whatever is still
//! in the spool: at one round every few minutes, no backoff is needed.
//!
//! No panics here: with `panic = "abort"` a panic in this thread would also
//! kill the control loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::channel::{Publisher, Subscriber};
use crate::control::Band;
use crate::firestore::client::{Client, MAX_BATCH, UploadError};
use crate::healthcheck::Heartbeat;
use crate::sample::Sample;
use crate::settings;
use crate::spool::Spool;
use crate::timestamp::{Stamp, SystemTimeSource};

/// Where the samples go. `Client` in production, a fake in tests.
pub trait Remote {
    fn commit(&mut self, samples: &[Sample], now_unix: i64) -> Result<(), UploadError>;
    /// The settings document, `None` if there is none.
    fn read_settings(&mut self, now_unix: i64) -> Result<Option<serde_json::Value>, UploadError>;
    fn invalidate_token(&mut self);
}

impl Remote for Client {
    fn commit(&mut self, samples: &[Sample], now_unix: i64) -> Result<(), UploadError> {
        Client::commit(self, samples, now_unix)
    }

    fn read_settings(&mut self, now_unix: i64) -> Result<Option<serde_json::Value>, UploadError> {
        Client::read_settings(self, now_unix)
    }

    fn invalidate_token(&mut self) {
        Client::invalidate_token(self);
    }
}

/// Batches per round: after a long outage the backlog goes out over several
/// rounds instead of one long burst.
const MAX_BATCHES_PER_ROUND: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_age: Duration,
    pub max_samples: usize,
}

/// What a round did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Round {
    /// A sample of the controller's state was recorded.
    pub recorded: bool,
    pub uploaded: usize,
    pub retention_dropped: usize,
    /// What went wrong, if anything.
    pub error: Option<String>,
    pub waiting_for_clock: bool,
    /// The newest sample uploaded in this round, in a few words.
    pub latest: Option<String>,
    /// The band changed this round (remote settings, or back to the config).
    pub band_changed: Option<Band>,
    /// The settings document exists but is not a valid band.
    pub settings_error: Option<String>,
}

/// Keeps the band in force in sync with the settings document.
struct SettingsSync {
    /// Where the last remote band is saved.
    path: PathBuf,
    /// The config file's band: used when there is no settings document.
    default: Band,
    /// The band the control thread is using.
    applied: Band,
    publisher: Publisher<Band>,
    /// The last invalid content seen, to log it once rather than every round.
    last_error: Option<String>,
}

pub struct Uploader<R> {
    spool_path: PathBuf,
    spool: Option<Spool>,
    remote: Option<R>,
    interval: Duration,
    retention: Retention,
    /// Samples lost because the spool could not be opened; logged on change.
    unspooled: u64,
    heartbeat: Option<Box<dyn Heartbeat>>,
    /// The newest uploaded sample had the overrun fault: heartbeats report a
    /// failure until a sample without it goes out.
    fault_active: bool,
    settings: Option<SettingsSync>,
}

impl<R: Remote> Uploader<R> {
    /// `remote` is `None` when uploading is disabled: samples are still spooled
    /// and trimmed by retention.
    pub fn new(
        spool_path: PathBuf,
        remote: Option<R>,
        interval: Duration,
        retention: Retention,
    ) -> Self {
        Self {
            spool_path,
            spool: None,
            remote,
            interval,
            retention,
            unspooled: 0,
            heartbeat: None,
            fault_active: false,
            settings: None,
        }
    }

    /// Reads the band from the settings document every round. `applied` is
    /// the band the control thread starts with, `default` the config file's;
    /// new bands are saved at `path` and published on `publisher`.
    pub fn with_settings(
        mut self,
        path: PathBuf,
        default: Band,
        applied: Band,
        publisher: Publisher<Band>,
    ) -> Self {
        self.settings = Some(SettingsSync {
            path,
            default,
            applied,
            publisher,
            last_error: None,
        });
        self
    }

    /// Reports every round (success or failure) to `heartbeat`.
    pub fn with_heartbeat(mut self, heartbeat: Box<dyn Heartbeat>) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    /// One round. `sample` is the controller's newest state (`None` if the
    /// control loop published nothing since the last round); `stamp` is now.
    pub fn round(&mut self, sample: Option<&Sample>, stamp: &Stamp) -> Round {
        let mut round = Round::default();
        if let Some(sample) = sample {
            round.recorded = self.store(sample);
        }
        self.trim(stamp, &mut round);
        if self.remote.is_some() {
            if stamp.synced {
                self.upload(stamp, &mut round);
                // A failed upload means the network is down: no point asking.
                if round.error.is_none() {
                    self.sync_settings(stamp, &mut round);
                }
            } else {
                // The token request is signed with the wall clock: it would be refused.
                log::debug!("clock not synchronized yet: upload postponed");
                round.waiting_for_clock = true;
            }
            self.report(sample.is_some(), &round);
        }
        log::debug!(
            "round: recorded {}, uploaded {}{}",
            round.recorded,
            round.uploaded,
            round
                .error
                .as_deref()
                .map(|e| format!(", error: {e}"))
                .unwrap_or_default()
        );
        round
    }

    /// Appends `sample` to the spool, opening it if needed.
    fn store(&mut self, sample: &Sample) -> bool {
        let Some(spool) = self.spool() else {
            self.unspooled += 1;
            if self.unspooled == 1 {
                log::error!("spool unavailable: samples are being lost until it can be opened");
            }
            return false;
        };
        match spool.append(sample) {
            Ok(()) => true,
            Err(e) => {
                log::error!("spool append failed: {e}");
                false
            }
        }
    }

    fn trim(&mut self, stamp: &Stamp, round: &mut Round) {
        let max_age_ms = i64::try_from(self.retention.max_age.as_millis()).unwrap_or(i64::MAX);
        // Without a synchronized clock, age means nothing: only the cap applies.
        let oldest_ms = if stamp.synced {
            stamp.time_ms.saturating_sub(max_age_ms)
        } else {
            i64::MIN
        };
        let max_samples = self.retention.max_samples;
        let Some(spool) = self.spool() else {
            return;
        };
        match spool.enforce_retention(oldest_ms, max_samples) {
            Ok(0) => {}
            Ok(n) => {
                round.retention_dropped = n;
                log::warn!("retention dropped {n} old samples");
            }
            Err(e) => log::error!("spool retention failed: {e}"),
        }
    }

    fn upload(&mut self, stamp: &Stamp, round: &mut Round) {
        for _ in 0..MAX_BATCHES_PER_ROUND {
            let (Some(spool), Some(remote)) = (self.spool.as_mut(), self.remote.as_mut()) else {
                if self.spool.is_none() {
                    round.error = Some("spool unavailable".to_owned());
                }
                return;
            };
            let mut batch = match spool.read_batch(MAX_BATCH) {
                Ok(b) => b,
                Err(e) => {
                    log::error!("spool read failed: {e}");
                    round.error = Some(format!("spool read failed: {e}"));
                    return;
                }
            };
            if batch.lines == 0 {
                return;
            }
            for sample in &mut batch.samples {
                sample.correct_time(stamp);
            }
            match remote.commit(&batch.samples, stamp.time_ms / 1000) {
                Ok(()) => {
                    if let Err(e) = spool.remove_front(batch.lines) {
                        // Uploaded but still spooled: the stable document ids
                        // make the next upload an overwrite, not a duplicate.
                        log::error!("spool trim after upload failed: {e}");
                        return;
                    }
                    round.uploaded += batch.samples.len();
                    if let Some(s) = batch.samples.last() {
                        round.latest = Some(describe(s));
                        self.fault_active = s.overrun_fault;
                    }
                    if batch.samples.len() < MAX_BATCH {
                        return;
                    }
                }
                Err(e) => {
                    if e.is_unauthorized() {
                        remote.invalidate_token();
                    }
                    let error = crate::error::chain(&e);
                    log::warn!(
                        "upload failed, retrying at the next round in {} s: {error}",
                        self.interval.as_secs()
                    );
                    round.error = Some(error);
                    return;
                }
            }
        }
    }

    fn sync_settings(&mut self, stamp: &Stamp, round: &mut Round) {
        let (Some(sync), Some(remote)) = (self.settings.as_mut(), self.remote.as_mut()) else {
            return;
        };
        let wanted = match remote.read_settings(stamp.time_ms / 1000) {
            Ok(Some(doc)) => match settings::parse(&doc) {
                Ok(band) => Some(band),
                Err(e) => {
                    if sync.last_error.as_deref() != Some(e.as_str()) {
                        log::warn!("invalid settings ignored, band stays {}: {e}", sync.applied);
                    }
                    sync.last_error = Some(e.clone());
                    round.settings_error = Some(e);
                    return;
                }
            },
            // No settings document: the config file's band.
            Ok(None) => None,
            Err(e) => {
                let error = format!("settings read failed: {}", crate::error::chain(&e));
                log::warn!("{error}");
                round.error = Some(error);
                return;
            }
        };
        sync.last_error = None;
        let band = wanted.unwrap_or(sync.default);
        if band == sync.applied {
            return;
        }
        let saved = match wanted {
            Some(band) => settings::save(&sync.path, band),
            None => settings::remove(&sync.path),
        };
        if let Err(e) = saved {
            // Applied anyway; only a reboot without network would lose it.
            log::error!("cannot update {}: {e}", sync.path.display());
        }
        match wanted {
            Some(_) => log::info!("new band from the settings: {} → {band}", sync.applied),
            None => log::info!("settings removed: back to the config band {band}"),
        }
        sync.applied = band;
        sync.publisher.publish(band);
        round.band_changed = Some(band);
    }

    fn report(&mut self, had_sample: bool, round: &Round) {
        let Some(hb) = self.heartbeat.as_mut() else {
            return;
        };
        if round.waiting_for_clock {
            // Nothing to say yet; if it lasts, missing pings raise the alarm.
            return;
        }
        if let Some(error) = &round.error {
            hb.failure(&format!("upload failed: {error}"));
        } else if !had_sample {
            // Should never happen: the systemd watchdog restarts a stuck loop.
            hb.failure("no state from the control loop since the last round");
        } else if let Some(error) = &round.settings_error {
            hb.failure(&format!("invalid settings: {error}"));
        } else if self.fault_active {
            // The data got through, but the fridge itself needs attention.
            hb.failure(&format!(
                "overrun fault: the compressor keeps running without reaching \
                 target_min; latest: {}",
                round.latest.as_deref().unwrap_or("?")
            ));
        } else {
            hb.success(&format!(
                "uploaded {}; latest: {}",
                round.uploaded,
                round.latest.as_deref().unwrap_or("?")
            ));
        }
    }

    fn spool(&mut self) -> Option<&mut Spool> {
        if self.spool.is_none() {
            match Spool::open(&self.spool_path) {
                Ok(s) => {
                    if self.unspooled > 0 {
                        log::warn!("spool open again after losing {} samples", self.unspooled);
                        self.unspooled = 0;
                    }
                    self.spool = Some(s);
                }
                Err(e) => {
                    if self.unspooled == 0 {
                        log::error!("cannot open spool {}: {e}", self.spool_path.display());
                    }
                    return None;
                }
            }
        }
        self.spool.as_mut()
    }

    /// The thread body: a round every `interval` from `start` until the
    /// control thread drops its publisher.
    pub fn run(mut self, rx: Subscriber<Sample>, time: &SystemTimeSource, start: Instant) {
        let mut next = start + self.interval;
        loop {
            if rx.wait_closed(next.saturating_duration_since(Instant::now())) {
                break;
            }
            next = next_round(next, Instant::now(), self.interval);
            let sample = rx.take();
            self.round(sample.as_ref(), &time.stamp());
        }
        log::debug!("uploader stopped");
    }
}

/// The first grid point after `now`: rounds stay on `start + k * interval`
/// even when one runs late, and missed ones are skipped, not caught up.
fn next_round(scheduled: Instant, now: Instant, interval: Duration) -> Instant {
    let mut next = scheduled + interval;
    while next <= now {
        next += interval;
    }
    next
}

/// "10.44 °C, compressor OFF, NORMAL, band 8.0–11.0 °C" for the heartbeat log.
fn describe(s: &Sample) -> String {
    let temp = s
        .temperature
        .map_or_else(|| "no reading".to_owned(), |t| format!("{t:.2} °C"));
    let mode = serde_json::to_value(s.mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let band = match (s.target_min, s.target_max) {
        (Some(min), Some(max)) => format!(", band {min:.1}–{max:.1} °C"),
        _ => String::new(),
    };
    let fault = if s.overrun_fault {
        ", OVERRUN FAULT"
    } else {
        ""
    };
    format!("{temp}, compressor {}, {mode}{band}{fault}", s.relay)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::spool::tests::{TempDir, sample};

    /// Scripted remote: pops one result per commit (Ok when the script is
    /// empty) and records what it accepted.
    #[derive(Default)]
    struct FakeRemote {
        script: VecDeque<Result<(), u16>>,
        accepted: Vec<Sample>,
        invalidated: usize,
        /// Settings reads, one per round (no document when empty).
        settings: VecDeque<Result<Option<serde_json::Value>, u16>>,
        settings_reads: usize,
    }

    impl Remote for FakeRemote {
        fn commit(&mut self, samples: &[Sample], _now: i64) -> Result<(), UploadError> {
            match self.script.pop_front().unwrap_or(Ok(())) {
                Ok(()) => {
                    self.accepted.extend_from_slice(samples);
                    Ok(())
                }
                Err(status) => Err(UploadError::Status {
                    status,
                    body: String::new(),
                }),
            }
        }

        fn read_settings(&mut self, _now: i64) -> Result<Option<serde_json::Value>, UploadError> {
            self.settings_reads += 1;
            self.settings
                .pop_front()
                .unwrap_or(Ok(None))
                .map_err(|status| UploadError::Status {
                    status,
                    body: String::new(),
                })
        }

        fn invalidate_token(&mut self) {
            self.invalidated += 1;
        }
    }

    /// Records heartbeat calls.
    #[derive(Clone, Default)]
    struct Beats(Arc<Mutex<Vec<String>>>);

    impl Beats {
        fn all(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Heartbeat for Beats {
        fn success(&mut self, message: &str) {
            self.0.lock().unwrap().push(format!("ok: {message}"));
        }
        fn failure(&mut self, message: &str) {
            self.0.lock().unwrap().push(format!("fail: {message}"));
        }
    }

    const INTERVAL: Duration = Duration::from_secs(300);
    const RETENTION: Retention = Retention {
        max_age: Duration::from_secs(7 * 86_400),
        max_samples: 10_000,
    };
    const T: i64 = 1_760_000_000_000;

    fn stamp(synced: bool) -> Stamp {
        Stamp {
            time_ms: T,
            synced,
            boot_id: "boot".to_owned(),
            uptime_ms: 1_000_000,
        }
    }

    fn uploader(dir: &TempDir, script: &[Result<(), u16>]) -> (Uploader<FakeRemote>, Beats) {
        let remote = FakeRemote {
            script: script.iter().copied().collect(),
            ..FakeRemote::default()
        };
        let beats = Beats::default();
        let u = Uploader::new(dir.spool(), Some(remote), INTERVAL, RETENTION)
            .with_heartbeat(Box::new(beats.clone()));
        (u, beats)
    }

    fn spooled(u: &mut Uploader<FakeRemote>) -> usize {
        u.spool().unwrap().len().unwrap()
    }

    fn accepted(u: &Uploader<FakeRemote>) -> Vec<i64> {
        let remote = u.remote.as_ref().unwrap();
        remote.accepted.iter().map(|s| s.time_ms).collect()
    }

    #[test]
    fn each_round_records_and_uploads_exactly_one_sample() {
        let dir = TempDir::new("up-round");
        let (mut u, beats) = uploader(&dir, &[]);
        for k in 0..3 {
            let round = u.round(Some(&sample(T + k * 300_000)), &stamp(true));
            assert!(round.recorded);
            assert_eq!(round.uploaded, 1);
        }
        assert_eq!(accepted(&u), [T, T + 300_000, T + 600_000]);
        assert_eq!(spooled(&mut u), 0);
        assert_eq!(
            beats.all(),
            ["ok: uploaded 1; latest: 12.00 °C, compressor OFF, NORMAL, band 8.0–11.0 °C"; 3]
        );
    }

    #[test]
    fn a_failed_upload_keeps_the_sample_for_the_next_round() {
        let dir = TempDir::new("up-fail");
        let (mut u, beats) = uploader(&dir, &[Err(503)]);
        let round = u.round(Some(&sample(T)), &stamp(true));
        assert_eq!(round.uploaded, 0);
        assert!(round.error.as_deref().unwrap().starts_with("HTTP 503"));
        assert_eq!(spooled(&mut u), 1);
        // Next round: the backlog and the new sample go out together.
        assert_eq!(
            u.round(Some(&sample(T + 300_000)), &stamp(true)).uploaded,
            2
        );
        assert_eq!(spooled(&mut u), 0);
        let b = beats.all();
        assert!(
            b[0].starts_with("fail: upload failed: HTTP 503"),
            "{}",
            b[0]
        );
        assert!(b[1].starts_with("ok: uploaded 2"), "{}", b[1]);
    }

    #[test]
    fn unauthorized_refreshes_the_token() {
        let dir = TempDir::new("up-401");
        let (mut u, _) = uploader(&dir, &[Err(401)]);
        u.round(Some(&sample(T)), &stamp(true));
        assert_eq!(u.remote.as_ref().unwrap().invalidated, 1);
        assert_eq!(
            u.round(Some(&sample(T + 300_000)), &stamp(true)).uploaded,
            2
        );
    }

    #[test]
    fn waits_for_the_clock_and_then_corrects_timestamps() {
        let dir = TempDir::new("up-clock");
        let (mut u, beats) = uploader(&dir, &[]);
        let mut early = sample(1_000); // restored clock, years off
        early.clock_synced = false;
        early.uptime_ms = 400_000;

        let round = u.round(Some(&early), &stamp(false));
        assert!(round.waiting_for_clock && round.recorded);
        assert_eq!(spooled(&mut u), 1);
        // No ping while waiting: if it lasts, missing pings are the alarm.
        assert!(beats.all().is_empty());

        u.round(None, &stamp(true));
        let sent = &u.remote.as_ref().unwrap().accepted[0];
        assert!(sent.time_corrected);
        // 600 s of uptime before the synced stamp.
        assert_eq!(sent.time_ms, T - 600_000);
    }

    #[test]
    fn a_big_backlog_goes_out_in_batches() {
        let dir = TempDir::new("up-backlog");
        let (mut u, _) = uploader(&dir, &[]);
        for k in 0..(MAX_BATCH * 2 + 6) {
            u.store(&sample(T + k as i64));
        }
        let round = u.round(Some(&sample(T + 999_999)), &stamp(true));
        assert_eq!(round.uploaded, MAX_BATCH * 2 + 7);
        assert_eq!(spooled(&mut u), 0);
    }

    #[test]
    fn no_state_from_the_control_loop_is_reported() {
        let dir = TempDir::new("up-nostate");
        let (mut u, beats) = uploader(&dir, &[]);
        let round = u.round(None, &stamp(true));
        assert!(!round.recorded);
        assert_eq!(
            beats.all(),
            ["fail: no state from the control loop since the last round"]
        );
    }

    #[test]
    fn overrun_fault_turns_heartbeats_into_failures_until_it_clears() {
        let dir = TempDir::new("up-fault");
        let (mut u, beats) = uploader(&dir, &[]);
        let mut faulty = sample(T);
        faulty.overrun_fault = true;
        u.round(Some(&faulty), &stamp(true));
        u.round(Some(&sample(T + 300_000)), &stamp(true));
        let b = beats.all();
        assert!(b[0].starts_with("fail: overrun fault"), "{}", b[0]);
        assert!(b[0].ends_with("OVERRUN FAULT"), "{}", b[0]);
        assert!(b[1].starts_with("ok: uploaded 1"), "{}", b[1]);
    }

    #[test]
    fn disabled_upload_still_spools_and_trims_without_pings() {
        let dir = TempDir::new("up-off");
        let beats = Beats::default();
        let retention = Retention {
            max_age: Duration::from_secs(86_400),
            max_samples: 3,
        };
        let mut u = Uploader::<FakeRemote>::new(dir.spool(), None, INTERVAL, retention)
            .with_heartbeat(Box::new(beats.clone()));
        for k in 0..5 {
            u.round(Some(&sample(T + k)), &stamp(true));
        }
        assert_eq!(spooled(&mut u), 3);
        assert!(beats.all().is_empty());
    }

    #[test]
    fn missing_spool_directory_loses_samples_but_never_panics() {
        let dir = TempDir::new("up-nodir");
        let beats = Beats::default();
        let mut u = Uploader::new(
            dir.0.join("missing/spool.jsonl"),
            Some(FakeRemote::default()),
            INTERVAL,
            RETENTION,
        )
        .with_heartbeat(Box::new(beats.clone()));
        assert!(!u.round(Some(&sample(T)), &stamp(true)).recorded);
        assert_eq!(u.unspooled, 1);
        assert_eq!(beats.all(), ["fail: upload failed: spool unavailable"]);
        // The directory appears: the next sample is spooled and uploaded.
        std::fs::create_dir_all(dir.0.join("missing")).unwrap();
        assert_eq!(u.round(Some(&sample(T + 1)), &stamp(true)).uploaded, 1);
        assert_eq!(u.unspooled, 0);
    }

    // --- remote settings --------------------------------------------------

    const CONFIG_BAND: Band = Band {
        min: 8.0,
        max: 11.0,
    };

    fn settings_doc(min: f64, max: f64) -> Result<Option<serde_json::Value>, u16> {
        Ok(Some(serde_json::json!({ "fields": {
            "targetMin": { "doubleValue": min },
            "targetMax": { "doubleValue": max },
        }})))
    }

    /// An uploader syncing settings, the subscriber the control thread would
    /// hold, and the heartbeat log.
    fn with_settings(
        dir: &TempDir,
        reads: Vec<Result<Option<serde_json::Value>, u16>>,
        applied: Band,
    ) -> (Uploader<FakeRemote>, Subscriber<Band>, Beats) {
        let (u, beats) = uploader(dir, &[]);
        let mut u = u;
        u.remote.as_mut().unwrap().settings = reads.into();
        let (tx, rx) = crate::channel::latest();
        let u = u.with_settings(
            settings::path_next_to(&dir.spool()),
            CONFIG_BAND,
            applied,
            tx,
        );
        (u, rx, beats)
    }

    #[test]
    fn a_new_band_is_saved_and_published_once() {
        let dir = TempDir::new("up-set");
        let (mut u, rx, beats) = with_settings(
            &dir,
            vec![settings_doc(7.0, 10.0), settings_doc(7.0, 10.0)],
            CONFIG_BAND,
        );
        let band = Band {
            min: 7.0,
            max: 10.0,
        };

        assert_eq!(
            u.round(Some(&sample(T)), &stamp(true)).band_changed,
            Some(band)
        );
        assert_eq!(rx.take(), Some(band));
        assert_eq!(
            settings::load(&settings::path_next_to(&dir.spool())),
            Some(band)
        );
        // Same document next round: nothing to do.
        assert_eq!(
            u.round(Some(&sample(T + 1)), &stamp(true)).band_changed,
            None
        );
        assert_eq!(rx.take(), None);
        assert!(
            beats.all().iter().all(|b| b.starts_with("ok:")),
            "{:?}",
            beats.all()
        );
    }

    #[test]
    fn deleting_the_document_goes_back_to_the_config_band() {
        let dir = TempDir::new("up-clear");
        let saved = Band {
            min: 7.0,
            max: 10.0,
        };
        let path = settings::path_next_to(&dir.spool());
        settings::save(&path, saved).unwrap();
        let (mut u, rx, _) = with_settings(&dir, vec![Ok(None)], saved);

        assert_eq!(
            u.round(Some(&sample(T)), &stamp(true)).band_changed,
            Some(CONFIG_BAND)
        );
        assert_eq!(rx.take(), Some(CONFIG_BAND));
        assert_eq!(settings::load(&path), None);
    }

    #[test]
    fn an_invalid_band_is_ignored_and_reported() {
        let dir = TempDir::new("up-invalid");
        let (mut u, rx, beats) = with_settings(&dir, vec![settings_doc(10.0, 7.0)], CONFIG_BAND);
        let round = u.round(Some(&sample(T)), &stamp(true));
        assert!(
            round
                .settings_error
                .as_deref()
                .unwrap()
                .contains("must exceed")
        );
        assert_eq!(round.uploaded, 1);
        assert_eq!(rx.take(), None);
        assert!(
            beats.all()[0].starts_with("fail: invalid settings: "),
            "{:?}",
            beats.all()
        );
    }

    #[test]
    fn a_failed_settings_read_keeps_the_band_and_is_reported() {
        let dir = TempDir::new("up-read-fail");
        let (mut u, rx, beats) = with_settings(&dir, vec![Err(503)], CONFIG_BAND);
        let round = u.round(Some(&sample(T)), &stamp(true));
        assert!(
            round
                .error
                .as_deref()
                .unwrap()
                .starts_with("settings read failed")
        );
        assert_eq!(rx.take(), None);
        assert!(beats.all()[0].starts_with("fail: upload failed: settings read failed"));
    }

    #[test]
    fn settings_are_not_read_after_a_failed_upload() {
        let dir = TempDir::new("up-no-read");
        let (u, rx, _) = with_settings(&dir, vec![settings_doc(7.0, 10.0)], CONFIG_BAND);
        let mut u = u;
        u.remote.as_mut().unwrap().script = [Err(503)].into();
        u.round(Some(&sample(T)), &stamp(true));
        assert_eq!(u.remote.as_ref().unwrap().settings_reads, 0);
        assert_eq!(rx.take(), None);
    }

    #[test]
    fn rounds_stay_on_the_grid() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        // On time, or a little late: the next point of the grid.
        assert_eq!(next_round(at(300), at(300), INTERVAL), at(600));
        assert_eq!(next_round(at(300), at(312), INTERVAL), at(600));
        // Very late (e.g. a slow upload): missed points are skipped.
        assert_eq!(next_round(at(300), at(950), INTERVAL), at(1200));
    }

    #[test]
    fn run_records_the_latest_state_once_per_interval_and_stops_on_close() {
        let dir = TempDir::new("up-run");
        let (tx, rx) = crate::channel::latest();
        let u =
            Uploader::<FakeRemote>::new(dir.spool(), None, Duration::from_millis(100), RETENTION);
        let start = Instant::now();
        let handle = std::thread::spawn(move || u.run(rx, &SystemTimeSource::new(), start));
        // Real timestamps: the round runs retention against the real clock.
        let now_ms = SystemTimeSource::new().stamp().time_ms;
        // Many publications per interval: only the newest of each is recorded.
        for k in 0..50 {
            tx.publish(sample(now_ms + k));
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(30));
        drop(tx);
        handle.join().unwrap();
        let n = Spool::open(&dir.spool()).unwrap().len().unwrap();
        // About 280 ms of publishing at 100 ms rounds: two or three rounds, but
        // under load the timing slips. What matters: rounds did record, and
        // far fewer samples than the 50 publications.
        assert!((1..=5).contains(&n), "{n} samples");
    }
}
