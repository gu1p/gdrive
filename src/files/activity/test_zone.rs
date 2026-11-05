//! `Europe/Berlin` as a chrono time zone, for tests of clock changes that must not depend on
//! the time zone of the machine (and must not set the process's, which tests running in
//! parallel share). CET (+01:00), and CEST (+02:00) from 01:00 UTC on the last Sunday of March
//! to 01:00 UTC on the last Sunday of October, as in the European Union since 1996.

use chrono::{
    Datelike, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeDelta, TimeZone, Weekday,
};

const CET: i32 = 3600;
const CEST: i32 = 2 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Berlin;

/// The last Sunday of `month` in `year`.
fn last_sunday(year: i32, month: u32) -> NaiveDate {
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let mut day = next_month.and_then(|first| first.pred_opt()).unwrap();
    while day.weekday() != Weekday::Sun {
        day = day.pred_opt().unwrap();
    }
    day
}

/// The offset in force at the UTC time `utc`.
fn offset_at(utc: &NaiveDateTime) -> FixedOffset {
    let change = |month| last_sunday(utc.year(), month).and_hms_opt(1, 0, 0).unwrap();
    let summer = (change(3)..change(10)).contains(utc);
    FixedOffset::east_opt(if summer { CEST } else { CET }).unwrap()
}

impl TimeZone for Berlin {
    type Offset = FixedOffset;

    fn from_offset(_: &FixedOffset) -> Self {
        Berlin
    }

    fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
        self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
    }

    /// Each offset that gives back `local`: none in the hour that spring skips, both (summer
    /// time first, the earlier instant) in the hour that autumn repeats.
    fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
        let fits = |seconds: i32| {
            let utc = *local - TimeDelta::seconds(seconds.into());
            let offset = offset_at(&utc);
            (offset.local_minus_utc() == seconds).then_some(offset)
        };
        match (fits(CEST), fits(CET)) {
            (Some(summer), Some(winter)) => LocalResult::Ambiguous(summer, winter),
            (Some(offset), None) | (None, Some(offset)) => LocalResult::Single(offset),
            (None, None) => LocalResult::None,
        }
    }

    fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
        offset_at(&utc.and_hms_opt(0, 0, 0).unwrap())
    }

    fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
        offset_at(utc)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};

    use super::Berlin;

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_zone_changes_its_clock_on_the_last_sundays_of_march_and_october() {
        let local = |rfc3339: &str| utc(rfc3339).with_timezone(&Berlin).to_rfc3339();
        assert_eq!(local("2026-03-29T00:59:59Z"), "2026-03-29T01:59:59+01:00");
        assert_eq!(local("2026-03-29T01:00:00Z"), "2026-03-29T03:00:00+02:00");
        assert_eq!(local("2026-10-25T00:59:59Z"), "2026-10-25T02:59:59+02:00");
        assert_eq!(local("2026-10-25T01:00:00Z"), "2026-10-25T02:00:00+01:00");
        assert_eq!(local("2026-07-01T12:00:00Z"), "2026-07-01T14:00:00+02:00");
        assert_eq!(local("2026-12-01T12:00:00Z"), "2026-12-01T13:00:00+01:00");

        let at = |h, m| {
            NaiveDate::from_ymd_opt(2026, 3, 29)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap()
        };
        assert!(Berlin.from_local_datetime(&at(2, 30)).single().is_none());
        assert_eq!(
            Berlin
                .from_local_datetime(&at(3, 30))
                .single()
                .map(|t| t.with_timezone(&Utc)),
            Some(utc("2026-03-29T01:30:00Z"))
        );
    }
}
