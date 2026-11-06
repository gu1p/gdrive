//! `gdrive files migrate`: copies files and folders of one account into another account that is
//! logged in on the same machine. The content streams from the source through this machine into
//! the destination ([`crate::transfer::relay`]); nothing is written to disk.
//!
//! ```text
//!   source: files.get of each `FILE_ID`; a folder is walked completely (transfer::walk)
//!   destination: one listing of the --into folder (the top of My Drive by default)
//!     -> Scheduler, like `files upload --recursive`: a folder's job finds the destination folder
//!        of the same name (one listing of its children) or creates it, then releases its
//!        subfolders and files; files are relayed `--jobs` at a time
//! ```
//!
//! A re-run is idempotent: a destination folder of the same name is reused, and a file is
//! compared with the destination's files of its name before it is created. The same size and
//! md5 is "already there"; another content fails that item. Workspace files (no size or md5)
//! are already there when the destination has a file of their name and type. Shortcuts and
//! Workspace types without an export are skipped and listed at the end. A destination listing
//! with `incompleteSearch` fails its folder, so that a missing file is not created twice.
//!
//! Both accounts may see the same objects (a shared folder): a destination folder that holds
//! the source item itself (the same Drive id), or a destination folder or file of the item's
//! name that is any migrated source item, fails that item, and an `--into` folder that is
//! migrated, or inside a migrated folder, is refused before anything starts.
//!
//! `--contents` leaves out the folder of each `FILE_ID` that is one: what is in it goes straight
//! into the destination folder, as the items of one folder (`FOLDER/*` to `DEST/*`). The given
//! folder has no counterpart in the destination then, and never goes to the trash.
//!
//! `--replace-smaller` settles one kind of conflict: when the destination's only file of the
//! item's name has other content and fewer bytes than the source's, the source's content goes up
//! as a new version of that file (the same id, folder and sharing), after one more look at it. A
//! file of the same size or larger, and one of several of that name, is a conflict as before.
//! Three checks around a replace: the folder is listed again for the name right before it (the
//! file must still be the only one, where and what it was); one run replaces a file once, and
//! never one that another item of the run found its own content in (`dest_use`); and the file is
//! read again once the upload returned, since a session completed by an earlier run answers with
//! what the file was then. A writer that changes the file while it is replaced is not seen: Drive
//! has no conditional update, and keeps what was replaced as an earlier version.
//!
//! `--ask` puts the conflicts that a replace can settle to the user (`ask.rs`): both files as
//! Drive has them now, and a yes replaces the destination's the same way. A no leaves it, and
//! the item is settled: no failure. With `--replace-smaller` too, only what that flag does not
//! settle is asked about. A dry run asks nothing and tells what it would ask about.
//!
//! `--dry-run` runs the same jobs without a write: folders that would be created are not
//! listed (they would be empty), and only the destination folders that exist are read.
//!
//! `--trash-source` moves a source file to the trash once it is in the destination (relayed and
//! verified, or already there with the same md5), and a source folder once everything in it is:
//! each folder counts its children that went to the trash, and the last one releases the job
//! that trashes the folder. A folder with anything that was not migrated (a failure, an
//! unsupported item, an incomplete listing) stays, and so do its ancestors. Right before an item
//! goes, it is read again: a file whose size, md5 or modification time changed since the listing
//! stays, and so does a folder that holds anything not in the trash (added during the run).
//!
//! `--archive` makes each `FILE_ID` one tar file in the destination instead (`archive.rs`), also
//! inside one account (`--to` the source account): nothing but gdrive puts a folder into one file
//! there. A copy of the tree inside one account is refused: Drive copies on its side.

mod archive;
mod ask;
mod source;
#[cfg(test)]
mod tests;

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    error,
    fmt::{self, Display, Formatter, Write as _},
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use futures::stream;
use google_drive3::api::{File, FileList};

use ask::{Ask, Side};
use source::{SourceFile, SourceFolder, SourceItem, SourceNode, Totals};

use crate::{
    app_config::{account_type::AccountType, AppConfig},
    auth::service::parse_key,
    common::{
        delegate::{ChunkSize, MultipartThreshold},
        drive_file,
        hub_helper::{get_hub_for, load_named_account, GetHubError},
        human_size::human_size,
        id_gen::NextError,
        media_client::get_file,
        retry::retry_read,
    },
    files::{
        generate_ids::generate_ids,
        list::{quote, ListQuery},
        mkdir,
        update::{update_metadata, PatchFile},
    },
    hub::Hub,
    transfer::{
        progress::{self, ConfirmedTotal, ProgressDisplay, ProgressEvent},
        relay::{self, RelayError, Unsupported},
        scheduler::{
            self, plural, Job, JobContext, JobKind, JobOutput, Outcome, Printer, Report, Scheduler,
            SkipReason, Subtree,
        },
        upload::{session, UploadOptions, UploadTarget},
    },
};

/// The destination folder without `--into`: the top folder of My Drive.
const MY_DRIVE: &str = "root";

/// How many ids one `files.generateIds` request asks for.
const ID_BATCH: i32 = 100;

/// The largest page of a destination folder's listing.
const LIST_PAGE_SIZE: i32 = 1000;

/// What a destination folder's listing returns of each file.
const DEST_FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,size,md5Checksum)";

/// A source folder is checked for leftovers with one small page: any file is one too many.
const LEFTOVER_PAGE_SIZE: i32 = 10;
const LEFTOVER_FIELDS: &str = "nextPageToken,incompleteSearch,files(id)";

/// What a source file is checked for right before it goes to the trash.
const RECHECK_FIELDS: &str = "id,size,md5Checksum,modifiedTime";

/// What a destination file is checked for right before it is replaced.
const REPLACED_FIELDS: &str = "id,size,md5Checksum,trashed";

#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags of the command line, orthogonal to each other"
)]
pub(crate) struct Config {
    /// The files and folders of the source account to migrate.
    pub(crate) file_ids: Vec<String>,
    /// The source account; `None`: the current account.
    pub(crate) from: Option<String>,
    /// The destination account.
    pub(crate) to: String,
    /// The destination folder; `None`: the top of My Drive.
    pub(crate) into: Option<String>,
    pub(crate) dry_run: bool,
    pub(crate) trash_source: bool,
    /// Each `FILE_ID` as one tar file (`--archive`).
    pub(crate) archive: bool,
    /// What is in each given folder goes straight into the destination folder (`--contents`).
    pub(crate) contents: bool,
    /// A smaller destination file of the same name is replaced (`--replace-smaller`).
    pub(crate) replace_smaller: bool,
    /// The user is asked about the conflicts that a replace can settle (`--ask`).
    pub(crate) ask: bool,
    pub(crate) chunk_size: ChunkSize,
    pub(crate) multipart_threshold: MultipartThreshold,
    pub(crate) print_chunk_errors: bool,
    pub(crate) print_chunk_info: bool,
    /// How many files are migrated at once, and whether the run goes on after a failure.
    pub(crate) transfer: scheduler::Options,
    pub(crate) progress: progress::Mode,
}

/// What a migration does, apart from the accounts.
#[derive(Debug, Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags of the command line and whether the accounts are one, orthogonal to \
              each other"
)]
struct Settings {
    file_ids: Vec<String>,
    into: Option<String>,
    dry_run: bool,
    trash_source: bool,
    archive: bool,
    /// A `FILE_ID` that is a folder is not migrated itself, only what is in it.
    contents: bool,
    /// A destination file of an item's name with other content is replaced when it is smaller.
    replace_smaller: bool,
    /// Who is asked about the other conflicts that a replace can settle (`--ask`).
    ask: Option<Arc<dyn Ask>>,
    /// The source and the destination are one account (only with `archive`): the top of My Drive
    /// is then a folder the source may hold too.
    same_account: bool,
    transfer: scheduler::Options,
}

