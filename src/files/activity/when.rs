//! `--since` and `--until`: a duration back from now (`12h`, `30d`, `2w`), a date or a date and
//! time in the local time zone (`2026-10-01`, `2026-10-01 14:30`, `2026-10-01 14:30:00`, the
//! form gdrive prints times in), or an RFC 3339 time (`2026-10-01T14:30:00Z`).

use std::fmt::{self, Display, Formatter};

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeDelta, TimeZone, Utc};

use crate::files::info::format_date_time;

/// How long the audit log keeps events.
pub(crate) const RETENTION_DAYS: i64 = 180;

/// A point in time as `--since` and `--until` take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    /// This long before now.
    Ago(TimeDelta),
    At(DateTime<Utc>),
}

impl When {
    /// The time this is, `now` being now.
    #[must_use]
    pub(crate) fn at(self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            When::Ago(ago) => now
                .checked_sub_signed(ago)
                .unwrap_or(DateTime::<Utc>::MIN_UTC),
            When::At(time) => time,
        }
    }
}

/// [`parse_in`] the local time zone, for clap.
pub(crate) fn parse(value: &str) -> Result<When, String> {
    parse_in(value, &Local)
}

/// `value` as a [`When`]; a date or a date and time without an offset is in `zone`.
pub(crate) fn parse_in<Tz: TimeZone>(value: &str, zone: &Tz) -> Result<When, String> {
    let value = value.trim();
    if let Some(ago) = duration(value) {
        return ago.map(When::Ago);
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Ok(When::At(time.with_timezone(&Utc)));
    }
    let local = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .or_else(|| {
            ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"]
                .iter()
                .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        });
    let Some(local) = local else {
        return Err(format!(
            "'{value}' is not a time; use a duration back from now (12h, 30d, 2w), a date \
             (2026-10-01) or an RFC 3339 time (2026-10-01T08:00:00Z)"
        ));
    };
    // A time that a clock change skips does not exist; one it repeats is taken the first time.
    zone.from_local_datetime(&local)
        .earliest()
        .map(|time| When::At(time.with_timezone(&Utc)))
        .ok_or_else(|| format!("'{value}' does not exist in the local time zone"))
}

/// `value` as a duration, `<N>h`, `<N>d` or `<N>w`; `None` if it is not of that form.
fn duration(value: &str) -> Option<Result<TimeDelta, String>> {
    let unit = value.chars().last()?;
    let number = value.strip_suffix(unit)?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let delta = match unit {
        'h' => number.parse().ok().and_then(TimeDelta::try_hours),
        'd' => number.parse().ok().and_then(TimeDelta::try_days),
        'w' => number.parse().ok().and_then(TimeDelta::try_weeks),
        _ => return None,
    };
    Some(delta.ok_or_else(|| format!("'{value}' is too long ago")))
}

/// The time range of a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    pub(crate) since: DateTime<Utc>,
    pub(crate) until: DateTime<Utc>,
    /// The `--since` asked for, when it was older than the audit log and `since` is its oldest
    /// time instead.
    pub(crate) clamped_from: Option<DateTime<Utc>>,
}

impl Range {
    /// The range from `since` to `until` (now without one): an `until` in the future is now,
    /// a `since` older than the audit log is its oldest time.
    pub(crate) fn resolve(
        since: When,
        until: Option<When>,
        now: DateTime<Utc>,
    ) -> Result<Range, RangeError> {
        let horizon = now - TimeDelta::days(RETENTION_DAYS);
        let until = until.map_or(now, |until| until.at(now)).min(now);
        if until <= horizon {
            return Err(RangeError::BeforeRetention { horizon });
        }
        let asked = since.at(now);
        let since = asked.max(horizon);
        if since >= until {
            return Err(RangeError::Empty { since, until });
        }
        Ok(Range {
            since,
            until,
            clamped_from: (asked < horizon).then_some(asked),
        })
    }
}

/// A time range there is nothing to read in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeError {
    /// `--until` is older than the oldest event of the audit log, `horizon`.
    BeforeRetention { horizon: DateTime<Utc> },
    /// `--since` is not before `--until`.
    Empty {
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    },
}

