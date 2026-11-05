//! `gdrive files diverged`: what changed in the origin of a migration (`files migrate`) after
//! it, and who did it, so that the owner can tell the people who keep working in the old
//! location where the files are now. It needs nothing but the Drive API, so it works for any
//! account, personal Gmail accounts included, which have no audit log (`files activity`).
//!
//! ```text
//!   origin: files.get of each FILE_ID; a folder is walked completely (transfer::walk) with
//!     lastModifyingUser and owners; the children in the trash of every listed folder (listing)
//!   with --to: the copy, found and compared by name as `files migrate` finds it (compare.rs),
//!     listed one level at a time; revisions.list of the copies that changed on both sides
//!   with --since: what was created or modified at or after WHEN (alone, or as a filter)
//!   the report on stdout once it is complete; what was read, left out and the summary on stderr
//! ```
//!
//! Nothing is ever changed in either account: every request is a read. A given id that does not
//! exist or is in the trash, a folder of the origin or of the copy that cannot be listed
//! completely and a missing copy of a given item fail the command with nothing on stdout,
//! because a partial comparison would call things added or unchanged wrongly. Revisions that
//! cannot be read are not a failure: the file is reported as `differs`.

mod compare;
mod listing;
mod origin;
mod report;
#[cfg(test)]
mod tests;

use std::{
    collections::HashSet,
    error,
    fmt::{self, Display, Formatter, Write as _},
    io,
    num::NonZeroUsize,
};

use chrono::{DateTime, Utc};

use compare::{Comparison, CopyFolder, Place, COPY_FIELDS};
use origin::{Node, Origin, Trashed};

use crate::{
    app_config::AppConfig,
    common::{
        drive_file,
        hub_helper::{get_hub_for, load_named_account, GetHubError},
        local_name::printable,
        retry::retry_read,
    },
    files::{
        activity::{report::render, when::When, Output},
        info::format_date_time,
    },
    hub::Hub,
    transfer::scheduler::{plural, OutputError, Printer},
};

/// The top folder of My Drive, where a copy is without `--into`.
const MY_DRIVE: &str = "root";

/// What the `--into` folder is checked for; a copy in a shared drive is listed in its corpus.
const INTO_FIELDS: &str = "id,name,mimeType,driveId";

/// `--by`: the rows of the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum By {
    /// One row per item that diverged, newest first
    Item,
    /// One row per person, most recent first
    User,
}

pub(crate) struct Config {
    /// The files and folders of the origin to check.
    pub(crate) file_ids: Vec<String>,
    /// The origin account; `None`: the current account.
    pub(crate) from: Option<String>,
    /// The account that holds the copy; `None`: no comparison, `--since` only.
    pub(crate) to: Option<String>,
    /// The folder of the copy's account that holds the copies; `None`: the top of My Drive.
    pub(crate) into: Option<String>,
    pub(crate) since: Option<When>,
    pub(crate) by: By,
    /// Email addresses whose changes are left out.
    pub(crate) exclude: Vec<String>,
    /// Requests at once.
    pub(crate) jobs: NonZeroUsize,
    pub(crate) output: Output,
}

/// What a report is about and how it is printed, apart from the accounts.
#[derive(Debug, Clone)]
struct Settings {
    file_ids: Vec<String>,
    into: Option<String>,
    since: Option<When>,
    by: By,
    exclude: Vec<String>,
    jobs: NonZeroUsize,
    output: Output,
}

/// Who changed an item, as Drive names them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Who {
    /// A lower-case email address.
    Email(String),
    /// A display name, when Drive shows no address.
    Name(String),
    /// Drive names nobody: `-`.
    Unknown,
}

impl Who {
    /// Whether this is one of the addresses of `exclude` (lower-case).
    fn is_excluded(&self, exclude: &HashSet<String>) -> bool {
        match self {
            Who::Email(email) => exclude.contains(email),
            Who::Name(_) | Who::Unknown => false,
        }
    }
}

impl Display for Who {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Who::Email(email) => f.write_str(email),
            Who::Name(name) => f.write_str(name),
            Who::Unknown => f.write_str("-"),
        }
    }
}