pub(crate) async fn migrate(config: Config) -> Result<(), Error> {
    // A dry run asks nothing.
    if config.ask && !config.dry_run && !ask::Terminal::available() {
        return Err(Error::AskWithoutTerminal);
    }
    let source_account = match &config.from {
        Some(name) => load_named_account(name),
        None => AppConfig::load_current_account().map_err(GetHubError::LoadCurrentAccount),
    }
    .map_err(|source| Error::SourceAccount {
        account: config.from.clone(),
        source,
    })?;
    let dest_account =
        load_named_account(&config.to).map_err(|source| Error::DestinationAccount {
            account: config.to.clone(),
            source,
        })?;
    let same_account = source_account.account.name == dest_account.account.name;
    if same_account && !config.archive {
        return Err(Error::SameAccount(dest_account.account.name));
    }

    let source = get_hub_for(&source_account)
        .await
        .map_err(|error| Error::SourceAccount {
            account: Some(source_account.account.name.clone()),
            source: error,
        })?;
    let dest = if same_account {
        // One account, one hub: its token is renewed and stored in one place.
        source.clone()
    } else {
        get_hub_for(&dest_account)
            .await
            .map_err(|error| Error::DestinationAccount {
                account: dest_account.account.name.clone(),
                source: error,
            })?
    };

    let options = UploadOptions {
        chunk_size: config.chunk_size.in_bytes(),
        multipart_threshold: config.multipart_threshold.in_bytes(),
        verify_md5: true,
        print_chunk_errors: config.print_chunk_errors,
        print_chunk_info: config.print_chunk_info,
        // The sessions belong to the account that receives the uploads.
        session_dir: Some(session_dir(&dest_account)),
        session_identity: session_identity(&dest_account).await,
    };
    let printer = Printer::stdio();
    let settings = Settings {
        file_ids: config.file_ids,
        into: config.into,
        dry_run: config.dry_run,
        trash_source: config.trash_source,
        archive: config.archive,
        contents: config.contents,
        replace_smaller: config.replace_smaller,
        ask: config
            .ask
            .then(|| Arc::new(ask::Terminal::new(printer.clone())) as Arc<dyn Ask>),
        same_account,
        transfer: config.transfer,
    };
    // Bars that draw while a question waits for its answer would write over it.
    let progress = if config.ask {
        progress::Mode::Never
    } else {
        config.progress
    };
    let display = ProgressDisplay::recursive(progress, config.transfer.jobs);
    run(
        &source,
        &dest,
        &settings,
        &options,
        &printer,
        display,
        scheduler::ctrl_c(),
    )
    .await
}

/// Where the sessions of resumable uploads into `account` are recorded, as `files upload` records
/// those of the current account (`uploads/` of the account directory).
fn session_dir(account: &AppConfig) -> PathBuf {
    account.account_base_path().join(session::UPLOADS_DIR)
}

/// Who `account` is, when that is known without asking Drive: a service account's `client_email`
/// and subject, as for the current account in `files upload`. `None` for a user and for
/// Application Default Credentials, which the uploader asks Drive about.
async fn session_identity(account: &AppConfig) -> Option<String> {
    let AccountType::ServiceAccount { subject } = account.account_type().ok()? else {
        return None;
    };
    let content = tokio::fs::read(account.service_account_key_path())
        .await
        .ok()?;
    let key = parse_key(&content).ok()?;
    Some(session::service_account_identity(
        &key.client_email,
        subject.as_deref(),
    ))
}

/// Migrates per `settings` from the account of `source` to the account of `dest`, printing
/// through `printer` and showing the progress on `progress`, until everything is done or
/// `cancel` resolves.
async fn run(
    source: &Hub,
    dest: &Hub,
    settings: &Settings,
    options: &UploadOptions,
    printer: &Printer,
    progress: ProgressDisplay,
    cancel: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    let into = match &settings.into {
        Some(id) => destination_folder(dest, id).await?,
        // Inside one account the top of My Drive may be a folder to archive: its id tells.
        None if settings.same_account => destination_folder(dest, MY_DRIVE).await?,
        None => MY_DRIVE.to_owned(),
    };
    let fields = if settings.archive {
        archive::DEST_FIELDS
    } else {
        DEST_FIELDS
    };
    let existing = list_existing(dest, &into, fields).await?;

    let listing_jobs = settings.transfer.metadata_jobs.get();
    let mut roots = Vec::new();
    let mut totals = Totals::default();
    // An id given twice would be copied twice: both copies compare with the same listing.
    let mut seen = HashSet::new();
    for id in settings.file_ids.iter().filter(|&id| seen.insert(id)) {
        let root = source::discover(source, id, listing_jobs, printer).await;
        match &root {
            Ok(node) => totals.add(node),
            Err(_) => totals.files += 1,
        }
        roots.push((id, root));
    }
    // Every source id, for the destination folders and files that both accounts see: an
    // `--into` among them is refused, and so is one reused or matched later. Trashing a source
    // folder would trash what was put in it, and its listing would include the copies.
    // A requested id that could not be read is a source item all the same.
    let mut source_ids = settings.file_ids.iter().cloned().collect::<HashSet<_>>();
    for (_, root) in &roots {
        if let Ok(root) = root {
            root.collect_ids(&mut source_ids);
        }
    }
    if source_ids.contains(&into) {
        return Err(Error::IntoInsideSource(into));
    }
    printer
        .out(format!(
            "Found {} in {} with a total size of {}",
            plural(totals.files, "file", "files"),
            plural(totals.folders, "folder", "folders"),
            human_size(totals.bytes),
        ))
        .await;

    let migration = Arc::new(Migration {
        source: source.clone(),
        dest: dest.clone(),
        options: options.clone(),
        dry_run: settings.dry_run,
        trash_source: settings.trash_source,
        replace_smaller: settings.replace_smaller,
        ask: settings.ask.clone(),
        dest_use: Mutex::new(HashMap::new()),
        unsupported: Mutex::new(Vec::new()),
        tally: Tally::default(),
        ids: tokio::sync::Mutex::new(Vec::new()),
        source_ids,
    });
    let place = Place {
        folder: Some(into),
        existing: Arc::new(existing),
        trash: None,
    };
    let mut planned = (totals.files, totals.bytes);
    let jobs = if settings.archive {
        // One transfer per archive.
        let mut bytes = 0;
        let jobs = roots
            .into_iter()
            .map(|(id, root)| match root {
                Ok(node) => {
                    let (job, size) = migration.archive_job(node, place.clone());
                    bytes += size;
                    job
                }
                Err(error) => Job::done(JobKind::Media, id.clone(), failed(id, error)),
            })
            .collect::<Vec<_>>();
        planned = (u64::try_from(jobs.len()).unwrap_or(u64::MAX), bytes);
        jobs
    } else {
        roots
            .into_iter()
            .map(|(id, root)| match root {
                Ok(SourceNode::Folder(folder)) if settings.contents => {
                    migration.contents_job(folder, place.clone())
                }
                Ok(SourceNode::Folder(folder)) => migration.folder_job(folder, place.clone()),
                Ok(SourceNode::File(item)) => migration.file_job(item, place.clone()),
                Err(error) => Job::done(JobKind::Media, id.clone(), failed(id, error)),
            })
            .collect::<Vec<_>>()
    };

    progress.sink().event(&ProgressEvent::Planned {
        files: planned.0,
        bytes: planned.1,
    });
    let report = Scheduler::new(settings.transfer, printer.clone())
        .with_progress(progress.sink())
        .run(stream::iter(jobs), cancel)
        .await;
    progress.finish();

    finish(&report, &migration, settings, totals.incomplete, printer).await
}

