//! Remote settings: the hysteresis band, changed while running.
//!
//! The band lives in Firestore at `devices/{deviceId}/settings/current`, with
//! the fields `targetMin` and `targetMax`; the uploader reads it every round.
//! The last valid band is saved next to the spool, so a reboot without network
//! keeps it instead of going back to the config file.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::control::Band;

/// Where the last applied remote band is saved: next to the spool, in the
/// directory systemd's `StateDirectory=` manages.
pub fn path_next_to(spool: &Path) -> PathBuf {
    spool.with_file_name("settings.json")
}

/// The band in a Firestore settings document (`{"fields": {...}}`).
/// The console stores whole numbers as `integerValue` (a string): both forms
/// are accepted.
pub fn parse(doc: &Value) -> Result<Band, String> {
    let field = |name: &str| -> Result<f64, String> {
        let v = &doc["fields"][name];
        if let Some(d) = v["doubleValue"].as_f64() {
            return Ok(d);
        }
        if let Some(i) = v["integerValue"].as_str() {
            return i
                .parse::<i64>()
                .map(|i| i as f64)
                .map_err(|_| format!("{name}: {i:?} is not a number"));
        }
        if v.is_null() {
            Err(format!("{name} is missing"))
        } else {
            Err(format!("{name} must be a number, got {v}"))
        }
    };
    Band::new(field("targetMin")?, field("targetMax")?)
}

/// The saved band, if there is a valid one.
pub fn load(path: &Path) -> Option<Band> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("cannot read {}: {e}", path.display());
            return None;
        }
    };
    let saved = serde_json::from_str::<Band>(&text)
        .map_err(|e| e.to_string())
        .and_then(|b| Band::new(b.min, b.max));
    match saved {
        Ok(band) => Some(band),
        Err(e) => {
            log::warn!("ignoring {}: {e}", path.display());
            None
        }
    }
}

/// Saves `band` atomically: temporary file, fsync, rename.
pub fn save(path: &Path, band: Band) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(&band)?)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Forgets the saved band: the config file applies again.
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::spool::tests::TempDir;

    fn doc(min: Value, max: Value) -> Value {
        json!({ "name": "x", "fields": { "targetMin": min, "targetMax": max } })
    }

    #[test]
    fn doubles_and_integers_are_accepted() {
        let band = parse(&doc(
            json!({"doubleValue": 7.5}),
            json!({"integerValue": "10"}),
        ));
        assert_eq!(
            band,
            Ok(Band {
                min: 7.5,
                max: 10.0
            })
        );
    }

    #[test]
    fn missing_or_wrong_fields_are_reported() {
        let only_min = json!({ "fields": { "targetMin": {"doubleValue": 7.0} } });
        assert_eq!(parse(&only_min).unwrap_err(), "targetMax is missing");
        let text = doc(json!({"stringValue": "7"}), json!({"doubleValue": 10.0}));
        assert!(
            parse(&text)
                .unwrap_err()
                .starts_with("targetMin must be a number")
        );
    }

    #[test]
    fn the_config_rules_apply() {
        let inverted = doc(json!({"doubleValue": 10.0}), json!({"doubleValue": 7.0}));
        assert!(parse(&inverted).unwrap_err().contains("must exceed"));
        let fahrenheit = doc(json!({"integerValue": "46"}), json!({"integerValue": "52"}));
        assert!(parse(&fahrenheit).unwrap_err().contains("outside"));
    }

    #[test]
    fn save_load_remove() {
        let dir = TempDir::new("settings");
        let path = path_next_to(&dir.spool());
        assert_eq!(path, dir.0.join("settings.json"));
        assert_eq!(load(&path), None);
        save(
            &path,
            Band {
                min: 7.0,
                max: 10.0,
            },
        )
        .unwrap();
        assert_eq!(
            load(&path),
            Some(Band {
                min: 7.0,
                max: 10.0
            })
        );
        remove(&path).unwrap();
        assert_eq!(load(&path), None);
        // Removing twice is fine.
        remove(&path).unwrap();
    }

    #[test]
    fn a_damaged_or_invalid_file_is_ignored() {
        let dir = TempDir::new("settings-bad");
        let path = path_next_to(&dir.spool());
        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), None);
        fs::write(&path, r#"{"min":10.0,"max":7.0}"#).unwrap();
        assert_eq!(load(&path), None);
    }
}