/// How an item of the origin diverged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Kind {
    /// It has no copy (its creation), or was created at or after `--since`.
    Added,
    /// It is newer than its copy, or it has no copy and was modified after its creation, or was
    /// modified at or after `--since`.
    Edited,
    /// Someone moved it to the trash.
    Trashed,
    /// Its content differs from the copy's, the copy is newer (or a time is missing), and no
    /// revision of the copy is the origin as it is now, or they cannot be read; or it is one of
    /// several origin files of one name left unmatched, which cannot be told apart.
    Differs,
    /// It was renamed or moved in the origin: its content is in the copy at `was`.
    Moved { was: String },
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Added => "added",
            Kind::Edited => "edited",
            Kind::Trashed => "trashed",
            Kind::Differs => "differs",
            Kind::Moved { .. } => "moved",
        }
    }
}

/// One row of `--by item`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    kind: Kind,
    /// `None`: Drive does not tell.
    when: Option<DateTime<Utc>>,
    who: Who,
    /// The names from the `FILE_ID` down, joined with `/`.
    path: String,
    id: String,
}

impl From<Trashed> for Change {
    fn from(trashed: Trashed) -> Self {
        Change {
            kind: Kind::Trashed,
            when: trashed.when,
            who: trashed.who,
            path: trashed.path,
            id: trashed.id,
        }
    }
}

/// Which account a listing failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Origin,
    Copy,
}

impl Display for Side {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Origin => "the origin",
            Side::Copy => "the copy",
        })
    }
}

pub(crate) async fn diverged(config: Config) -> Result<(), Error> {
    let origin_account = match &config.from {
        Some(name) => load_named_account(name),
        None => AppConfig::load_current_account().map_err(GetHubError::LoadCurrentAccount),
    }
    .map_err(|source| Error::OriginAccount {
        account: config.from.clone(),
        source,
    })?;
    let copy_account = config
        .to
        .as_deref()
        .map(|name| {
            load_named_account(name).map_err(|source| Error::CopyAccount {
                account: name.to_owned(),
                source,
            })
        })
        .transpose()?;
    if let Some(copy_account) = &copy_account {
        if copy_account.account.name == origin_account.account.name {
            return Err(Error::SameAccount(copy_account.account.name.clone()));
        }
    }

    let origin = get_hub_for(&origin_account)
        .await
        .map_err(|source| Error::OriginAccount {
            account: Some(origin_account.account.name.clone()),
            source,
        })?;
    let copy = match &copy_account {
        Some(account) => Some(
            get_hub_for(account)
                .await
                .map_err(|source| Error::CopyAccount {
                    account: account.account.name.clone(),
                    source,
                })?,
        ),
        None => None,
    };
    let settings = Settings {
        file_ids: config.file_ids,
        into: config.into,
        since: config.since,
        by: config.by,
        exclude: config.exclude,
        jobs: config.jobs,
        output: config.output,
    };
    let printer = Printer::stdio();
    run(&origin, copy.as_ref(), &settings, Utc::now(), &printer).await
}

/// Makes the report of `settings` about the origin account of `origin`, compared with the copy
/// in the account of `copy` if there is one, `now` being now, printing through `printer`.
async fn run(
    origin: &Hub,
    copy: Option<&Hub>,
    settings: &Settings,
    now: DateTime<Utc>,
    printer: &Printer,
) -> Result<(), Error> {
    let result = make_report(origin, copy, settings, now, printer).await;
    let flushed = printer.flush().await.map_err(Error::WriteOutput);
    result.and(flushed)
}

