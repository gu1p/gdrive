//! The comparison of the origin with its copy. The copy of a `FILE_ID` is found by name in the
//! `--into` folder (the top of My Drive by default), as `files migrate` finds or makes it; then
//! the copy is listed one level at a time, only the folders that are the copies of origin
//! folders ([`listing`]), and each origin item is compared with the items of its name in the copy
//! of its folder:
//!
//! ```text
//!   no item of its name (a file for a file, a folder for a folder)      added, and all below
//!   a file of its name with the same content                            not reported
//!     (a blob: the same size and md5; a Google file: the same modifiedTime, which the
//!      migration gave the copy)
//!   other content, and the origin is newer than every file of its name  edited
//!   other content, and a file of its name is newer                      settled by revisions
//! ```
//!
//! The last case is a file that someone changed in the copy, which is expected; the origin may
//! have changed after the migration as well. The first revision of the copy is what the
//! migration wrote: the origin changed after it if its `modifiedTime` is later and, for a blob,
//! its md5 is another. When the revisions cannot tell (none, refused, or a first revision that
//! cannot be the migrated content), the item `differs`.
//!
//! An added blob with the size and md5 of a file of the copy that has no file of its name in the
//! origin's folder was moved or renamed in the origin: `moved`, each file of the copy used once.
//!
//! Items that both accounts see are never compared with themselves (`files migrate` refuses
//! them too): an item whose copy's folder holds the item itself, or whose candidate in the copy
//! is one of the origin's items, is left out, and so is an item a migration does not copy
//! (shortcuts, Drawings, Forms, ...), which `--since` alone can report.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use google_drive3::api::{File, Revision};

use super::{
    listing::{self, Trash},
    origin::{child_path, Item, Node, Origin},
    Change, Error, Kind, Side, Who,
};
use crate::{
    common::{drive_file, retry::retry_read},
    hub::Hub,
};

/// What a listing of the copy returns of each file.
pub(super) const COPY_FIELDS: &str =
    "nextPageToken,incompleteSearch,files(id,name,mimeType,size,md5Checksum,modifiedTime,parents,\
     trashed)";

/// What the revisions of a file of the copy are read for.
pub(super) const REVISION_FIELDS: &str = "revisions(id,modifiedTime,md5Checksum,size)";

/// A file or folder of the copy.
#[derive(Debug, Clone)]
struct CopyFile {
    id: String,
    name: String,
    folder: bool,
    size: Option<i64>,
    md5: Option<String>,
    modified: Option<DateTime<Utc>>,
}

/// The items of one folder of the copy.
#[derive(Debug, Default)]
pub(super) struct CopyFolder {
    files: Vec<CopyFile>,
    ids: HashSet<String>,
}

impl CopyFolder {
    pub(super) fn of(files: &[File]) -> Self {
        let files = files
            .iter()
            .filter_map(|file| {
                Some(CopyFile {
                    id: file.id.clone()?,
                    name: file.name.clone()?,
                    folder: drive_file::is_directory(file),
                    size: file.size,
                    md5: file.md5_checksum.clone(),
                    modified: file.modified_time,
                })
            })
            .collect::<Vec<_>>();
        let ids = files.iter().map(|file| file.id.clone()).collect();
        CopyFolder { files, ids }
    }

    /// Whether the folder holds the origin item `id` itself: both accounts see it.
    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The first folder named `name`, as a migration reuses it.
    fn folder(&self, name: &str) -> Option<&CopyFile> {
        self.files
            .iter()
            .find(|file| file.folder && file.name == name)
    }

    /// The files (not folders) named `name`.
    fn files_named(&self, name: &str) -> Vec<&CopyFile> {
        self.files
            .iter()
            .filter(|file| !file.folder && file.name == name)
            .collect()
    }

    fn counts(&self) -> (u64, u64) {
        let folders = self.files.iter().filter(|file| file.folder).count();
        let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        (count(self.files.len() - folders), count(folders))
    }
}

/// How an origin file compares with the files of its name in the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// One has the same content.
    Same,
    /// The origin is newer than all of them.
    Newer,
    /// One of them is newer, or a time is missing: the revisions tell.
    Ambiguous,
}

