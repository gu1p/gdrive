//! The source side of a migration: each `FILE_ID`, and for a folder everything in it, listed by
//! the walk ([`crate::transfer::walk`]) before anything is migrated, and kept as a compact tree
//! (the fields a migration needs, not Drive's whole file resource).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use google_drive3::api::File;

use super::Error;
use crate::{
    common::{drive_file, local_name, retry::retry_read},
    hub::Hub,
    transfer::{
        scheduler::Printer,
        walk::{Children, EntryKind, FolderKey, Walk, WalkItem, WalkOptions, WalkRoot},
    },
};

/// The fields of a `FILE_ID` that is migrated.
const ROOT_FIELDS: &str =
    "id,name,mimeType,size,md5Checksum,modifiedTime,createdTime,driveId,resourceKey,trashed";

/// The prefix of the Google Workspace MIME types.
const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// What a migration knows about a source file (not a folder).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SourceFile {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) mime_type: String,
    pub(super) size: Option<i64>,
    pub(super) md5: Option<String>,
    pub(super) modified_time: Option<DateTime<Utc>>,
    /// Copied to the new file by the relay.
    pub(super) created_time: Option<DateTime<Utc>>,
    /// Needed to read a file that is shared by link only: the relay sends it with every request
    /// about the source.
    pub(super) resource_key: Option<String>,
}

impl SourceFile {
    fn of(file: &File) -> Self {
        SourceFile {
            id: file.id.clone().unwrap_or_default(),
            name: file.name.clone().unwrap_or_default(),
            mime_type: file.mime_type.clone().unwrap_or_default(),
            size: file.size,
            md5: file.md5_checksum.clone(),
            modified_time: file.modified_time,
            created_time: file.created_time,
            resource_key: file.resource_key.clone(),
        }
    }

    /// The file as the relay takes it: the fields of a listing.
    pub(super) fn to_drive_file(&self) -> File {
        File {
            id: Some(self.id.clone()),
            name: Some(self.name.clone()),
            mime_type: Some(self.mime_type.clone()),
            size: self.size,
            md5_checksum: self.md5.clone(),
            modified_time: self.modified_time,
            created_time: self.created_time,
            resource_key: self.resource_key.clone(),
            ..File::default()
        }
    }

    /// The size of the content of a blob; `None` when Drive has none (shortcuts) and for
    /// Workspace files, whose size says nothing about their export.
    pub(super) fn content_size(&self) -> Option<u64> {
        if self.is_native() {
            return None;
        }
        self.size.and_then(|size| u64::try_from(size).ok())
    }

    /// [`Self::content_size`], 0 without one.
    pub(super) fn bytes(&self) -> u64 {
        self.content_size().unwrap_or_default()
    }

    /// A Google Docs, Sheets, ... file: migrated by export and conversion.
    pub(super) fn is_native(&self) -> bool {
        self.mime_type.starts_with(GOOGLE_APPS_PREFIX)
            && self.mime_type != drive_file::MIME_TYPE_DRIVE_SHORTCUT
            && self.mime_type != drive_file::MIME_TYPE_DRIVE_FOLDER
    }
}

/// A source file and its path from the `FILE_ID` it was found under.
#[derive(Debug, Clone)]
pub(super) struct SourceItem {
    pub(super) file: SourceFile,
    /// The Drive names joined with `/`, starting with the name of the `FILE_ID`, with control
    /// characters escaped ([`label`]): for output only.
    pub(super) label: String,
}

/// `name` below the folder labelled `parent` (none for a `FILE_ID`), with control characters
/// escaped: a name with a newline or an escape sequence cannot break or forge a line.
pub(super) fn label(parent: Option<&str>, name: &str) -> String {
    let name = local_name::printable(name);
    match parent {
        Some(parent) => format!("{parent}/{name}"),
        None => name.into_owned(),
    }
}

/// A source folder and what the walk found in it.
#[derive(Debug)]
pub(super) struct SourceFolder {
    pub(super) id: String,
    pub(super) name: String,
    /// The time of an archive's directory entry.
    pub(super) modified_time: Option<DateTime<Utc>>,
    pub(super) label: String,
    pub(super) children: Vec<SourceNode>,
    /// Drive reported an incomplete search for it: some children may be missing.
    pub(super) incomplete: bool,
    /// The folder could not be listed ([`Error::ListSource`]) or is its own ancestor
    /// ([`Error::Cycle`]): it is not migrated.
    pub(super) error: Option<Error>,
}