async fn make_report(
    origin_hub: &Hub,
    copy_hub: Option<&Hub>,
    settings: &Settings,
    now: DateTime<Utc>,
    printer: &Printer,
) -> Result<(), Error> {
    if copy_hub.is_none() && settings.into.is_some() {
        return Err(Error::IntoWithoutTo);
    }
    let since = settings.since.map(|when| when.at(now));
    let jobs = settings.jobs.get();

    // An id given twice is one item.
    let mut seen = HashSet::new();
    let ids = settings
        .file_ids
        .iter()
        .filter(|id| seen.insert(*id))
        .cloned()
        .collect::<Vec<_>>();
    let (changes, left_out) = match (copy_hub, since) {
        (None, None) => return Err(Error::NothingToCompare),
        (Some(copy), since) => {
            let (origin, trashed) = read_origin(origin_hub, &ids, jobs, printer).await?;
            let comparison = compare_with_copy(copy, &origin, settings, jobs, printer).await?;
            let mut left_out = LeftOut {
                shared: comparison.shared,
                ..LeftOut::default()
            };
            let mut changes = comparison.changes;
            for node in comparison.unsupported {
                // Never in the copy: only their times can tell.
                if since.is_some() {
                    changes.extend(new_item(&node));
                } else {
                    left_out.unsupported.push(node);
                }
            }
            changes.extend(trashed.into_iter().map(Change::from));
            (changes, left_out)
        }
        (None, Some(since)) => {
            let (origin, trashed) = read_origin(origin_hub, &ids, jobs, printer).await?;
            printer
                .err(format!(
                    "Reporting what was created or modified in the origin since {}, and what was \
                     moved from its folders to the trash; nothing is compared with a copy (--to)",
                    format_date_time(since)
                ))
                .await;
            let mut changes = origin
                .roots
                .iter()
                .flat_map(Node::all)
                .flat_map(new_item)
                .collect::<Vec<_>>();
            changes.extend(trashed.into_iter().map(Change::from));
            (changes, LeftOut::default())
        }
    };
    let mut changes = report::one_path_per_item(changes);
    // Each row on its own; a row without a time is kept, since nothing tells it is older.
    if let Some(since) = since {
        changes.retain(|change| change.when.is_none_or(|when| when >= since));
    }
    let exclude = settings
        .exclude
        .iter()
        .map(|email| email.trim().to_lowercase())
        .collect::<HashSet<_>>();
    let before = changes.len();
    changes.retain(|change| !change.who.is_excluded(&exclude));
    let excluded = before - changes.len();
    report::collapse(&mut changes);

    let text = match settings.by {
        By::Item => render(
            report::ITEM_HEADER,
            report::by_item(&changes),
            &settings.output,
        ),
        By::User => render(
            report::USER_HEADER,
            report::by_user(&changes),
            &settings.output,
        ),
    }
    .map_err(Error::Render)?;
    for line in text.lines() {
        printer.out(line).await;
    }

    left_out.print(printer).await;
    if excluded > 0 {
        printer
            .err(format!(
                "Left out {} of the people of --exclude",
                plural(count(excluded), "change", "changes")
            ))
            .await;
    }
    printer
        .err(summary(&changes, since, copy_hub.is_some()))
        .await;
    Ok(())
}