/// The id of the destination folder `id` (`--into`), which must be a folder.
async fn destination_folder(dest: &Hub, id: &str) -> Result<String, Error> {
    let (_, file) = retry_read(dest.retrier(), "files.get", |mut delegate| async move {
        dest.files()
            .get(id)
            .param("fields", "id,name,mimeType")
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(|source| Error::GetDestinationFolder {
        id: id.to_owned(),
        source: Box::new(source),
    })?;
    if !drive_file::is_directory(&file) {
        return Err(Error::NotAFolder(id.to_owned()));
    }
    Ok(file.id.unwrap_or_else(|| id.to_owned()))
}

/// Prints the end of a migration and tells whether it failed.
async fn finish(
    report: &Report,
    migration: &Migration,
    settings: &Settings,
    incomplete_listing: bool,
    printer: &Printer,
) -> Result<(), Error> {
    let mut result = report.check().map_err(Error::Incomplete);
    if result.is_ok() && incomplete_listing && !settings.transfer.continue_on_error {
        result = Err(Error::IncompleteListing);
    }

    let mut unsupported = std::mem::take(
        &mut *migration
            .unsupported
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    if !unsupported.is_empty() {
        unsupported.sort();
        let count = u64::try_from(unsupported.len()).unwrap_or(u64::MAX);
        printer
            .err(format!(
                "Skipped {}, which cannot be migrated:",
                plural(count, "unsupported item", "unsupported items")
            ))
            .await;
        for line in unsupported {
            printer.err(format!("  {line}")).await;
        }
    }
    printer
        .err(summary(report, &migration.tally, settings))
        .await;
    printer.flush().await.map_err(Error::WriteOutput)?;
    result
}

/// The last line on stderr, for example `Summary: 12 migrated (3.4 MB), 2 already there,
/// 1 skipped (unsupported), 0 failed, 0 cancelled; 3 folders created, 1 reused`.
fn summary(report: &Report, tally: &Tally, settings: &Settings) -> String {
    if settings.archive {
        return archive::summary(report, tally, settings);
    }
    let (migrated, converted, created, trashed) = if settings.dry_run {
        (
            "to migrate",
            "to convert",
            "to create",
            "to move to the trash in the source",
        )
    } else {
        (
            "migrated",
            "converted",
            "created",
            "moved to the trash in the source",
        )
    };
    let files = &report.files;
    let mut line = String::new();
    if settings.dry_run {
        line.push_str("Dry run, nothing was changed. ");
    }
    let _ = write!(
        line,
        "Summary: {} {migrated} ({}",
        files.transferred,
        human_size(files.bytes)
    );
    let conversions = tally.converted.load(Ordering::SeqCst);
    if conversions > 0 {
        let conversions = plural(
            conversions,
            "Google Workspace file",
            "Google Workspace files",
        );
        let _ = write!(line, ", {conversions} {converted}");
    }
    let _ = write!(
        line,
        "), {} already there, {} skipped (unsupported), {} failed, {} cancelled; {} {created}, {} \
         reused",
        // A conflict that was left as it is counts further on.
        files
            .unchanged
            .saturating_sub(tally.left.load(Ordering::SeqCst)),
        files.skipped,
        files.failed,
        files.cancelled,
        plural(
            tally.folders_created.load(Ordering::SeqCst),
            "folder",
            "folders"
        ),
        tally.folders_reused.load(Ordering::SeqCst),
    );
    if report.folders.failed > 0 {
        let folders = plural(report.folders.failed, "folder", "folders");
        let _ = write!(line, "; {folders} failed");
    }
    line.push_str(&replaced(tally, settings));
    if settings.trash_source {
        let _ = write!(line, "; {} {trashed}", tally.trashed.load(Ordering::SeqCst));
        let kept = tally.kept.load(Ordering::SeqCst);
        if kept > 0 {
            let _ = write!(line, ", {kept} kept there");
        }
    }
    if report.interrupted {
        line.push_str("; interrupted");
    }
    line
}

/// The part of a summary about `--replace-smaller` and `--ask`, for example `; 2 smaller files
/// replaced` or `; 1 file replaced, 2 conflicts left alone`; nothing without the flags.
fn replaced(tally: &Tally, settings: &Settings) -> String {
    let replaced = tally.replaced.load(Ordering::SeqCst);
    if settings.ask.is_some() {
        let files = plural(replaced, "file", "files");
        let left = plural(tally.left.load(Ordering::SeqCst), "conflict", "conflicts");
        return if settings.dry_run {
            format!("; {files} to replace, {left} to ask about")
        } else {
            format!("; {files} replaced, {left} left alone")
        };
    }
    if !settings.replace_smaller {
        return String::new();
    }
    let files = plural(replaced, "smaller file", "smaller files");
    if settings.dry_run {
        format!("; {files} to replace")
    } else {
        format!("; {files} replaced")
    }
}

/// The destination's files in one folder, by name.
#[derive(Debug, Default)]
struct Existing {
    by_name: HashMap<String, Vec<DestFile>>,
    /// The ids of all of them.
    ids: HashSet<String>,
}

#[derive(Debug)]
struct DestFile {
    id: String,
    mime_type: String,
    size: Option<i64>,
    md5: Option<String>,
    /// The manifest digest of an archive (`appProperties`), listed in archive runs only.
    archive: Option<String>,
    /// The md5 the archive had when it was checked (`appProperties`), likewise.
    archive_md5: Option<String>,
}

/// A destination file that `--replace-smaller` or `--ask` replaces, as the listing of its
/// folder has it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Smaller {
    id: String,
    size: u64,
    md5: Option<String>,
}

/// What an item of a run does with a destination file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    /// It found its content there: the file is what it is migrated to.
    Relied,
    /// It replaces the file's content, or is asking the user about that.
    Replaced,
}

/// What becomes of a conflict.
enum Settled {
    /// The destination's file gets the item's content.
    Replace(Smaller),
    /// The destination's file stays: the user said so, or it is a dry run, which only tells.
    Keep,
    /// Nobody was asked: the conflict it was.
    Conflict,
}

/// The content of an item in conflict, which would go into the destination's file.
enum Incoming<'a> {
    File(&'a SourceFile),
    /// A new archive of that many bytes.
    Archive(u64),
}

/// How a source file compares with the destination's files of its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// None has its name: create it.
    None,
    /// One has the same content (for a Workspace file, the same type). `verified`: the same
    /// md5.
    Same { verified: bool },
    /// Files of its name exist, all with other content.
    Different,
    /// A file of its name is one of the migrated source files, which both accounts see.
    Source,
}

impl Existing {
    fn of(files: Vec<File>) -> Self {
        let mut by_name = HashMap::<String, Vec<DestFile>>::new();
        let mut ids = HashSet::new();
        for file in files {
            let (Some(id), Some(name)) = (file.id, file.name) else {
                continue;
            };
            ids.insert(id.clone());
            let mut properties = file.app_properties.unwrap_or_default();
            by_name.entry(name).or_default().push(DestFile {
                id,
                mime_type: file.mime_type.unwrap_or_default(),
                size: file.size,
                md5: file.md5_checksum,
                archive: properties.remove(archive::PROPERTY),
                archive_md5: properties.remove(archive::PROPERTY_MD5),
            });
        }
        Existing { by_name, ids }
    }

    /// Whether the folder holds the file or folder `id` itself: one that both accounts see.
    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The first folder named `name`.
    fn folder(&self, name: &str) -> Option<&str> {
        self.by_name
            .get(name)?
            .iter()
            .find(|file| file.mime_type == drive_file::MIME_TYPE_DRIVE_FOLDER)
            .map(|file| file.id.as_str())
    }

