//! `gdrive files migrate --archive`: each `FILE_ID` arrives in the destination as one tar file,
//! `<Drive name>.tar`, made while it is uploaded ([`crate::transfer::archive`]).
//!
//! ```text
//!   the walk of the source, as for a migration (source.rs)
//!     -> plan, per FILE_ID, from the listing alone: the entries in their order, the items
//!        left out (shortcuts, Workspace types without export), the manifest digest
//!     -> one job per FILE_ID on the Scheduler:
//!        the destination's files named <name>.tar: the same manifest is already-there,
//!        another file of that name a conflict
//!        -> exports (or the exports cache of an interrupted run), the layout, the upload
//! ```
//!
//! Names: every Drive name becomes one path component through `local_name` with the rules of
//! every platform at once ([`local_name::unique_portable_names`]), so the archive extracts on
//! Windows, macOS and Linux and its bytes do not depend on where gdrive runs; siblings whose
//! names collide get the suffixes of a recursive download. Entries come in the order of their
//! names, a folder's entry before what is in it: not in the order Drive lists them.
//!
//! A root with a folder that could not be listed, or was listed incompletely, fails before
//! anything is uploaded: an archive that silently misses files is worse than none.

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashSet},
    fmt::{Display, Write as _},
    sync::{atomic::Ordering, Arc, PoisonError},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};

use super::{
    failed,
    source::{label, SourceFolder, SourceItem, SourceNode},
    Error, Existing, Match, Migration, Place, Tally,
};
use crate::{
    common::{
        drive_file::{self, DocType, FileExtension},
        human_size::human_size,
        local_name::{self, NameEntry, UnusableName},
    },
    files::export::{export_file_name, ExportFormats},
    transfer::{
        archive::{
            self, ArchiveError, ArchiveSource, Blob, CachedExport, Content, Entry, ExportsCache,
            Layout, Manifest,
        },
        progress::ConfirmedTotal,
        relay::{self, RelayError, Unsupported, EXPORT_LIMIT},
        scheduler::{plural, Job, JobContext, JobKind, JobOutput, Outcome, Report, SkipReason},
        upload::{Upload, UploadTarget},
    },
};

/// The `appProperties` key of an archive, whose value is its manifest digest.
pub(super) const PROPERTY: &str = "gdriveArchive";

/// What the destination folder's listing returns of each file in an archive run: the
/// archive's property too.
pub(super) const DEST_FIELDS: &str =
    "nextPageToken,incompleteSearch,files(id,name,mimeType,size,md5Checksum,appProperties)";

/// What the archive of one `FILE_ID` is made of, from the listing alone.
pub(super) struct Plan {
    root_id: String,
    /// `<Drive name>.tar`.
    name: String,
    /// The name as output shows it.
    label: String,
    entries: Vec<Planned>,
    /// The items left out, known from their metadata: their labels and why.
    skipped: Vec<(String, Unsupported)>,
    manifest: String,
}

/// An entry of an archive before the exports are made.
struct Planned {
    /// The path in the archive.
    path: String,
    /// The Drive path, as output shows it.
    label: String,
    mtime: u64,
    kind: Kind,
}

enum Kind {
    Folder,
    Blob(Blob),
    Export(Export),
}

/// A Workspace file of an archive, exported in `format`.
struct Export {
    id: String,
    resource_key: Option<String>,
    modified_time: Option<DateTime<Utc>>,
    format: FileExtension,
}

/// What a source file becomes in an archive.
enum Role {
    Blob,
    Export(FileExtension),
    Skipped(Unsupported),
}

impl Role {
    /// Workspace files go in as the exports of the `default` profile of `files download
    /// --recursive --export` (docx, xlsx, pptx, png).
    fn of(mime_type: &str, is_native: bool) -> Role {
        if mime_type == drive_file::MIME_TYPE_DRIVE_SHORTCUT {
            return Role::Skipped(Unsupported::Shortcut);
        }
        let format = DocType::from_mime_type(mime_type)
            .and_then(|doc_type| ExportFormats::DEFAULT.format(doc_type));
        match format {
            Some(format) => Role::Export(format),
            None if is_native => Role::Skipped(Unsupported::NativeType(mime_type.to_owned())),
            None => Role::Blob,
        }
    }
}

