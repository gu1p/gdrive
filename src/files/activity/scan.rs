//! How the audit log is read (`--scan`). Google has no filter for "everything under this
//! folder", so the items of the report always filter the events on this machine; the scan only
//! chooses how much Google sends:
//!
//! ```text
//!   domain  every Drive event of the organization         windows of 24 hours, no filters
//!   drive   the events of the shared drives of the items   windows of 24 hours,
//!                                                          filters=shared_drive_id==<ID>
//!   file    the events of each item                        the whole range,
//!                                                          filters=doc_id==<ID>
//!   auto    drive when every FILE_ID is in a shared drive, else domain (never file)
//! ```
//!
//! Each window (or item) is a series of requests that follows `nextPageToken`; up to `--jobs`
//! series run at once, and each page is counted ([`Tally`]) as it arrives, so that only the
//! events about the items stay in memory. A window ends a second after the next one starts: an
//! event at the border is read twice rather than lost, whichever end of its range Google
//! includes, and counted once.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, PoisonError,
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeDelta, Utc};
use futures::{stream, TryStreamExt};

use super::{
    report::{Selection, Tally},
    tree::Items,
    when::Range,
    Error,
};
use crate::{
    common::audit_log::{ActivityQuery, AuditLog, AuditLogError, Filter},
    transfer::scheduler::{plural, Printer},
};

/// How long a window of a `domain` or `drive` scan is.
pub(super) const WINDOW_HOURS: i64 = 24;

/// How far a window reaches into the next one.
const OVERLAP: TimeDelta = TimeDelta::seconds(1);

/// `--scan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Scan {
    /// The drive scan when every given item is in a shared drive, otherwise the domain scan
    Auto,
    /// Every Drive event of the organization: always complete, slow for a large organization
    Domain,
    /// The events of the shared drives of the given items, which must all be in shared drives
    Drive,
    /// The events of each item, one series of requests per item: fast for a few files
    File,
}

/// The requests a scan sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
    Domain,
    /// The shared drives, each once.
    Drives(Vec<String>),
    /// Every item.
    Items(Vec<String>),
}

impl Plan {
    /// The plan of `scan` for `items`. `drive` fails for an item outside shared drives.
    pub(super) fn choose(scan: Scan, items: &Items) -> Result<Plan, Error> {
        let mut drives = Vec::new();
        let mut outside = None;
        for root in &items.roots {
            match &root.drive_id {
                Some(drive) if !drives.contains(drive) => drives.push(drive.clone()),
                Some(_) => {}
                None => outside = outside.or(Some(root)),
            }
        }
        Ok(match (scan, outside) {
            (Scan::Domain, _) | (Scan::Auto, Some(_)) => Plan::Domain,
            (Scan::Drive, Some(root)) => {
                return Err(Error::NotInSharedDrive {
                    path: root.name.clone(),
                })
            }
            (Scan::Drive | Scan::Auto, None) => Plan::Drives(drives),
            (Scan::File, _) => {
                let mut ids = items.paths.keys().cloned().collect::<Vec<_>>();
                ids.sort();
                Plan::Items(ids)
            }
        })
    }

    /// The value of `--scan` that reads this way.
    pub(super) fn scan_name(&self) -> &'static str {
        match self {
            Plan::Domain => "domain",
            Plan::Drives(_) => "drive",
            Plan::Items(_) => "file",
        }
    }

    /// What the series of requests of this plan cover, for the progress lines.
    pub(super) fn units(&self) -> &'static str {
        match self {
            Plan::Domain | Plan::Drives(_) => "windows",
            Plan::Items(_) => "items",
        }
    }
}

/// One series of requests: the pages of one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Series {
    pub(super) query: ActivityQuery,
    /// Where the next window starts, if the query reaches into it: the events from there on
    /// are counted as read by the next window.
    next_start: Option<DateTime<Utc>>,
}

