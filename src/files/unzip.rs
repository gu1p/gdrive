//! `files unzip`: a zip file on Drive extracted into a folder on Drive, in the same account.
//!
//! Nothing goes through the disk: the list of entries is read from the end of the zip file, and
//! each entry goes up as it is inflated ([`crate::transfer::zip`]). The zip file is only read.
//!
//! A new file carries the CRC-32 of its entry in `appProperties`. That is what tells a later run
//! that the file is already there: Drive's md5 cannot be compared with anything in a zip. A file
//! of that name without it is another file, and the entry fails rather than touch it or make a
//! second one. A file that did not finish starts over, the ones before it stay. The mark is
//! trusted as it is: the file on Drive is not read, so one that was edited since to the same size
//! still counts as there. It stays as it is, a run never overwrites anything.

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashMap},
    error,
    fmt::{self, Display, Formatter, Write as _},
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use futures::stream;
use google_drive3::api::File;

use super::{
    generate_ids::generate_ids,
    list::ListQuery,
    migrate::list_page,
    mkdir,
    update::{update_metadata, PatchFile},
};
use crate::{
    common::{
        drive_file,
        hub_helper::{get_hub, get_hub_for, load_named_account, GetHubError},
        human_size::human_size,
        local_name::printable,
        media_client::{get_file, MediaError},
    },
    hub::Hub,
    transfer::{
        progress::{self, ConfirmedTotal, ProgressDisplay, ProgressEvent},
        relay::DriveSource,
        scheduler::{
            self, plural, Job, JobContext, JobKind, JobOutput, Outcome, Printer, Report, Scheduler,
            Subtree,
        },
        upload::{FileTimes, Upload, UploadError, UploadOptions, UploadTarget},
        zip::{self, Entry, EntrySource, ZipError},
    },
};

const ZIP_FIELDS: &str =
    "id,name,mimeType,size,md5Checksum,modifiedTime,version,resourceKey,parents";
const FOLDER_FIELDS: &str = "id,name,mimeType";
const LIST_FIELDS: &str =
    "nextPageToken,incompleteSearch,files(id,name,mimeType,size,appProperties)";
const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";
const LIST_PAGE_SIZE: i32 = 1000;
const ID_BATCH: i32 = 100;
/// The folder without `--into`, for a zip file in no folder of this account (shared with it).
const MY_DRIVE: &str = "root";
/// The `appProperties` key of the CRC-32 of the entry a file was extracted from.
const PROPERTY: &str = "gdriveZipCrc32";
/// Its value on a file that Drive holds with other bytes than were sent: no CRC-32 is like it.
const NOT_THE_CONTENT: &str = "not-the-content";

pub(crate) struct Config {
    pub(crate) file_id: String,
    /// The account that does all of it; `None`: the current account.
    pub(crate) from: Option<String>,
    /// The folder to extract into; `None`: the folder of the zip file.
    pub(crate) into: Option<String>,
    pub(crate) transfer: scheduler::Options,
    pub(crate) progress: progress::Mode,
}