    /// The file named `name`, when it is the only one (folders aside) and a file with content
    /// of a known size, no Google Workspace file or shortcut: what a source file of that name
    /// can replace.
    fn only_file(&self, name: &str) -> Option<Smaller> {
        let mut files = self
            .by_name
            .get(name)?
            .iter()
            .filter(|file| file.mime_type != drive_file::MIME_TYPE_DRIVE_FOLDER);
        let file = files.next().filter(|_| files.next().is_none())?;
        if file.mime_type.starts_with(source::GOOGLE_APPS_PREFIX) {
            return None;
        }
        Some(Smaller {
            id: file.id.clone(),
            size: u64::try_from(file.size?).ok()?,
            md5: file.md5.clone(),
        })
    }

    /// How `source` compares with the files of its name; one of them that is a source item
    /// itself (in `source_ids`) is [`Match::Source`].
    fn file(&self, source: &SourceFile, source_ids: &HashSet<String>) -> Match {
        let mut candidates = self
            .by_name
            .get(&source.name)
            .into_iter()
            .flatten()
            .filter(|file| file.mime_type != drive_file::MIME_TYPE_DRIVE_FOLDER)
            .peekable();
        if candidates.peek().is_none() {
            return Match::None;
        }
        if candidates.clone().any(|file| source_ids.contains(&file.id)) {
            return Match::Source;
        }
        if self.same(source).next().is_some() {
            Match::Same {
                verified: !source.is_native() && source.md5.is_some(),
            }
        } else {
            Match::Different
        }
    }

    /// The files of `source`'s name that have its content. A Workspace file's export is
    /// converted again: the copy has another size than the original, and neither has an md5.
    /// Its type is what can be compared.
    fn same<'a>(&'a self, source: &'a SourceFile) -> impl Iterator<Item = &'a DestFile> {
        let native = source.is_native();
        self.by_name
            .get(&source.name)
            .into_iter()
            .flatten()
            .filter(|file| file.mime_type != drive_file::MIME_TYPE_DRIVE_FOLDER)
            .filter(move |file| {
                if native {
                    file.mime_type == source.mime_type
                } else {
                    file.size == source.size && file.md5 == source.md5
                }
            })
    }
}

/// Lists the destination folder `folder_id`: the files that are not in the trash, with the
/// `fields` mask `fields`.
///
/// Fails when Drive answers a page with `incompleteSearch`: a file missing from the listing would
/// be taken for absent and created a second time.
async fn list_existing(dest: &Hub, folder_id: &str, fields: &str) -> Result<Existing, Error> {
    let q = ListQuery::files_in_folder(folder_id.to_owned()).q();
    list_files(dest, &q, fields).await
}

/// The destination's files that `q` names, with the `fields` mask `fields`; fails like
/// [`list_existing`].
async fn list_files(dest: &Hub, q: &str, fields: &str) -> Result<Existing, Error> {
    let mut files = Vec::new();
    let mut page_token = None;
    loop {
        let list = list_page(dest, q, LIST_PAGE_SIZE, fields, page_token.as_deref())
            .await
            .map_err(Error::ListDestination)?;
        if list.incomplete_search == Some(true) {
            return Err(Error::IncompleteDestination);
        }
        files.extend(list.files.unwrap_or_default());
        page_token = list.next_page_token;
        if page_token.is_none() {
            return Ok(Existing::of(files));
        }
    }
}

/// One page of the files `q` names (in any drive), with the `fields` mask `fields`.
async fn list_page(
    hub: &Hub,
    q: &str,
    page_size: i32,
    fields: &str,
    page_token: Option<&str>,
) -> Result<FileList, Box<google_drive3::Error>> {
    let (_, list) = retry_read(hub.retrier(), "files.list", |mut delegate| async move {
        let mut call = hub
            .files()
            .list()
            .q(q)
            .page_size(page_size)
            .supports_all_drives(true)
            .include_items_from_all_drives(true)
            .param("fields", fields)
            .add_scope(google_drive3::api::Scope::Full);
        if let Some(token) = page_token {
            call = call.page_token(token);
        }
        call.delegate(&mut delegate).doit().await
    })
    .await
    .map_err(Box::new)?;
    Ok(list)
}

/// What a source folder holds that is not in the trash, checked right before it goes there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leftover {
    Nothing,
    /// Something that was not migrated: added to the folder during the run.
    Items,
    /// Drive did not search everything.
    Unknown,
}

impl Leftover {
    /// Lists the first few files of the source folder `id` that are not in the trash.
    async fn of(source: &Hub, id: &str) -> Result<Leftover, Error> {
        let q = ListQuery::files_in_folder(id.to_owned()).q();
        let list = list_page(source, &q, LEFTOVER_PAGE_SIZE, LEFTOVER_FIELDS, None)
            .await
            .map_err(Error::Recheck)?;
        Ok(if list.files.is_some_and(|files| !files.is_empty()) {
            Leftover::Items
        } else if list.incomplete_search == Some(true) || list.next_page_token.is_some() {
            Leftover::Unknown
        } else {
            Leftover::Nothing
        })
    }
}

/// What the jobs of a migration share.
struct Migration {
    source: Hub,
    dest: Hub,
    options: UploadOptions,
    dry_run: bool,
    trash_source: bool,
    replace_smaller: bool,
    ask: Option<Arc<dyn Ask>>,
    /// What the items of this run do with the destination files they found by name, when
    /// replaces can happen: a file is replaced by one item at most, and never one that another
    /// item found its content in.
    dest_use: Mutex<HashMap<String, Use>>,
    /// `PATH: REASON` of every unsupported item, for the end of the output.
    unsupported: Mutex<Vec<String>>,
    tally: Tally,
    /// Ids from `files.generateIds` of the destination, not used yet.
    ids: tokio::sync::Mutex<Vec<String>>,
    /// The ids of every source file and folder that is migrated. A destination folder or file
    /// with one of them is part of the source, which both accounts see: it is never reused or
    /// matched.
    source_ids: HashSet<String>,
}

/// What the scheduler's report does not count.
#[derive(Debug, Default)]
struct Tally {
    folders_created: AtomicU64,
    folders_reused: AtomicU64,
    /// Workspace files migrated by export and conversion.
    converted: AtomicU64,
    /// Source files and folders moved to the trash.
    trashed: AtomicU64,
    /// Source files and folders that stay although they were to go to the trash.
    kept: AtomicU64,
    /// Destination files replaced (`--replace-smaller`, `--ask`).
    replaced: AtomicU64,
    /// Conflicts left alone (`--ask`): the user said so, or a dry run that would ask.
    left: AtomicU64,
    /// Items left out of the archives (`--archive`): shortcuts, Workspace types without an
    /// export, exports over Drive's limit.
    skipped: AtomicU64,
}

/// Where the items of a source folder go.
#[derive(Clone)]
struct Place {
    /// The destination folder. `None` in a dry run, for a folder that would be created.
    folder: Option<String>,
    /// The destination folder's files.
    existing: Arc<Existing>,
    /// The source folder that goes to the trash once everything in it did (`--trash-source`).
    trash: Option<Arc<FolderTrash>>,
}

/// A source folder of a `--trash-source` migration, until everything in it went to the trash.
struct FolderTrash {
    id: String,
    label: String,
    /// The children that did not go to the trash yet. A folder listed incompletely counts one
    /// more, which never goes, so that it stays.
    remaining: AtomicUsize,
    parent: Option<Arc<FolderTrash>>,
}

impl FolderTrash {
    /// A child went to the trash; `true` if it was the last one.
    fn child_trashed(&self) -> bool {
        self.remaining.fetch_sub(1, Ordering::SeqCst) == 1
    }
}

