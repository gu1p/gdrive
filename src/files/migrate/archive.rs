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
//!        -> once the upload returned, checked: the mark, appProperties
//!           gdriveArchive=<manifest> gdriveArchiveMd5=<md5 of the archive>
//! ```
//!
//! The mark means "verified": the archive is created without it, and gets it (one metadata
//! update) only once the uploader compared Drive's md5 of the whole tar with the bytes read and
//! every file with an md5 was checked against it (`transfer/archive.rs`). It holds the
//! manifest digest and the md5 of the archive then, and is already-there only on a file whose
//! listed md5 is still that one. An archive without it (one that failed a check), or with other
//! bytes than it was marked with (its content replaced since), is a file of that name with other
//! content for a later run: a conflict, never already-there. An update whose reply failed may
//! or may not have written the mark.
//!
//! `--replace-smaller`: a file of the archive's name that is no archive of this manifest is
//! replaced when it is the only one and smaller than the new archive, whose size is known once
//! its exports are made (a dry run compares with the size without them, and with exports says
//! `replace ... if the exports make it larger` for a file that is not smaller than that). The new
//! archive goes up
//! as a new version of that file, which keeps its old mark until the new one is checked: with
//! other bytes than it was marked with, it is no archive of any manifest in between.
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
    ask, failed,
    source::{label, SourceFolder, SourceItem, SourceNode},
    Error, Existing, Incoming, Match, Migration, Place, Settings, Settled, Smaller, Tally,
};
use crate::{
    common::{
        drive_file::{self, DocType, FileExtension},
        human_size::human_size,
        local_name::{self, NameEntry, UnusableName},
        parse_md5_digest,
    },
    files::{
        export::{export_file_name, ExportFormats},
        update::{update_metadata, PatchFile},
    },
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

/// The `appProperties` key of an archive, whose value is its manifest digest: the mark of an
/// archive that was checked.
pub(super) const PROPERTY: &str = "gdriveArchive";

/// The `appProperties` key of the md5 an archive had when it was checked, which binds the mark
/// to those bytes: an archive whose content was replaced since has another md5.
pub(super) const PROPERTY_MD5: &str = "gdriveArchiveMd5";

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

    /// Where a browser opens what the archive is made of: the folder, or the one file.
    fn link(&self) -> String {
        match self.entries.first().map(|entry| &entry.kind) {
            Some(Kind::Folder) => ask::folder_link(&self.root_id),
            _ => ask::file_link(&self.root_id),
        }
    }

    /// The size of the archive with every export empty: what the listing tells.
    fn planned_size(&self) -> u64 {
        let entries = self.entries.iter().map(|entry| entry.entry(None)).collect();
        Layout::new(entries).size()
    }

    /// The size of the archive without its exports: what it has at least, since an export
    /// over Drive's limit is left out. Nothing at all when it is exports only: no archive is
    /// made of nothing.
    fn least_size(&self) -> u64 {
        let entries = self
            .entries
            .iter()
            .filter(|entry| !matches!(entry.kind, Kind::Export(_)))
            .map(|entry| entry.entry(None))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return 0;
        }
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

/// How the making of an archive ended.
enum Made {
    /// Uploaded and marked: its size.
    Archive(u64),
    /// Nothing was left to archive.
    Nothing,
    /// The file of its name in the destination stays (`--ask`).
    Kept,
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
        if self.archives(name, manifest).next().is_some() {
            Match::Same { verified: true }
        } else {
            Match::Different
        }
    }

    /// The files named `name` that are the archive of `manifest`: with its mark, on the bytes
    /// it was checked with.
    fn archives<'a>(
        &'a self,
        name: &str,
        manifest: &'a str,
    ) -> impl Iterator<Item = &'a super::DestFile> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|file| file.mime_type != drive_file::MIME_TYPE_DRIVE_FOLDER)
            .filter(move |file| {
                file.archive.as_deref() == Some(manifest)
                    && matches!(
                        (file.archive_md5.as_deref(), file.md5.as_deref()),
                        (Some(marked), Some(listed)) if same_md5(marked, listed)
                    )
            })
    }
}

