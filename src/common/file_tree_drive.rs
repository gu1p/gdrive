//! Maps the incremental walk of a Drive folder ([`crate::transfer::walk`]) to local paths for a
//! recursive download, while the walk runs.
//!
//! The local names of a folder's children depend on all of them (two Drive files may share a
//! name, see [`local_name::unique_local_names`]), so they are assigned once the walk reports the
//! folder complete ([`WalkItem::FolderDone`]). A folder's local path is known once its parent's
//! names are, which may be later: the walk lists folders concurrently. [`LocalTree`] keeps the
//! children of a folder, in compact form, until both are known, and then releases [`Action`]s in
//! an order a download can follow: the [`Action::CreateDir`] of a folder comes before the
//! actions about anything in it.
//!
//! Folders and files with content get local names, as before the walk existed. Google Docs
//! (and the other Workspace types) get local names only when they are exported
//! ([`LocalTree::with_export`]): the name is the Drive name plus the format's extension
//! ([`export_file_name`]), unique among the folder's children like any other name, so `report`
//! exported as `report.docx` and a file `report.docx` next to it never take the same path.
//! Otherwise they are counted as skipped as they arrive, as are shortcuts and the Workspace
//! types that have no export format. A child that cannot be mapped (an unusable or missing
//! name, a folder that is its own ancestor) fails alone; a folder that fails (its listing, its
//! name) cancels everything below it, whose local paths stay unknown.

pub(crate) mod errors;

use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    mem,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};

use crate::{
    common::{
        drive_file::{self, DocType, FileExtension},
        file_tree_drive::errors::FileIdentifier,
        local_name::{self, NameChange, NameEntry, UnusableName},
        parse_md5_digest,
    },
    files::export::{export_file_name, ExportFormats},
    transfer::{
        scheduler::{JobKind, Outcome, SkipReason},
        walk::{Children, EntryKind, FolderError, FolderKey, WalkEntry, WalkItem, WalkRoot},
    },
};

/// What a recursive download does next. Paths are relative to the destination.
#[derive(Debug)]
pub(crate) enum Action {
    /// Create this local directory. Comes before every action about its contents.
    CreateDir(PathBuf),
    /// Download a file into its folder, which a [`Action::CreateDir`] before created.
    Download(PlannedFile),
    /// Export a Workspace document into its folder, which a [`Action::CreateDir`] before
    /// created.
    Export(PlannedExport),
    /// An item that is not downloaded: skipped, failed or cancelled. `label` is its Drive path.
    Done {
        kind: JobKind,
        label: String,
        outcome: Outcome,
    },
    /// A line for stderr.
    Warning(String),
}

/// A file with its local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedFile {
    pub(crate) path: PathBuf,
    pub(crate) id: String,
    pub(crate) size: u64,
    pub(crate) md5: Option<md5::Digest>,
}

/// A Workspace document with the local path of its export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedExport {
    pub(crate) path: PathBuf,
    pub(crate) id: String,
    pub(crate) doc_type: DocType,
    pub(crate) format: FileExtension,
    /// Drive's `modifiedTime`, which the exported file gets as its modification time.
    pub(crate) modified_time: Option<DateTime<Utc>>,
}

/// What a walk found, once it ended. Before, the counts so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    /// Files with content with local paths.
    pub(crate) files: u64,
    /// Workspace documents to export, with local paths.
    pub(crate) exports: u64,
    /// Folders with local paths, the root included.
    pub(crate) folders: u64,
    /// The size of the files.
    pub(crate) bytes: u64,
    /// Drive reported `incompleteSearch` for some folder: files may be missing.
    pub(crate) incomplete: bool,
}

/// The local tree of a recursive download, assembled from the walk; see the module
/// documentation.
#[derive(Debug)]
pub(crate) struct LocalTree {
    /// The root folder's Drive name, which starts every label.
    root_label: String,
    folders: HashMap<FolderKey, FolderState>,
    totals: Totals,
    actions: Vec<Action>,
    /// The export formats; `None` skips every Workspace document.
    export: Option<ExportFormats>,
}

/// A folder location of the walk.
#[derive(Debug, Default)]
struct FolderState {
    /// The Drive path, for labels: the root's name, then the components.
    label: String,
    /// The local path, once the parent's names are assigned.
    path: Option<PathBuf>,
    /// The children so far, until the listing is complete.
    children: Vec<Child>,
    /// The children with their local names, once the listing is complete.
    listed: Option<ListedFolder>,
    /// The folder or one above it failed: everything in it is cancelled.
    failed: bool,
}