/// Seconds since the Unix epoch of a time of Drive; 0 without one.
fn mtime(time: Option<DateTime<Utc>>) -> u64 {
    time.and_then(|time| u64::try_from(time.timestamp()).ok())
        .unwrap_or_default()
}

/// The child of a folder in an archive, before its name is decided.
enum Child {
    Folder(SourceFolder),
    File(SourceItem, Option<FileExtension>),
}

impl Child {
    fn id(&self) -> &str {
        match self {
            Child::Folder(folder) => &folder.id,
            Child::File(item, _) => &item.file.id,
        }
    }
}

impl Plan {
    /// The archive of `root`, or why there can be none.
    pub(super) fn new(root: SourceNode) -> Result<Plan, Error> {
        let name = format!("{}.tar", root.name());
        let mut plan = Plan {
            root_id: String::new(),
            label: label(None, &name),
            name,
            entries: Vec::new(),
            skipped: Vec::new(),
            manifest: String::new(),
        };
        match root {
            SourceNode::File(item) => {
                plan.root_id.clone_from(&item.file.id);
                let format = match Role::of(&item.file.mime_type, item.file.is_native()) {
                    Role::Skipped(reason) => {
                        plan.skipped.push((item.label, reason));
                        plan.manifest = plan.manifest();
                        return Ok(plan);
                    }
                    Role::Blob => None,
                    Role::Export(format) => Some(format),
                };
                let name = portable(&item, format)?;
                plan.file(item, format, name)?;
            }
            SourceNode::Folder(folder) => {
                plan.root_id.clone_from(&folder.id);
                let path = local_name::portable_name(&folder.name, &folder.id)
                    .map_err(|source| unusable(&folder.name, &folder.id, source))?;
                plan.folder(folder, &path)?;
            }
        }
        plan.manifest = plan.manifest();
        Ok(plan)
    }

    /// Adds `folder` at `path`, and everything in it.
    fn folder(&mut self, folder: SourceFolder, path: &str) -> Result<(), Error> {
        if let Some(error) = folder.error {
            return Err(Error::ArchiveUnlisted {
                folder: folder.label,
                source: Box::new(error),
            });
        }
        if folder.incomplete {
            return Err(Error::ArchiveIncomplete(folder.label));
        }
        self.entries.push(Planned {
            path: path.to_owned(),
            label: folder.label,
            mtime: mtime(folder.modified_time),
            kind: Kind::Folder,
        });

        let mut children = Vec::new();
        let mut drive_names = Vec::new();
        for child in folder.children {
            match child {
                SourceNode::Folder(folder) => {
                    drive_names.push(folder.name.clone());
                    children.push(Child::Folder(folder));
                }
                SourceNode::File(item) => {
                    match Role::of(&item.file.mime_type, item.file.is_native()) {
                        Role::Skipped(reason) => self.skipped.push((item.label, reason)),
                        Role::Blob => {
                            drive_names.push(item.file.name.clone());
                            children.push(Child::File(item, None));
                        }
                        Role::Export(format) => {
                            // The Drive name is checked before the extension is added, as a
                            // recursive download does: a document named `..` fails.
                            local_name::portable_name(&item.file.name, &item.file.id).map_err(
                                |source| unusable(&item.file.name, &item.file.id, source),
                            )?;
                            drive_names.push(export_file_name(&item.file.name, format));
                            children.push(Child::File(item, Some(format)));
                        }
                    }
                }
            }
        }
        let entries = drive_names
            .iter()
            .zip(&children)
            .map(|(name, child)| NameEntry {
                id: child.id(),
                name,
            })
            .collect::<Vec<_>>();
        let unique = local_name::unique_portable_names(&entries).map_err(Error::ArchiveName)?;
        let mut sorted = unique.names.into_iter().zip(children).collect::<Vec<_>>();
        sorted.sort_by(|(one, _), (other, _)| one.cmp(other));
        for (name, child) in sorted {
            let child_path = format!("{path}/{name}");
            match child {
                Child::Folder(folder) => self.folder(folder, &child_path)?,
                Child::File(item, format) => self.file(item, format, child_path)?,
            }
        }
        Ok(())
    }