/// `item` against `candidates`, the files of its name in the copy (at least one).
fn verdict(item: &Item, candidates: &[&CopyFile]) -> Verdict {
    let native = item.is_native();
    let same = candidates.iter().any(|copy| {
        if native {
            item.modified.is_some() && copy.modified == item.modified
        } else {
            copy.size == item.size && copy.md5 == item.md5
        }
    });
    if same {
        return Verdict::Same;
    }
    let newer = item.modified.is_some_and(|origin| {
        candidates
            .iter()
            .all(|copy| copy.modified.is_some_and(|copy| origin > copy))
    });
    if newer {
        Verdict::Newer
    } else {
        Verdict::Ambiguous
    }
}

/// What the first revision of the copy tells about an origin file whose copy is newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Settled {
    /// The origin changed after the migration.
    Edited,
    /// The origin is what the migration copied.
    Untouched,
    /// The revisions cannot tell.
    Unknown,
}

/// `item` against `first`, the first revision of its copy: the content the migration wrote,
/// with the time the migration gave it or the time it was written.
pub(super) fn settle(item: &Item, first: &Revision) -> Settled {
    let (Some(origin), Some(copied)) = (item.modified, first.modified_time) else {
        return Settled::Unknown;
    };
    let later = origin > copied;
    if item.is_native() {
        return if later {
            Settled::Edited
        } else {
            Settled::Untouched
        };
    }
    let (Some(md5), Some(first_md5)) = (&item.md5, &first.md5_checksum) else {
        return Settled::Unknown;
    };
    let sizes = match (item.size, first.size) {
        (Some(origin), Some(copied)) => origin == copied,
        _ => true,
    };
    match (md5.eq_ignore_ascii_case(first_md5) && sizes, later) {
        (true, _) => Settled::Untouched,
        (false, true) => Settled::Edited,
        // Content that changed since the migration was changed after it: this first revision
        // is not what the migration wrote (Drive pruned that one), or the origin's time was set.
        (false, false) => Settled::Unknown,
    }
}

/// An origin file that changed in the copy, and maybe in the origin too.
#[derive(Debug, Clone)]
struct Ambiguous {
    item: Item,
    path: String,
    /// The ids of the files of its name in the copy.
    copies: Vec<String>,
}

/// A file of the copy without a file of its name in the origin's folder.
#[derive(Debug, Clone)]
struct OnlyInCopy {
    id: String,
    path: String,
    size: Option<i64>,
    md5: String,
}

/// What the comparison found.
#[derive(Debug, Default)]
pub(super) struct Comparison {
    pub(super) changes: Vec<Change>,
    /// The items a migration does not copy, which are never in the copy.
    pub(super) unsupported: Vec<Node>,
    /// The paths of the items that both accounts see, which are not compared.
    pub(super) shared: Vec<String>,
    /// What the listings of the copy found.
    pub(super) copy_files: u64,
    pub(super) copy_folders: u64,
    /// How many files of the copy had their revisions read, and how many of the origin's files
    /// they could not settle.
    pub(super) revisions_read: u64,
    pub(super) unsettled: u64,
    /// The `added` blobs that may have been moved: the index of the change, size and md5.
    added_blobs: Vec<(usize, Option<i64>, String)>,
    only_in_copy: Vec<OnlyInCopy>,
    ambiguous: Vec<Ambiguous>,
}

/// Where the copy is: the `--into` folder or the top of My Drive, and its items.
pub(super) struct Place<'a> {
    pub(super) folder: &'a CopyFolder,
    /// `--into` as given and the folder's name, `None` for the top of My Drive.
    pub(super) into: Option<(&'a str, &'a str)>,
    /// The shared drive of `--into`, whose corpus the copy is listed in.
    pub(super) drive_id: Option<&'a str>,
}