pub(crate) async fn unzip(config: Config) -> Result<(), Error> {
    let hub = match &config.from {
        Some(name) => {
            let account = load_named_account(name).map_err(Error::Hub)?;
            get_hub_for(&account).await
        }
        None => get_hub().await,
    }
    .map_err(Error::Hub)?;
    let display = ProgressDisplay::recursive(config.progress, config.transfer.jobs);
    Box::pin(run(
        &hub,
        &config.file_id,
        config.into.as_deref(),
        config.transfer,
        &UploadOptions::default(),
        &Printer::stdio(),
        display,
        scheduler::ctrl_c(),
    ))
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "what a test replaces is each its own argument, as in the migration's `run`"
)]
async fn run(
    hub: &Hub,
    file_id: &str,
    into: Option<&str>,
    transfer: scheduler::Options,
    options: &UploadOptions,
    printer: &Printer,
    progress: ProgressDisplay,
    cancel: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    let zip_file = get_file(hub, file_id, ZIP_FIELDS, None)
        .await
        .map_err(|source| Error::GetZip {
            id: file_id.to_owned(),
            source,
        })?;
    let zip_name = zip_file.name.clone().unwrap_or_else(|| file_id.to_owned());
    let size = zip_file
        .size
        .and_then(|size| u64::try_from(size).ok())
        .filter(|_| drive_file::is_binary(&zip_file))
        .ok_or_else(|| Error::NotAFile(zip_name.clone()))?;

    let unzip = Arc::new(Unzip {
        hub: hub.clone(),
        id: file_id.to_owned(),
        zip_name: zip_name.clone(),
        zip_file,
        size,
        options: options.clone(),
        ids: tokio::sync::Mutex::new(Vec::new()),
        folders_created: AtomicU64::new(0),
        folders_reused: AtomicU64::new(0),
    });
    let mut reader = unzip.reader().map_err(Error::Open)?;
    let entries = zip::read_directory(&mut reader, size)
        .await
        .map_err(|source| Error::Zip {
            name: zip_name.clone(),
            source,
        })?;
    drop(reader);

    let tree = Dir::of(folder_name(&zip_name), entries);
    let subtree = tree.subtree();
    printer
        .out(format!(
            "Found {} in {} with a total size of {}",
            plural(subtree.files, "file", "files"),
            plural(subtree.folders + 1, "folder", "folders"),
            human_size(subtree.bytes),
        ))
        .await;

    let parent = match into {
        Some(id) => folder_id(hub, id).await?,
        None => unzip
            .zip_file
            .parents
            .iter()
            .flatten()
            .next()
            .cloned()
            .unwrap_or_else(|| MY_DRIVE.to_owned()),
    };
    let parent = Place {
        existing: list(hub, &parent).await?,
        folder: parent,
    };
    let place = unzip
        .enter(&tree.name, &tree.label, &parent, printer)
        .await?;
    let folder = place.folder.clone();

    progress.sink().event(&ProgressEvent::Planned {
        files: subtree.files,
        bytes: subtree.bytes,
    });
    let jobs = unzip.jobs_of(tree, &Arc::new(place));
    let report = Scheduler::new(transfer, printer.clone())
        .with_progress(progress.sink())
        .run(stream::iter(jobs), cancel)
        .await;
    progress.finish();

    printer
        .out(format!(
            "Folder: https://drive.google.com/drive/folders/{folder}"
        ))
        .await;
    printer.err(unzip.summary(&report)).await;
    printer.flush().await.map_err(Error::WriteOutput)?;
    report.check().map_err(Error::Incomplete)
}

/// The name of the zip file without `.zip`, in any case; all of it when nothing else is left.
fn folder_name(zip_name: &str) -> &str {
    let stem = zip_name
        .len()
        .checked_sub(4)
        .filter(|&at| zip_name.is_char_boundary(at))
        .filter(|&at| zip_name[at..].eq_ignore_ascii_case(".zip"))
        .map(|at| &zip_name[..at]);
    stem.filter(|stem| !stem.is_empty()).unwrap_or(zip_name)
}

/// The id of the folder `id` (`--into`), which must be a folder.
async fn folder_id(hub: &Hub, id: &str) -> Result<String, Error> {
    let file = get_file(hub, id, FOLDER_FIELDS, None)
        .await
        .map_err(|source| Error::GetFolder {
            id: id.to_owned(),
            source,
        })?;
    if !drive_file::is_directory(&file) {
        return Err(Error::NotAFolder(id.to_owned()));
    }
    Ok(file.id.unwrap_or_else(|| id.to_owned()))
}

/// What the folder `folder` holds now.
async fn list(hub: &Hub, folder: &str) -> Result<Listing, Error> {
    let q = ListQuery::files_in_folder(folder.to_owned()).q();
    let mut listing = Listing::default();
    let mut page_token = None;
    loop {
        let page = list_page(hub, &q, LIST_PAGE_SIZE, LIST_FIELDS, page_token.as_deref())
            .await
            .map_err(Error::List)?;
        if page.incomplete_search == Some(true) {
            return Err(Error::IncompleteListing);
        }
        for file in page.files.unwrap_or_default() {
            if let Some(name) = file.name.clone() {
                listing.by_name.entry(name).or_default().push(file);
            }
        }
        page_token = page.next_page_token;
        if page_token.is_none() {
            return Ok(listing);
        }
    }
}

/// What a folder holds, by name.
#[derive(Debug, Default)]
struct Listing {
    by_name: HashMap<String, Vec<File>>,
}

impl Listing {
    /// The first folder named `name`.
    fn folder(&self, name: &str) -> Option<&str> {
        self.by_name
            .get(name)?
            .iter()
            .find(|file| drive_file::is_directory(file))?
            .id
            .as_deref()
    }

    /// The other files named `name`.
    fn files<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a File> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|file| !drive_file::is_directory(file))
    }
}

