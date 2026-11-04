//! The end of phase 1: both trees of the [`Inventory`] matched by relative path and every pair
//! classified, into a [`Plan`] that the dry run prints and the real run carries out.
//!
//! # Matching
//!
//! Drive names are mapped to local names exactly as a recursive download maps them
//! ([`local_name::local_name`]: NFC, unsafe characters replaced, reserved names changed; a
//! document exported with `--export` gets the format's extension first), so a sync matches
//! what a download would have written. Local names go through the same mapping, so a local
//! `con.txt` meets the Drive `con.txt` (both `con_.txt`) and an NFD name meets its NFC twin.
//! Within one folder both sides are grouped by the mapped name with letter case and Unicode
//! normalization ignored:
//!
//! - a group with two Drive files, or two local entries, or a local name that is not UTF-8,
//!   is **ambiguous**: nothing in it is transferred or deleted, and the names a download would
//!   give the duplicates (`x~1a2b3c4d`) are protected too;
//! - a Drive file and a local entry whose mapped names differ only in case are ambiguous too;
//! - a Google Docs file without an export format, a Workspace file gdrive cannot export and a
//!   shortcut are skipped, and so is the local entry of the same name or of any name the
//!   document's exports would have (`Report.docx`, `Report.pdf`, ...);
//! - otherwise one Drive file and one local entry are a pair, compared below, and a lone entry
//!   is new on the source side or extra on the destination side.
//!
//! # Classification of a pair of files
//!
//! - Different sizes: changed.
//! - `down`: same size and the local modification time is Drive's `modifiedTime` exactly (a
//!   download sets it): unchanged, without reading the file. Otherwise the local md5 is
//!   computed; equal to Drive's, the file is unchanged (and gets Drive's time, so the next run
//!   does not read it again), else changed.
//! - `up`: same size: the local md5 is computed and compared with Drive's. Drive's
//!   `modifiedTime` is the time of the upload, not of the local file, so the time is no hint;
//!   nothing is recorded, every run reads every file whose size matches.
//! - A folder against a file, either way: skipped (`type-conflict`), with all below it.
//!
//! # Extras
//!
//! An entry that exists only on the destination is an extra. An extra folder is one deletion
//! candidate when everything below it could be deleted; otherwise the folder stays (`protected`)
//! and the candidates below it are listed one by one. Never candidates: skipped and ambiguous
//! entries and everything below them, symlinks, temporary download files, unreadable entries,
//! folders that could not be listed completely. Whether candidates are deleted at all is decided
//! after phase 2 (see the parent module).

use std::{
    collections::{BTreeMap, HashMap},
    fmt::{self, Display, Formatter},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use unicode_normalization::UnicodeNormalization;

use super::inventory::{
    same_time, FileId, FileState, Inventory, Listing, LocalKind, LocalNode, RemoteFolder,
    RemoteKind, RemoteNode, LOCAL_TRASH,
};
use crate::{
    common::{
        drive_file::{DocType, FileExtension},
        local_name::{self, NameEntry},
    },
    files::export::{export_file_name, ExportFormats},
};

/// Which side is the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// Local directory to Drive folder.
    Up,
    /// Drive folder to local directory.
    Down,
}

/// Why an entry is neither transferred nor deleted: the `<reason>` of `skip:<reason>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// A Google Docs, Sheets, Slides or Drawings file without an export format, or a local
    /// entry named like it or like one of its exports.
    Document,
    /// A Workspace file gdrive has no export format for (Forms, Sites, ...), or a local entry
    /// with its name.
    NotExportable,
    /// A shortcut, or a local entry with its name.
    Shortcut,
    /// Names that are the same when case and normalization are ignored, duplicates on Drive,
    /// a local name that is not UTF-8.
    Ambiguous,
    /// A Drive name that cannot be a local name (`..`, `.`, empty).
    UnusableName,
    /// A Drive file named like the local trash, [`LOCAL_TRASH`], at the top.
    ReservedName,
    /// A temporary download file (`.gdrive-*.part`).
    TempFile,
    /// A local symlink, which `sync down` never writes through or removes.
    Symlink,
    /// A folder on one side, a file on the other.
    TypeConflict,
    /// A local entry the inventory could not read.
    Unreadable,
    /// A Drive folder that could not be listed.
    Unlisted,
    /// A local entry in a Drive folder whose listing Drive reported incomplete: it may exist
    /// on Drive already.
    IncompleteListing,
    /// A folder that contains itself (a symlink cycle, or a Drive folder that is its own
    /// ancestor).
    Cycle,
    /// A Drive file without a size.
    Unsupported,
    /// An extra folder that is kept because something below it is never deleted.
    Protected,
}

impl Display for Reason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reason::Document => "document",
            Reason::NotExportable => "not-exportable",
            Reason::Shortcut => "shortcut",
            Reason::Ambiguous => "ambiguous",
            Reason::UnusableName => "unusable-name",
            Reason::ReservedName => "reserved-name",
            Reason::TempFile => "temp-file",
            Reason::Symlink => "symlink",
            Reason::TypeConflict => "type-conflict",
            Reason::Unreadable => "unreadable",
            Reason::Unlisted => "unlisted",
            Reason::IncompleteListing => "incomplete-listing",
            Reason::Cycle => "cycle",
            Reason::Unsupported => "unsupported",
            Reason::Protected => "protected",
        })
    }
}