impl Migration {
    /// The job that releases what is in `folder` straight into `place` (`--contents`): the
    /// folder itself gets no destination folder and stays in the source, also when everything
    /// in it went to the trash.
    fn contents_job(self: &Arc<Self>, mut folder: SourceFolder, place: Place) -> Job {
        let label = folder.label.clone();
        let subtree = subtree(&folder);
        if let Some(error) = folder.error.take() {
            return Job::done(JobKind::Metadata, label.clone(), failed(&label, error))
                .with_subtree(subtree);
        }
        // In the destination its items have no folder of its name above them.
        folder.label_from_inside();
        let migration = Arc::clone(self);
        Job::new(JobKind::Metadata, label, move |_| async move {
            let output = JobOutput::from(Outcome::Transferred { bytes: 0 });
            migration.release(folder.children, place, output)
        })
        .with_subtree(subtree)
    }

    /// The job that finds or creates the destination folder of `folder` in `place`, and then
    /// releases its contents.
    fn folder_job(self: &Arc<Self>, mut folder: SourceFolder, place: Place) -> Job {
        let label = folder.label.clone();
        let subtree = subtree(&folder);
        if let Some(error) = folder.error.take() {
            return Job::done(JobKind::Metadata, label.clone(), failed(&label, error))
                .with_subtree(subtree);
        }
        let migration = Arc::clone(self);
        Job::new(JobKind::Metadata, label, move |context| async move {
            migration.migrate_folder(folder, place, &context).await
        })
        .with_subtree(subtree)
    }

    async fn migrate_folder(
        self: Arc<Self>,
        folder: SourceFolder,
        place: Place,
        context: &JobContext,
    ) -> JobOutput {
        let SourceFolder {
            id,
            name,
            label,
            children,
            incomplete,
            modified_time: _,
            error: _,
        } = folder;

        // Both accounts see this folder, and it is in the destination folder already.
        if place.existing.contains(&id) {
            return failed(&label, Error::SameItem { folder: true }).into();
        }
        let (folder_id, existing) = if let Some(dest_id) = place.existing.folder(&name) {
            // A source folder, which would go to the trash with what is put in it.
            if self.source_ids.contains(dest_id) {
                return failed(&label, Error::InsideSource { folder: true }).into();
            }
            context.printer.out(format!("reuse {label}/")).await;
            match list_existing(&self.dest, dest_id, DEST_FIELDS).await {
                Ok(existing) => {
                    self.tally.folders_reused.fetch_add(1, Ordering::SeqCst);
                    (Some(dest_id.to_owned()), existing)
                }
                Err(error) => return failed(&label, error).into(),
            }
        } else {
            context.printer.out(format!("create {label}/")).await;
            let folder_id = match place.folder.as_deref().filter(|_| !self.dry_run) {
                Some(parent) => match self.create_folder(&name, parent).await {
                    Ok(folder_id) => Some(folder_id),
                    Err(error) => return failed(&label, error).into(),
                },
                None => None,
            };
            self.tally.folders_created.fetch_add(1, Ordering::SeqCst);
            (folder_id, Existing::default())
        };

        let trash = self.trash_source.then(|| {
            Arc::new(FolderTrash {
                id,
                label,
                remaining: AtomicUsize::new(children.len() + usize::from(incomplete)),
                parent: place.trash.clone(),
            })
        });
        let place = Place {
            folder: folder_id,
            existing: Arc::new(existing),
            trash: trash.clone(),
        };

        let mut output = JobOutput::from(Outcome::Transferred { bytes: 0 });
        if let Some(trash) = trash.filter(|trash| trash.remaining.load(Ordering::SeqCst) == 0) {
            output = output.follow_up([self.trash_folder_job(trash)]);
        }
        self.release(children, place, output)
    }

    /// `output` followed by the jobs of `children`, the items of one source folder, in `place`.
    fn release(
        self: Arc<Self>,
        mut children: Vec<SourceNode>,
        place: Place,
        output: JobOutput,
    ) -> JobOutput {
        children.sort_by(|a, b| a.name().cmp(b.name()));
        let mut folders = Vec::new();
        let mut files = Vec::new();
        for child in children {
            match child {
                SourceNode::Folder(folder) => folders.push(folder),
                SourceNode::File(item) => files.push(item),
            }
        }

        // Two iterators: the subfolders are created while the files are migrated.
        let migration = Arc::clone(&self);
        let folder_place = place.clone();
        let folders = folders
            .into_iter()
            .map(move |folder| migration.folder_job(folder, folder_place.clone()));
        let files = files
            .into_iter()
            .map(move |item| self.file_job(item, place.clone()));
        output.follow_up(folders).follow_up(files)
    }

    fn file_job(self: &Arc<Self>, item: SourceItem, place: Place) -> Job {
        let label = item.label.clone();
        let size = item.file.content_size();
        let migration = Arc::clone(self);
        let job = Job::new(JobKind::Media, label, move |context| async move {
            migration.migrate_file(item, place, &context).await
        });
        match size {
            Some(size) => job.with_size(size),
            None => job,
        }
    }

    async fn migrate_file(
        self: Arc<Self>,
        item: SourceItem,
        place: Place,
        context: &JobContext,
    ) -> JobOutput {
        let SourceItem { file, label } = item;
        let drive_file = file.to_drive_file();
        if let Err(reason) = relay::check_supported(&drive_file) {
            return self.unsupported(&label, &reason, context).await;
        }

        if place.existing.contains(&file.id) {
            return failed(&label, Error::SameItem { folder: false }).into();
        }
        let matched = place.existing.file(&file, &self.source_ids);
        // The file of its name that it replaces: a smaller one (`--replace-smaller`), or the
        // one the user said to (`--ask`).
        let mut smaller = None;
        if matched == Match::Different {
            let incoming = Incoming::File(&file);
            match self
                .settle(&place, &file.name, &label, incoming, context)
                .await
            {
                Ok(Settled::Replace(old)) => smaller = Some(old),
                Ok(Settled::Keep) => return Outcome::Unchanged.into(),
                Ok(Settled::Conflict) => return failed(&label, Error::Conflict).into(),
                Err(error) => return failed(&label, error).into(),
            }
        }
        let (outcome, verified) = match matched {
            Match::Source => return failed(&label, Error::InsideSource { folder: false }).into(),
            Match::Same { verified } => {
                // Another item of this run replaces the files that had its content.
                let same = place.existing.same(&file).map(|file| file.id.as_str());
                if !self.rely(same) {
                    return failed(&label, Error::Conflict).into();
                }
                context.printer.out(format!("already-there {label}")).await;
                (Outcome::Unchanged, verified)
            }
            Match::None | Match::Different => {
                let line = match &smaller {
                    Some(old) => format!(
                        "replace {label} ({}, was {})",
                        human_size(file.bytes()),
                        human_size(old.size)
                    ),
                    None if file.is_native() => format!("convert {label}"),
                    None => format!("create {label} ({})", human_size(file.bytes())),
                };
                context.printer.out(line).await;
                let bytes = if self.dry_run {
                    file.bytes()
                } else {
                    let replaced = smaller.as_ref();
                    match self
                        .relay(&file, &drive_file, &place, replaced, context)
                        .await
                    {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            if let Some(reason) = error.unsupported() {
                                return self.unsupported(&label, reason, context).await;
                            }
                            return failed(&label, error).into();
                        }
                    }
                };
                if file.is_native() {
                    self.tally.converted.fetch_add(1, Ordering::SeqCst);
                }
                if smaller.is_some() {
                    self.tally.replaced.fetch_add(1, Ordering::SeqCst);
                }
                (Outcome::Transferred { bytes }, true)
            }
        };

