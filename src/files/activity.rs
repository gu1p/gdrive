//! `gdrive files activity`: who used files and folders, per person, from the Drive audit log of
//! the Google Workspace organization (Admin SDK Reports API, [`crate::common::audit_log`]). The
//! owner of an old location that was migrated (`files migrate`) learns who still opens it, to
//! contact them.
//!
//! ```text
//!   files.get of each FILE_ID; a folder is walked completely (transfer::walk)
//!     -> the items: id -> path
//!   permissions.list of each FILE_ID (--by user without --active-only): who has access
//!   activities.list over the time range, per --scan, --jobs series at once (scan.rs)
//!     -> each event about an item, primary, of a person not excluded, counted once (report.rs)
//!   the report on stdout; what was read, warnings and the summary on stderr
//! ```
//!
//! Nothing is ever changed in Drive: every request is a read. A given id that does not exist or
//! is in the trash, a folder that cannot be listed completely, a sharing list that cannot be read
//! and a page of the audit log that cannot be read fail the command, because a report over part
//! of the items or of the time range would tell wrongly that people are gone. The report is made
//! whole before its first line is printed, so a failure leaves stdout empty.

mod report;
mod scan;
mod sharing;
#[cfg(test)]
mod test_zone;
#[cfg(test)]
mod tests;
mod tree;
pub(crate) mod when;

use std::{
    collections::HashSet,
    error,
    fmt::{self, Display, Formatter, Write as _},
    io::{self, IsTerminal},
    num::NonZeroUsize,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use chrono::{DateTime, Utc};

pub(crate) use scan::Scan;
use scan::{Plan, Progress, WINDOW_HOURS};
use when::{Range, RangeError, When, RETENTION_DAYS};

use crate::{
    app_config::AppConfig,
    common::{
        audit_log::{AuditLog, AuditLogError},
        hub_helper::{get_audit_hub_for, load_named_account, GetHubError},
        local_name::printable,
    },
    files::info::format_date_time,
    hub::Hub,
    transfer::scheduler::{plural, OutputError, Printer},
};

/// How often a long scan prints how far it got, when stderr is a terminal.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// `--by`: the rows of the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum By {
    /// One row per person, most recent activity first, then the people with access and no activity
    User,
    /// One row per item and person with activity, by path
    File,
    /// One row per event, oldest first
    Event,
}

/// How the report is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Output {
    /// Aligned columns, as `files list` prints them.
    Table {
        skip_header: bool,
        field_separator: String,
    },
    /// Tab-separated values with escaped values, as `files list --tsv` prints them.
    Tsv { skip_header: bool },
}

pub(crate) struct Config {
    /// The files and folders of the report.
    pub(crate) file_ids: Vec<String>,
    /// The account that reads the audit log; `None`: the current account.
    pub(crate) account: Option<String>,
    pub(crate) since: When,
    /// `None`: now.
    pub(crate) until: Option<When>,
    pub(crate) by: By,
    /// Email addresses whose activity does not count.
    pub(crate) exclude: Vec<String>,
    pub(crate) active_only: bool,
    pub(crate) scan: Scan,
    /// Requests at once.
    pub(crate) jobs: NonZeroUsize,
    pub(crate) output: Output,
}

/// What a report is about and how it is printed, apart from the account's hub.
#[derive(Debug, Clone)]
struct Settings {
    file_ids: Vec<String>,
    /// As `gdrive account list` prints it, for the hints of errors.
    account: String,
    since: When,
    until: Option<When>,
    by: By,
    exclude: Vec<String>,
    active_only: bool,
    scan: Scan,
    jobs: NonZeroUsize,
    output: Output,
    /// How often a long scan prints how far it got; `None`: never.
    progress_every: Option<Duration>,
}

/// Who did something, or who has access.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Who {
    /// A lower-case email address.
    Person(String),
    /// Someone the audit log names no address of.
    Anonymous,
    /// A group on a sharing list.
    Group(String),
    /// Everyone of a domain on a sharing list.
    Domain(String),
    /// Anyone with the link.
    Anyone,
}

impl Who {
    /// Whether the address of this person or group is one of `exclude` (lower-case addresses).
    fn is_excluded(&self, exclude: &HashSet<String>) -> bool {
        match self {
            Who::Person(email) | Who::Group(email) => exclude.contains(email),
            Who::Anonymous | Who::Domain(_) | Who::Anyone => false,
        }
    }
}

impl Display for Who {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Who::Person(email) => f.write_str(email),
            Who::Anonymous => f.write_str("(anonymous)"),
            Who::Group(email) => write!(f, "group:{email}"),
            Who::Domain(domain) => write!(f, "domain:{domain}"),
            Who::Anyone => f.write_str("anyone"),
        }
    }
}