/// The plan of a sync.
#[derive(Debug)]
pub(crate) struct Plan {
    /// The items of the top folder, in name order; folders hold theirs.
    pub(crate) items: Vec<Item>,
    /// The id of the Drive folder.
    pub(crate) remote_root: String,
    /// The deletion candidates, in name order.
    pub(crate) extras: Vec<Extra>,
    /// The local files to read for their md5, with Drive's md5, by [`Work::Compare`] index.
    pub(crate) compares: Vec<(PathBuf, md5::Digest)>,
    /// Lines for stderr about names that are not synced.
    pub(crate) warnings: Vec<String>,
}

/// One entry of the plan: its path for the output and what happens to it.
#[derive(Debug)]
pub(crate) struct Item {
    /// The path below the synced folders, with `/`; a folder's ends with `/`.
    pub(crate) label: String,
    pub(crate) work: Work,
}

#[derive(Debug)]
pub(crate) enum Work {
    /// A folder on both sides: its items.
    Descend {
        remote_id: String,
        items: Vec<Item>,
    },
    /// A folder to create on the destination, and its items.
    CreateDir {
        /// The name: of the local directory for `up`, the mapped Drive name for `down`.
        name: String,
        /// Below the local root.
        rel: PathBuf,
        items: Vec<Item>,
    },
    Create(FileWork),
    Update(FileWork),
    /// Same size: the md5 of the local file decides, see [`Plan::resolve`].
    Compare {
        index: usize,
        differ: FileWork,
        touch: Option<Touch>,
    },
    /// Nothing to transfer; `down` gives the file Drive's time when it was compared by md5.
    Unchanged(Option<Touch>),
    Skip(Reason),
}

/// A file transfer.
#[derive(Debug, Clone)]
pub(crate) enum FileWork {
    /// `up`: the local file `path` as `name`, a new revision of `existing` or a new file.
    Upload {
        path: PathBuf,
        name: String,
        size: u64,
        existing: Option<String>,
    },
    /// `down`: the Drive file `id` to `rel` below the local root.
    Download {
        rel: PathBuf,
        id: String,
        size: u64,
        md5: Option<md5::Digest>,
        modified: Option<DateTime<Utc>>,
    },
    /// `down --export`: the document `id` exported to `rel` below the local root.
    Export {
        rel: PathBuf,
        id: String,
        doc_type: DocType,
        format: FileExtension,
        modified: Option<DateTime<Utc>>,
    },
}

impl FileWork {
    /// The bytes it transfers, as far as known: none for an export.
    #[must_use]
    pub(crate) fn size(&self) -> u64 {
        match self {
            FileWork::Upload { size, .. } | FileWork::Download { size, .. } => *size,
            FileWork::Export { .. } => 0,
        }
    }
}

/// Set the modification time of the local file `path` to Drive's `modified`, if it is still the
/// file that was read for its md5.
#[derive(Debug, Clone)]
pub(crate) struct Touch {
    pub(crate) path: PathBuf,
    pub(crate) modified: DateTime<Utc>,
    /// The file as it was read; `None` until it was.
    pub(crate) read: Option<FileState>,
}

/// The md5 of a local file, and the file as it was read.
pub(crate) type Digest = io::Result<(md5::Digest, FileState)>;

/// A deletion candidate.
#[derive(Debug, Clone)]
pub(crate) struct Extra {
    pub(crate) label: String,
    pub(crate) target: ExtraTarget,
}

#[derive(Debug, Clone)]
pub(crate) enum ExtraTarget {
    /// `up`: a Drive file or folder, with everything in it.
    Drive { id: String },
    /// `down`: a local file or directory, as the inventory found it.
    Local {
        path: PathBuf,
        /// Below the local root.
        rel: PathBuf,
        snapshot: Snapshot,
    },
}

/// A local entry as the inventory found it, checked again before it is removed: a directory with
/// all its entries, each with its identity, and a file with its size and modification time too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Snapshot {
    File {
        size: u64,
        modified: Option<SystemTime>,
        id: Option<FileId>,
    },
    Dir {
        id: Option<FileId>,
        /// Every entry of the directory, by path.
        children: Vec<(PathBuf, Snapshot)>,
    },
}

/// Builds the plan of `inventory`; `export` only applies to `down`.
#[must_use]
pub(crate) fn build(
    inventory: &Inventory,
    direction: Direction,
    export: Option<ExportFormats>,
) -> Plan {
    let mut builder = Builder {
        direction,
        export,
        local_root: &inventory.local.root,
        extras: Vec::new(),
        compares: Vec::new(),
        warnings: Vec::new(),
    };
    let local = inventory.local.children.as_deref().unwrap_or_default();
    let remote = &inventory.remote;
    let items = builder.matched(
        &Folder {
            prefix: "",
            rel: Path::new(""),
            listing: remote.listing,
            top: true,
        },
        &remote.children,
        local,
    );
    Plan {
        items,
        remote_root: remote.id.clone(),
        extras: builder.extras,
        compares: builder.compares,
        warnings: builder.warnings,
    }
}

impl Plan {
    /// Replaces every [`Work::Compare`] by its result, `digests[index]` being the md5 of the
    /// local file. A file that could not be read is skipped as unreadable; the errors are
    /// returned with the items' labels.
    pub(crate) fn resolve(
        &mut self,
        mut digests: Vec<Option<Digest>>,
    ) -> Vec<(String, PathBuf, io::Error)> {
        let mut errors = Vec::new();
        resolve_items(&mut self.items, &self.compares, &mut digests, &mut errors);
        errors
    }
}