impl LocalTree {
    /// Checks the root folder of a recursive download and names its local directory. The first
    /// actions create that directory.
    pub(crate) fn new(
        file: google_drive3::api::File,
    ) -> Result<(LocalTree, WalkRoot), errors::Folder> {
        if !drive_file::is_directory(&file) {
            return Err(errors::Folder::NotDirectory);
        }

        let name = file.name.ok_or(errors::Folder::MissingFileName)?;
        let id = file.id.ok_or(errors::Folder::MissingFileId)?;
        let local_name = local_name::local_name(&name, &id).map_err(|source| {
            errors::Folder::UnusableName(UnusableName::new(&name, &id, source))
        })?;

        let mut actions = Vec::new();
        if local_name::is_renamed(&name, &local_name) {
            let change = NameChange::Sanitized {
                id: id.clone(),
                drive_name: name.clone(),
                local_name: local_name.clone(),
            };
            actions.push(Action::Warning(format!("Warning: {change}")));
        }
        let path = local_name::safe_join(Path::new(""), &local_name);
        actions.push(Action::CreateDir(path.clone()));

        let root = FolderState {
            label: name.clone(),
            path: Some(path),
            ..FolderState::default()
        };
        let tree = LocalTree {
            root_label: name.clone(),
            folders: HashMap::from([(FolderKey::ROOT, root)]),
            totals: Totals {
                folders: 1,
                ..Totals::default()
            },
            actions,
            export: None,
        };
        let root = WalkRoot {
            id,
            name,
            drive_id: file.drive_id,
            resource_key: file.resource_key,
        };
        Ok((tree, root))
    }

    /// Exports the Workspace documents per `export` instead of skipping them; `None` skips them.
    #[must_use]
    pub(crate) fn with_export(mut self, export: Option<ExportFormats>) -> LocalTree {
        self.export = export;
        self
    }

    /// Takes the next item of the walk. The actions it releases are in [`Self::take_actions`].
    pub(crate) fn push(&mut self, item: WalkItem) {
        match item {
            WalkItem::Entry(entry) => self.entry(entry),
            WalkItem::FolderDone(key) => self.folder_done(key),
            WalkItem::FolderError(error) => self.folder_error(error),
            WalkItem::IncompleteSearch(incomplete) => {
                self.totals.incomplete = true;
                for folder in &incomplete.folders {
                    self.actions.push(Action::Warning(format!(
                        "Warning: Drive did not search everything while listing '{}', some \
                        files may be missing",
                        folder.display_path()
                    )));
                }
            }
        }
    }

    /// The actions released so far, in the order to follow.
    pub(crate) fn take_actions(&mut self) -> Vec<Action> {
        mem::take(&mut self.actions)
    }

    #[must_use]
    pub(crate) fn totals(&self) -> Totals {
        self.totals
    }

    fn entry(&mut self, entry: WalkEntry) {
        let WalkEntry {
            components,
            file,
            kind,
            parent,
            children,
            ..
        } = entry;
        let parent_failed = self.folders.get(&parent).is_some_and(|state| state.failed);
        let label = || format!("{}/{}", self.root_label, components.join("/"));

        let export = match kind {
            EntryKind::Native => match self.export_format(&file) {
                Ok(export) => Some(export),
                Err(reason) => {
                    let label = label();
                    if reason == SkipReason::NotExportable {
                        let mime = file.mime_type.as_deref().unwrap_or("no MIME type");
                        self.actions.push(Action::Warning(format!(
                            "Warning: skipping '{}' ({mime}): gdrive has no export format for \
                            this type",
                            label.escape_debug()
                        )));
                    }
                    self.done(JobKind::Media, label, Outcome::Skipped { reason });
                    return;
                }
            },
            EntryKind::Shortcut => {
                let label = label();
                let reason = SkipReason::Shortcut;
                self.done(JobKind::Media, label, Outcome::Skipped { reason });
                return;
            }
            EntryKind::Folder | EntryKind::Blob => None,
        };

        if let Children::Listed(key) = children {
            let state = FolderState {
                label: label(),
                failed: parent_failed,
                ..FolderState::default()
            };
            self.folders.insert(key, state);
        }
        if parent_failed {
            if matches!(kind, EntryKind::Blob | EntryKind::Native) {
                let label = label();
                self.done(JobKind::Media, label, Outcome::Cancelled);
            }
            return;
        }
        let child = match export {
            Some((doc_type, format)) => Some(Child::export(*file, doc_type, format)),
            None => Child::from_file(*file, kind, children),
        };
        if let Some(child) = child {
            self.folders.entry(parent).or_default().children.push(child);
        }
    }