/// Compares `origin` with its copy in `place`, listing the copy with `copy`, `jobs` requests at
/// once.
pub(super) async fn compare(
    copy: &Hub,
    origin: &Origin,
    place: &Place<'_>,
    jobs: usize,
) -> Result<Comparison, Error> {
    let mut comparison = Comparison::default();
    let mut level = Vec::new();
    for root in &origin.roots {
        if root.item.unsupported().is_some() {
            comparison.unsupported.push(root.clone());
            continue;
        }
        let missing = || Error::MissingCopy {
            name: root.item.name.clone(),
            folder: root.item.is_folder(),
            into: place
                .into
                .map(|(id, name)| (id.to_owned(), name.to_owned())),
        };
        if place.folder.contains(&root.item.id) {
            return Err(Error::SameItem {
                name: root.item.name.clone(),
                folder: root.item.is_folder(),
            });
        }
        if root.item.is_folder() {
            let copy = place.folder.folder(&root.item.name).ok_or_else(missing)?;
            if origin.ids.contains(&copy.id) {
                return Err(Error::InsideOrigin {
                    name: root.item.name.clone(),
                    folder: true,
                });
            }
            comparison.copy_folders += 1;
            level.push((root, copy.id.clone()));
        } else {
            let candidates = place.folder.files_named(&root.item.name);
            if candidates.is_empty() {
                return Err(missing());
            }
            if candidates.iter().any(|copy| origin.ids.contains(&copy.id)) {
                return Err(Error::InsideOrigin {
                    name: root.item.name.clone(),
                    folder: false,
                });
            }
            comparison.copy_files += u64::try_from(candidates.len()).unwrap_or(u64::MAX);
            comparison.compare_file(root, &candidates);
        }
    }

    while !level.is_empty() {
        let folders = level
            .iter()
            .map(|(node, copy_id)| listing::Folder {
                id: copy_id,
                path: &node.path,
                drive_id: place.drive_id,
                resource_key: None,
            })
            .collect::<Vec<_>>();
        let listed = listing::list_children(copy, &folders, Trash::Without, COPY_FIELDS, jobs)
            .await
            .map_err(|error| error.into_error(Side::Copy))?;
        let mut copies: HashMap<&str, CopyFolder> = HashMap::new();
        for (id, files) in &listed {
            let folder = CopyFolder::of(files);
            let (files, folders) = folder.counts();
            comparison.copy_files += files;
            comparison.copy_folders += folders;
            copies.insert(id, folder);
        }
        let mut next = Vec::new();
        for (node, copy_id) in &level {
            let empty = CopyFolder::default();
            let folder = copies.get(copy_id.as_str()).unwrap_or(&empty);
            next.extend(comparison.compare_folder(node, folder, &origin.ids));
        }
        level = next;
    }

    comparison.settle(copy, jobs).await;
    comparison.find_moves();
    Ok(comparison)
}