/// The origin's items of `ids` and what was moved from its folders to the trash, `jobs`
/// requests at once.
async fn read_origin(
    hub: &Hub,
    ids: &[String],
    jobs: usize,
    printer: &Printer,
) -> Result<(Origin, Vec<Trashed>), Error> {
    let origin = origin::resolve(hub, ids, jobs).await?;
    let trashed = origin::trash(hub, &origin, jobs).await?;
    printer
        .err(format!(
            "Found {} in {} in the origin, and {} moved from them to the trash",
            plural(origin.files, "file", "files"),
            plural(origin.folders, "folder", "folders"),
            plural(count(trashed.len()), "item", "items"),
        ))
        .await;
    Ok((origin, trashed))
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Finds the copy in the account of `copy` and compares `origin` with it.
async fn compare_with_copy(
    copy: &Hub,
    origin: &Origin,
    settings: &Settings,
    jobs: usize,
    printer: &Printer,
) -> Result<Comparison, Error> {
    let (into_id, into_name, drive_id) = match &settings.into {
        Some(id) => {
            let into = into_folder(copy, id).await?;
            // Both accounts see it: the copy would be compared with the origin's own items.
            if origin.ids.contains(&into.id) {
                return Err(Error::IntoInsideOrigin(into.id));
            }
            (into.id, Some(into.name), into.drive_id)
        }
        None => (MY_DRIVE.to_owned(), None, None),
    };
    let top_path = into_name
        .clone()
        .unwrap_or_else(|| String::from("My Drive"));
    let folder = listing::Folder {
        id: &into_id,
        path: &top_path,
        drive_id: drive_id.as_deref(),
        resource_key: None,
    };
    let mut top = listing::list_children(copy, &[folder], listing::Trash::Without, COPY_FIELDS, 1)
        .await
        .map_err(|error| error.into_error(Side::Copy))?;
    let top = CopyFolder::of(&top.remove(&into_id).unwrap_or_default());
    let place = Place {
        folder: &top,
        into: settings.into.as_deref().zip(into_name.as_deref()),
        drive_id: drive_id.as_deref(),
    };
    let comparison = compare::compare(copy, origin, &place, jobs).await?;
    printer
        .err(format!(
            "Found {} in {} in the copy",
            plural(comparison.copy_files, "file", "files"),
            plural(comparison.copy_folders, "folder", "folders"),
        ))
        .await;
    let place = match &into_name {
        Some(name) => format!("the folder '{}' (--into)", printable(name)),
        None => String::from("the top of My Drive"),
    };
    printer
        .err(format!(
            "Compared by name with the copy in {place}: files by size and MD5 checksum, Google \
             Docs, Sheets and Slides by modification time"
        ))
        .await;
    if comparison.revisions_read > 0 {
        let mut line = format!(
            "Read the revisions of {}, newer than the origin's file of the same name",
            plural(
                comparison.revisions_read,
                "file of the copy",
                "files of the copy"
            ),
        );
        if comparison.unsettled > 0 {
            let _ = write!(
                line,
                "; differs: {} whose present content no revision there holds",
                plural(
                    comparison.unsettled,
                    "file of the origin",
                    "files of the origin"
                )
            );
        }
        printer.err(line).await;
    }
    Ok(comparison)
}

/// The `--into` folder of the copy's account.
struct Into {
    id: String,
    name: String,
    /// The shared drive it is in, if any.
    drive_id: Option<String>,
}

/// The `--into` folder `id` of the copy's account, which must be a folder.
async fn into_folder(copy: &Hub, id: &str) -> Result<Into, Error> {
    let (_, file) = retry_read(copy.retrier(), "files.get", |mut delegate| async move {
        copy.files()
            .get(id)
            .param("fields", INTO_FIELDS)
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(|source| Error::GetInto {
        id: id.to_owned(),
        source: Box::new(source),
    })?;
    if !drive_file::is_directory(&file) {
        return Err(Error::IntoNotAFolder(id.to_owned()));
    }
    Ok(Into {
        id: file.id.unwrap_or_else(|| id.to_owned()),
        name: file.name.unwrap_or_default(),
        drive_id: file.drive_id,
    })
}

/// The rows of `node` when it is new to the copy, or by time: `added` when it was created (by
/// its owner, see [`origin::Item::creator`]), and `edited` too when it was modified after its
/// creation (by `lastModifyingUser`). Each is filtered by `--since` on its own.
fn new_item(node: &Node) -> Vec<Change> {
    let item = &node.item;
    let mut rows = vec![Change {
        kind: Kind::Added,
        when: item.created,
        who: item.creator(),
        path: node.path.clone(),
        id: item.id.clone(),
    }];
    if item.modified_after_creation() {
        rows.push(Change {
            kind: Kind::Edited,
            when: item.modified,
            who: item.editor(),
            path: node.path.clone(),
            id: item.id.clone(),
        });
    }
    rows
}

/// What the comparison did not look at.
#[derive(Debug, Default)]
struct LeftOut {
    /// Items a migration does not copy, without `--since`.
    unsupported: Vec<Node>,
    /// The paths of items both accounts see.
    shared: Vec<String>,
}

impl LeftOut {
    async fn print(mut self, printer: &Printer) {
        if !self.unsupported.is_empty() {
            self.unsupported.sort_by(|a, b| a.path.cmp(&b.path));
            printer
                .err(format!(
                    "Left out {} that a migration does not copy and the copy never has (--since \
                     reports those created or modified since a time):",
                    plural(count(self.unsupported.len()), "item", "items"),
                ))
                .await;
            for node in &self.unsupported {
                let reason = node
                    .item
                    .unsupported()
                    .map(|reason| reason.to_string())
                    .unwrap_or_default();
                printer
                    .err(format!("  {}: {reason}", printable(&node.path)))
                    .await;
            }
        }
        if !self.shared.is_empty() {
            self.shared.sort();
            printer
                .err(format!(
                    "Left out {} that both accounts see, not compared with themselves: the \
                     copy's folder holds the item itself, or another item of the origin under its \
                     name:",
                    plural(count(self.shared.len()), "item", "items"),
                ))
                .await;
            for path in &self.shared {
                printer.err(format!("  {}", printable(path))).await;
            }
        }
    }
}

/// The last line on stderr: `Summary: 6 items diverged in the origin (2 added, 3 edited,
/// 1 trashed) by 2 people`; with more rows than items, `6 items diverged in the origin, in 7
/// changes (...)`; without a comparison, `... changed in the origin since ...`.
fn summary(changes: &[Change], since: Option<DateTime<Utc>>, compared: bool) -> String {
    let what = if compared { "diverged" } else { "changed" };
    let items = report::items(changes);
    let mut line = format!(
        "Summary: {} {what} in the origin",
        plural(items, "item", "items")
    );
    if let Some(since) = since {
        let _ = write!(line, " since {}", format_date_time(since));
    }
    if changes.is_empty() {
        return line;
    }
    if count(changes.len()) != items {
        let _ = write!(line, ", in {} changes", changes.len());
    }
    let counts = report::counts(changes)
        .into_iter()
        .map(|(name, count)| format!("{count} {name}"))
        .collect::<Vec<_>>();
    let _ = write!(
        line,
        " ({}) by {}",
        counts.join(", "),
        plural(report::people(changes), "person", "people")
    );
    let unknown = report::unknown(changes);
    if unknown > 0 {
        let _ = write!(
            line,
            "; {} without a known person",
            plural(unknown, "change", "changes")
        );
    }
    line
}

#[derive(Debug)]
pub(crate) enum Error {
    /// The origin account cannot be used; `account` is `None` for the current account.
    OriginAccount {
        account: Option<String>,
        source: GetHubError,
    },
    CopyAccount {
        account: String,
        source: GetHubError,
    },
    /// `--to` names the origin account.
    SameAccount(String),
    /// Neither `--to` nor `--since`.
    NothingToCompare,
    IntoWithoutTo,
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
    /// A folder of `side` could not be listed; `path` is its path from the name of its
    /// `FILE_ID`.
    ListFolder {
        side: Side,
        path: String,
        source: Box<google_drive3::Error>,
    },
    /// Drive reported `incompleteSearch` while listing the folder `path` of `side`.
    IncompleteListing {
        side: Side,
        path: String,
    },
    GetInto {
        id: String,
        source: Box<google_drive3::Error>,
    },
    IntoNotAFolder(String),
    /// `--into` is one of the origin's folders or inside one: both accounts see it.
    IntoInsideOrigin(String),
    /// The given item `name` has no copy; `into` is the `--into` folder's id and name.
    MissingCopy {
        name: String,
        folder: bool,
        into: Option<(String, String)>,
    },
    /// The place of the copy of the given item `name` holds the item itself.
    SameItem {
        name: String,
        folder: bool,
    },
    /// The copy of the given item `name` is one of the origin's items.
    InsideOrigin {
        name: String,
        folder: bool,
    },
    Render(io::Error),
    /// stdout or stderr could not be written.
    WriteOutput(OutputError),
}

/// The ways out of a comparison that has no copy to compare with.
const NO_COPY_HINT: &str = "give the folder that holds the copy with --into, or run without --to \
                            and with --since <WHEN>, the time of the migration";

fn kind(folder: bool) -> &'static str {
    if folder {
        "folder"
    } else {
        "file"
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::OriginAccount {
                account: Some(account),
                ..
            } => write!(f, "unable to use the origin account '{}'", printable(account)),
            Error::OriginAccount { account: None, .. } => {
                f.write_str("unable to use the current account as the origin")
            }
            Error::CopyAccount { account, .. } => write!(
                f,
                "unable to use the account of the copy '{}'",
                printable(account)
            ),
            Error::SameAccount(account) => write!(
                f,
                "'{}' is both the origin and the account of the copy; --to must name the account \
                 the files were migrated to, see 'gdrive account list'",
                printable(account)
            ),
            Error::NothingToCompare => f.write_str(
                "nothing to compare with: give --to <ACCOUNT>, the account the files were migrated \
                 to, or --since <WHEN>, the time of the migration, or both",
            ),
            Error::IntoWithoutTo => f.write_str(
                "--into names a folder of the copy's account, which needs --to <ACCOUNT>",
            ),
            Error::GetItem { id, .. } => write!(f, "unable to get '{}'", printable(id)),
            Error::Trashed {
                id,
                name,
                by_folder: false,
            } => {
                let id = printable(id);
                write!(
                    f,
                    "'{}' ('{id}') is in the trash; restore it with `gdrive files untrash {id}`, \
                     or leave it out",
                    printable(name)
                )
            }
            Error::Trashed {
                id,
                name,
                by_folder: true,
            } => write!(
                f,
                "'{}' ('{}') is in a folder that is in the trash; restore that folder with \
                 `gdrive files untrash <FOLDER_ID>`, or leave it out",
                printable(name),
                printable(id)
            ),
            Error::ListFolder { side, path, .. } => {
                write!(f, "unable to list the folder '{}' of {side}", printable(path))
            }
            Error::IncompleteListing { side, path } => write!(
                f,
                "Drive did not list the folder '{}' of {side} completely, the report could call \
                 what is missing added or unchanged; run the command again later",
                printable(path)
            ),
            Error::GetInto { id, .. } => write!(
                f,
                "unable to get the folder '{}' of the copy's account (--into)",
                printable(id)
            ),
            Error::IntoNotAFolder(id) => write!(
                f,
                "'{}' is not a folder of the copy's account; --into takes the id of the folder \
                 that holds the copy",
                printable(id)
            ),
            Error::IntoInsideOrigin(id) => write!(
                f,
                "the folder '{}' of the copy's account is one of the origin's folders, or inside \
                 one; --into must name the folder that holds the copy",
                printable(id)
            ),
            Error::MissingCopy { .. } | Error::SameItem { .. } | Error::InsideOrigin { .. } => {
                self.fmt_no_copy(f)
            }
            Error::Render(_) => f.write_str("unable to format the report"),
            Error::WriteOutput(_) => f.write_str("unable to write the report"),
        }
    }
}

impl Error {
    /// The message of an error about the copy of a given item, which ends with the ways out.
    fn fmt_no_copy(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::MissingCopy { name, folder, into } => {
                let place = match into {
                    Some((id, into)) => {
                        format!("the folder '{}' ('{}')", printable(into), printable(id))
                    }
                    None => String::from("the top of its My Drive"),
                };
                write!(
                    f,
                    "'{}' has no copy in the copy's account: there is no {} of this name in \
                     {place}",
                    printable(name),
                    kind(*folder),
                )?;
            }
            Error::SameItem { name, folder } => write!(
                f,
                "the {} '{}' in the copy's account is the item itself, which both accounts see, \
                 not a copy",
                kind(*folder),
                printable(name)
            )?,
            Error::InsideOrigin { name, folder } => write!(
                f,
                "the {} named '{}' in the copy's account is one of the origin's items, which both \
                 accounts see, not a copy",
                kind(*folder),
                printable(name)
            )?,
            _ => return Ok(()),
        }
        write!(f, "; {NO_COPY_HINT}")
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::OriginAccount { source, .. } | Error::CopyAccount { source, .. } => Some(source),
            Error::GetItem { source, .. }
            | Error::ListFolder { source, .. }
            | Error::GetInto { source, .. } => Some(source.as_ref()),
            Error::Render(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
            Error::SameAccount(_)
            | Error::NothingToCompare
            | Error::IntoWithoutTo
            | Error::Trashed { .. }
            | Error::IncompleteListing { .. }
            | Error::IntoNotAFolder(_)
            | Error::IntoInsideOrigin(_)
            | Error::MissingCopy { .. }
            | Error::SameItem { .. }
            | Error::InsideOrigin { .. } => None,
        }
    }
}