fn resolve_items(
    items: &mut [Item],
    compares: &[(PathBuf, md5::Digest)],
    digests: &mut [Option<Digest>],
    errors: &mut Vec<(String, PathBuf, io::Error)>,
) {
    for item in items {
        match &mut item.work {
            Work::Descend { items, .. } | Work::CreateDir { items, .. } => {
                resolve_items(items, compares, digests, errors);
            }
            Work::Compare { index, .. } => {
                let index = *index;
                let result = digests.get_mut(index).and_then(Option::take);
                let work = std::mem::replace(&mut item.work, Work::Skip(Reason::Unreadable));
                let Work::Compare { differ, touch, .. } = work else {
                    continue;
                };
                let Some((path, expected)) = compares.get(index) else {
                    continue;
                };
                item.work = match result {
                    Some(Ok((digest, read))) if digest == *expected => {
                        Work::Unchanged(touch.map(|touch| Touch {
                            read: Some(read),
                            ..touch
                        }))
                    }
                    Some(Ok(_)) => Work::Update(differ),
                    Some(Err(error)) => {
                        errors.push((item.label.clone(), path.clone(), error));
                        Work::Skip(Reason::Unreadable)
                    }
                    None => {
                        let error = io::Error::other("not read");
                        errors.push((item.label.clone(), path.clone(), error));
                        Work::Skip(Reason::Unreadable)
                    }
                };
            }
            Work::Create(_) | Work::Update(_) | Work::Unchanged(_) | Work::Skip(_) => {}
        }
    }
}

/// The folder whose children are matched.
struct Folder<'a> {
    /// The label of the folder followed by `/`, empty for the top.
    prefix: &'a str,
    /// The folder below the local root.
    rel: &'a Path,
    /// How complete the Drive side's listing is.
    listing: Listing,
    /// The synced folders themselves.
    top: bool,
}

impl Folder<'_> {
    fn label(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    fn dir_label(&self, name: &str) -> String {
        format!("{}{name}/", self.prefix)
    }
}

/// What a Drive file is to the matching.
#[derive(Debug, Clone)]
enum Role {
    /// It gets a local name: a folder, a file with content, a document exported by `down`.
    Named {
        mapped: String,
        /// The name the local name is made from (with the export's extension), as the
        /// recursive download gives it to [`local_name::unique_local_names`].
        source: String,
        export: Option<(DocType, FileExtension)>,
    },
    /// A document that is not exported, or a Workspace file that cannot be.
    Document {
        base: String,
        reason: Reason,
        /// The local names of its exports.
        protects: Vec<String>,
    },
    Shortcut {
        base: String,
    },
    /// Without a usable name or id; `shown` is for the output.
    Unusable {
        shown: String,
    },
}

#[derive(Debug, Default)]
struct Group {
    remote: Vec<usize>,
    local: Vec<usize>,
}

/// The children of one folder by key.
#[derive(Debug, Default)]
struct Grouping {
    /// Items decided without a group: unusable and reserved names, temporary files.
    items: Vec<Item>,
    groups: BTreeMap<String, Group>,
    /// Keys whose lone local entry is skipped: the exports of a skipped document, the names a
    /// download gives duplicates.
    protected: HashMap<String, Reason>,
    /// Keys whose lone Drive file is skipped.
    shielded: HashMap<String, Reason>,
    /// The local name a download gives each named Drive file, by index.
    unique: HashMap<usize, String>,
}

struct Builder<'a> {
    direction: Direction,
    export: Option<ExportFormats>,
    local_root: &'a Path,
    extras: Vec<Extra>,
    compares: Vec<(PathBuf, md5::Digest)>,
    warnings: Vec<String>,
}

/// The key two names are the same under: case and Unicode normalization ignored.
fn fold(name: &str) -> String {
    name.to_lowercase().nfc().collect()
}

/// A local name as the download mapping makes it, and its key.
fn local_key(node: &LocalNode) -> (String, String) {
    let mapped = local_name::local_name(&node.name, "").unwrap_or_else(|_| node.name.clone());
    (fold(&mapped), mapped)
}