/// A folder on Drive that things go into, with what it held when it was listed.
#[derive(Debug)]
struct Place {
    folder: String,
    existing: Listing,
}

/// The value of [`PROPERTY`] of an extracted file.
fn mark(crc32: u32) -> String {
    format!("{crc32:08x}")
}

/// Whether `file` is what `entry` was extracted to.
fn is_extracted_from(file: &File, entry: &Entry) -> bool {
    file.size.and_then(|size| u64::try_from(size).ok()) == Some(entry.size)
        && file
            .app_properties
            .as_ref()
            .and_then(|properties| properties.get(PROPERTY))
            .is_some_and(|value| *value == mark(entry.crc32))
}

/// A folder of the zip file, with everything below it.
#[derive(Debug, Default)]
struct Dir {
    name: String,
    label: String,
    dirs: BTreeMap<String, Dir>,
    /// By name: of several entries of one path the last counts, as in other extractors. A zip
    /// file that was added to has the newer one last.
    files: BTreeMap<String, Entry>,
}

impl Dir {
    /// The folder `name` that holds `entries`.
    fn of(name: &str, entries: Vec<Entry>) -> Dir {
        let mut top = Dir {
            name: name.to_owned(),
            label: printable(name).into_owned(),
            ..Dir::default()
        };
        for entry in entries {
            let folders = if entry.is_folder {
                entry.path.len()
            } else {
                entry.path.len() - 1
            };
            let mut dir = &mut top;
            for name in &entry.path[..folders] {
                let label = format!("{}/{}", dir.label, printable(name));
                dir = dir.dirs.entry(name.clone()).or_insert_with(|| Dir {
                    name: name.clone(),
                    label,
                    ..Dir::default()
                });
            }
            if !entry.is_folder {
                dir.files.insert(entry.name().to_owned(), entry);
            }
        }
        top
    }

    /// The files and folders below it, and the sizes of the files.
    fn subtree(&self) -> Subtree {
        let mut subtree = Subtree::default();
        let mut pending = vec![self];
        while let Some(dir) = pending.pop() {
            subtree.files += dir.files.len() as u64;
            subtree.bytes += dir.files.values().map(|entry| entry.size).sum::<u64>();
            subtree.folders += dir.dirs.len() as u64;
            pending.extend(dir.dirs.values());
        }
        subtree
    }
}

/// One run.
struct Unzip {
    hub: Hub,
    id: String,
    zip_name: String,
    zip_file: File,
    size: u64,
    options: UploadOptions,
    /// Ids from `files.generateIds` for the new files and folders.
    ids: tokio::sync::Mutex<Vec<String>>,
    folders_created: AtomicU64,
    folders_reused: AtomicU64,
}