impl Series {
    /// Whether an event of `time` counts as read by this series (and not by the next window).
    fn reads(&self, time: Option<DateTime<Utc>>) -> bool {
        match (self.next_start, time) {
            (Some(next), Some(time)) => time < next,
            _ => true,
        }
    }
}

/// `range` in windows of [`WINDOW_HOURS`], the last one shorter.
pub(super) fn windows(range: &Range) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut windows = Vec::new();
    let mut start = range.since;
    while start < range.until {
        let end = start
            .checked_add_signed(TimeDelta::hours(WINDOW_HOURS))
            .map_or(range.until, |end| end.min(range.until));
        windows.push((start, end));
        start = end;
    }
    windows
}

/// The series of requests of `plan` over `range`.
pub(super) fn series(plan: &Plan, range: &Range) -> Vec<Series> {
    let windowed = |filter: Option<Filter>| {
        windows(range).into_iter().map(move |(start, end)| {
            let next_start = (end < range.until).then_some(end);
            Series {
                query: ActivityQuery {
                    start,
                    end: next_start.map_or(end, |next| (next + OVERLAP).min(range.until)),
                    filter: filter.clone(),
                },
                next_start,
            }
        })
    };
    match plan {
        Plan::Domain => windowed(None).collect(),
        Plan::Drives(drives) => drives
            .iter()
            .flat_map(|drive| windowed(Some(Filter::SharedDriveId(drive.clone()))))
            .collect(),
        Plan::Items(ids) => ids
            .iter()
            .map(|id| Series {
                query: ActivityQuery {
                    start: range.since,
                    end: range.until,
                    filter: Some(Filter::DocId(id.clone())),
                },
                next_start: None,
            })
            .collect(),
    }
}

/// Prints how far a long scan got, from time to time.
pub(super) struct Progress<'a> {
    printer: &'a Printer,
    /// How often; `None`: never.
    every: Option<Duration>,
    last: Mutex<Instant>,
    total: usize,
    unit: &'static str,
    done: AtomicUsize,
}

impl<'a> Progress<'a> {
    pub(super) fn new(
        printer: &'a Printer,
        every: Option<Duration>,
        total: usize,
        unit: &'static str,
    ) -> Self {
        Progress {
            printer,
            every,
            last: Mutex::new(Instant::now()),
            total,
            unit,
            done: AtomicUsize::new(0),
        }
    }

    /// A page was counted into `tally`.
    async fn page(&self, tally: &Mutex<Tally>) {
        let Some(every) = self.every else {
            return;
        };
        {
            let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            if last.elapsed() < every {
                return;
            }
            *last = Instant::now();
        }
        let (read, counted) = {
            let tally = tally.lock().unwrap_or_else(PoisonError::into_inner);
            (tally.read, tally.counted)
        };
        self.printer
            .err(format!(
                "Read {} so far, {counted} about these items ({} of {} {} done)",
                plural(read, "event", "events"),
                self.done.load(Ordering::SeqCst),
                self.total,
                self.unit,
            ))
            .await;
    }
}

/// Reads every series, `jobs` at once, counting each page into `tally` as it arrives. The first
/// failure ends the scan.
pub(super) async fn read(
    log: &AuditLog<'_>,
    series: Vec<Series>,
    jobs: usize,
    selection: &Selection<'_>,
    tally: &Mutex<Tally>,
    progress: &Progress<'_>,
) -> Result<(), AuditLogError> {
    stream::iter(series.into_iter().map(Ok))
        .try_for_each_concurrent(jobs, |series| async move {
            let mut page_token = None::<String>;
            loop {
                let page = log.page(&series.query, page_token.as_deref()).await?;
                {
                    let mut tally = tally.lock().unwrap_or_else(PoisonError::into_inner);
                    for record in &page.items {
                        tally.add(record, selection, |time| series.reads(time));
                    }
                }
                progress.page(tally).await;
                page_token = page.next_page_token;
                if page_token.is_none() {
                    break;
                }
            }
            progress.done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
}