pub(crate) async fn activity(config: Config) -> Result<(), Error> {
    let account = match &config.account {
        Some(name) => load_named_account(name),
        None => AppConfig::load_current_account().map_err(GetHubError::LoadCurrentAccount),
    }
    .map_err(|source| Error::Account {
        account: config.account.clone(),
        source,
    })?;
    let hub = get_audit_hub_for(&account)
        .await
        .map_err(|source| Error::Account {
            account: Some(account.account.name.clone()),
            source,
        })?;
    let settings = Settings {
        file_ids: config.file_ids,
        account: account.account.name.clone(),
        since: config.since,
        until: config.until,
        by: config.by,
        exclude: config.exclude,
        active_only: config.active_only,
        scan: config.scan,
        jobs: config.jobs,
        output: config.output,
        progress_every: io::stderr().is_terminal().then_some(PROGRESS_EVERY),
    };
    let printer = Printer::stdio();
    run(&hub, &settings, Utc::now(), &printer).await
}

/// Makes the report of `settings` with the account of `hub`, `now` being now, printing through
/// `printer`.
async fn run(
    hub: &Hub,
    settings: &Settings,
    now: DateTime<Utc>,
    printer: &Printer,
) -> Result<(), Error> {
    let result = report(hub, settings, now, printer).await;
    let flushed = printer.flush().await.map_err(Error::WriteOutput);
    result.and(flushed)
}

async fn report(
    hub: &Hub,
    settings: &Settings,
    now: DateTime<Utc>,
    printer: &Printer,
) -> Result<(), Error> {
    let range = Range::resolve(settings.since, settings.until, now).map_err(Error::Range)?;
    let jobs = settings.jobs.get();

    // An id given twice is one item.
    let mut seen = HashSet::new();
    let ids = settings
        .file_ids
        .iter()
        .filter(|id| seen.insert(*id))
        .cloned()
        .collect::<Vec<_>>();
    let items = tree::resolve(hub, &ids, jobs).await?;
    let plan = Plan::choose(settings.scan, &items)?;
    printer
        .err(format!(
            "Found {} in {}",
            plural(items.files, "file", "files"),
            plural(items.folders, "folder", "folders"),
        ))
        .await;

    let exclude = settings
        .exclude
        .iter()
        .map(|email| email.trim().to_lowercase())
        .collect::<HashSet<_>>();
    // Before the audit log, which takes long: a sharing list that cannot be read fails early.
    // An excluded address is left out of it as well.
    let access = if settings.by == By::User && !settings.active_only {
        let mut access = sharing::read(hub, &items.roots).await?;
        access.retain(|who, _| !who.is_excluded(&exclude));
        Some(access)
    } else {
        None
    };

    if let Some(asked) = range.clamped_from {
        printer
            .err(format!(
                "Warning: the audit log keeps {RETENTION_DAYS} days; reading from {} instead of {}",
                format_date_time(range.since),
                format_date_time(asked),
            ))
            .await;
    }
    let series = scan::series(&plan, &range);
    printer.err(reading_line(&plan, &range, series.len())).await;

    let selection = report::Selection {
        items: &items.paths,
        exclude: &exclude,
    };
    let tally = Mutex::new(report::Tally::new(settings.by == By::Event));
    let progress = Progress::new(printer, settings.progress_every, series.len(), plan.units());
    let log = AuditLog::new(hub, &settings.account);
    scan::read(&log, series, jobs, &selection, &tally, &progress)
        .await
        .map_err(Error::ReadLog)?;
    let tally = tally.into_inner().unwrap_or_else(PoisonError::into_inner);
    printer
        .err(format!(
            "Read {} of the audit log, {} of them about these items",
            plural(tally.read, "event", "events"),
            tally.counted,
        ))
        .await;
    if tally.without_time > 0 {
        printer
            .err(format!(
                "Warning: {} about these items without a readable time are not counted",
                plural(tally.without_time, "event", "events"),
            ))
            .await;
    }

    let text = match settings.by {
        By::User => report::render(
            report::USER_HEADER,
            report::by_user(&tally, access.as_ref()),
            &settings.output,
        ),
        By::File => report::render(
            report::FILE_HEADER,
            report::by_file(&tally, &items.paths),
            &settings.output,
        ),
        By::Event => report::render(
            report::EVENT_HEADER,
            report::by_event(&tally, &items.paths),
            &settings.output,
        ),
    }
    .map_err(Error::Render)?;
    for line in text.lines() {
        printer.out(line).await;
    }

    let idle = access
        .as_ref()
        .map(|access| report::idle(&tally, Some(access)).len());
    printer
        .err(summary(&tally, &range, settings.until.is_some(), idle))
        .await;
    Ok(())
}

/// How the log is read: `Reading the audit log from A to B: every Drive event of the
/// organization, in 30 windows of 24 hours (--scan domain; ...)`. The complete default names the
/// faster scans, once.
fn reading_line(plan: &Plan, range: &Range, series: usize) -> String {
    let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let windows = |n: usize| {
        format!(
            "in {} of {WINDOW_HOURS} hours",
            plural(count(n), "window", "windows")
        )
    };
    let what = match plan {
        Plan::Domain => format!("every Drive event of the organization, {}", windows(series)),
        Plan::Drives(drives) => format!(
            "the events of {}, {}",
            plural(count(drives.len()), "shared drive", "shared drives"),
            windows(series / drives.len().max(1))
        ),
        Plan::Items(ids) => format!(
            "the events of each of the {}, one series of requests each",
            plural(count(ids.len()), "item", "items")
        ),
    };
    let faster = match plan {
        Plan::Domain => {
            "; --scan file is faster for a few items, --scan drive for items in shared drives"
        }
        Plan::Drives(_) | Plan::Items(_) => "",
    };
    format!(
        "Reading the audit log from {} to {}: {what} (--scan {}{faster})",
        format_date_time(range.since),
        format_date_time(range.until),
        plan.scan_name(),
    )
}