    /// The type and export format of a Workspace document, or why it is skipped.
    fn export_format(
        &self,
        file: &google_drive3::api::File,
    ) -> Result<(DocType, FileExtension), SkipReason> {
        let Some(formats) = &self.export else {
            return Err(SkipReason::GoogleDocument);
        };
        let doc_type = file
            .mime_type
            .as_deref()
            .and_then(DocType::from_mime_type)
            .ok_or(SkipReason::NotExportable)?;
        let format = formats.format(doc_type).ok_or(SkipReason::GoogleDocument)?;
        Ok((doc_type, format))
    }

    fn folder_done(&mut self, key: FolderKey) {
        let state = self.folders.entry(key).or_default();
        let children = mem::take(&mut state.children);
        if state.failed {
            let label = state.label.clone();
            self.cancel_children(&label, children);
            return;
        }
        state.listed = Some(ListedFolder::finish(children));
        if state.path.is_some() {
            self.release(key);
        }
    }

    fn folder_error(&mut self, error: FolderError) {
        let key = error.folder.key;
        let label = self
            .folders
            .get(&key)
            .map_or_else(|| error.folder.display_path(), |state| state.label.clone());
        self.done(JobKind::Metadata, label, Outcome::failed(error));
        self.fail(key);
    }

    fn done(&mut self, kind: JobKind, label: String, outcome: Outcome) {
        self.actions.push(Action::Done {
            kind,
            label,
            outcome,
        });
    }

    /// Releases the folder `key`, whose local path and children's names are known, and then
    /// every folder below it that is complete too.
    fn release(&mut self, key: FolderKey) {
        let mut ready = VecDeque::from([key]);
        while let Some(key) = ready.pop_front() {
            let Some(state) = self.folders.remove(&key) else {
                continue;
            };
            let (Some(path), Some(listed)) = (state.path, state.listed) else {
                continue;
            };

            for change in &listed.changes {
                self.actions.push(Action::Warning(format!(
                    "Warning: In '{}': {change}",
                    local_name::tree_path(&path)
                )));
            }

            for child in listed.children {
                match child {
                    Ok(ListedChild::Folder { local_name, key }) => {
                        let Some(child) = self.folders.get_mut(&key) else {
                            continue;
                        };
                        if child.failed {
                            continue;
                        }
                        let child_path = local_name::safe_join(&path, &local_name);
                        child.path = Some(child_path.clone());
                        if child.listed.is_some() {
                            ready.push_back(key);
                        }
                        self.totals.folders += 1;
                        self.actions.push(Action::CreateDir(child_path));
                    }
                    Ok(ListedChild::File(file)) => {
                        self.totals.files += 1;
                        self.totals.bytes += file.size;
                        self.actions.push(Action::Download(PlannedFile {
                            path: local_name::safe_join(&path, &file.local_name),
                            id: file.drive_id,
                            size: file.size,
                            md5: file.md5,
                        }));
                    }
                    Ok(ListedChild::Export(export)) => {
                        self.totals.exports += 1;
                        self.actions.push(Action::Export(PlannedExport {
                            path: local_name::safe_join(&path, &export.local_name),
                            id: export.drive_id,
                            doc_type: export.doc_type,
                            format: export.format,
                            modified_time: export.modified_time,
                        }));
                    }
                    Err(failed) => {
                        let ChildError {
                            name,
                            folder,
                            error,
                        } = failed;
                        let kind = if folder.is_some() {
                            JobKind::Metadata
                        } else {
                            JobKind::Media
                        };
                        let error = errors::InFolder {
                            folder: path.clone(),
                            source: error,
                        };
                        let label = format!("{}/{name}", state.label);
                        self.done(kind, label, Outcome::failed(error));
                        if let Some(Children::Listed(key)) = folder {
                            self.fail(key);
                        }
                    }
                }
            }
        }
    }

    /// Marks the folder `key` and every folder below it failed, and cancels the files in them.
    fn fail(&mut self, key: FolderKey) {
        let mut failed = vec![key];
        while let Some(key) = failed.pop() {
            let state = self.folders.entry(key).or_default();
            state.failed = true;
            let label = state.label.clone();
            let children = mem::take(&mut state.children);
            let listed = state.listed.take();

            failed.extend(self.cancel_children(&label, children));
            for child in listed.into_iter().flat_map(|listed| listed.children) {
                match child {
                    Ok(ListedChild::Folder { key, .. })
                    | Err(ChildError {
                        folder: Some(Children::Listed(key)),
                        ..
                    }) => failed.push(key),
                    Ok(
                        ListedChild::File(ListedFile { name, .. })
                        | ListedChild::Export(ListedExport { name, .. }),
                    ) => {
                        let label = format!("{label}/{name}");
                        self.done(JobKind::Media, label, Outcome::Cancelled);
                    }
                    Err(_) => {}
                }
            }
        }
    }