impl Builder<'_> {
    fn role(&self, node: &RemoteNode) -> Role {
        let (Some(name), Some(id)) = (node.name.as_deref(), node.id.as_deref()) else {
            let shown = node.name.as_deref().or(node.id.as_deref()).unwrap_or("?");
            return Role::Unusable {
                shown: shown.escape_debug().to_string(),
            };
        };
        let unusable = || Role::Unusable {
            shown: name.escape_debug().to_string(),
        };
        let Ok(base) = local_name::local_name(name, id) else {
            return unusable();
        };
        match &node.kind {
            RemoteKind::Folder(_) | RemoteKind::Blob { .. } => Role::Named {
                mapped: base,
                source: name.to_owned(),
                export: None,
            },
            RemoteKind::Shortcut => Role::Shortcut { base },
            RemoteKind::Native { mime, .. } => {
                let Some(doc_type) = mime.as_deref().and_then(DocType::from_mime_type) else {
                    return Role::Document {
                        base,
                        reason: Reason::NotExportable,
                        protects: Vec::new(),
                    };
                };
                let format = match (self.direction, self.export) {
                    (Direction::Down, Some(formats)) => formats.format(doc_type),
                    _ => None,
                };
                if let Some(format) = format {
                    let source = export_file_name(name, format);
                    return match local_name::local_name(&source, id) {
                        Ok(mapped) => Role::Named {
                            mapped,
                            source,
                            export: Some((doc_type, format)),
                        },
                        Err(_) => unusable(),
                    };
                }
                // Every name a download could have given the document or one of its exports: the
                // plain one, and the one it gets next to another file of that name.
                let sources = doc_type
                    .supported_export_types()
                    .iter()
                    .map(|&format| export_file_name(name, format))
                    .chain([name.to_owned()]);
                let protects = sources
                    .flat_map(|source| {
                        [
                            local_name::local_name(&source, id),
                            local_name::duplicate_local_name(&source, id),
                        ]
                    })
                    .filter_map(Result::ok)
                    .collect();
                Role::Document {
                    base,
                    reason: Reason::Document,
                    protects,
                }
            }
        }
    }

    /// Matches the children of one folder: `remote` (empty for a local directory that is new on
    /// Drive) and `local` (empty for a Drive folder that is new locally).
    fn matched(
        &mut self,
        folder: &Folder<'_>,
        remote: &[RemoteNode],
        local: &[LocalNode],
    ) -> Vec<Item> {
        let roles = remote
            .iter()
            .map(|node| self.role(node))
            .collect::<Vec<_>>();
        let Grouping {
            mut items,
            groups,
            protected,
            shielded,
            unique,
        } = self.group(folder, remote, &roles, local);

        for (key, group) in &groups {
            let ambiguous = group.remote.len() > 1
                || group.local.len() > 1
                || group.local.iter().any(|&index| !local[index].utf8);
            if ambiguous {
                self.ambiguous(folder, remote, &roles, local, group);
                for &index in &group.remote {
                    // Duplicates are shown under the names a download gives them.
                    let label = match unique.get(&index) {
                        Some(name) if group.remote.len() > 1 => {
                            if matches!(remote[index].kind, RemoteKind::Folder(_)) {
                                folder.dir_label(name)
                            } else {
                                folder.label(name)
                            }
                        }
                        _ => remote_label(folder, &remote[index], &roles[index]),
                    };
                    items.push(skip(label, Reason::Ambiguous));
                }
                for &index in &group.local {
                    items.push(skip(local_label(folder, &local[index]), Reason::Ambiguous));
                }
                continue;
            }
            match (group.remote.first(), group.local.first()) {
                (Some(&r), Some(&l)) => {
                    items.push(self.pair(folder, &remote[r], &roles[r], &local[l]));
                }
                (Some(&r), None) => match shielded.get(key) {
                    Some(&reason) => {
                        items.push(skip(remote_label(folder, &remote[r], &roles[r]), reason));
                    }
                    None => items.extend(self.remote_only(folder, &remote[r], &roles[r])),
                },
                (None, Some(&l)) => match protected.get(key) {
                    Some(&reason) => items.push(skip(local_label(folder, &local[l]), reason)),
                    None => items.extend(self.local_only(folder, &local[l])),
                },
                (None, None) => {}
            }
        }
        items.sort_by(|a, b| a.label.cmp(&b.label));
        items
    }

    /// Groups the children of one folder by key, and finds the names that are protected.
    fn group(
        &self,
        folder: &Folder<'_>,
        remote: &[RemoteNode],
        roles: &[Role],
        local: &[LocalNode],
    ) -> Grouping {
        let mut grouping = Grouping::default();
        let reserved = fold(LOCAL_TRASH);
        for (index, role) in roles.iter().enumerate() {
            let key = match role {
                Role::Unusable { .. } => {
                    let label = remote_label(folder, &remote[index], role);
                    grouping.items.push(skip(label, Reason::UnusableName));
                    continue;
                }
                Role::Named { mapped, .. } => fold(mapped),
                Role::Document {
                    base,
                    reason,
                    protects,
                } => {
                    for name in protects {
                        grouping.protected.entry(fold(name)).or_insert(*reason);
                    }
                    fold(base)
                }
                Role::Shortcut { base } => fold(base),
            };
            if folder.top && key == reserved {
                let label = remote_label(folder, &remote[index], role);
                grouping.items.push(skip(label, Reason::ReservedName));
                continue;
            }
            grouping.groups.entry(key).or_default().remote.push(index);
        }
        for (index, node) in local.iter().enumerate() {
            if local_name::is_reserved_temp_name(&node.name) {
                grouping
                    .items
                    .push(skip(local_label(folder, node), Reason::TempFile));
                // `up`: the file is not uploaded, and the Drive file a download would have
                // saved under its name (`.gdrive-x.part_`) is not an extra either.
                if self.direction == Direction::Up {
                    grouping
                        .shielded
                        .insert(local_key(node).0, Reason::TempFile);
                }
                continue;
            }
            let key = local_key(node).0;
            grouping.groups.entry(key).or_default().local.push(index);
        }

        // The names a download gives Drive duplicates, `x` and `x~1a2b3c4d`: none of them is
        // anybody's to delete.
        grouping.unique = unique_names(remote, roles);
        let duplicates = grouping
            .groups
            .values()
            .filter(|group| group.remote.len() > 1)
            .flat_map(|group| group.remote.iter());
        let mut names = Vec::new();
        for &index in duplicates {
            names.extend(grouping.unique.get(&index).cloned());
            // The name it gets next to another file, whichever keeps the plain one.
            if let (Role::Named { source, .. }, Some(id)) = (&roles[index], &remote[index].id) {
                names.extend(local_name::duplicate_local_name(source, id).ok());
            }
        }
        for name in names {
            grouping
                .protected
                .entry(fold(&name))
                .or_insert(Reason::Ambiguous);
        }
        grouping
    }

    fn ambiguous(
        &mut self,
        folder: &Folder<'_>,
        remote: &[RemoteNode],
        roles: &[Role],
        local: &[LocalNode],
        group: &Group,
    ) {
        let mut names = group
            .remote
            .iter()
            .map(|&index| {
                let id = remote[index].id.as_deref().unwrap_or("?");
                format!("'{}' (Drive id {id})", remote_name(&roles[index]))
            })
            .collect::<Vec<_>>();
        names.extend(
            group
                .local
                .iter()
                .map(|&index| format!("'{}' (local)", local[index].name.escape_debug())),
        );
        let place = if folder.prefix.is_empty() {
            String::from("the top folder")
        } else {
            format!("'{}'", folder.prefix.trim_end_matches('/'))
        };
        let why = if group.local.iter().any(|&index| !local[index].utf8) {
            "a local name that is not UTF-8"
        } else if names.len() == 1 {
            "one name"
        } else {
            "names that are the same, or differ only in letter case or Unicode normalization"
        };
        let names = match names.split_last() {
            Some((last, first)) if !first.is_empty() => {
                format!("{} and {last}", first.join(", "))
            }
            _ => names.join(""),
        };
        self.warnings.push(format!(
            "Warning: In {place}, {names}: {why}; not synced, never deleted"
        ));
    }

    /// A Drive file and a local entry with the same key.
    fn pair(
        &mut self,
        folder: &Folder<'_>,
        remote: &RemoteNode,
        role: &Role,
        local: &LocalNode,
    ) -> Item {
        let label = match self.direction {
            Direction::Up => local_label(folder, local),
            Direction::Down => remote_label(folder, remote, role),
        };
        let (mapped, export) = match role {
            Role::Document { reason, .. } => return skip(label, *reason),
            Role::Shortcut { .. } => return skip(label, Reason::Shortcut),
            Role::Unusable { .. } => return skip(label, Reason::UnusableName),
            Role::Named { mapped, export, .. } => (mapped, *export),
        };
        if local_key(local).1 != *mapped {
            self.warnings.push(format!(
                "Warning: '{}' on Drive and '{}' locally differ in letter case; they are not \
                synced and never deleted",
                remote_name(role),
                local.name.escape_debug()
            ));
            return skip(label, Reason::Ambiguous);
        }
        if local.symlink && self.direction == Direction::Down {
            return skip(label, Reason::Symlink);
        }
        let rel = self.rel(local);
        match (&remote.kind, &local.kind) {
            (_, LocalKind::Unreadable { .. }) => skip(label, Reason::Unreadable),
            (_, LocalKind::Cycle) | (RemoteKind::Folder(None), LocalKind::Dir(_)) => {
                skip(label, Reason::Cycle)
            }
            (RemoteKind::Folder(Some(remote_folder)), LocalKind::Dir(children)) => {
                if remote_folder.listing == Listing::Failed {
                    return skip(label, Reason::Unlisted);
                }
                let inner = Folder {
                    prefix: &label,
                    rel: &rel,
                    listing: remote_folder.listing,
                    top: false,
                };
                let items = self.matched(&inner, &remote_folder.children, children);
                Item {
                    work: Work::Descend {
                        remote_id: remote_folder.id.clone(),
                        items,
                    },
                    label,
                }
            }
            (RemoteKind::Blob { .. } | RemoteKind::Native { .. }, LocalKind::File { .. }) => {
                let work = self.pair_files(remote, local, rel, export);
                Item { label, work }
            }
            _ => skip(label, Reason::TypeConflict),
        }
    }

    /// A Drive file and a local file under one name: what changed, see the module
    /// documentation.
    fn pair_files(
        &mut self,
        remote: &RemoteNode,
        local: &LocalNode,
        rel: PathBuf,
        export: Option<(DocType, FileExtension)>,
    ) -> Work {
        let LocalKind::File {
            size: local_size,
            modified: local_modified,
        } = local.kind
        else {
            return Work::Skip(Reason::TypeConflict);
        };
        let id = remote.id.clone().unwrap_or_default();
        match (&remote.kind, self.direction) {
            (RemoteKind::Blob { size, md5, .. }, Direction::Up) => {
                let differ = FileWork::Upload {
                    path: local.path.clone(),
                    name: local.name.clone(),
                    size: local_size,
                    existing: Some(id),
                };
                match md5 {
                    Some(md5) if *size == Some(local_size) => {
                        self.compare(local, *md5, differ, None)
                    }
                    _ => Work::Update(differ),
                }
            }
            (RemoteKind::Blob { size: None, .. }, Direction::Down) => {
                Work::Skip(Reason::Unsupported)
            }
            (
                RemoteKind::Blob {
                    size: Some(size),
                    md5,
                    modified,
                },
                Direction::Down,
            ) => {
                let differ = FileWork::Download {
                    rel,
                    id,
                    size: *size,
                    md5: *md5,
                    modified: *modified,
                };
                if *size != local_size {
                    return Work::Update(differ);
                }
                if same_time(local_modified, *modified) {
                    return Work::Unchanged(None);
                }
                let touch = modified.map(|modified| Touch {
                    path: local.path.clone(),
                    modified,
                    read: None,
                });
                match md5 {
                    Some(md5) => self.compare(local, *md5, differ, touch),
                    None => Work::Update(differ),
                }
            }
            (RemoteKind::Native { modified, .. }, _) => match export {
                Some(_) if same_time(local_modified, *modified) => Work::Unchanged(None),
                Some((doc_type, format)) => Work::Update(FileWork::Export {
                    rel,
                    id,
                    doc_type,
                    format,
                    modified: *modified,
                }),
                None => Work::Skip(Reason::Document),
            },
            (RemoteKind::Folder(_) | RemoteKind::Shortcut, _) => Work::Skip(Reason::TypeConflict),
        }
    }

    fn compare(
        &mut self,
        local: &LocalNode,
        md5: md5::Digest,
        differ: FileWork,
        touch: Option<Touch>,
    ) -> Work {
        self.compares.push((local.path.clone(), md5));
        Work::Compare {
            index: self.compares.len() - 1,
            differ,
            touch,
        }
    }

    /// A Drive file without a local entry: new for `down`, extra for `up`.
    fn remote_only(&mut self, folder: &Folder<'_>, remote: &RemoteNode, role: &Role) -> Vec<Item> {
        let label = remote_label(folder, remote, role);
        let (mapped, export) = match role {
            Role::Document { reason, .. } => return vec![skip(label, *reason)],
            Role::Shortcut { .. } => return vec![skip(label, Reason::Shortcut)],
            Role::Unusable { .. } => return vec![skip(label, Reason::UnusableName)],
            Role::Named { mapped, export, .. } => (mapped.clone(), *export),
        };
        if self.direction == Direction::Up {
            return self.extra_remote(folder, &mapped, remote);
        }

        let rel = local_name::safe_join(folder.rel, &mapped);
        let id = remote.id.clone().unwrap_or_default();
        let work = match &remote.kind {
            RemoteKind::Folder(None) => Work::Skip(Reason::Cycle),
            RemoteKind::Folder(Some(remote_folder)) => {
                if remote_folder.listing == Listing::Failed {
                    Work::Skip(Reason::Unlisted)
                } else {
                    let inner = Folder {
                        prefix: &label,
                        rel: &rel,
                        listing: remote_folder.listing,
                        top: false,
                    };
                    let items = self.matched(&inner, &remote_folder.children, &[]);
                    Work::CreateDir {
                        name: mapped,
                        rel,
                        items,
                    }
                }
            }
            RemoteKind::Blob { size: None, .. } => Work::Skip(Reason::Unsupported),
            RemoteKind::Blob {
                size: Some(size),
                md5,
                modified,
            } => Work::Create(FileWork::Download {
                rel,
                id,
                size: *size,
                md5: *md5,
                modified: *modified,
            }),
            RemoteKind::Native { modified, .. } => match export {
                Some((doc_type, format)) => Work::Create(FileWork::Export {
                    rel,
                    id,
                    doc_type,
                    format,
                    modified: *modified,
                }),
                None => Work::Skip(Reason::Document),
            },
            RemoteKind::Shortcut => Work::Skip(Reason::Shortcut),
        };
        vec![Item { label, work }]
    }

    /// A local entry without a Drive file: new for `up`, extra for `down`.
    fn local_only(&mut self, folder: &Folder<'_>, local: &LocalNode) -> Vec<Item> {
        if self.direction == Direction::Down {
            return self.extra_local(folder, local);
        }
        let label = local_label(folder, local);
        // It may be on Drive already, in what the listing missed.
        match folder.listing {
            Listing::Complete => {}
            Listing::Incomplete => return vec![skip(label, Reason::IncompleteListing)],
            Listing::Failed => return vec![skip(label, Reason::Unlisted)],
        }
        let work = match &local.kind {
            LocalKind::Unreadable { .. } => Work::Skip(Reason::Unreadable),
            LocalKind::Cycle => Work::Skip(Reason::Cycle),
            LocalKind::Dir(children) => {
                let rel = self.rel(local);
                let inner = Folder {
                    prefix: &label,
                    rel: &rel,
                    listing: Listing::Complete,
                    top: false,
                };
                let items = self.matched(&inner, &[], children);
                Work::CreateDir {
                    name: local.name.clone(),
                    rel,
                    items,
                }
            }
            LocalKind::File { size, .. } => Work::Create(FileWork::Upload {
                path: local.path.clone(),
                name: local.name.clone(),
                size: *size,
                existing: None,
            }),
        };
        vec![Item { label, work }]
    }

    /// `up`: a Drive file or folder that is not local. A folder is one candidate if all below
    /// it can go, else it stays and its children are looked at one by one.
    fn extra_remote(
        &mut self,
        folder: &Folder<'_>,
        mapped: &str,
        remote: &RemoteNode,
    ) -> Vec<Item> {
        let id = remote.id.clone().unwrap_or_default();
        match &remote.kind {
            RemoteKind::Blob { .. } => {
                self.extras.push(Extra {
                    label: folder.label(mapped),
                    target: ExtraTarget::Drive { id },
                });
                Vec::new()
            }
            RemoteKind::Folder(Some(remote_folder)) => {
                let label = folder.dir_label(mapped);
                if self.remote_deletable(remote_folder) {
                    self.extras.push(Extra {
                        label,
                        target: ExtraTarget::Drive { id },
                    });
                    return Vec::new();
                }
                if remote_folder.listing == Listing::Failed {
                    return vec![skip(label, Reason::Unlisted)];
                }
                let inner = Folder {
                    prefix: &label,
                    rel: folder.rel,
                    listing: remote_folder.listing,
                    top: false,
                };
                let mut items = vec![skip(label.clone(), Reason::Protected)];
                let children = &remote_folder.children;
                let roles = children
                    .iter()
                    .map(|child| self.role(child))
                    .collect::<Vec<_>>();
                let ambiguous = ambiguous_remote(&roles);
                if !ambiguous.is_empty() {
                    let group = Group {
                        remote: ambiguous.clone(),
                        local: Vec::new(),
                    };
                    self.ambiguous(&inner, children, &roles, &[], &group);
                }
                let unique = unique_names(children, &roles);
                for (index, (child, role)) in children.iter().zip(roles).enumerate() {
                    let child_label = remote_label(&inner, child, &role);
                    if ambiguous.contains(&index) {
                        // Under the name a download gives it, with everything below it.
                        let label = match unique.get(&index) {
                            Some(name) if matches!(child.kind, RemoteKind::Folder(_)) => {
                                inner.dir_label(name)
                            }
                            Some(name) => inner.label(name),
                            None => child_label,
                        };
                        items.push(skip(label, Reason::Ambiguous));
                        continue;
                    }
                    match role {
                        Role::Named { mapped, .. } => {
                            items.extend(self.extra_remote(&inner, &mapped, child));
                        }
                        Role::Document { reason, .. } => items.push(skip(child_label, reason)),
                        Role::Shortcut { .. } => items.push(skip(child_label, Reason::Shortcut)),
                        Role::Unusable { .. } => {
                            items.push(skip(child_label, Reason::UnusableName));
                        }
                    }
                }
                items
            }
            RemoteKind::Folder(None) => vec![skip(folder.dir_label(mapped), Reason::Cycle)],
            RemoteKind::Native { .. } => vec![skip(folder.label(mapped), Reason::Document)],
            RemoteKind::Shortcut => vec![skip(folder.label(mapped), Reason::Shortcut)],
        }
    }

    /// Whether everything in the Drive folder can be deleted with it: it is listed completely,
    /// and in it and in every folder below, every entry has a name of its own and is a file with
    /// content or such a folder.
    fn remote_deletable(&self, remote_folder: &RemoteFolder) -> bool {
        let roles = remote_folder
            .children
            .iter()
            .map(|child| self.role(child))
            .collect::<Vec<_>>();
        remote_folder.listing == Listing::Complete
            && ambiguous_remote(&roles).is_empty()
            && remote_folder
                .children
                .iter()
                .zip(&roles)
                .all(|(child, role)| {
                    matches!(role, Role::Named { .. })
                        && match &child.kind {
                            RemoteKind::Blob { .. } => true,
                            RemoteKind::Folder(Some(inner)) => self.remote_deletable(inner),
                            RemoteKind::Folder(None)
                            | RemoteKind::Native { .. }
                            | RemoteKind::Shortcut => false,
                        }
                })
    }

    /// `down`: a local entry that is not on Drive.
    fn extra_local(&mut self, folder: &Folder<'_>, local: &LocalNode) -> Vec<Item> {
        let label = local_label(folder, local);
        if local_name::is_reserved_temp_name(&local.name) {
            return vec![skip(label, Reason::TempFile)];
        }
        if local.symlink {
            return vec![skip(label, Reason::Symlink)];
        }
        match &local.kind {
            LocalKind::Unreadable { .. } => vec![skip(label, Reason::Unreadable)],
            LocalKind::Cycle => vec![skip(label, Reason::Cycle)],
            LocalKind::File { .. } | LocalKind::Dir(_) if local_deletable(local) => {
                self.extras.push(Extra {
                    label,
                    target: ExtraTarget::Local {
                        path: local.path.clone(),
                        rel: self.rel(local),
                        snapshot: snapshot(local),
                    },
                });
                Vec::new()
            }
            LocalKind::File { .. } => vec![skip(label, Reason::Protected)],
            LocalKind::Dir(children) => {
                let rel = self.rel(local);
                let inner = Folder {
                    prefix: &label,
                    rel: &rel,
                    listing: Listing::Complete,
                    top: false,
                };
                let mut items = vec![skip(label.clone(), Reason::Protected)];
                let ambiguous = ambiguous_local(children);
                if !ambiguous.is_empty() {
                    let group = Group {
                        remote: Vec::new(),
                        local: ambiguous.clone(),
                    };
                    self.ambiguous(&inner, &[], &[], children, &group);
                }
                for (index, child) in children.iter().enumerate() {
                    if ambiguous.contains(&index) {
                        items.push(skip(local_label(&inner, child), Reason::Ambiguous));
                    } else {
                        items.extend(self.extra_local(&inner, child));
                    }
                }
                items
            }
        }
    }

    /// The local entry's path below the local root.
    fn rel(&self, local: &LocalNode) -> PathBuf {
        local
            .path
            .strip_prefix(self.local_root)
            .map_or_else(|_| PathBuf::from(&local.name), Path::to_path_buf)
    }
}