impl Display for RangeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::BeforeRetention { horizon } => write!(
                f,
                "the audit log keeps {RETENTION_DAYS} days, from {}: --until must be later",
                format_date_time(*horizon)
            ),
            RangeError::Empty { since, until } => write!(
                f,
                "--since ({}) must be before --until ({})",
                format_date_time(*since),
                format_date_time(*until)
            ),
        }
    }
}

impl std::error::Error for RangeError {}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, FixedOffset, TimeDelta, Utc};

    use super::{parse_in, Range, RangeError, When};

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn durations_dates_and_times_are_read() {
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let parse = |value: &str| parse_in(value, &zone);
        assert_eq!(parse("12h"), Ok(When::Ago(TimeDelta::hours(12))));
        assert_eq!(parse("30d"), Ok(When::Ago(TimeDelta::days(30))));
        assert_eq!(parse(" 2w "), Ok(When::Ago(TimeDelta::weeks(2))));
        assert_eq!(parse("0d"), Ok(When::Ago(TimeDelta::zero())));
        // A date without a time starts at midnight of the local time zone.
        assert_eq!(
            parse("2026-10-01"),
            Ok(When::At(utc("2026-09-30T22:00:00Z")))
        );
        assert_eq!(
            parse("2026-10-01 14:30"),
            Ok(When::At(utc("2026-10-01T12:30:00Z")))
        );
        assert_eq!(
            parse("2026-10-01 14:30:15"),
            Ok(When::At(utc("2026-10-01T12:30:15Z")))
        );
        assert_eq!(
            parse("2026-10-01T14:30:00Z"),
            Ok(When::At(utc("2026-10-01T14:30:00Z")))
        );
        assert_eq!(
            parse("2026-10-01T14:30:00-05:00"),
            Ok(When::At(utc("2026-10-01T19:30:00Z")))
        );
        assert_eq!(
            parse_in("2026-10-01", &Utc),
            Ok(When::At(utc("2026-10-01T00:00:00Z")))
        );

        for bad in [
            "",
            "d",
            "30",
            "30m",
            "-5d",
            "1.5d",
            "30 d",
            "yesterday",
            "2026-13-01",
            "2026-10-01T25:00:00Z",
        ] {
            let error = parse(bad).unwrap_err();
            assert!(error.contains("is not a time"), "{bad}: {error}");
        }
        assert!(parse("99999999999999w")
            .unwrap_err()
            .contains("too long ago"));
    }

    #[test]
    fn the_range_is_clamped_to_the_audit_log_and_must_not_be_empty() {
        let now = utc("2026-11-05T12:00:00Z");
        let range = Range::resolve(When::Ago(TimeDelta::days(30)), None, now).unwrap();
        assert_eq!(range.since, utc("2026-10-06T12:00:00Z"));
        assert_eq!(range.until, now);
        assert_eq!(range.clamped_from, None);

        let range = Range::resolve(When::Ago(TimeDelta::days(200)), None, now).unwrap();
        assert_eq!(range.since, now - TimeDelta::days(180));
        assert_eq!(range.clamped_from, Some(now - TimeDelta::days(200)));

        let future = When::At(utc("2027-01-01T00:00:00Z"));
        let range = Range::resolve(When::Ago(TimeDelta::days(1)), Some(future), now).unwrap();
        assert_eq!(range.until, now, "the future is not read");

        assert_eq!(
            Range::resolve(
                When::Ago(TimeDelta::days(300)),
                Some(When::Ago(TimeDelta::days(181))),
                now
            ),
            Err(RangeError::BeforeRetention {
                horizon: now - TimeDelta::days(180)
            })
        );
        let since = When::At(utc("2026-11-01T00:00:00Z"));
        for until in [since, When::At(utc("2026-10-01T00:00:00Z"))] {
            assert!(matches!(
                Range::resolve(since, Some(until), now),
                Err(RangeError::Empty { .. })
            ));
        }
        assert!(matches!(
            Range::resolve(When::Ago(TimeDelta::zero()), None, now),
            Err(RangeError::Empty { .. })
        ));
        let message = Range::resolve(since, Some(since), now)
            .unwrap_err()
            .to_string();
        assert!(message.starts_with("--since ("), "{message}");
        assert!(message.contains(") must be before --until ("), "{message}");
    }
}