impl Comparison {
    /// Compares the children of the origin folder `node` with its copy `copy`; the subfolders
    /// that have a copy, to compare next.
    fn compare_folder<'a>(
        &mut self,
        node: &'a Node,
        copy: &CopyFolder,
        origin_ids: &HashSet<String>,
    ) -> Vec<(&'a Node, String)> {
        let mut next = Vec::new();
        for child in &node.children {
            if child.item.unsupported().is_some() {
                self.unsupported.push(child.clone());
                continue;
            }
            if copy.contains(&child.item.id) {
                self.shared.push(child.path.clone());
                continue;
            }
            if child.item.is_folder() {
                match copy.folder(&child.item.name) {
                    Some(folder) if origin_ids.contains(&folder.id) => {
                        self.shared.push(child.path.clone());
                    }
                    Some(folder) => next.push((child, folder.id.clone())),
                    None => self.added(child),
                }
                continue;
            }
            let candidates = copy.files_named(&child.item.name);
            if candidates.iter().any(|file| origin_ids.contains(&file.id)) {
                self.shared.push(child.path.clone());
            } else if candidates.is_empty() {
                self.added(child);
            } else {
                self.compare_file(child, &candidates);
            }
        }

        let names = node
            .children
            .iter()
            .filter(|child| !child.item.is_folder())
            .map(|child| child.item.name.as_str())
            .collect::<HashSet<_>>();
        for file in &copy.files {
            if file.folder || names.contains(file.name.as_str()) || origin_ids.contains(&file.id) {
                continue;
            }
            if let Some(md5) = &file.md5 {
                self.only_in_copy.push(OnlyInCopy {
                    id: file.id.clone(),
                    path: child_path(&node.path, &file.name),
                    size: file.size,
                    md5: md5.clone(),
                });
            }
        }
        next
    }

    /// The origin file `node` against the files of its name in the copy.
    fn compare_file(&mut self, node: &Node, candidates: &[&CopyFile]) {
        match verdict(&node.item, candidates) {
            Verdict::Same => {}
            Verdict::Newer => self.changes.push(edited(node, Kind::Edited)),
            Verdict::Ambiguous => self.ambiguous.push(Ambiguous {
                item: node.item.clone(),
                path: node.path.clone(),
                copies: candidates.iter().map(|copy| copy.id.clone()).collect(),
            }),
        }
    }

    /// `node`, which has no copy, and everything below it.
    fn added(&mut self, node: &Node) {
        for node in node.all() {
            if node.item.unsupported().is_some() {
                self.unsupported.push(node.clone());
                continue;
            }
            if !node.item.is_folder() && !node.item.is_native() {
                if let Some(md5) = &node.item.md5 {
                    self.added_blobs
                        .push((self.changes.len(), node.item.size, md5.clone()));
                }
            }
            self.changes.push(Change {
                kind: Kind::Added,
                when: node.item.created,
                who: node.item.creator(),
                path: node.path.clone(),
                id: node.item.id.clone(),
            });
        }
    }

    /// Reads the revisions of the copies of the ambiguous files, `jobs` at once, and adds the
    /// files that changed in the origin as `edited`, those the revisions cannot settle as
    /// `differs`.
    async fn settle(&mut self, copy: &Hub, jobs: usize) {
        let ambiguous = std::mem::take(&mut self.ambiguous);
        let settled = stream::iter(ambiguous)
            .map(|ambiguous| async move {
                let mut firsts = Vec::new();
                for id in &ambiguous.copies {
                    firsts.push(first_revision(copy, id).await);
                }
                (ambiguous, firsts)
            })
            .buffer_unordered(jobs.max(1))
            .collect::<Vec<_>>()
            .await;
        for (ambiguous, firsts) in settled {
            self.revisions_read += u64::try_from(firsts.len()).unwrap_or(u64::MAX);
            // With several files of its name, the oldest one is the migration's copy.
            let first = firsts
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .and_then(|firsts| {
                    firsts
                        .into_iter()
                        .map(|first| first.modified_time.map(|time| (time, first)))
                        .collect::<Option<Vec<_>>>()
                })
                .and_then(|firsts| {
                    firsts
                        .into_iter()
                        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)))
                })
                .map(|(_, first)| first);
            let settled = first.map_or(Settled::Unknown, |first| settle(&ambiguous.item, &first));
            let kind = match settled {
                Settled::Untouched => continue,
                Settled::Edited => Kind::Edited,
                Settled::Unknown => {
                    self.unsettled += 1;
                    Kind::Differs
                }
            };
            self.changes.push(Change {
                kind,
                when: ambiguous.item.modified,
                who: ambiguous.item.editor(),
                path: ambiguous.path,
                id: ambiguous.item.id,
            });
        }
    }

    /// Turns each `added` blob that has the size and md5 of a file only in the copy into
    /// `moved`: by path, each file of the copy at most once.
    fn find_moves(&mut self) {
        let mut pool = std::mem::take(&mut self.only_in_copy);
        pool.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
        let mut seen = HashSet::new();
        pool.retain(|file| seen.insert(file.id.clone()));
        let mut used = vec![false; pool.len()];
        let mut blobs = std::mem::take(&mut self.added_blobs);
        blobs.sort_by(|a, b| self.changes[a.0].path.cmp(&self.changes[b.0].path));
        for (index, size, md5) in blobs {
            let found = pool.iter().enumerate().position(|(n, file)| {
                !used[n] && file.size == size && file.md5.eq_ignore_ascii_case(&md5)
            });
            if let Some(n) = found {
                used[n] = true;
                let change = &mut self.changes[index];
                change.kind = Kind::Moved {
                    was: pool[n].path.clone(),
                };
                change.when = None;
                change.who = Who::Unknown;
            }
        }
    }
}

/// An `edited` (or `differs`) change of `node`: when it was last modified, by whom.
fn edited(node: &Node, kind: Kind) -> Change {
    Change {
        kind,
        when: node.item.modified,
        who: node.item.editor(),
        path: node.path.clone(),
        id: node.item.id.clone(),
    }
}