/// Whether the local entry, and everything in it, can be deleted: no symlink, temporary file,
/// unreadable entry or cycle in it, and in no directory of it two names that are the same when
/// case and normalization are ignored, or a name that is not UTF-8.
fn local_deletable(local: &LocalNode) -> bool {
    !local.symlink
        && !local_name::is_reserved_temp_name(&local.name)
        && match &local.kind {
            LocalKind::File { .. } => true,
            LocalKind::Dir(children) => {
                ambiguous_local(children).is_empty() && children.iter().all(local_deletable)
            }
            LocalKind::Unreadable { .. } | LocalKind::Cycle => false,
        }
}

/// The entries of one local directory that are ambiguous among their siblings: the same key as
/// another, or a name that is not UTF-8. Temporary files are skipped on their own.
fn ambiguous_local(children: &[LocalNode]) -> Vec<usize> {
    let mut keys = HashMap::<String, Vec<usize>>::new();
    let mut ambiguous = Vec::new();
    for (index, child) in children.iter().enumerate() {
        if local_name::is_reserved_temp_name(&child.name) {
            continue;
        }
        if !child.utf8 {
            ambiguous.push(index);
        }
        keys.entry(local_key(child).0).or_default().push(index);
    }
    ambiguous.extend(
        keys.into_values()
            .filter(|indexes| indexes.len() > 1)
            .flatten(),
    );
    ambiguous.sort_unstable();
    ambiguous.dedup();
    ambiguous
}