    /// Cancels the files among `children` of the folder labeled `label`; returns the folders,
    /// whose subtrees are to be cancelled too.
    fn cancel_children(&mut self, label: &str, children: Vec<Child>) -> Vec<FolderKey> {
        let mut folders = Vec::new();
        for child in children {
            match child.kind {
                ChildKind::Folder(Children::Listed(key)) => folders.push(key),
                ChildKind::Folder(_) => {}
                ChildKind::File { .. } | ChildKind::Export { .. } => {
                    let identifier = child.identifier();
                    let name = identifier.display_name();
                    self.done(
                        JobKind::Media,
                        format!("{label}/{name}"),
                        Outcome::Cancelled,
                    );
                }
            }
        }
        for &key in &folders {
            if let Some(state) = self.folders.get_mut(&key) {
                state.failed = true;
            }
        }
        folders
    }
}

impl FileIdentifier {
    /// The name, else the id, for labels.
    fn display_name(&self) -> &str {
        match self {
            FileIdentifier::Name(name) | FileIdentifier::Id(name) => name,
            FileIdentifier::None => "?",
        }
    }
}

/// What the tree keeps of a child while its folder is being listed: only folders, files with
/// content and documents to export, and only the fields the download uses.
#[derive(Debug, Clone)]
struct Child {
    name: Option<String>,
    id: Option<String>,
    kind: ChildKind,
}

#[derive(Debug, Clone)]
enum ChildKind {
    Folder(Children),
    File {
        size: Option<i64>,
        md5: Option<md5::Digest>,
    },
    Export {
        doc_type: DocType,
        format: FileExtension,
        modified_time: Option<DateTime<Utc>>,
    },
}

impl Child {
    /// `None` for a document or a shortcut, which get no local names.
    fn from_file(
        file: google_drive3::api::File,
        kind: EntryKind,
        children: Children,
    ) -> Option<Child> {
        let kind = match kind {
            EntryKind::Folder => ChildKind::Folder(children),
            EntryKind::Blob => ChildKind::File {
                size: file.size,
                md5: file.md5_checksum.as_deref().and_then(parse_md5_digest),
            },
            EntryKind::Native | EntryKind::Shortcut => return None,
        };
        Some(Child {
            name: file.name,
            id: file.id,
            kind,
        })
    }

    /// A Workspace document to export as `format`.
    fn export(file: google_drive3::api::File, doc_type: DocType, format: FileExtension) -> Child {
        Child {
            name: file.name,
            id: file.id,
            kind: ChildKind::Export {
                doc_type,
                format,
                modified_time: file.modified_time,
            },
        }
    }

    /// The name the child's local name is made from: the Drive name, with the format's
    /// extension for an export.
    fn local_source_name(&self) -> Option<Cow<'_, str>> {
        let name = self.name.as_deref()?;
        Some(match self.kind {
            ChildKind::Export { format, .. } => Cow::Owned(export_file_name(name, format)),
            ChildKind::Folder(_) | ChildKind::File { .. } => Cow::Borrowed(name),
        })
    }

    fn is_folder(&self) -> bool {
        matches!(self.kind, ChildKind::Folder(_))
    }

    fn identifier(&self) -> FileIdentifier {
        FileIdentifier::new(self.name.clone(), self.id.clone())
    }

    fn error(&self, error: errors::Folder) -> ChildError {
        ChildError {
            name: self.identifier().display_name().to_owned(),
            folder: match self.kind {
                ChildKind::Folder(children) => Some(children),
                ChildKind::File { .. } | ChildKind::Export { .. } => None,
            },
            error,
        }
    }

    /// Checks that the child has a name and an id and that the name can be used locally. The
    /// Drive name of an export is checked before the extension is added, so a document named
    /// `..` fails like a file of that name.
    fn check(self) -> Result<Child, ChildError> {
        match (self.name.as_deref(), self.id.as_deref()) {
            (Some(name), Some(id)) => match local_name::local_name(name, id) {
                Ok(_) => Ok(self),
                Err(source) => {
                    let unusable = UnusableName::new(name, id, source);
                    Err(self.error(errors::Folder::UnusableName(unusable)))
                }
            },
            (name, _) => Err(self.error(missing_name_or_id(
                self.identifier(),
                self.is_folder(),
                name.is_none(),
            ))),
        }
    }

    /// Gives a checked child its local name.
    fn finish(self, local_name: String) -> Result<ListedChild, ChildError> {
        let identifier = self.identifier();
        let (Some(name), Some(id)) = (self.name.clone(), self.id.clone()) else {
            let error = missing_name_or_id(identifier, self.is_folder(), self.name.is_none());
            return Err(self.error(error));
        };

        match self.kind {
            ChildKind::Folder(Children::Listed(key)) => Ok(ListedChild::Folder { local_name, key }),
            ChildKind::Folder(Children::None) => Err(self.error(errors::Folder::MissingFileId)),
            ChildKind::Folder(Children::Cycle) => {
                Err(self.error(errors::Folder::Cycle(identifier)))
            }
            ChildKind::File { size, md5 } => {
                let file_error = |source| errors::Folder::File {
                    identifier: FileIdentifier::Name(name.clone()),
                    source,
                };
                let size = match size.map(u64::try_from) {
                    Some(Ok(size)) => size,
                    Some(Err(source)) => {
                        return Err(self.error(file_error(errors::File::InvalidFileSize(source))))
                    }
                    None => return Err(self.error(file_error(errors::File::MissingFileSize))),
                };
                Ok(ListedChild::File(ListedFile {
                    name,
                    local_name,
                    size,
                    drive_id: id,
                    md5,
                }))
            }
            ChildKind::Export {
                doc_type,
                format,
                modified_time,
            } => Ok(ListedChild::Export(ListedExport {
                name,
                local_name,
                drive_id: id,
                doc_type,
                format,
                modified_time,
            })),
        }
    }
}

