//! Samples in Firestore's typed JSON format.
//!
//! Firestore REST does not take plain JSON: every field is wrapped in its type,
//! e.g. `{"doubleValue": 12.5}`, and 64-bit integers travel as strings.

use serde_json::{Value, json};

use crate::sample::Sample;

/// Stable id for a sample's document: the same sample always maps to the same
/// document, so retrying an upload overwrites instead of duplicating. Boot id
/// and uptime never change, unlike the wall time (re-dated after NTP sync).
pub fn document_id(sample: &Sample) -> String {
    format!("{}-{:012}", sample.boot_id, sample.uptime_ms)
}

/// The `fields` object of a sample's document.
pub fn fields(sample: &Sample) -> Value {
    json!({
        "time": { "timestampValue": rfc3339(sample.time_ms) },
        "timeMs": integer(sample.time_ms),
        "clockSynced": { "booleanValue": sample.clock_synced },
        "timeCorrected": { "booleanValue": sample.time_corrected },
        "bootId": { "stringValue": sample.boot_id },
        "uptimeMs": integer(sample.uptime_ms),
        "temperature": match sample.temperature {
            Some(t) => json!({ "doubleValue": t }),
            None => json!({ "nullValue": null }),
        },
        "relay": enum_string(&sample.relay),
        "wanted": enum_string(&sample.wanted),
        "reason": enum_string(&sample.reason),
        "mode": enum_string(&sample.mode),
        "overrunFault": { "booleanValue": sample.overrun_fault },
        "targetMin": optional_double(sample.target_min),
        "targetMax": optional_double(sample.target_max),
    })
}

fn optional_double(v: Option<f64>) -> Value {
    match v {
        Some(d) => json!({ "doubleValue": d }),
        None => json!({ "nullValue": null }),
    }
}

fn integer(n: impl ToString) -> Value {
    json!({ "integerValue": n.to_string() })
}

/// A unit enum as the string serde gives it (`"ON"`, `"aboveMax"`, ...).
fn enum_string(v: &impl serde::Serialize) -> Value {
    let s = serde_json::to_value(v).ok();
    json!({ "stringValue": s.as_ref().and_then(Value::as_str).unwrap_or("") })
}

/// `2025-10-15T19:58:00.123Z` from milliseconds since the Unix epoch.
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::tests::sample;

    #[test]
    fn rfc3339_vectors() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z"); // leap day
        assert_eq!(rfc3339(1_760_558_280_123), "2025-10-15T19:58:00.123Z");
        assert_eq!(rfc3339(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn rfc3339_walks_every_day_of_a_leap_cycle() {
        // Day by day over 400 years: dates must advance by exactly one.
        let mut prev = civil_from_days(-1);
        for day in 0..146_097 {
            let (y, m, d) = civil_from_days(day);
            let next_ok = (y, m, d) == (prev.0, prev.1, prev.2 + 1)
                || (d == 1 && ((y, m) == (prev.0, prev.1 + 1) || (y, m) == (prev.0 + 1, 1)));
            assert!(next_ok, "{prev:?} → {:?}", (y, m, d));
            prev = (y, m, d);
        }
    }

    #[test]
    fn document_fields() {
        let mut s = sample(1_760_558_280_123);
        s.boot_id = "204aae0c".to_owned();
        s.uptime_ms = 42;
        assert_eq!(document_id(&s), "204aae0c-000000000042");
        assert_eq!(
            fields(&s),
            json!({
                "time": { "timestampValue": "2025-10-15T19:58:00.123Z" },
                "timeMs": { "integerValue": "1760558280123" },
                "clockSynced": { "booleanValue": true },
                "timeCorrected": { "booleanValue": false },
                "bootId": { "stringValue": "204aae0c" },
                "uptimeMs": { "integerValue": "42" },
                "temperature": { "doubleValue": 12.0 },
                "relay": { "stringValue": "OFF" },
                "wanted": { "stringValue": "OFF" },
                "reason": { "stringValue": "inBand" },
                "mode": { "stringValue": "NORMAL" },
                "overrunFault": { "booleanValue": false },
                "targetMin": { "doubleValue": 8.0 },
                "targetMax": { "doubleValue": 11.0 },
            })
        );
    }

    #[test]
    fn missing_temperature_is_null() {
        let mut s = sample(0);
        s.temperature = None;
        assert_eq!(fields(&s)["temperature"], json!({ "nullValue": null }));
    }
}