        let mut output = JobOutput::from(outcome);
        if self.trash_source && verified {
            match self.unchanged(&file).await {
                Ok(true) => {}
                Ok(false) => {
                    self.keep(
                        format!("{label}: it changed in the source during the migration"),
                        context,
                    )
                    .await;
                    return output;
                }
                Err(error) => return failed(&label, error).into(),
            }
            if let Err(error) = self
                .trash(&file.id, format!("trash {label}"), context)
                .await
            {
                return failed(&label, error).into();
            }
            if let Some(job) = self.trashed(place.trash.as_ref()) {
                output = output.follow_up([job]);
            }
        }
        output
    }

    /// Relays `file` into the folder of `place`, or as the new content of the destination file
    /// `replaced`; the size of the new file.
    async fn relay(
        &self,
        file: &SourceFile,
        drive_file: &File,
        place: &Place,
        replaced: Option<&Smaller>,
        context: &JobContext,
    ) -> Result<u64, Error> {
        let target = if let Some(old) = replaced {
            self.still_listed(place, &file.name, old).await?;
            UploadTarget {
                name: Some(file.name.clone()),
                existing_file_id: Some(old.id.clone()),
                ..UploadTarget::default()
            }
        } else {
            // A conversion takes no id.
            let new_file_id = if file.is_native() {
                None
            } else {
                Some(self.new_id().await?)
            };
            UploadTarget {
                parents: place.folder.iter().cloned().collect(),
                name: Some(file.name.clone()),
                new_file_id,
                ..UploadTarget::default()
            }
        };
        // The relay tells the bytes Drive holds so far; progress takes what is new.
        let confirmed = ConfirmedTotal::new(context.progress.clone());
        let report = |held| confirmed.held(held);
        let created = relay::relay_file(
            &self.source,
            &self.dest,
            drive_file,
            &target,
            &self.options,
            Some(&report),
        )
        .await
        .map_err(|error| Error::Relay(Box::new(error)))?;
        if let Some(old) = replaced {
            self.holds(&old.id, created.md5_checksum.as_deref()).await?;
        }
        Ok(created
            .size
            .and_then(|size| u64::try_from(size).ok())
            .unwrap_or_else(|| confirmed.total()))
    }

    /// Whether replaces can happen in this run.
    fn replaces(&self) -> bool {
        self.replace_smaller || self.ask.is_some()
    }

    /// Notes that an item found its content in the destination files `ids`: it is already
    /// there. `false` when another item of this run replaces every one of them, which makes
    /// the listing that had them out of date.
    fn rely<'a>(&self, ids: impl Iterator<Item = &'a str>) -> bool {
        if !self.replaces() {
            return true;
        }
        let mut uses = self.dest_use.lock().unwrap_or_else(PoisonError::into_inner);
        let mut any = false;
        for id in ids {
            any |= *uses.entry(id.to_owned()).or_insert(Use::Relied) == Use::Relied;
        }
        any
    }

    /// Whether this item is the one of the run that replaces the destination file `id`, or
    /// asks about it: no other item does, and none found its own content in it.
    fn claim(&self, id: &str) -> bool {
        let mut uses = self.dest_use.lock().unwrap_or_else(PoisonError::into_inner);
        match uses.entry(id.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(Use::Replaced);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    /// What becomes of the conflict of the item `label`, named `name`: whether its content
    /// replaces the file of that name in `place`. Only the folder's only file of that name
    /// with content is replaced, by one item of a run: a smaller one (`--replace-smaller`), or
    /// the one the user says to (`--ask`). A Workspace file replaces none.
    async fn settle(
        &self,
        place: &Place,
        name: &str,
        label: &str,
        incoming: Incoming<'_>,
        context: &JobContext,
    ) -> Result<Settled, Error> {
        if !self.replaces() {
            return Ok(Settled::Conflict);
        }
        let size = match &incoming {
            Incoming::File(file) => file.content_size(),
            Incoming::Archive(size) => Some(*size),
        };
        let (Some(size), Some(old)) = (size, place.existing.only_file(name)) else {
            return Ok(Settled::Conflict);
        };
        let smaller = self.replace_smaller && old.size < size;
        if !(smaller || self.ask.is_some()) || !self.claim(&old.id) {
            return Ok(Settled::Conflict);
        }
        if smaller {
            return Ok(Settled::Replace(old));
        }

        if self.dry_run {
            let line = format!(
                "ask {label} ({}, the destination's has {})",
                human_size(size),
                human_size(old.size)
            );
            context.printer.out(line).await;
            self.tally.left.fetch_add(1, Ordering::SeqCst);
            return Ok(Settled::Keep);
        }
        if self.asked(place, name, label, size, incoming, &old).await? {
            return Ok(Settled::Replace(old));
        }
        // It stays what it is: no other item replaces it either, and one with its content
        // finds it there.
        self.dest_use
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(old.id.clone(), Use::Relied);
        context
            .printer
            .out(format!("keep-destination {label}"))
            .await;
        self.tally.left.fetch_add(1, Ordering::SeqCst);
        Ok(Settled::Keep)
    }

    /// Asks the user whether the item `label` replaces the destination file `old`, showing
    /// both as Drive has them now.
    async fn asked(
        &self,
        place: &Place,
        name: &str,
        label: &str,
        size: u64,
        incoming: Incoming<'_>,
        old: &Smaller,
    ) -> Result<bool, Error> {
        let Some(ask) = &self.ask else {
            return Ok(false);
        };
        self.still_listed(place, name, old).await?;
        let now = get_file(&self.dest, &old.id, ask::FIELDS, None)
            .await
            .map_err(|error| Error::Replace(ReplaceError::Check(error)))?;
        let (what, new) = match incoming {
            Incoming::File(file) => {
                let key = file.resource_key.as_deref();
                let shown = match get_file(&self.source, &file.id, ask::FIELDS, key).await {
                    Ok(now) => Side::of(&now),
                    // The listing has less to tell, and is enough.
                    Err(_) => Side {
                        size: Some(size),
                        modified: file.modified_time,
                        by: None,
                    },
                };
                ("source", shown)
            }
            Incoming::Archive(size) => {
                let shown = Side {
                    size: Some(size),
                    ..Side::default()
                };
                ("new archive", shown)
            }
        };
        let question = ask::question(label, what, &new, &Side::of(&now));
        Ok(ask.ask(question).await)
    }

    /// Fails unless `old` is still the only file named `name` in the folder of `place`, with
    /// the size and md5 that the listing of the folder had: asked right before it is replaced.
    /// A file that was renamed, moved, put in the trash or changed since, or that got a
    /// namesake, is not replaced.
    async fn still_listed(&self, place: &Place, name: &str, old: &Smaller) -> Result<(), Error> {
        let changed = || Error::Replace(ReplaceError::Changed);
        let folder = place.folder.as_deref().ok_or_else(changed)?;
        let q = format!(
            "{} in parents and name = {} and trashed = false",
            quote(folder),
            quote(name)
        );
        let now = list_files(&self.dest, &q, DEST_FIELDS).await?;
        if now.only_file(name).as_ref() == Some(old) {
            Ok(())
        } else {
            Err(changed())
        }
    }

    /// Fails unless the destination file `id` holds the content with the md5 `md5` now: asked
    /// once the upload that replaced it returned. A session that an earlier run completed
    /// answers with what the file was then, which need not be what it is.
    async fn holds(&self, id: &str, md5: Option<&str>) -> Result<(), Error> {
        let now = get_file(&self.dest, id, REPLACED_FIELDS, None)
            .await
            .map_err(|error| Error::Replace(ReplaceError::Check(error)))?;
        let held = matches!(
            (md5, now.md5_checksum.as_deref()),
            (Some(sent), Some(held)) if archive::same_md5(sent, held)
        );
        if held && now.trashed != Some(true) {
            Ok(())
        } else {
            Err(Error::Replace(ReplaceError::Unconfirmed))
        }
    }

    /// A new id for a file or folder of the destination, from `files.generateIds`.
    async fn new_id(&self) -> Result<String, Error> {
        let mut ids = self.ids.lock().await;
        if ids.is_empty() {
            *ids = generate_ids(&self.dest, ID_BATCH)
                .await
                .map_err(|error| Error::GenerateId(NextError::GenerateIds(error)))?;
        }
        ids.pop().ok_or(Error::GenerateId(NextError::OutOfIds))
    }

    /// Creates the folder `name` in the destination folder `parent`, under a pre-generated id
    /// (so that a retried create cannot make a second folder); its id.
    async fn create_folder(&self, name: &str, parent: &str) -> Result<String, Error> {
        let config = mkdir::Config {
            id: Some(self.new_id().await?),
            name: name.to_owned(),
            parents: Some(vec![parent.to_owned()]),
            print_only_id: false,
        };
        let created = mkdir::create_directory(&self.dest, &config)
            .await
            .map_err(|error| Error::CreateFolder(Box::new(error)))?;
        created.id.ok_or(Error::FolderMissingId)
    }

    async fn unsupported(
        &self,
        label: &str,
        reason: &Unsupported,
        context: &JobContext,
    ) -> JobOutput {
        self.note_skipped(label, reason, context).await;
        let reason = match reason {
            Unsupported::Shortcut => SkipReason::Shortcut,
            Unsupported::NativeType(_) => SkipReason::NotExportable,
        };
        Outcome::Skipped { reason }.into()
    }

    /// Whether the source `file` is still what was migrated or matched: the same size, md5 and
    /// modification time (each where both are known). A dry run changes nothing to look at.
    async fn unchanged(&self, file: &SourceFile) -> Result<bool, Error> {
        if self.dry_run {
            return Ok(true);
        }
        let source = &self.source;
        let id = file.id.as_str();
        let (_, now) = retry_read(source.retrier(), "files.get", |mut delegate| async move {
            source
                .files()
                .get(id)
                .param("fields", RECHECK_FIELDS)
                .supports_all_drives(true)
                .add_scope(google_drive3::api::Scope::Full)
                .delegate(&mut delegate)
                .doit()
                .await
        })
        .await
        .map_err(|error| Error::Recheck(Box::new(error)))?;
        let modified = match (now.modified_time, file.modified_time) {
            (Some(now), Some(then)) => now == then,
            _ => true,
        };
        Ok(now.size == file.size && now.md5_checksum == file.md5 && modified)
    }

    /// Prints that the source item `what` (its label and why) stays where it is.
    async fn keep(&self, what: String, context: &JobContext) {
        context.printer.out(format!("keep {what}")).await;
        self.tally.kept.fetch_add(1, Ordering::SeqCst);
    }

    /// Moves the source file or folder `id` to the trash (not in a dry run) and prints `line`.
    ///
    /// This is the only request of a migration that changes the source account, and nothing is
    /// ever deleted there for good. It is sent only with `--trash-source`: both callers ask
    /// that first, and without it this does nothing whoever calls it.
    async fn trash(&self, id: &str, line: String, context: &JobContext) -> Result<(), Error> {
        debug_assert!(self.trash_source, "the trash was not asked for");
        if !self.trash_source {
            return Ok(());
        }
        if !self.dry_run {
            let patch = PatchFile::new(id.to_owned()).with_trashed(true);
            update_metadata(&self.source, patch)
                .await
                .map_err(|error| Error::Trash(Box::new(error)))?;
        }
        context.printer.out(line).await;
        self.tally.trashed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// A child of `folder` went to the trash: the job that trashes `folder` when it was the
    /// last one.
    fn trashed(self: &Arc<Self>, folder: Option<&Arc<FolderTrash>>) -> Option<Job> {
        let folder = folder.filter(|folder| folder.child_trashed())?;
        Some(self.trash_folder_job(Arc::clone(folder)))
    }

    fn trash_folder_job(self: &Arc<Self>, folder: Arc<FolderTrash>) -> Job {
        let migration = Arc::clone(self);
        Job::new(
            JobKind::Metadata,
            folder.label.clone(),
            move |context| async move {
                let leftover = if migration.dry_run {
                    Ok(Leftover::Nothing)
                } else {
                    Leftover::of(&migration.source, &folder.id).await
                };
                let reason = match leftover {
                    Ok(Leftover::Nothing) => None,
                    Ok(Leftover::Items) => Some("it has files that were not migrated"),
                    Ok(Leftover::Unknown) => Some("Drive did not list it completely"),
                    Err(error) => return failed(&folder.label, error).into(),
                };
                if let Some(reason) = reason {
                    // It stays, and so do the folders around it.
                    migration
                        .keep(format!("{}/: {reason}", folder.label), &context)
                        .await;
                    return Outcome::Transferred { bytes: 0 }.into();
                }
                let line = format!("trash {}/", folder.label);
                if let Err(error) = migration.trash(&folder.id, line, &context).await {
                    return failed(&folder.label, error).into();
                }
                let output = JobOutput::from(Outcome::Transferred { bytes: 0 });
                match migration.trashed(folder.parent.as_ref()) {
                    Some(job) => output.follow_up([job]),
                    None => output,
                }
            },
        )
    }
}

fn failed(label: &str, source: Error) -> Outcome {
    Outcome::failed(Error::Item {
        label: label.to_owned(),
        source: Box::new(source),
    })
}

/// The files and folders below `folder`, and the sizes of the files.
fn subtree(folder: &SourceFolder) -> Subtree {
    let mut subtree = Subtree::default();
    let mut pending = vec![folder];
    while let Some(folder) = pending.pop() {
        for child in &folder.children {
            match child {
                SourceNode::File(item) => {
                    subtree.files += 1;
                    subtree.bytes += item.file.bytes();
                }
                SourceNode::Folder(folder) => {
                    subtree.folders += 1;
                    pending.push(folder);
                }
            }
        }
    }
    subtree
}

/// Why a destination file was not replaced (`--replace-smaller`, `--ask`).
#[derive(Debug)]
pub(crate) enum ReplaceError {
    /// The file could not be read for the check before or after its replacement.
    Check(Box<google_drive3::Error>),
    /// It is no longer the folder's only file of its name with the content that was listed.
    Changed,
    /// After the upload, it does not hold the content that was sent.
    Unconfirmed,
}

impl Display for ReplaceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ReplaceError::Check(_) => {
                "unable to check the file in the destination that it replaces"
            }
            ReplaceError::Changed => {
                "the file of this name in the destination changed during the migration; it was \
                 not replaced"
            }
            ReplaceError::Unconfirmed => {
                "the file in the destination does not hold the content that was sent to replace \
                 it; run the command again"
            }
        })
    }
}