impl SourceFolder {
    /// Labels everything in the folder by its path from the folder, without the folder's own
    /// name: as its items are placed in the destination with `--contents`.
    pub(super) fn label_from_inside(&mut self) {
        let prefix = format!("{}/", self.label);
        let mut pending = self.children.iter_mut().collect::<Vec<_>>();
        while let Some(node) = pending.pop() {
            let label = match node {
                SourceNode::File(item) => &mut item.label,
                SourceNode::Folder(SourceFolder {
                    label, children, ..
                }) => {
                    pending.extend(children.iter_mut());
                    label
                }
            };
            if let Some(inside) = label.strip_prefix(&prefix) {
                *label = inside.to_owned();
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum SourceNode {
    File(SourceItem),
    Folder(SourceFolder),
}

impl SourceNode {
    /// Adds the ids of this file or folder and of everything below it to `ids`.
    pub(super) fn collect_ids(&self, ids: &mut HashSet<String>) {
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            match node {
                SourceNode::File(item) => {
                    ids.insert(item.file.id.clone());
                }
                SourceNode::Folder(folder) => {
                    ids.insert(folder.id.clone());
                    pending.extend(&folder.children);
                }
            }
        }
    }

    pub(super) fn name(&self) -> &str {
        match self {
            SourceNode::File(item) => &item.file.name,
            SourceNode::Folder(folder) => &folder.name,
        }
    }
}

/// What the walk found, for the first line of output and for the progress.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Totals {
    pub(super) files: u64,
    pub(super) folders: u64,
    pub(super) bytes: u64,
    /// Some folder was listed incompletely.
    pub(super) incomplete: bool,
}

impl Totals {
    pub(super) fn add(&mut self, node: &SourceNode) {
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            match node {
                SourceNode::File(item) => {
                    self.files += 1;
                    self.bytes += item.file.bytes();
                }
                SourceNode::Folder(folder) => {
                    self.folders += 1;
                    self.incomplete |= folder.incomplete;
                    pending.extend(&folder.children);
                }
            }
        }
    }
}

/// Gets the source file `id` and, for a folder, lists everything in it, `listing_jobs`
/// requests at once. Incomplete searches are warned about on stderr as they come.
pub(super) async fn discover(
    hub: &Hub,
    id: &str,
    listing_jobs: usize,
    printer: &Printer,
) -> Result<SourceNode, Error> {
    let file = get(hub, id).await.map_err(|source| Error::GetSource {
        id: id.to_owned(),
        source,
    })?;
    if !drive_file::is_directory(&file) {
        let file = SourceFile::of(&file);
        let label = label(None, &file.name);
        return Ok(SourceNode::File(SourceItem { file, label }));
    }
    Ok(SourceNode::Folder(
        walk(hub, file, listing_jobs, printer).await,
    ))
}

async fn get(hub: &Hub, id: &str) -> Result<File, Box<google_drive3::Error>> {
    let (_, file) = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
        hub.files()
            .get(id)
            .param("fields", ROOT_FIELDS)
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(Box::new)?;
    Ok(file)
}

/// A folder while the walk runs.
#[derive(Debug, Default)]
struct Building {
    children: Vec<Pending>,
    done: bool,
    incomplete: bool,
    error: Option<Error>,
}

/// A child whose folder, if it is one, is assembled after the walk.
#[derive(Debug)]
enum Pending {
    File(SourceFile),
    /// `None`: not listed, a folder that is its own ancestor (or has no id).
    Folder(SourceFile, Option<FolderKey>),
}

async fn walk(hub: &Hub, root: File, listing_jobs: usize, printer: &Printer) -> SourceFolder {
    let walk_root = WalkRoot {
        id: root.id.clone().unwrap_or_default(),
        name: root.name.clone().unwrap_or_default(),
        drive_id: root.drive_id.clone(),
        resource_key: root.resource_key.clone(),
    };
    let root = SourceFile::of(&root);
    let options = WalkOptions {
        listing_jobs,
        ..WalkOptions::default()
    };
    let mut walk = Walk::start(Arc::new(hub.clone()), walk_root, options);

    let mut folders: HashMap<FolderKey, Building> = HashMap::new();
    while let Some(item) = walk.next().await {
        match item {
            WalkItem::Entry(entry) => {
                let file = SourceFile::of(&entry.file);
                let pending = match (entry.kind, entry.children) {
                    (EntryKind::Folder, Children::Listed(key)) => Pending::Folder(file, Some(key)),
                    (EntryKind::Folder, Children::None | Children::Cycle) => {
                        Pending::Folder(file, None)
                    }
                    _ => Pending::File(file),
                };
                folders
                    .entry(entry.parent)
                    .or_default()
                    .children
                    .push(pending);
            }
            WalkItem::FolderDone(key) => folders.entry(key).or_default().done = true,
            WalkItem::FolderError(error) => {
                let key = error.folder.key;
                folders.entry(key).or_default().error = Some(Error::ListSource(error));
            }
            WalkItem::IncompleteSearch(incomplete) => {
                for folder in &incomplete.folders {
                    folders.entry(folder.key).or_default().incomplete = true;
                    printer
                        .err(format!(
                            "Warning: Drive did not search everything while listing '{}', some \
                            files may be missing",
                            label_of(&root.name, &folder.components)
                        ))
                        .await;
                }
            }
        }
    }
    let root_label = label(None, &root.name);
    assemble(FolderKey::ROOT, root, root_label, &mut folders)
}

/// The label of a folder of the walk below the root `root`.
fn label_of(root: &str, components: &[String]) -> String {
    components
        .iter()
        .fold(label(None, root), |parent, name| label(Some(&parent), name))
}

/// The folder `key` (the source `file`) with everything below it, from the folders of the walk.
/// A folder the walk did not finish counts as listed incompletely.
fn assemble(
    key: FolderKey,
    file: SourceFile,
    label: String,
    folders: &mut HashMap<FolderKey, Building>,
) -> SourceFolder {
    let building = folders.remove(&key).unwrap_or_default();
    let incomplete = building.incomplete || !building.done;
    let children = building
        .children
        .into_iter()
        .map(|child| match child {
            Pending::File(file) => {
                let label = self::label(Some(&label), &file.name);
                SourceNode::File(SourceItem { file, label })
            }
            Pending::Folder(file, Some(key)) => {
                let child_label = self::label(Some(&label), &file.name);
                SourceNode::Folder(assemble(key, file, child_label, folders))
            }
            Pending::Folder(file, None) => SourceNode::Folder(SourceFolder {
                label: self::label(Some(&label), &file.name),
                id: file.id,
                name: file.name,
                modified_time: file.modified_time,
                children: Vec::new(),
                incomplete: false,
                error: Some(Error::Cycle),
            }),
        })
        .collect();
    SourceFolder {
        id: file.id,
        name: file.name,
        modified_time: file.modified_time,
        label,
        children,
        incomplete: incomplete && building.error.is_none(),
        error: building.error,
    }
}