    /// Adds the file of `item` at `path`: its content, or its export as `format`.
    fn file(
        &mut self,
        item: SourceItem,
        format: Option<FileExtension>,
        path: String,
    ) -> Result<(), Error> {
        let SourceItem { file, label } = item;
        let kind = if let Some(format) = format {
            Kind::Export(Export {
                id: file.id,
                resource_key: file.resource_key,
                modified_time: file.modified_time,
                format,
            })
        } else {
            let size = file
                .size
                .and_then(|size| u64::try_from(size).ok())
                .ok_or_else(|| Error::NoSize(label.clone()))?;
            Kind::Blob(Blob {
                id: file.id,
                size,
                md5: file.md5,
                resource_key: file.resource_key,
                modified_time: file.modified_time,
            })
        };
        self.entries.push(Planned {
            path,
            label,
            mtime: mtime(file.modified_time),
            kind,
        });
        Ok(())
    }

    fn manifest(&self) -> String {
        let mut manifest = Manifest::new();
        for entry in &self.entries {
            match &entry.kind {
                Kind::Folder => manifest.folder(&entry.path),
                Kind::Blob(blob) => manifest.blob(&entry.path, blob),
                Kind::Export(export) => manifest.export(
                    &entry.path,
                    &export.id,
                    export.modified_time.as_ref(),
                    &export.format.to_string(),
                ),
            }
        }
        manifest.finish()
    }

    /// The Workspace files to export.
    fn exports(&self) -> u64 {
        let exports = self
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, Kind::Export(_)))
            .count();
        u64::try_from(exports).unwrap_or(u64::MAX)
    }

    /// The size of the archive with every export empty: what the listing tells.
    fn planned_size(&self) -> u64 {
        let entries = self.entries.iter().map(|entry| entry.entry(None)).collect();
        Layout::new(entries).size()
    }
}

impl Planned {
    /// The entry, with `export` as the content of an export (none: empty).
    fn entry(&self, export: Option<Bytes>) -> Entry {
        Entry {
            path: self.path.clone(),
            mtime: self.mtime,
            content: match &self.kind {
                Kind::Folder => Content::Folder,
                Kind::Blob(blob) => Content::Blob(blob.clone()),
                Kind::Export(_) => Content::Bytes(export.unwrap_or_default()),
            },
        }
    }
}

/// The name of a `FILE_ID` that is a file: its Drive name (with the extension of its export),
/// checked like any other.
fn portable(item: &SourceItem, format: Option<FileExtension>) -> Result<String, Error> {
    let file = &item.file;
    local_name::portable_name(&file.name, &file.id)
        .and_then(|name| match format {
            Some(format) => {
                local_name::portable_name(&export_file_name(&file.name, format), &file.id)
            }
            None => Ok(name),
        })
        .map_err(|source| unusable(&file.name, &file.id, source))
}

fn unusable(name: &str, id: &str, source: local_name::InvalidName) -> Error {
    Error::ArchiveName(UnusableName::new(name, id, source))
}

impl Existing {
    /// How the archive `name` with the manifest digest `manifest` compares with the
    /// destination's files of that name; one of them that is a source item itself (in
    /// `source_ids`) is [`Match::Source`].
    fn archive(&self, name: &str, manifest: &str, source_ids: &HashSet<String>) -> Match {
        let mut candidates = self
            .by_name
            .get(name)
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
        if candidates.any(|file| file.archive.as_deref() == Some(manifest)) {
            Match::Same { verified: true }
        } else {
            Match::Different
        }
    }
}

impl Migration {
    /// The job that makes the archive of `root` in the folder of `place`, and the size of the
    /// archive as the listing tells it (without the exports).
    pub(super) fn archive_job(self: &Arc<Self>, root: SourceNode, place: Place) -> (Job, u64) {
        let label = label(None, &format!("{}.tar", root.name()));
        match Plan::new(root) {
            Err(error) => (
                Job::done(JobKind::Media, label.clone(), failed(&label, error)),
                0,
            ),
            Ok(plan) => {
                let size = plan.planned_size();
                let migration = Arc::clone(self);
                let job = Job::new(JobKind::Media, label, move |context| async move {
                    migration.archive(plan, size, place, &context).await
                })
                .with_size(size);
                (job, size)
            }
        }
    }