impl ReplaceError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            ReplaceError::Check(source) => Some(source.as_ref()),
            ReplaceError::Changed | ReplaceError::Unconfirmed => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    /// The source account cannot be used; `account` is `None` for the current account.
    SourceAccount {
        account: Option<String>,
        source: GetHubError,
    },
    DestinationAccount {
        account: String,
        source: GetHubError,
    },
    /// `--to` names the source account.
    SameAccount(String),
    GetDestinationFolder {
        id: String,
        source: Box<google_drive3::Error>,
    },
    /// `--into` is not a folder.
    NotAFolder(String),
    ListDestination(Box<google_drive3::Error>),
    /// Drive did not search everything while listing a destination folder.
    IncompleteDestination,
    /// `--into` is a folder that is migrated, or is inside one: both accounts see it.
    IntoInsideSource(String),
    /// The destination folder holds the source item itself: both accounts see it.
    SameItem {
        folder: bool,
    },
    /// The destination folder or file of the item's name is a source item that is migrated (both
    /// accounts see it), and not the item itself.
    InsideSource {
        folder: bool,
    },
    /// The source item could not be checked right before it would go to the trash.
    Recheck(Box<google_drive3::Error>),
    GetSource {
        id: String,
        source: Box<google_drive3::Error>,
    },
    ListSource(crate::transfer::walk::FolderError),
    /// A source folder that is its own ancestor.
    Cycle,
    /// The destination has a file of the same name with other content.
    Conflict,
    /// A destination file was not replaced, or its replacement could not be confirmed.
    Replace(ReplaceError),
    /// `--ask` without a terminal to ask on.
    AskWithoutTerminal,
    CreateFolder(Box<google_drive3::Error>),
    FolderMissingId,
    /// No id for a new file or folder of the destination.
    GenerateId(NextError),
    Relay(Box<RelayError>),
    /// A folder of an archive could not be listed or is its own ancestor (`source`): an archive
    /// without it is not made.
    ArchiveUnlisted {
        folder: String,
        source: Box<Error>,
    },
    /// Drive reported `incompleteSearch` while listing this folder of an archive.
    ArchiveIncomplete(String),
    /// A name in an archive cannot be used as a file name.
    ArchiveName(crate::common::local_name::UnusableName),
    /// The listing has no size for this file of an archive.
    NoSize(String),
    /// The Workspace file of an archive could not be exported.
    Export {
        label: String,
        source: Box<RelayError>,
    },
    Archive(Box<crate::transfer::archive::ArchiveError>),
    /// The archive `name` (the file `id`) is in the destination and checked, but its mark could
    /// not be confirmed (the update failed, or its reply was lost after it applied).
    ArchiveMark {
        name: String,
        id: String,
        source: Box<google_drive3::Error>,
    },
    /// The item is in the destination, but could not be moved to the trash in the source.
    Trash(Box<google_drive3::Error>),
    /// An item of the migration failed; `label` is its path, starting with the name of its
    /// `FILE_ID`.
    Item {
        label: String,
        source: Box<Error>,
    },
    /// Items failed, or the migration was interrupted.
    Incomplete(scheduler::Incomplete),
    /// Drive reported `incompleteSearch` while listing a source folder.
    IncompleteListing,
    /// stdout or stderr could not be written.
    WriteOutput(scheduler::OutputError),
}

