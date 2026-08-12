//! `Timestamp` newtype: RFC 3339 UTC, seconds precision, no external date dependency.

use serde::{Deserialize, Serialize};

/// RFC 3339 UTC timestamp, seconds precision, `Z` suffix: `2026-08-01T17:03:11Z`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub String);

impl Timestamp {
    /// Current wall-clock time in UTC.
    #[must_use]
    pub fn now() -> Timestamp {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Timestamp::from_unix_seconds(secs)
    }

    /// Formats Unix seconds as RFC 3339 UTC with seconds precision.
    #[must_use]
    pub fn from_unix_seconds(secs: u64) -> Timestamp {
        let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
        let rem = secs % 86_400;
        let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let mut year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        if month <= 2 {
            year += 1;
        }
        Timestamp(format!(
            "{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z"
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::time::Timestamp;

    #[test]
    fn epoch_zero_formats() {
        assert_eq!(Timestamp::from_unix_seconds(0).0, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn leap_day_2000_formats() {
        assert_eq!(
            Timestamp::from_unix_seconds(951_782_400).0,
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn spec_example_formats() {
        assert_eq!(
            Timestamp::from_unix_seconds(1_785_603_791).0,
            "2026-08-01T17:03:11Z"
        );
    }

    #[test]
    fn now_is_rfc3339_shape() {
        let ts = Timestamp::now().0;
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