impl Unzip {
    /// A reader of the zip file.
    fn reader(&self) -> Result<DriveSource<'_>, MediaError> {
        DriveSource::new(
            &self.hub,
            &self.zip_file,
            (&self.id, self.zip_file.resource_key.as_deref()),
            self.size,
            format!("download of '{}'", printable(&self.zip_name)),
        )
    }

    async fn new_id(&self) -> Result<String, Error> {
        let mut ids = self.ids.lock().await;
        if ids.is_empty() {
            *ids = generate_ids(&self.hub, ID_BATCH)
                .await
                .map_err(Error::GenerateId)?;
        }
        ids.pop().ok_or(Error::NoId)
    }

    /// The folder `name` in `parent`, the one there or a new one, with what it holds.
    async fn enter(
        &self,
        name: &str,
        label: &str,
        parent: &Place,
        printer: &Printer,
    ) -> Result<Place, Error> {
        if let Some(id) = parent.existing.folder(name) {
            printer.out(format!("reuse {label}/")).await;
            let existing = list(&self.hub, id).await?;
            self.folders_reused.fetch_add(1, Ordering::SeqCst);
            return Ok(Place {
                folder: id.to_owned(),
                existing,
            });
        }
        printer.out(format!("create {label}/")).await;
        let config = mkdir::Config {
            id: Some(self.new_id().await?),
            name: name.to_owned(),
            parents: Some(vec![parent.folder.clone()]),
            print_only_id: false,
        };
        let created = mkdir::create_directory(&self.hub, &config)
            .await
            .map_err(|error| Error::CreateFolder(Box::new(error)))?;
        self.folders_created.fetch_add(1, Ordering::SeqCst);
        Ok(Place {
            folder: created.id.ok_or(Error::NoId)?,
            existing: Listing::default(),
        })
    }

    /// The jobs of what `dir` holds, which goes to `place`.
    fn jobs_of(self: &Arc<Self>, dir: Dir, place: &Arc<Place>) -> Vec<Job> {
        let Dir {
            label, dirs, files, ..
        } = dir;
        let mut jobs = Vec::with_capacity(files.len() + dirs.len());
        for entry in files.into_values() {
            jobs.push(self.file_job(entry, &label, place));
        }
        for dir in dirs.into_values() {
            jobs.push(self.folder_job(dir, Arc::clone(place)));
        }
        jobs
    }

    fn folder_job(self: &Arc<Self>, dir: Dir, parent: Arc<Place>) -> Job {
        let subtree = dir.subtree();
        let unzip = Arc::clone(self);
        Job::new(
            JobKind::Metadata,
            dir.label.clone(),
            move |context| async move {
                let entered = unzip
                    .enter(&dir.name, &dir.label, &parent, &context.printer)
                    .await;
                match entered {
                    Ok(place) => JobOutput::from(Outcome::Transferred { bytes: 0 })
                        .follow_up(unzip.jobs_of(dir, &Arc::new(place))),
                    Err(error) => failed(&dir.label, error).into(),
                }
            },
        )
        .with_subtree(subtree)
    }

    fn file_job(self: &Arc<Self>, entry: Entry, folder: &str, place: &Arc<Place>) -> Job {
        let label = format!("{folder}/{}", printable(entry.name()));
        let size = entry.size;
        if let Some(error) = entry.unreadable() {
            let outcome = failed(&label, Error::Entry(error));
            return Job::done(JobKind::Media, label, outcome).with_size(size);
        }
        let unzip = Arc::clone(self);
        let place = Arc::clone(place);
        Job::new(JobKind::Media, label.clone(), move |context| async move {
            unzip.extract(&entry, &label, &place, &context).await
        })
        .with_size(size)
    }

    async fn extract(
        &self,
        entry: &Entry,
        label: &str,
        place: &Place,
        context: &JobContext,
    ) -> JobOutput {
        let mut there = place.existing.files(entry.name()).peekable();
        if there.peek().is_some() {
            if there.any(|file| is_extracted_from(file, entry)) {
                context.printer.out(format!("already-there {label}")).await;
                return Outcome::Unchanged.into();
            }
            return failed(label, Error::Conflict).into();
        }
        context
            .printer
            .out(format!("create {label} ({})", human_size(entry.size)))
            .await;
        match self.upload(entry, place, context).await {
            Ok(()) => Outcome::Transferred { bytes: entry.size }.into(),
            Err(error) => failed(label, error).into(),
        }
    }

    async fn upload(
        &self,
        entry: &Entry,
        place: &Place,
        context: &JobContext,
    ) -> Result<(), Error> {
        // `.gdoc` and the like are guessed as Google types, which only a conversion can create.
        let media_type = mime_guess::from_path(entry.name())
            .first()
            .filter(|mime| !mime.essence_str().starts_with(GOOGLE_APPS_PREFIX))
            .unwrap_or(mime::APPLICATION_OCTET_STREAM);
        let target = UploadTarget {
            parents: vec![place.folder.clone()],
            name: Some(entry.name().to_owned()),
            new_file_id: Some(self.new_id().await?),
            ..UploadTarget::default()
        };
        // The upload tells the bytes Drive holds so far; progress takes what is new.
        let confirmed = ConfirmedTotal::new(context.progress.clone());
        let report = |held| confirmed.held(held);
        let upload = Upload::new(
            &self.hub,
            self.hub.retrier().state(),
            &target,
            &self.options,
            Some(&report),
            entry.size,
            media_type,
        )
        .with_times(FileTimes {
            modified: entry.modified,
            created: None,
        })
        .with_property(PROPERTY, mark(entry.crc32));
        // The most an upload goes back: one request's bytes.
        let window = self
            .options
            .chunk_len()
            .max(self.options.multipart_threshold);
        let reader = self.reader().map_err(Error::Open)?;
        let mut source = EntrySource::new(reader, entry, window);
        // Boxed: the upload's state is large, and many of these futures wait at once.
        let result = Box::pin(upload.run(&mut source)).await;
        if let Err(UploadError::Md5Mismatch { file_id, .. }) = &result {
            self.take_back_mark(file_id, context).await;
        }
        result.map_err(|error| Error::Upload(Box::new(error)))?;
        Ok(())
    }

    /// The file `id` was created with its mark and did not pass Drive's md5: a later run must
    /// not take it for extracted.
    async fn take_back_mark(&self, id: &str, context: &JobContext) {
        let patch = PatchFile::new(id.to_owned()).with_app_property(PROPERTY, NOT_THE_CONTENT);
        if update_metadata(&self.hub, patch).await.is_err() {
            context
                .printer
                .err(format!(
                    "Warning: the file {id} on Drive has damaged content, and a later run will \
                     take it for extracted; remove it"
                ))
                .await;
        }
    }

    /// The last line on stderr, for example `Summary: 12 extracted (3.4 MB), 2 already there,
    /// 0 failed, 0 cancelled; 3 folders created, 1 reused`.
    fn summary(&self, report: &Report) -> String {
        let files = &report.files;
        let mut line = format!(
            "Summary: {} extracted ({}), {} already there, {} failed, {} cancelled; {} created, \
             {} reused",
            files.transferred,
            human_size(files.bytes),
            files.unchanged,
            files.failed,
            files.cancelled,
            plural(
                self.folders_created.load(Ordering::SeqCst),
                "folder",
                "folders"
            ),
            self.folders_reused.load(Ordering::SeqCst),
        );
        if report.folders.failed > 0 {
            let folders = plural(report.folders.failed, "folder", "folders");
            let _ = write!(line, "; {folders} failed");
        }
        if report.interrupted {
            line.push_str("; interrupted");
        }
        line
    }
}

