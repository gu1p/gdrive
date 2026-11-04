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
//! Workspace types without an export are skipped and listed at the end.
//!
//! `--dry-run` runs the same jobs without a write: folders that would be created are not
//! listed (they would be empty), and only the destination folders that exist are read.
//!
//! `--trash-source` moves a source file to the trash once it is in the destination (relayed and
//! verified, or already there with the same md5), and a source folder once everything in it is:
//! each folder counts its children that went to the trash, and the last one releases the job
//! that trashes the folder. A folder with anything that was not migrated (a failure, an
//! unsupported item, an incomplete listing) stays, and so do its ancestors.

mod source;
#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
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
use google_drive3::api::File;

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
        retry::retry_read,
    },
    files::{
        self,
        generate_ids::generate_ids,
        list::{list_files, ListFilesConfig, ListQuery, ListSortOrder},
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
struct Settings {
    file_ids: Vec<String>,
    into: Option<String>,
    dry_run: bool,
    trash_source: bool,
    transfer: scheduler::Options,
}

pub(crate) async fn migrate(config: Config) -> Result<(), Error> {
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
    if source_account.account.name == dest_account.account.name {
        return Err(Error::SameAccount(dest_account.account.name));
    }

    let source = get_hub_for(&source_account)
        .await
        .map_err(|error| Error::SourceAccount {
            account: Some(source_account.account.name.clone()),
            source: error,
        })?;
    let dest = get_hub_for(&dest_account)
        .await
        .map_err(|error| Error::DestinationAccount {
            account: dest_account.account.name.clone(),
            source: error,
        })?;

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
    let settings = Settings {
        file_ids: config.file_ids,
        into: config.into,
        dry_run: config.dry_run,
        trash_source: config.trash_source,
        transfer: config.transfer,
    };
    let printer = Printer::stdio();
    let display = ProgressDisplay::recursive(config.progress, config.transfer.jobs);
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
        None => MY_DRIVE.to_owned(),
    };
    let existing = list_existing(dest, &into)
        .await
        .map_err(|error| Error::ListDestination(Box::new(error)))?;

    let listing_jobs = settings.transfer.metadata_jobs.get();
    let mut roots = Vec::new();
    let mut totals = Totals::default();
    for id in &settings.file_ids {
        let root = source::discover(source, id, listing_jobs, printer).await;
        match &root {
            Ok(node) => totals.add(node),
            Err(_) => totals.files += 1,
        }
        roots.push((id, root));
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
        unsupported: Mutex::new(Vec::new()),
        tally: Tally::default(),
        ids: tokio::sync::Mutex::new(Vec::new()),
    });
    let place = Place {
        folder: Some(into),
        existing: Arc::new(existing),
        trash: None,
    };
    let jobs = roots
        .into_iter()
        .map(|(id, root)| match root {
            Ok(SourceNode::Folder(folder)) => migration.folder_job(folder, place.clone()),
            Ok(SourceNode::File(item)) => migration.file_job(item, place.clone()),
            Err(error) => Job::done(JobKind::Media, id.clone(), failed(id, error)),
        })
        .collect::<Vec<_>>();

    progress.sink().event(&ProgressEvent::Planned {
        files: totals.files,
        bytes: totals.bytes,
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
        files.unchanged,
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
    if settings.trash_source {
        let _ = write!(line, "; {} {trashed}", tally.trashed.load(Ordering::SeqCst));
    }
    if report.interrupted {
        line.push_str("; interrupted");
    }
    line
}

/// The destination's files in one folder, by name.
#[derive(Debug, Default)]
struct Existing {
    by_name: HashMap<String, Vec<DestFile>>,
}

#[derive(Debug)]
struct DestFile {
    id: String,
    mime_type: String,
    size: Option<i64>,
    md5: Option<String>,
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
}

impl Existing {
    fn of(files: Vec<File>) -> Self {
        let mut by_name = HashMap::<String, Vec<DestFile>>::new();
        for file in files {
            let (Some(id), Some(name)) = (file.id, file.name) else {
                continue;
            };
            by_name.entry(name).or_default().push(DestFile {
                id,
                mime_type: file.mime_type.unwrap_or_default(),
                size: file.size,
                md5: file.md5_checksum,
            });
        }
        Existing { by_name }
    }

    /// The first folder named `name`.
    fn folder(&self, name: &str) -> Option<&str> {
        self.by_name
            .get(name)?
            .iter()
            .find(|file| file.mime_type == drive_file::MIME_TYPE_DRIVE_FOLDER)
            .map(|file| file.id.as_str())
    }

    fn file(&self, source: &SourceFile) -> Match {
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
        // A Workspace file's export is converted again: the copy has another size than the
        // original, and neither has an md5. Its type is what can be compared.
        let native = source.is_native();
        let same = candidates.any(|file| {
            if native {
                file.mime_type == source.mime_type
            } else {
                file.size == source.size && file.md5 == source.md5
            }
        });
        if same {
            Match::Same {
                verified: !native && source.md5.is_some(),
            }
        } else {
            Match::Different
        }
    }
}

/// Lists the destination folder `folder_id`: the files that are not in the trash.
async fn list_existing(dest: &Hub, folder_id: &str) -> Result<Existing, files::list::Error> {
    let query = ListQuery::files_in_folder(folder_id.to_owned());
    let files = list_files(
        dest,
        ListFilesConfig {
            query: &query,
            order_by: &ListSortOrder::default(),
            max_files: usize::MAX,
        },
    )
    .await?;
    Ok(Existing::of(files))
}

/// What the jobs of a migration share.
struct Migration {
    source: Hub,
    dest: Hub,
    options: UploadOptions,
    dry_run: bool,
    trash_source: bool,
    /// `PATH: REASON` of every unsupported item, for the end of the output.
    unsupported: Mutex<Vec<String>>,
    tally: Tally,
    /// Ids from `files.generateIds` of the destination, not used yet.
    ids: tokio::sync::Mutex<Vec<String>>,
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
            mut children,
            incomplete,
            error: _,
        } = folder;

        let (folder_id, existing) = if let Some(dest_id) = place.existing.folder(&name) {
            context.printer.out(format!("reuse {label}/")).await;
            match list_existing(&self.dest, dest_id).await {
                Ok(existing) => {
                    self.tally.folders_reused.fetch_add(1, Ordering::SeqCst);
                    (Some(dest_id.to_owned()), existing)
                }
                Err(error) => {
                    return failed(&label, Error::ListDestination(Box::new(error))).into();
                }
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

        children.sort_by(|a, b| a.name().cmp(b.name()));
        let mut folders = Vec::new();
        let mut files = Vec::new();
        for child in children {
            match child {
                SourceNode::Folder(folder) => folders.push(folder),
                SourceNode::File(item) => files.push(item),
            }
        }

        let mut output = JobOutput::from(Outcome::Transferred { bytes: 0 });
        if let Some(trash) = trash.filter(|trash| trash.remaining.load(Ordering::SeqCst) == 0) {
            output = output.follow_up([self.trash_folder_job(trash)]);
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

        let (outcome, verified) = match place.existing.file(&file) {
            Match::Different => return failed(&label, Error::Conflict).into(),
            Match::Same { verified } => {
                context.printer.out(format!("already-there {label}")).await;
                (Outcome::Unchanged, verified)
            }
            Match::None => {
                let line = if file.is_native() {
                    format!("convert {label}")
                } else {
                    format!("create {label} ({})", human_size(file.bytes()))
                };
                context.printer.out(line).await;
                let bytes = if self.dry_run {
                    file.bytes()
                } else {
                    match self.relay(&file, &drive_file, &place, context).await {
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
                (Outcome::Transferred { bytes }, true)
            }
        };

        let mut output = JobOutput::from(outcome);
        if self.trash_source && verified {
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

    /// Relays `file` into the folder of `place`; the size of the new file.
    async fn relay(
        &self,
        file: &SourceFile,
        drive_file: &File,
        place: &Place,
        context: &JobContext,
    ) -> Result<u64, Error> {
        // A conversion takes no id.
        let new_file_id = if file.is_native() {
            None
        } else {
            Some(self.new_id().await?)
        };
        let target = UploadTarget {
            parents: place.folder.iter().cloned().collect(),
            name: Some(file.name.clone()),
            new_file_id,
            ..UploadTarget::default()
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
        Ok(created
            .size
            .and_then(|size| u64::try_from(size).ok())
            .unwrap_or_else(|| confirmed.total()))
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
        context.printer.out(format!("skip {label}: {reason}")).await;
        self.unsupported
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!("{label}: {reason}"));
        let reason = match reason {
            Unsupported::Shortcut => SkipReason::Shortcut,
            Unsupported::NativeType(_) => SkipReason::NotExportable,
        };
        Outcome::Skipped { reason }.into()
    }

    /// Moves the source file or folder `id` to the trash (not in a dry run) and prints `line`.
    async fn trash(&self, id: &str, line: String, context: &JobContext) -> Result<(), Error> {
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
    ListDestination(Box<files::list::Error>),
    GetSource {
        id: String,
        source: Box<google_drive3::Error>,
    },
    ListSource(crate::transfer::walk::FolderError),
    /// A source folder that is its own ancestor.
    Cycle,
    /// The destination has a file of the same name with other content.
    Conflict,
    CreateFolder(Box<google_drive3::Error>),
    FolderMissingId,
    /// No id for a new file or folder of the destination.
    GenerateId(NextError),
    Relay(Box<RelayError>),
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
                 another account, see 'gdrive account list'"
            ),
            Error::GetDestinationFolder { id, .. } => {
                write!(f, "unable to get the destination folder '{id}'")
            }
            Error::NotAFolder(id) => write!(
                f,
                "'{id}' is not a folder of the destination account; --into takes a folder id"
            ),
            Error::ListDestination(_) => f.write_str("unable to list the destination folder"),
            Error::GetSource { id, .. } => {
                write!(f, "unable to get '{id}' from the source account")
            }
            Error::ListSource(error) => error.fmt(f),
            Error::Cycle => f.write_str("the folder is inside itself, it is not migrated"),
            Error::Conflict => f.write_str("it exists in the destination with different content"),
            Error::CreateFolder(_) => f.write_str("unable to create the folder in the destination"),
            Error::FolderMissingId => {
                f.write_str("the folder created in the destination has no id")
            }
            Error::GenerateId(_) => f.write_str("unable to get an id for the destination"),
            Error::Relay(error) => error.fmt(f),
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
            | Error::Trash(source) => Some(source.as_ref()),
            Error::ListDestination(source) => Some(source.as_ref()),
            // The message is the source's own.
            Error::ListSource(error) => error::Error::source(error),
            Error::Relay(error) => error::Error::source(error.as_ref()),
            Error::Item { source, .. } => Some(source.as_ref()),
            Error::Incomplete(source) => Some(source),
            Error::GenerateId(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
            Error::SameAccount(_)
            | Error::NotAFolder(_)
            | Error::Cycle
            | Error::Conflict
            | Error::FolderMissingId
            | Error::IncompleteListing => None,
        }
    }
}