/// The last line on stderr: `Summary: 3 people active since 2026-10-06 00:00:00; 2 with access
/// and no activity`. `idle` is how many entries of the sharing lists had no activity, if they
/// were read.
fn summary(tally: &report::Tally, range: &Range, until_given: bool, idle: Option<usize>) -> String {
    let people = tally
        .users
        .keys()
        .filter(|who| matches!(who, Who::Person(_)))
        .count();
    let mut line = format!(
        "Summary: {} active",
        plural(
            u64::try_from(people).unwrap_or(u64::MAX),
            "person",
            "people"
        )
    );
    if until_given {
        let _ = write!(
            line,
            " from {} to {}",
            format_date_time(range.since),
            format_date_time(range.until)
        );
    } else {
        let _ = write!(line, " since {}", format_date_time(range.since));
    }
    if tally.users.contains_key(&Who::Anonymous) {
        line.push_str(", and anonymous users");
    }
    if let Some(idle) = idle {
        let _ = write!(line, "; {idle} with access and no activity");
    }
    line
}

#[derive(Debug)]
pub(crate) enum Error {
    /// The account cannot be used; `account` is `None` for the current account.
    Account {
        account: Option<String>,
        source: GetHubError,
    },
    Range(RangeError),
    GetItem {
        id: String,
        source: Box<google_drive3::Error>,
    },
    /// The `FILE_ID` `id`, named `name`, is in the trash; `by_folder`: because a folder above it
    /// is.
    Trashed {
        id: String,
        name: String,
        by_folder: bool,
    },
    /// A folder could not be listed; `path` is its path from the name of its `FILE_ID`.
    ListFolder {
        path: String,
        source: Box<google_drive3::Error>,
    },
    /// Drive reported `incompleteSearch` while listing the folder `path`.
    IncompleteListing {
        path: String,
    },
    /// `--scan drive` for a `FILE_ID` that is not in a shared drive.
    NotInSharedDrive {
        path: String,
    },
    /// The sharing list of the `FILE_ID` `path` could not be read.
    Sharing {
        path: String,
        source: Box<google_drive3::Error>,
    },
    ReadLog(AuditLogError),
    Render(io::Error),
    /// stdout or stderr could not be written.
    WriteOutput(OutputError),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Account {
                account: Some(account),
                ..
            } => write!(f, "unable to use the account '{}'", printable(account)),
            Error::Account { account: None, .. } => {
                f.write_str("unable to use the current account")
            }
            Error::Range(error) => error.fmt(f),
            Error::GetItem { id, .. } => write!(f, "unable to get '{}'", printable(id)),
            Error::Trashed {
                id,
                name,
                by_folder: false,
            } => {
                let id = printable(id);
                write!(
                    f,
                    "'{}' ('{id}') is in the trash, whose items the report leaves out; restore it \
                     with `gdrive files untrash {id}`, or leave it out",
                    printable(name)
                )
            }
            Error::Trashed {
                id,
                name,
                by_folder: true,
            } => write!(
                f,
                "'{}' ('{}') is in a folder that is in the trash, whose items the report leaves \
                 out; restore that folder with `gdrive files untrash <FOLDER_ID>`, or leave it out",
                printable(name),
                printable(id)
            ),
            Error::ListFolder { path, .. } => {
                write!(f, "unable to list the folder '{}'", printable(path))
            }
            Error::IncompleteListing { path } => write!(
                f,
                "Drive did not list '{}' completely, the report could miss what is in it; run the \
                 command again later",
                printable(path)
            ),
            Error::NotInSharedDrive { path } => write!(
                f,
                "'{}' is not in a shared drive, which --scan drive reads; use --scan domain or \
                 --scan file",
                printable(path)
            ),
            Error::Sharing { path, .. } => write!(
                f,
                "unable to read who has access to '{}' (--active-only goes without it)",
                printable(path)
            ),
            Error::ReadLog(_) => f.write_str("unable to read the audit log"),
            Error::Render(_) => f.write_str("unable to format the report"),
            Error::WriteOutput(_) => f.write_str("unable to write the report"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Account { source, .. } => Some(source),
            Error::GetItem { source, .. }
            | Error::ListFolder { source, .. }
            | Error::Sharing { source, .. } => Some(source.as_ref()),
            Error::ReadLog(source) => Some(source),
            Error::Render(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
            Error::Range(_)
            | Error::Trashed { .. }
            | Error::IncompleteListing { .. }
            | Error::NotInSharedDrive { .. } => None,
        }
    }
}