/// The children of one Drive folder, by their `roles`, that share their key with a sibling.
fn ambiguous_remote(roles: &[Role]) -> Vec<usize> {
    let mut keys = HashMap::<String, Vec<usize>>::new();
    for (index, role) in roles.iter().enumerate() {
        let key = match role {
            Role::Named { mapped, .. } => fold(mapped),
            Role::Document { base, .. } | Role::Shortcut { base } => fold(base),
            Role::Unusable { .. } => continue,
        };
        keys.entry(key).or_default().push(index);
    }
    let mut ambiguous = keys
        .into_values()
        .filter(|indexes| indexes.len() > 1)
        .flatten()
        .collect::<Vec<_>>();
    ambiguous.sort_unstable();
    ambiguous
}

/// The local name a download gives each named child of a Drive folder, by index.
fn unique_names(children: &[RemoteNode], roles: &[Role]) -> HashMap<usize, String> {
    let named = roles
        .iter()
        .enumerate()
        .filter_map(|(index, role)| match role {
            Role::Named { source, .. } => Some((index, children[index].id.as_deref()?, source)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let entries = named
        .iter()
        .map(|(_, id, name)| NameEntry { id, name })
        .collect::<Vec<_>>();
    let mut unique = HashMap::new();
    if let Ok(names) = local_name::unique_local_names(&entries) {
        unique.extend(named.iter().map(|(index, _, _)| *index).zip(names.names));
    }
    unique
}

/// The snapshot of a deletable local entry.
#[must_use]
pub(crate) fn snapshot(local: &LocalNode) -> Snapshot {
    match &local.kind {
        LocalKind::File { size, modified } => Snapshot::File {
            size: *size,
            modified: *modified,
            id: local.id,
        },
        LocalKind::Dir(children) => Snapshot::Dir {
            id: local.id,
            children: children
                .iter()
                .map(|child| (child.path.clone(), snapshot(child)))
                .collect(),
        },
        // Not deletable, never asked for: a directory that never matches.
        LocalKind::Unreadable { .. } | LocalKind::Cycle => Snapshot::File {
            size: u64::MAX,
            modified: None,
            id: None,
        },
    }
}

fn skip(label: String, reason: Reason) -> Item {
    Item {
        label,
        work: Work::Skip(reason),
    }
}

/// The name of a Drive file in the output: its local name, or the escaped Drive name.
fn remote_name(role: &Role) -> &str {
    match role {
        Role::Named { mapped, .. } => mapped,
        Role::Document { base, .. } | Role::Shortcut { base } => base,
        Role::Unusable { shown } => shown,
    }
}

fn remote_label(folder: &Folder<'_>, remote: &RemoteNode, role: &Role) -> String {
    let name = remote_name(role);
    if matches!(remote.kind, RemoteKind::Folder(_)) {
        folder.dir_label(name)
    } else {
        folder.label(name)
    }
}

fn local_label(folder: &Folder<'_>, local: &LocalNode) -> String {
    let name = local.name.escape_debug().to_string();
    if matches!(local.kind, LocalKind::Dir(_) | LocalKind::Cycle)
        || matches!(local.kind, LocalKind::Unreadable { is_dir: true })
    {
        folder.dir_label(&name)
    } else {
        folder.label(&name)
    }
}

/// What the plan would do, by class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) created: u64,
    pub(crate) updated: u64,
    pub(crate) unchanged: u64,
    pub(crate) skipped: BTreeMap<Reason, u64>,
}

impl Tally {
    pub(crate) fn of(items: &[Item]) -> Tally {
        let mut tally = Tally::default();
        tally.add(items);
        tally
    }

    fn add(&mut self, items: &[Item]) {
        for item in items {
            match &item.work {
                Work::Descend { items, .. } => self.add(items),
                Work::CreateDir { items, .. } => {
                    self.created += 1;
                    self.add(items);
                }
                Work::Create(_) => self.created += 1,
                Work::Update(_) | Work::Compare { .. } => self.updated += 1,
                Work::Unchanged(_) => self.unchanged += 1,
                Work::Skip(reason) => *self.skipped.entry(*reason).or_default() += 1,
            }
        }
    }

    pub(crate) fn skipped_total(&self) -> u64 {
        self.skipped.values().sum()
    }
}

/// The line of an item, `None` for a folder on both sides.
#[must_use]
pub(crate) fn line(item: &Item) -> Option<String> {
    let action = match &item.work {
        Work::Descend { .. } => return None,
        Work::CreateDir { .. } | Work::Create(_) => String::from("create"),
        Work::Update(_) | Work::Compare { .. } => String::from("update"),
        Work::Unchanged(_) => String::from("unchanged"),
        Work::Skip(reason) => format!("skip:{reason}"),
    };
    Some(format!("{action} {}", item.label))
}

/// Every line of `items`, folders before what is in them.
pub(crate) fn lines(items: &[Item], out: &mut Vec<String>) {
    for item in items {
        out.extend(line(item));
        match &item.work {
            Work::Descend { items, .. } | Work::CreateDir { items, .. } => lines(items, out),
            _ => {}
        }
    }
}
