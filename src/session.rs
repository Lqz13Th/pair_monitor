use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use chrono_tz::America::New_York;

/// US equity session of a timestamp, in New York time. pair_arb trades Sunday 20:00 to Friday 20:00 ET
/// (everything but `Closed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 09:30-16:00
    Regular,
    /// 04:00-09:30
    Pre,
    /// 16:00-20:00
    Post,
    /// 20:00-04:00
    Overnight,
    /// Friday 20:00 to Sunday 20:00
    Closed,
}

impl Phase {
    pub fn at(t_us: u64) -> Phase {
        let Some(utc) = DateTime::<Utc>::from_timestamp_micros(t_us as i64) else {
            return Phase::Closed;
        };
        let et = utc.with_timezone(&New_York);
        let m = et.hour() * 60 + et.minute();
        let closed = match et.weekday() {
            Weekday::Sat => true,
            Weekday::Sun => m < 20 * 60,
            Weekday::Fri => m >= 20 * 60,
            _ => false,
        };
        if closed {
            return Phase::Closed;
        }
        match m {
            570..960 if !matches!(et.weekday(), Weekday::Sun) => Phase::Regular,
            240..570 if !matches!(et.weekday(), Weekday::Sun) => Phase::Pre,
            960..1200 if !matches!(et.weekday(), Weekday::Sun) => Phase::Post,
            _ => Phase::Overnight,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Phase::Regular => "regular",
            Phase::Pre => "pre",
            Phase::Post => "post",
            Phase::Overnight => "overnight",
            Phase::Closed => "closed",
        }
    }

    pub fn in_session(self) -> bool {
        self != Phase::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> Phase {
        let t = DateTime::parse_from_rfc3339(rfc3339).unwrap();
        Phase::at(t.timestamp_micros() as u64)
    }

    #[test]
    fn phases_follow_new_york_time() {
        assert_eq!(at("2026-10-07T13:35:00Z"), Phase::Regular); // Wed 09:35 EDT
        assert_eq!(at("2026-10-07T13:25:00Z"), Phase::Pre);
        assert_eq!(at("2026-10-07T20:05:00Z"), Phase::Post); // 16:05
        assert_eq!(at("2026-10-08T01:00:00Z"), Phase::Overnight); // Wed 21:00
        assert_eq!(at("2026-10-09T07:00:00Z"), Phase::Overnight); // Fri 03:00
        assert_eq!(at("2026-10-10T00:30:00Z"), Phase::Closed); // Fri 20:30
        assert_eq!(at("2026-10-10T15:00:00Z"), Phase::Closed); // Sat
        assert_eq!(at("2026-10-11T23:30:00Z"), Phase::Closed); // Sun 19:30
        assert_eq!(at("2026-10-12T00:30:00Z"), Phase::Overnight); // Sun 20:30
        assert_eq!(at("2026-12-07T14:35:00Z"), Phase::Regular); // Mon 09:35 EST
    }
}