    async fn archive(
        self: Arc<Self>,
        plan: Plan,
        planned: u64,
        place: Place,
        context: &JobContext,
    ) -> JobOutput {
        // A FILE_ID that is a shortcut, or of a type without an export.
        if plan.entries.is_empty() {
            if let Some((label, reason)) = plan.skipped.first() {
                return self.unsupported(label, reason, context).await;
            }
        }
        let label = &plan.label;
        match place
            .existing
            .archive(&plan.name, &plan.manifest, &self.source_ids)
        {
            Match::None => {}
            Match::Same { .. } => {
                context.printer.out(format!("already-there {label}")).await;
                return Outcome::Unchanged.into();
            }
            Match::Different => return failed(label, Error::Conflict).into(),
            Match::Source => return failed(label, Error::InsideSource { folder: false }).into(),
        }
        for (label, reason) in &plan.skipped {
            self.left_out(label, reason, context).await;
        }

        if self.dry_run {
            let exports = plan.exports();
            let mut line = format!("create {label} ({}", human_size(planned));
            if exports > 0 {
                let _ = write!(
                    line,
                    " without the exports of {}",
                    plural(exports, "Google Workspace file", "Google Workspace files")
                );
            }
            line.push(')');
            context.printer.out(line).await;
            self.tally.converted.fetch_add(exports, Ordering::SeqCst);
            return Outcome::Transferred { bytes: planned }.into();
        }

        match self.make_archive(&plan, &place, context).await {
            Ok(Some(bytes)) => Outcome::Transferred { bytes }.into(),
            // A FILE_ID that is a Workspace file whose export is too large.
            Ok(None) => Outcome::Skipped {
                reason: SkipReason::NotExportable,
            }
            .into(),
            Err(error) => failed(label, error).into(),
        }
    }

    /// Prints that the item `label` is not in its archive, and why, and counts it.
    async fn left_out(&self, label: &str, reason: &(dyn Display + Sync), context: &JobContext) {
        self.note_skipped(label, reason, context).await;
        self.tally.skipped.fetch_add(1, Ordering::SeqCst);
    }

    /// The exports of `plan` by file id, from `cache` when it has them; whether any was made
    /// in this run.
    async fn exports(
        &self,
        plan: &Plan,
        cache: Option<&ExportsCache>,
    ) -> Result<(BTreeMap<String, CachedExport>, bool), Error> {
        let cached = match cache {
            Some(cache) => cache.load().await.unwrap_or_default(),
            None => BTreeMap::new(),
        };
        let mut exports = BTreeMap::new();
        let mut fresh = false;
        for entry in &plan.entries {
            let Kind::Export(export) = &entry.kind else {
                continue;
            };
            let made = if let Some(made) = cached.get(&export.id) {
                made.clone()
            } else {
                fresh = true;
                let reading = format!("export of '{}'", entry.label);
                let format = export.format.get_export_mime();
                let resource_key = export.resource_key.as_deref();
                match relay::read_export(&self.source, &export.id, resource_key, format, &reading)
                    .await
                {
                    Ok(bytes) => CachedExport::Content(bytes),
                    Err(RelayError::ExportTooLarge { .. }) => CachedExport::TooLarge,
                    Err(error) => {
                        return Err(Error::Export {
                            label: entry.label.clone(),
                            source: Box::new(error),
                        })
                    }
                }
            };
            exports.insert(export.id.clone(), made);
        }
        Ok((exports, fresh))
    }

    /// The entries of `plan` with its `exports`, how many of them are exports, and how many
    /// exports over Drive's limit were left out (printed, not counted yet).
    async fn entries(
        &self,
        plan: &Plan,
        exports: &BTreeMap<String, CachedExport>,
        context: &JobContext,
    ) -> (Vec<Entry>, u64, u64) {
        let mut entries = Vec::new();
        let mut exported = 0;
        let mut left_out = 0;
        for planned in &plan.entries {
            let export = match &planned.kind {
                Kind::Export(export) => {
                    if let Some(CachedExport::Content(bytes)) = exports.get(&export.id) {
                        exported += 1;
                        Some(bytes.clone())
                    } else {
                        let too_large = RelayError::ExportTooLarge {
                            limit: EXPORT_LIMIT,
                        };
                        self.note_skipped(&planned.label, &too_large, context).await;
                        left_out += 1;
                        continue;
                    }
                }
                Kind::Folder | Kind::Blob(_) => None,
            };
            entries.push(planned.entry(export));
        }
        (entries, exported, left_out)
    }