fn failed(label: &str, source: Error) -> Outcome {
    Outcome::failed(Error::Item {
        label: label.to_owned(),
        source: Box::new(source),
    })
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    GetZip {
        id: String,
        source: Box<google_drive3::Error>,
    },
    NotAFile(String),
    Open(MediaError),
    Zip {
        name: String,
        source: ZipError,
    },
    Entry(ZipError),
    GetFolder {
        id: String,
        source: Box<google_drive3::Error>,
    },
    NotAFolder(String),
    List(Box<google_drive3::Error>),
    IncompleteListing,
    GenerateId(Box<google_drive3::Error>),
    NoId,
    CreateFolder(Box<google_drive3::Error>),
    Conflict,
    Upload(Box<UploadError>),
    Item {
        label: String,
        source: Box<Error>,
    },
    Incomplete(scheduler::Incomplete),
    WriteOutput(scheduler::OutputError),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::GetZip { id, .. } => write!(f, "unable to get the file '{id}'"),
            Error::NotAFile(name) => write!(
                f,
                "'{}' has no content to read as a zip: it is a folder or a Google Workspace file",
                printable(name)
            ),
            Error::Open(_) => f.write_str("unable to read the zip file"),
            Error::Zip { name, .. } => write!(f, "unable to read '{}'", printable(name)),
            Error::Entry(source) => Display::fmt(source, f),
            Error::GetFolder { id, .. } => write!(f, "unable to get the folder '{id}'"),
            Error::NotAFolder(id) => {
                write!(f, "'{id}' is not a folder; --into takes a folder id")
            }
            Error::List(_) => f.write_str("unable to list the folder"),
            Error::IncompleteListing => f.write_str(
                "Drive did not list the folder completely, a file might be made twice; run the \
                 command again later",
            ),
            Error::GenerateId(_) => f.write_str("unable to get an id for a new file"),
            Error::NoId => f.write_str("Drive gave no id for a new file"),
            Error::CreateFolder(_) => f.write_str("unable to create the folder"),
            Error::Conflict => f.write_str(
                "the folder holds another file of this name, which was left alone; rename or \
                 remove it",
            ),
            Error::Upload(_) => f.write_str("unable to upload the file"),
            Error::Item { label, .. } => f.write_str(label),
            Error::Incomplete(_) => f.write_str("the zip file was not extracted completely"),
            Error::WriteOutput(_) => f.write_str("unable to write the output"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::GetZip { source, .. }
            | Error::GetFolder { source, .. }
            | Error::List(source)
            | Error::GenerateId(source)
            | Error::CreateFolder(source) => Some(source),
            Error::Open(source) => Some(source),
            Error::Zip { source, .. } => Some(source),
            Error::Entry(source) => source.source(),
            Error::Upload(source) => Some(source),
            Error::Item { source, .. } => Some(source),
            Error::Incomplete(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
            Error::NotAFile(_)
            | Error::NotAFolder(_)
            | Error::IncompleteListing
            | Error::NoId
            | Error::Conflict => None,
        }
    }
}
