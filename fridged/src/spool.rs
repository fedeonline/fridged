//! Local JSONL spool: samples wait here until they are uploaded.
//!
//! - [`Spool::append`] writes one line and `fsync`s it: once it returns, the
//!   sample survives a power cut.
//! - [`Spool::read_batch`] reads the oldest samples; once they are uploaded,
//!   [`Spool::remove_front`] drops them.
//! - Every rewrite (removal, retention) goes through a temporary file, `fsync`
//!   and `rename`: a crash leaves either the old file or the new one, never a
//!   half-written mix.
//! - A line that does not parse (a write cut short by `kill -9` or a power cut)
//!   is skipped and dropped at the next rewrite; it never blocks the rest.
//!
//! Single owner: the uploader thread. Nothing here is synchronized.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::sample::Sample;

pub struct Spool {
    path: PathBuf,
    file: File,
}

/// The oldest samples in the spool.
#[derive(Debug, Default)]
pub struct Batch {
    pub samples: Vec<Sample>,
    /// Records covered by this batch, corrupted ones included: what to pass to
    /// [`Spool::remove_front`] once the samples are uploaded.
    pub lines: usize,
    /// Lines in that range that did not parse.
    pub corrupted: usize,
}

impl Spool {
    /// Opens (creating if needed) the spool at `path`. The directory must exist.
    ///
    /// If the last write was cut short, the file does not end with a newline:
    /// one is added, so the next sample starts on a line of its own instead of
    /// being glued to the broken one.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        if !ends_with_newline(&mut file)? {
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        Ok(Self {
            path: path.to_owned(),
            file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `sample` and waits until it is on disk.
    pub fn append(&mut self, sample: &Sample) -> io::Result<()> {
        let mut line = serde_json::to_vec(sample)?;
        line.push(b'\n');
        // One write: a crash can cut it short, never interleave it.
        self.file.write_all(&line)?;
        self.file.sync_data()
    }

    /// Reads up to `max` samples from the front of the spool.
    pub fn read_batch(&self, max: usize) -> io::Result<Batch> {
        let mut batch = Batch::default();
        if max == 0 {
            return Ok(batch);
        }
        for line in self.records()? {
            batch.lines += 1;
            match serde_json::from_slice::<Sample>(&line?) {
                Ok(sample) => {
                    batch.samples.push(sample);
                    if batch.samples.len() == max {
                        break;
                    }
                }
                Err(_) => batch.corrupted += 1,
            }
        }
        Ok(batch)
    }

    /// Drops the first `lines` records (see [`Batch::lines`]).
    pub fn remove_front(&mut self, lines: usize) -> io::Result<()> {
        if lines == 0 {
            return Ok(());
        }
        let mut seen = 0;
        self.rewrite(|_| {
            seen += 1;
            seen > lines
        })
    }

    /// Enforces retention; returns how many records were dropped.
    ///
    /// Drops samples older than `oldest_ms` (only those with a synchronized
    /// clock: an unsynchronized time says nothing about age), corrupted lines,
    /// and then the oldest samples beyond `max_samples`, so the spool stays
    /// bounded even if the clock is wrong for days.
    pub fn enforce_retention(&mut self, oldest_ms: i64, max_samples: usize) -> io::Result<usize> {
        let mut keep = Vec::new();
        for line in self.records()? {
            keep.push(match serde_json::from_slice::<Sample>(&line?) {
                Ok(s) => !s.clock_synced || s.time_ms >= oldest_ms,
                Err(_) => false,
            });
        }
        // Over the cap: drop more, oldest first.
        let mut excess = keep
            .iter()
            .filter(|&&k| k)
            .count()
            .saturating_sub(max_samples);
        for k in keep.iter_mut().filter(|k| **k) {
            if excess == 0 {
                break;
            }
            *k = false;
            excess -= 1;
        }
        let dropped = keep.iter().filter(|&&k| !k).count();
        if dropped > 0 {
            let mut i = 0;
            self.rewrite(|_| {
                i += 1;
                keep[i - 1]
            })?;
        }
        Ok(dropped)
    }

    /// Number of records, corrupted ones included.
    pub fn len(&self) -> io::Result<usize> {
        let mut n = 0;
        for line in self.records()? {
            line?;
            n += 1;
        }
        Ok(n)
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The non-blank lines, as raw bytes: a corrupted line may not even be
    /// valid UTF-8, and must not stop the ones after it from being read.
    fn records(&self) -> io::Result<impl Iterator<Item = io::Result<Vec<u8>>>> {
        let reader = BufReader::new(File::open(&self.path)?);
        Ok(reader.split(b'\n').filter(|line| {
            line.as_ref()
                .map_or(true, |l| !l.iter().all(u8::is_ascii_whitespace))
        }))
    }

    /// Rewrites the spool keeping the records for which `keep` returns true
    /// (blank lines always go), atomically: temporary file, fsync, rename,
    /// fsync of the directory.
    fn rewrite(&mut self, mut keep: impl FnMut(&[u8]) -> bool) -> io::Result<()> {
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut out = BufWriter::new(File::create(&tmp)?);
            for line in self.records()? {
                let line = line?;
                if keep(&line) {
                    out.write_all(&line)?;
                    out.write_all(b"\n")?;
                }
            }
            out.into_inner()
                .map_err(io::IntoInnerError::into_error)?
                .sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        // Make the rename itself durable.
        if let Some(dir) = self.path.parent() {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            File::open(dir)?.sync_all()?;
        }
        // The old handle points at the replaced file.
        self.file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)?;
        Ok(())
    }
}

fn ends_with_newline(file: &mut File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::control::Reason;
    use crate::relay::RelayState;
    use crate::sample::Mode;

    /// A directory removed when dropped.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fridged-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn spool(&self) -> PathBuf {
            self.0.join("spool.jsonl")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn sample(time_ms: i64) -> Sample {
        Sample {
            time_ms,
            clock_synced: true,
            boot_id: "boot".to_owned(),
            uptime_ms: 1_000,
            temperature: Some(12.0),
            relay: RelayState::Off,
            wanted: RelayState::Off,
            reason: Reason::InBand,
            mode: Mode::Normal,
            overrun_fault: false,
            target_min: Some(8.0),
            target_max: Some(11.0),
            time_corrected: false,
        }
    }

    fn times(samples: &[Sample]) -> Vec<i64> {
        samples.iter().map(|s| s.time_ms).collect()
    }

    #[test]
    fn append_then_read_in_order() {
        let dir = TempDir::new("append");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        for t in 1..=5 {
            spool.append(&sample(t)).unwrap();
        }
        let batch = spool.read_batch(3).unwrap();
        assert_eq!(times(&batch.samples), [1, 2, 3]);
        assert_eq!((batch.lines, batch.corrupted), (3, 0));
        assert_eq!(spool.len().unwrap(), 5);
    }

    #[test]
    fn remove_front_then_append_continues() {
        let dir = TempDir::new("remove");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        for t in 1..=5 {
            spool.append(&sample(t)).unwrap();
        }
        let batch = spool.read_batch(2).unwrap();
        spool.remove_front(batch.lines).unwrap();
        // The append handle follows the rewritten file.
        spool.append(&sample(6)).unwrap();
        assert_eq!(times(&spool.read_batch(10).unwrap().samples), [3, 4, 5, 6]);
        assert!(!dir.0.join("spool.jsonl.tmp").exists());
    }

    #[test]
    fn survives_reopening() {
        let dir = TempDir::new("reopen");
        {
            let mut spool = Spool::open(&dir.spool()).unwrap();
            spool.append(&sample(1)).unwrap();
        }
        let mut spool = Spool::open(&dir.spool()).unwrap();
        spool.append(&sample(2)).unwrap();
        assert_eq!(times(&spool.read_batch(10).unwrap().samples), [1, 2]);
    }

    #[test]
    fn empty_spool() {
        let dir = TempDir::new("empty");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        assert!(spool.is_empty().unwrap());
        let batch = spool.read_batch(10).unwrap();
        assert!(batch.samples.is_empty());
        spool.remove_front(batch.lines).unwrap();
        assert_eq!(spool.enforce_retention(0, 10).unwrap(), 0);
    }

    #[test]
    fn retention_by_age_keeps_unsynced_samples() {
        let dir = TempDir::new("age");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        for t in [100, 200, 300] {
            spool.append(&sample(t)).unwrap();
        }
        // Unsynced, with a wall time that looks ancient: age unknown, kept.
        let mut unsynced = sample(5);
        unsynced.clock_synced = false;
        spool.append(&unsynced).unwrap();

        assert_eq!(spool.enforce_retention(250, 100).unwrap(), 2);
        assert_eq!(times(&spool.read_batch(10).unwrap().samples), [300, 5]);
    }

    // --- damage ------------------------------------------------------------

    #[test]
    fn corrupted_line_in_the_middle_is_skipped_and_counted() {
        let dir = TempDir::new("corrupt");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        spool.append(&sample(1)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.spool())
            .unwrap()
            .write_all(b"{\"timeMs\":2,\"clockSy\xff garbage\n")
            .unwrap();
        spool.append(&sample(3)).unwrap();

        let batch = spool.read_batch(10).unwrap();
        assert_eq!(times(&batch.samples), [1, 3]);
        assert_eq!((batch.lines, batch.corrupted), (3, 1));
        // Uploading the batch also removes the broken line.
        spool.remove_front(batch.lines).unwrap();
        assert!(spool.is_empty().unwrap());
    }

    #[test]
    fn a_write_cut_short_does_not_swallow_the_next_sample() {
        let dir = TempDir::new("cut");
        {
            let mut spool = Spool::open(&dir.spool()).unwrap();
            spool.append(&sample(1)).unwrap();
        }
        // What a power cut in the middle of the second append leaves behind.
        let full = serde_json::to_string(&sample(2)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.spool())
            .unwrap()
            .write_all(&full.as_bytes()[..full.len() / 2])
            .unwrap();

        let mut spool = Spool::open(&dir.spool()).unwrap();
        spool.append(&sample(3)).unwrap();
        let batch = spool.read_batch(10).unwrap();
        assert_eq!(times(&batch.samples), [1, 3]);
        assert_eq!(batch.corrupted, 1);
    }

    #[test]
    fn retention_also_drops_corrupted_lines() {
        let dir = TempDir::new("retcorrupt");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        spool.append(&sample(1)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.spool())
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        assert_eq!(spool.enforce_retention(0, 10).unwrap(), 1);
        assert_eq!(spool.len().unwrap(), 1);
    }

    const CHILD_ENV: &str = "FRIDGED_SPOOL_CHILD";

    /// Not a test on its own: the body of the process `kill_9_mid_append` kills.
    #[test]
    #[ignore = "helper process for kill_9_mid_append"]
    fn appender_child() {
        let path = std::env::var(CHILD_ENV).expect("run by kill_9_mid_append");
        let mut spool = Spool::open(Path::new(&path)).unwrap();
        let mut t = 0;
        loop {
            spool.append(&sample(t)).unwrap();
            t += 1;
        }
    }

    #[test]
    fn kill_9_mid_append() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let dir = TempDir::new("kill9");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "spool::tests::appender_child"])
            .env(CHILD_ENV, dir.spool())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Let it write for a while, then SIGKILL it wherever it is.
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::metadata(dir.spool()).map_or(0, |m| m.len()) < 20_000 {
            assert!(Instant::now() < deadline, "child wrote nothing");
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        child.wait().unwrap();

        let mut spool = Spool::open(&dir.spool()).unwrap();
        spool.append(&sample(-1)).unwrap();
        let batch = spool.read_batch(usize::MAX).unwrap();
        // At most the one line being written when it died is lost...
        assert!(batch.corrupted <= 1, "{} corrupted", batch.corrupted);
        // ...every sample before it is intact and in order, and the spool
        // keeps working after it.
        let t = times(&batch.samples);
        let (last, before) = t.split_last().unwrap();
        assert_eq!(*last, -1);
        assert!(before.len() > 10);
        assert!(before.iter().enumerate().all(|(i, &v)| v == i as i64));
    }

    #[test]
    fn retention_caps_the_number_of_samples() {
        let dir = TempDir::new("cap");
        let mut spool = Spool::open(&dir.spool()).unwrap();
        for t in 1..=10 {
            spool.append(&sample(t)).unwrap();
        }
        assert_eq!(spool.enforce_retention(0, 4).unwrap(), 6);
        assert_eq!(times(&spool.read_batch(10).unwrap().samples), [7, 8, 9, 10]);
    }
}