/// Whether two md5s in hex are the same, whatever the case of their digits.
pub(super) fn same_md5(one: &str, other: &str) -> bool {
    matches!(
        (parse_md5_digest(one), parse_md5_digest(other)),
        (Some(one), Some(other)) if one == other
    )
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
        // Whether a file of its name with other content is there, which it may replace.
        let replacing = match place
            .existing
            .archive(&plan.name, &plan.manifest, &self.source_ids)
        {
            Match::None => false,
            Match::Same { .. } => {
                // Another archive of this run replaces the file that is this one.
                let same = place.existing.archives(&plan.name, &plan.manifest);
                if !self.rely(same.map(|file| file.id.as_str())) {
                    return failed(label, Error::Conflict).into();
                }
                context.printer.out(format!("already-there {label}")).await;
                return Outcome::Unchanged.into();
            }
            Match::Different if self.replaces() => true,
            Match::Different => return failed(label, Error::Conflict).into(),
            Match::Source => return failed(label, Error::InsideSource { folder: false }).into(),
        };
        // What a dry run tells of the file it would replace, and whether that hangs on the
        // exports, which only a real run makes.
        let (smaller, unsure) = if replacing && self.dry_run {
            match self.dry_replace(&plan, &place, context).await {
                Ok(Some(told)) => (Some(told.0), told.1),
                // It told what it would ask about.
                Ok(None) => return Outcome::Unchanged.into(),
                Err(error) => return failed(label, error).into(),
            }
        } else {
            (None, false)
        };
        for (label, reason) in &plan.skipped {
            self.left_out(label, reason, context).await;
        }

        if self.dry_run {
            let exports = plan.exports();
            let verb = if smaller.is_some() {
                "replace"
            } else {
                "create"
            };
            let mut line = format!("{verb} {label} ({}", human_size(planned));
            if exports > 0 {
                let _ = write!(
                    line,
                    " without the exports of {}",
                    plural(exports, "Google Workspace file", "Google Workspace files")
                );
            }
            if let Some(old) = &smaller {
                let _ = write!(line, ", was {}", human_size(old.size));
                self.tally.replaced.fetch_add(1, Ordering::SeqCst);
            }
            line.push(')');
            if unsure {
                line.push_str(" if the exports make it larger");
            }
            context.printer.out(line).await;
            self.tally.converted.fetch_add(exports, Ordering::SeqCst);
            return Outcome::Transferred { bytes: planned }.into();
        }

        match self.make_archive(&plan, &place, replacing, context).await {
            Ok(Made::Archive(bytes)) => Outcome::Transferred { bytes }.into(),
            // A FILE_ID that is a Workspace file whose export is too large.
            Ok(Made::Nothing) => Outcome::Skipped {
                reason: SkipReason::NotExportable,
            }
            .into(),
            Ok(Made::Kept) => Outcome::Unchanged.into(),
            Err(error) => failed(label, error).into(),
        }
    }

    /// The file that the archive of `plan` replaces, as far as a dry run can tell, and whether
    /// that hangs on its exports; `None` once it printed what it would ask about. A dry run
    /// knows the size without the exports: the archive has at least the bytes of everything
    /// else, and replaces a file smaller than that for sure.
    async fn dry_replace(
        &self,
        plan: &Plan,
        place: &Place,
        context: &JobContext,
    ) -> Result<Option<(Smaller, bool)>, Error> {
        let (name, label) = (&plan.name, &plan.label);
        let link = plan.link();
        let least = Incoming::Archive {
            size: plan.least_size(),
            link: &link,
        };
        match self.settle(place, name, label, least, context).await? {
            Settled::Replace(old) => return Ok(Some((old, false))),
            Settled::Keep => return Ok(None),
            Settled::Conflict => {}
        }
        if plan.exports() > 0 {
            // Large enough exports make it larger than any file.
            let most = Incoming::Archive {
                size: u64::MAX,
                link: &link,
            };
            if let Settled::Replace(old) = self.settle(place, name, label, most, context).await? {
                return Ok(Some((old, true)));
            }
        }
        Err(Error::Conflict)
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

    /// Exports, lays out and uploads the archive of `plan`. `replacing`: a file of its name
    /// with other content is there, and the archive replaces it if it is smaller or the user
    /// says so, and is a conflict otherwise.
    async fn make_archive(
        &self,
        plan: &Plan,
        place: &Place,
        replacing: bool,
        context: &JobContext,
    ) -> Result<Made, Error> {
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
            return Ok(Made::Nothing);
        }
        self.tally.skipped.fetch_add(left_out, Ordering::SeqCst);

        let layout = Layout::new(entries);
        let size = layout.size();
        let label = &plan.label;
        let smaller = if replacing {
            let link = plan.link();
            let incoming = Incoming::Archive { size, link: &link };
            match self
                .settle(place, &plan.name, label, incoming, context)
                .await?
            {
                Settled::Replace(old) => Some(old),
                Settled::Keep => return Ok(Made::Kept),
                Settled::Conflict => return Err(Error::Conflict),
            }
        } else {
            None
        };
        let line = match &smaller {
            Some(old) => format!(
                "replace {label} ({}, was {})",
                human_size(size),
                human_size(old.size)
            ),
            None => format!("create {label} ({})", human_size(size)),
        };
        context.printer.out(line).await;
        let media_type = archive::MEDIA_TYPE
            .parse::<mime::Mime>()
            .unwrap_or(mime::APPLICATION_OCTET_STREAM);
        let target = UploadTarget {
            parents: place.folder.iter().cloned().collect(),
            name: Some(plan.name.clone()),
            mime: Some(media_type.clone()),
            // A new version of the smaller file, which keeps its id and folder.
            existing_file_id: smaller.as_ref().map(|old| old.id.clone()),
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
        );
        // Only resumable uploads have sessions to record. A continued session is checked whole,
        // as one run (no trust_continued_content): the part it holds is read again, see
        // transfer/archive.rs.
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
        // Right before the upload: what came before may have waited for a question's answer.
        if let Some(old) = &smaller {
            self.still_listed(place, &plan.name, old).await?;
        }
        // Boxed, as the relay's: the upload's state is large.
        let file = Box::pin(upload.run(&mut source))
            .await
            .map_err(|error| Error::Archive(Box::new(ArchiveError::of_upload(error))))?;
        if let Some(cache) = &cache {
            cache.remove().await;
        }
        let id = file.id.unwrap_or_default();
        let md5 = file.md5_checksum.as_deref();
        if smaller.is_some() {
            self.holds(&id, md5).await?;
            self.tally.replaced.fetch_add(1, Ordering::SeqCst);
        } else if !self.rely(std::iter::once(id.as_str())) {
            // Another archive of the run, which listed the folder later, replaced this one.
            return Err(Error::Conflict);
        }
        self.mark(plan, id, md5).await?;
        self.tally.converted.fetch_add(exported, Ordering::SeqCst);
        Ok(Made::Archive(size))
    }

    /// Marks the destination file `id` as the archive of `plan`, once it is checked: with the
    /// md5 it was checked with (without one, the mark never matches).
    async fn mark(&self, plan: &Plan, id: String, md5: Option<&str>) -> Result<(), Error> {
        let mut mark = PatchFile::new(id.clone()).with_app_property(PROPERTY, &plan.manifest);
        if let Some(md5) = md5 {
            mark = mark.with_app_property(PROPERTY_MD5, md5);
        }
        update_metadata(&self.dest, mark)
            .await
            .map_err(|source| Error::ArchiveMark {
                name: plan.label.clone(),
                id,
                source: Box::new(source),
            })?;
        Ok(())
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
pub(super) fn summary(report: &Report, tally: &Tally, settings: &Settings) -> String {
    let dry_run = settings.dry_run;
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
        // A conflict that was left as it is counts further on.
        files
            .unchanged
            .saturating_sub(tally.left.load(Ordering::SeqCst)),
        files.skipped + tally.skipped.load(Ordering::SeqCst),
        files.failed,
        files.cancelled,
    );
    line.push_str(&super::replaced(tally, settings));
    if report.interrupted {
        line.push_str("; interrupted");
    }
    line
}