    /// Exports, lays out and uploads the archive of `plan`; its size, or `None` when nothing is
    /// left to archive.
    async fn make_archive(
        &self,
        plan: &Plan,
        place: &Place,
        context: &JobContext,
    ) -> Result<Option<u64>, Error> {
        let cache = self
            .options
            .session_dir
            .as_deref()
            .filter(|_| plan.exports() > 0)
            .map(|dir| ExportsCache::new(dir, &plan.manifest));
        let (exports, fresh) = self.exports(plan, cache.as_ref()).await?;
        let (entries, exported, left_out) = self.entries(plan, &exports, context).await;
        if entries.is_empty() {
            // A FILE_ID that is a Workspace file whose export is too large: the job's outcome
            // counts it.
            return Ok(None);
        }
        self.tally.skipped.fetch_add(left_out, Ordering::SeqCst);

        let layout = Layout::new(entries);
        let size = layout.size();
        let label = &plan.label;
        context
            .printer
            .out(format!("create {label} ({})", human_size(size)))
            .await;
        let media_type = archive::MEDIA_TYPE
            .parse::<mime::Mime>()
            .unwrap_or(mime::APPLICATION_OCTET_STREAM);
        let target = UploadTarget {
            parents: place.folder.iter().cloned().collect(),
            name: Some(plan.name.clone()),
            mime: Some(media_type.clone()),
            app_properties: BTreeMap::from([(PROPERTY.to_owned(), plan.manifest.clone())]),
            ..UploadTarget::default()
        };
        // The uploader tells the bytes Drive holds so far; progress takes what is new.
        let confirmed = ConfirmedTotal::new(context.progress.clone());
        let report = |held| confirmed.held(held);
        let mut upload = Upload::new(
            &self.dest,
            self.dest.retrier().state(),
            &target,
            &self.options,
            Some(&report),
            size,
            media_type,
        )
        .trust_continued_content();
        // Only resumable uploads have sessions to record.
        if let Some(dir) = &self.options.session_dir {
            if size > self.options.multipart_threshold {
                let identity =
                    archive::session_identity(&self.source, &plan.root_id, &layout, label).await;
                if let Some(identity) = identity {
                    if let (Some(cache), true) = (&cache, fresh) {
                        cache.save(&exports).await;
                    }
                    upload.record_in(dir, identity);
                }
            }
        }
        let mut source = ArchiveSource::new(&self.source, &layout);
        // Boxed, as the relay's: the upload's state is large.
        Box::pin(upload.run(&mut source))
            .await
            .map_err(|error| Error::Archive(Box::new(ArchiveError::of_upload(error))))?;
        if let Some(cache) = &cache {
            cache.remove().await;
        }
        self.tally.converted.fetch_add(exported, Ordering::SeqCst);
        Ok(Some(size))
    }

    /// Prints `skip LABEL: REASON` and keeps the line for the end of the output.
    pub(super) async fn note_skipped(
        &self,
        label: &str,
        reason: &(dyn Display + Sync),
        context: &JobContext,
    ) {
        context.printer.out(format!("skip {label}: {reason}")).await;
        self.unsupported
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!("{label}: {reason}"));
    }
}

/// The last line on stderr of an archive run, for example `Summary: 2 archives migrated
/// (3.4 MB, 1 Google Workspace file exported), 0 already there, 1 skipped (unsupported), 0
/// failed, 0 cancelled`. A dry run's size leaves out the exports, and says so.
pub(super) fn summary(report: &Report, tally: &Tally, dry_run: bool) -> String {
    let files = &report.files;
    let mut line = String::new();
    if dry_run {
        line.push_str("Dry run, nothing was changed. ");
    }
    let _ = write!(
        line,
        "Summary: {} {} ({}",
        plural(files.transferred, "archive", "archives"),
        if dry_run { "to migrate" } else { "migrated" },
        human_size(files.bytes)
    );
    let exports = tally.converted.load(Ordering::SeqCst);
    if exports > 0 {
        let documents = plural(exports, "Google Workspace file", "Google Workspace files");
        if dry_run {
            let _ = write!(line, " without the exports of {documents}");
        } else {
            let _ = write!(line, ", {documents} exported");
        }
    }
    let _ = write!(
        line,
        "), {} already there, {} skipped (unsupported), {} failed, {} cancelled",
        files.unchanged,
        files.skipped + tally.skipped.load(Ordering::SeqCst),
        files.failed,
        files.cancelled,
    );
    if report.interrupted {
        line.push_str("; interrupted");
    }
    line
}