/// The first revision of the file `id` of the copy; `None` when there is none or it cannot be
/// read (a 403 for a file the account may not edit, a 404, or any other failure).
async fn first_revision(hub: &Hub, id: &str) -> Option<Revision> {
    let (_, list) = retry_read(hub.retrier(), "revisions.list", |mut delegate| async move {
        hub.revisions()
            .list(id)
            .param("fields", REVISION_FIELDS)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .ok()?;
    list.revisions?.into_iter().next()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use google_drive3::api::Revision;

    use super::{settle, verdict, CopyFile, Settled, Verdict};
    use crate::files::diverged::origin::Item;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn blob(md5: &str, modified: &str) -> Item {
        Item {
            id: String::from("o"),
            name: String::from("a.bin"),
            mime_type: String::from("application/octet-stream"),
            size: Some(4),
            md5: Some(md5.to_owned()),
            modified: Some(at(modified)),
            ..Item::default()
        }
    }

    fn doc(modified: &str) -> Item {
        Item {
            mime_type: String::from("application/vnd.google-apps.document"),
            size: None,
            md5: None,
            ..blob("", modified)
        }
    }

    fn copy(md5: Option<&str>, modified: Option<&str>) -> CopyFile {
        CopyFile {
            id: String::from("c"),
            name: String::from("a.bin"),
            folder: false,
            size: md5.map(|_| 4),
            md5: md5.map(str::to_owned),
            modified: modified.map(at),
        }
    }

    fn revision(md5: Option<&str>, modified: Option<&str>) -> Revision {
        Revision {
            id: Some(String::from("1")),
            md5_checksum: md5.map(str::to_owned),
            size: md5.map(|_| 4),
            modified_time: modified.map(at),
            ..Revision::default()
        }
    }

    #[test]
    fn the_newer_side_decides_unless_the_content_is_the_same() {
        let item = blob("aa", "2026-11-02T00:00:00Z");
        let same = copy(Some("aa"), Some("2026-12-01T00:00:00Z"));
        let older = copy(Some("bb"), Some("2026-11-01T00:00:00Z"));
        let newer = copy(Some("cc"), Some("2026-11-03T00:00:00Z"));
        let timeless = copy(Some("dd"), None);
        assert_eq!(verdict(&item, &[&older, &same]), Verdict::Same);
        assert_eq!(verdict(&item, &[&older]), Verdict::Newer);
        assert_eq!(verdict(&item, &[&older, &newer]), Verdict::Ambiguous);
        assert_eq!(verdict(&item, &[&timeless]), Verdict::Ambiguous);

        let item = doc("2026-11-02T00:00:00Z");
        let migrated = copy(None, Some("2026-11-02T00:00:00Z"));
        assert_eq!(verdict(&item, &[&migrated]), Verdict::Same);
        assert_eq!(
            verdict(&item, &[&copy(None, Some("2026-11-01T00:00:00Z"))]),
            Verdict::Newer
        );
        assert_eq!(
            verdict(&doc("2026-11-02T00:00:00Z"), &[&copy(None, None)]),
            Verdict::Ambiguous
        );
    }

    #[test]
    fn the_first_revision_settles_what_it_can() {
        let item = blob("aa", "2026-11-05T00:00:00Z");
        let first = |md5, time| revision(md5, Some(time));
        assert_eq!(
            settle(&item, &first(Some("aa"), "2026-11-01T00:00:00Z")),
            Settled::Untouched
        );
        assert_eq!(
            settle(&item, &first(Some("AA"), "2026-11-09T00:00:00Z")),
            Settled::Untouched,
            "the same content, in any case"
        );
        assert_eq!(
            settle(&item, &first(Some("bb"), "2026-11-01T00:00:00Z")),
            Settled::Edited
        );
        assert_eq!(
            settle(&item, &first(Some("bb"), "2026-11-09T00:00:00Z")),
            Settled::Unknown,
            "other content, older than the first revision: not the migration's"
        );
        assert_eq!(
            settle(&item, &first(None, "2026-11-01T00:00:00Z")),
            Settled::Unknown
        );
        assert_eq!(settle(&item, &revision(Some("bb"), None)), Settled::Unknown);

        let item = doc("2026-11-05T00:00:00Z");
        assert_eq!(
            settle(&item, &first(None, "2026-11-01T00:00:00Z")),
            Settled::Edited
        );
        assert_eq!(
            settle(&item, &first(None, "2026-11-05T00:00:00Z")),
            Settled::Untouched
        );
        assert_eq!(settle(&item, &revision(None, None)), Settled::Unknown);
    }
}