impl Error {
    /// Why the relay refused the file, if it is unsupported.
    fn unsupported(&self) -> Option<&Unsupported> {
        match self {
            Error::Relay(error) => error.unsupported(),
            _ => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::SourceAccount {
                account: Some(account),
                ..
            } => write!(f, "unable to use the source account '{account}'"),
            Error::SourceAccount { account: None, .. } => {
                f.write_str("unable to use the current account as the source")
            }
            Error::DestinationAccount { account, .. } => {
                write!(f, "unable to use the destination account '{account}'")
            }
            Error::SameAccount(account) => write!(
                f,
                "'{account}' is both the source and the destination account; --to must name \
                 another account, see 'gdrive account list' (inside one account only --archive \
                 works)"
            ),
            Error::GetDestinationFolder { id, .. } => {
                write!(f, "unable to get the destination folder '{id}'")
            }
            Error::NotAFolder(id) => write!(
                f,
                "'{id}' is not a folder of the destination account; --into takes a folder id"
            ),
            Error::ListDestination(_) => f.write_str("unable to list the destination folder"),
            Error::IncompleteDestination => f.write_str(
                "Drive did not list the destination folder completely, a file of the same name \
                 might be missed and made twice; run the command again later",
            ),
            Error::IntoInsideSource(id) => write!(
                f,
                "the destination folder '{id}' is one of the folders to migrate, or inside one; \
                 --into must name a folder outside them"
            ),
            Error::SameItem { folder } => write!(
                f,
                "the source and the destination are the same {}; nothing to do",
                if *folder { "folder" } else { "file" }
            ),
            Error::InsideSource { folder } => write!(
                f,
                "the {} of this name in the destination is one of the source items that are \
                 migrated; nothing to do",
                if *folder { "folder" } else { "file" }
            ),
            Error::Recheck(_) => {
                f.write_str("unable to check the source file before moving it to the trash")
            }
            Error::GetSource { id, .. } => {
                write!(f, "unable to get '{id}' from the source account")
            }
            // The walk's own message names the folder unescaped; the label of the item is enough.
            Error::ListSource(_) => f.write_str("unable to list the folder in the source"),
            Error::Cycle => f.write_str("the folder is inside itself, it is not migrated"),
            Error::Conflict => f.write_str("it exists in the destination with different content"),
            Error::Replace(error) => Display::fmt(error, f),
            Error::AskWithoutTerminal => f.write_str(
                "--ask needs a terminal: it asks on stderr and reads the answers from the \
                 keyboard. In a script, use --replace-smaller or neither",
            ),
            Error::CreateFolder(_) => f.write_str("unable to create the folder in the destination"),
            Error::FolderMissingId => {
                f.write_str("the folder created in the destination has no id")
            }
            Error::GenerateId(_) => f.write_str("unable to get an id for the destination"),
            Error::Relay(error) => error.fmt(f),
            Error::ArchiveUnlisted { folder, .. } => {
                write!(f, "the archive would miss what is in '{folder}/'")
            }
            Error::ArchiveIncomplete(folder) => write!(
                f,
                "Drive did not list '{folder}/' completely, the archive would miss files; run the \
                 command again later"
            ),
            Error::ArchiveName(_) => f.write_str("a name cannot be used in the archive"),
            Error::NoSize(label) => write!(
                f,
                "Drive lists no size for '{label}'; run the command again later"
            ),
            Error::Export { label, .. } => write!(f, "unable to export '{label}'"),
            Error::Archive(error) => error.fmt(f),
            Error::ArchiveMark { name, id, .. } => write!(
                f,
                "the archive '{name}' is complete and checked in the destination (id {id}), but \
                 marking it could not be confirmed: a later run reports it as already there if \
                 the mark was written, and otherwise as a file of that name with other content; \
                 nothing was deleted"
            ),
            Error::Trash(_) => f.write_str(
                "it is in the destination, but it could not be moved to the trash in the source",
            ),
            Error::Item { label, .. } => write!(f, "unable to migrate '{label}'"),
            Error::Incomplete(_) => f.write_str("the migration is incomplete"),
            Error::IncompleteListing => f.write_str(
                "Drive did not search everything while listing the source folders, some files \
                 may be missing; use --continue-on-error to accept an incomplete listing",
            ),
            Error::WriteOutput(_) => f.write_str("unable to write the output of the migration"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::SourceAccount { source, .. } | Error::DestinationAccount { source, .. } => {
                Some(source)
            }
            Error::GetDestinationFolder { source, .. }
            | Error::GetSource { source, .. }
            | Error::CreateFolder(source)
            | Error::Trash(source)
            | Error::ArchiveMark { source, .. }
            | Error::ListDestination(source)
            | Error::Recheck(source) => Some(source.as_ref()),
            Error::Replace(error) => error.source(),
            Error::ListSource(error) => Some(error.error.as_ref()),
            Error::Relay(error) => error::Error::source(error.as_ref()),
            Error::Archive(error) => error::Error::source(error.as_ref()),
            Error::ArchiveUnlisted { source, .. } | Error::Item { source, .. } => {
                Some(source.as_ref())
            }
            Error::Export { source, .. } => Some(source.as_ref()),
            Error::ArchiveName(source) => Some(source),
            Error::Incomplete(source) => Some(source),
            Error::GenerateId(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
            Error::SameAccount(_)
            | Error::IncompleteDestination
            | Error::IntoInsideSource(_)
            | Error::SameItem { .. }
            | Error::InsideSource { .. }
            | Error::NotAFolder(_)
            | Error::Cycle
            | Error::Conflict
            | Error::AskWithoutTerminal
            | Error::FolderMissingId
            | Error::IncompleteListing
            | Error::ArchiveIncomplete(_)
            | Error::NoSize(_) => None,
        }
    }
}