/// A child of a folder whose listing is complete, with its local name.
#[derive(Debug, Clone)]
enum ListedChild {
    Folder { local_name: String, key: FolderKey },
    File(ListedFile),
    Export(ListedExport),
}

#[derive(Debug, Clone)]
struct ListedFile {
    name: String,
    local_name: String,
    size: u64,
    drive_id: String,
    md5: Option<md5::Digest>,
}

#[derive(Debug, Clone)]
struct ListedExport {
    name: String,
    local_name: String,
    drive_id: String,
    doc_type: DocType,
    format: FileExtension,
    modified_time: Option<DateTime<Utc>>,
}

/// A child that cannot be downloaded.
#[derive(Debug)]
struct ChildError {
    /// The Drive name, else the id.
    name: String,
    /// For a folder, whether the walk lists it.
    folder: Option<Children>,
    error: errors::Folder,
}

/// A folder whose listing is complete.
#[derive(Debug, Default)]
struct ListedFolder {
    children: Vec<Result<ListedChild, ChildError>>,
    /// Local names that differ from the Drive names, reported when the folder's local path is
    /// known.
    changes: Vec<NameChange>,
}

impl ListedFolder {
    /// Assigns the local names of a folder's children. A child without a usable name fails
    /// alone: the others get the names they would get without it.
    fn finish(children: Vec<Child>) -> ListedFolder {
        let mut checked = Vec::with_capacity(children.len());
        let mut failed = Vec::new();
        for child in children {
            match child.check() {
                Ok(child) => checked.push(child),
                Err(error) => failed.push(error),
            }
        }

        let names = checked
            .iter()
            .filter_map(|child| Some((child.id.as_deref()?, child.local_source_name()?)))
            .collect::<Vec<_>>();
        let entries = names
            .iter()
            .map(|(id, name)| NameEntry { id, name })
            .collect::<Vec<_>>();
        let unique = match local_name::unique_local_names(&entries) {
            Ok(unique) => unique,
            // Not reached: every name was checked.
            Err(error) => {
                failed.extend(
                    checked
                        .into_iter()
                        .map(|child| child.error(errors::Folder::UnusableName(error.clone()))),
                );
                return ListedFolder {
                    children: failed.into_iter().map(Err).collect(),
                    changes: Vec::new(),
                };
            }
        };

        let mut children = checked
            .into_iter()
            .zip(unique.names)
            .map(|(child, local_name)| child.finish(local_name))
            .collect::<Vec<_>>();
        children.extend(failed.into_iter().map(Err));
        ListedFolder {
            children,
            changes: unique.changes,
        }
    }
}

fn missing_name_or_id(
    identifier: FileIdentifier,
    is_folder: bool,
    missing_name: bool,
) -> errors::Folder {
    if is_folder {
        if missing_name {
            errors::Folder::MissingFileName
        } else {
            errors::Folder::MissingFileId
        }
    } else {
        let source = if missing_name {
            errors::File::MissingFileName
        } else {
            errors::File::MissingFileId
        };
        errors::Folder::File { identifier, source }
    }
}

#[cfg(test)]
mod tests;
