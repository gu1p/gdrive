//! The comparison of the origin with its copy. The copy of a `FILE_ID` is found by name in the
//! `--into` folder (the top of My Drive by default), as `files migrate` finds or makes it. Then
//! the copy is listed one level at a time, only where the origin has folders ([`listing`]).
//!
//! Which of several folders of one name in the copy belongs to which folder of that name in the
//! origin cannot be known, so folders are compared in groups: all the origin's folders of one
//! name under one parent group, against all the copy's folders of that name under the copy of
//! that group. Their children are pooled and matched by name; one folder on each side is a group
//! of one. Inside one name, files are matched one to one: each origin file is paired with a file
//! of the copy with the same content, each used once. An origin file left without a partner is
//! compared with the files of its name that are left:
//!
//! ```text
//!   none is left (or none has its name)                     added (and edited), and all below
//!   the origin was modified later than every one left       edited
//!   one left is newer, or a time is missing                 a revision of one of them holds the
//!                                                           origin's present content: nothing;
//!                                                           else differs
//! ```
//!
//! The same content is that of a blob with the same size and md5, and for a Google file (no md5,
//! its copy is a conversion) the same `modifiedTime`, which the migration gave the copy. Times are
//! compared to the millisecond. A definite answer needs evidence: revisions that cannot be read,
//! or fields Drive leaves out, give `differs`, never nothing.
//!
//! An added blob with the size, md5 and creation time of a file of the copy that no origin file
//! of its name was matched or compared with was moved or renamed in the origin: `moved`, each
//! file of the copy used once. The migration gave the copy the origin's `createdTime`, which a
//! rename or a move keeps; a file uploaded again under another name has a new one, and stays
//! `added`.
//!
//! Items that both accounts see are never compared with themselves (`files migrate` refuses
//! them too): an item that the copy's folders hold itself, or whose candidates in the copy include
//! one of the origin's items, is left out, and so is an item a migration does not copy
//! (shortcuts, Drawings, Forms, ...), which `--since` alone can report.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use google_drive3::api::{File, Revision};

use super::{
    listing::{self, Trash},
    new_item,
    origin::{child_path, ms, Item, Node, Origin},
    Change, Error, Kind, Side, Who,
};
use crate::{
    common::{drive_file, retry::retry_read},
    hub::Hub,
};

/// What a listing of the copy returns of each file.
pub(super) const COPY_FIELDS: &str =
    "nextPageToken,incompleteSearch,files(id,name,mimeType,size,md5Checksum,modifiedTime,\
     createdTime,parents,trashed)";

/// What the revisions of a file of the copy are read for.
pub(super) const REVISION_FIELDS: &str =
    "nextPageToken,revisions(id,modifiedTime,md5Checksum,size)";

/// The largest page of `revisions.list`.
const REVISION_PAGE_SIZE: i32 = 1000;

/// A file or folder of the copy.
#[derive(Debug, Clone)]
struct CopyFile {
    id: String,
    name: String,
    folder: bool,
    size: Option<i64>,
    md5: Option<String>,
    modified: Option<DateTime<Utc>>,
    created: Option<DateTime<Utc>>,
}

/// The items of one or more folders of the copy, pooled.
#[derive(Debug, Default)]
pub(super) struct CopyFolder {
    files: Vec<CopyFile>,
    ids: HashSet<String>,
}

impl CopyFolder {
    pub(super) fn of(files: &[File]) -> Self {
        let mut folder = CopyFolder::default();
        folder.add(files);
        folder
    }

    /// Adds `files`, each id once.
    fn add(&mut self, files: &[File]) {
        for file in files {
            let (Some(id), Some(name)) = (&file.id, &file.name) else {
                continue;
            };
            if !self.ids.insert(id.clone()) {
                continue;
            }
            self.files.push(CopyFile {
                id: id.clone(),
                name: name.clone(),
                folder: drive_file::is_directory(file),
                size: file.size,
                md5: file.md5_checksum.clone(),
                modified: file.modified_time,
                created: file.created_time,
            });
        }
    }

    /// Whether these folders hold the origin item `id` itself: both accounts see it.
    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The folders named `name`.
    fn folders_named(&self, name: &str) -> Vec<&CopyFile> {
        self.files
            .iter()
            .filter(|file| file.folder && file.name == name)
            .collect()
    }

    /// The files (not folders) named `name`, by id.
    fn files_named(&self, name: &str) -> Vec<&CopyFile> {
        let mut files = self
            .files
            .iter()
            .filter(|file| !file.folder && file.name == name)
            .collect::<Vec<_>>();
        files.sort_by(|a, b| a.id.cmp(&b.id));
        files
    }

    fn counts(&self) -> (u64, u64) {
        let folders = self.files.iter().filter(|file| file.folder).count();
        let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        (count(self.files.len() - folders), count(folders))
    }
}

/// Whether the copy's file `copy` has the content of the origin's `item`: a blob the same size
/// and md5, a Google file the same modification time. A missing field is no evidence.
fn same(item: &Item, copy: &CopyFile) -> bool {
    if item.is_native() {
        return same_time(item.modified, copy.modified);
    }
    same_content(item, copy.size, copy.md5.as_deref())
}

/// Two times that are both known and the same to the millisecond.
fn same_time(one: Option<DateTime<Utc>>, other: Option<DateTime<Utc>>) -> bool {
    matches!((one, other), (Some(one), Some(other)) if ms(one) == ms(other))
}

/// Whether `size` and `md5`, both known, are those of the blob `item`.
fn same_content(item: &Item, size: Option<i64>, md5: Option<&str>) -> bool {
    match (item.size, &item.md5, size, md5) {
        (Some(size), Some(md5), Some(other_size), Some(other_md5)) => {
            size == other_size && md5.eq_ignore_ascii_case(other_md5)
        }
        _ => false,
    }
}

/// Whether the origin's `item` was modified later than every one of `copies`, all with a time.
fn newer(item: &Item, copies: &[&CopyFile]) -> bool {
    item.modified.is_some_and(|origin| {
        copies
            .iter()
            .all(|copy| copy.modified.is_some_and(|copy| ms(origin) > ms(copy)))
    })
}

/// Whether some revision among `revisions` holds the content of the origin's `item` as it is
/// now: for a blob the same size and md5, for a Google file the same modification time. The
/// copy was then written from the origin as it is.
pub(super) fn copied(item: &Item, revisions: &[Revision]) -> bool {
    revisions.iter().any(|revision| {
        if item.is_native() {
            same_time(item.modified, revision.modified_time)
        } else {
            same_content(item, revision.size, revision.md5_checksum.as_deref())
        }
    })
}

/// An origin file whose files of the same name in the copy are newer, or of unknown time.
#[derive(Debug, Clone)]
struct Ambiguous {
    item: Item,
    path: String,
    /// The ids of the files of its name in the copy it is compared with.
    copies: Vec<String>,
}

/// A file of the copy that no origin file was matched or compared with.
#[derive(Debug, Clone)]
struct OnlyInCopy {
    id: String,
    path: String,
    size: Option<i64>,
    md5: String,
    created: Option<DateTime<Utc>>,
}

/// An `added` blob that may have been moved.
#[derive(Debug, Clone)]
struct AddedBlob {
    id: String,
    path: String,
    size: Option<i64>,
    md5: String,
    created: Option<DateTime<Utc>>,
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
    /// How many files of the copy had their revisions read, and for how many of the origin's
    /// files none held their present content.
    pub(super) revisions_read: u64,
    pub(super) unsettled: u64,
    added_blobs: Vec<AddedBlob>,
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

/// The origin's folders of one name under one parent group, and the copy's folders of that name
/// under the copy of that group.
#[derive(Debug)]
struct Group<'a> {
    origin: Vec<&'a Node>,
    copies: Vec<String>,
    /// The path they share.
    path: String,
}

/// `nodes` by name, in the order of the names.
fn by_name<'a>(nodes: impl IntoIterator<Item = &'a Node>) -> BTreeMap<&'a str, Vec<&'a Node>> {
    let mut names: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    for node in nodes {
        names.entry(node.item.name.as_str()).or_default().push(node);
    }
    names
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
    for (name, roots) in by_name(&origin.roots) {
        let (unsupported, roots): (Vec<&Node>, Vec<&Node>) = roots
            .into_iter()
            .partition(|root| root.item.unsupported().is_some());
        comparison
            .unsupported
            .extend(unsupported.into_iter().cloned());
        if let Some(root) = roots
            .iter()
            .find(|root| place.folder.contains(&root.item.id))
        {
            return Err(Error::SameItem {
                name: root.item.name.clone(),
                folder: root.item.is_folder(),
            });
        }
        let (folders, files): (Vec<&Node>, Vec<&Node>) =
            roots.into_iter().partition(|root| root.item.is_folder());
        let missing = |folder: bool| Error::MissingCopy {
            name: name.to_owned(),
            folder,
            into: place
                .into
                .map(|(id, name)| (id.to_owned(), name.to_owned())),
        };
        let inside = |folder: bool| Error::InsideOrigin {
            name: name.to_owned(),
            folder,
        };
        if !folders.is_empty() {
            let copies = place.folder.folders_named(name);
            if copies.is_empty() {
                return Err(missing(true));
            }
            if copies.iter().any(|copy| origin.ids.contains(&copy.id)) {
                return Err(inside(true));
            }
            comparison.copy_folders += count(copies.len());
            level.push(Group {
                origin: folders,
                copies: copies.iter().map(|copy| copy.id.clone()).collect(),
                path: name.to_owned(),
            });
        }
        if !files.is_empty() {
            let candidates = place.folder.files_named(name);
            if candidates.is_empty() {
                return Err(missing(false));
            }
            if candidates.iter().any(|copy| origin.ids.contains(&copy.id)) {
                return Err(inside(false));
            }
            comparison.copy_files += count(candidates.len());
            // Files of the copy's top are no evidence of a move: leave them.
            comparison.match_files(&files, &candidates);
        }
    }

    while !level.is_empty() {
        let folders = level
            .iter()
            .flat_map(|group| {
                group.copies.iter().map(|copy_id| listing::Folder {
                    id: copy_id,
                    path: &group.path,
                    drive_id: place.drive_id,
                    resource_key: None,
                })
            })
            .collect::<Vec<_>>();
        let listed = listing::list_children(copy, &folders, Trash::Without, COPY_FIELDS, jobs)
            .await
            .map_err(|error| error.into_error(Side::Copy))?;
        for files in listed.values() {
            let (files, folders) = CopyFolder::of(files).counts();
            comparison.copy_files += files;
            comparison.copy_folders += folders;
        }
        let mut next = Vec::new();
        for group in &level {
            let mut pooled = CopyFolder::default();
            for copy_id in &group.copies {
                pooled.add(listed.get(copy_id).map_or(&[][..], Vec::as_slice));
            }
            next.extend(comparison.compare_group(group, &pooled, &origin.ids));
        }
        level = next;
    }

    comparison.settle(copy, jobs).await;
    comparison.find_moves();
    Ok(comparison)
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

impl Comparison {
    /// Compares the children of the origin folders of `group` with `copy`, the pooled children of
    /// its copy folders; the groups of subfolders that have a copy, to compare next.
    fn compare_group<'a>(
        &mut self,
        group: &Group<'a>,
        copy: &CopyFolder,
        origin_ids: &HashSet<String>,
    ) -> Vec<Group<'a>> {
        let mut next = Vec::new();
        let children = group.origin.iter().flat_map(|node| &node.children);
        // Every name of an origin file: a file of the copy of another name is matched with none.
        let file_names = children
            .clone()
            .filter(|child| !child.item.is_folder())
            .map(|child| child.item.name.as_str())
            .collect::<HashSet<_>>();
        for (name, nodes) in by_name(children) {
            let mut folders = Vec::new();
            let mut files = Vec::new();
            for node in nodes {
                if node.item.unsupported().is_some() {
                    self.unsupported.push(node.clone());
                } else if copy.contains(&node.item.id) {
                    self.shared.push(node.path.clone());
                } else if node.item.is_folder() {
                    folders.push(node);
                } else {
                    files.push(node);
                }
            }
            let path = child_path(&group.path, name);
            if !folders.is_empty() {
                let copies = copy.folders_named(name);
                if copies.iter().any(|copy| origin_ids.contains(&copy.id)) {
                    self.shared
                        .extend(folders.iter().map(|node| node.path.clone()));
                } else if copies.is_empty() {
                    for node in folders {
                        self.added(node);
                    }
                } else {
                    next.push(Group {
                        origin: folders,
                        copies: copies.iter().map(|copy| copy.id.clone()).collect(),
                        path: path.clone(),
                    });
                }
            }
            if !files.is_empty() {
                let candidates = copy.files_named(name);
                if candidates.iter().any(|copy| origin_ids.contains(&copy.id)) {
                    self.shared
                        .extend(files.iter().map(|node| node.path.clone()));
                } else if candidates.is_empty() {
                    for node in files {
                        self.added(node);
                    }
                } else {
                    // Files of this name left over when every origin file found its partner.
                    let left = self.match_files(&files, &candidates);
                    self.leave_in_copy(&left, &path);
                }
            }
        }
        for file in &copy.files {
            let other_name = !file_names.contains(file.name.as_str());
            if !file.folder && other_name && !origin_ids.contains(&file.id) {
                self.leave_in_copy(&[file], &child_path(&group.path, &file.name));
            }
        }
        next
    }

    /// Notes the files `files` of the copy at `path`, which no origin file was matched or compared
    /// with, as possible sources of a move.
    fn leave_in_copy(&mut self, files: &[&CopyFile], path: &str) {
        for file in files {
            if let Some(md5) = &file.md5 {
                self.only_in_copy.push(OnlyInCopy {
                    id: file.id.clone(),
                    path: path.to_owned(),
                    size: file.size,
                    md5: md5.clone(),
                    created: file.created,
                });
            }
        }
    }

    /// Matches the origin's files `files` of one name with the copy's files of that name,
    /// `candidates`, one to one by content, first those that have the same modification time too
    /// (the migration gave the copy the origin's): of two origin files with one content, the one
    /// that kept its time is the copy's partner. Compares each origin file left with the
    /// candidates left. The candidates that no origin file was matched or compared with.
    fn match_files<'a>(
        &mut self,
        files: &[&Node],
        candidates: &[&'a CopyFile],
    ) -> Vec<&'a CopyFile> {
        let mut files = files.to_vec();
        files.sort_by(|a, b| a.item.id.cmp(&b.item.id));
        let mut used = vec![false; candidates.len()];
        let mut paired = vec![false; files.len()];
        for exact in [true, false] {
            for (node, paired) in files.iter().zip(paired.iter_mut()) {
                if *paired {
                    continue;
                }
                let partner = candidates.iter().enumerate().position(|(n, copy)| {
                    !used[n]
                        && same(&node.item, copy)
                        && (!exact || same_time(node.item.modified, copy.modified))
                });
                if let Some(n) = partner {
                    used[n] = true;
                    *paired = true;
                }
            }
        }
        let unmatched = files
            .iter()
            .zip(&paired)
            .filter(|(_, paired)| !**paired)
            .map(|(node, _)| *node)
            .collect::<Vec<_>>();
        let left = candidates
            .iter()
            .zip(&used)
            .filter(|(_, used)| !**used)
            .map(|(copy, _)| *copy)
            .collect::<Vec<_>>();
        if unmatched.is_empty() {
            return left;
        }
        for node in unmatched {
            if left.is_empty() {
                self.added(node);
            } else if newer(&node.item, &left) {
                self.changes.push(Change {
                    kind: Kind::Edited,
                    when: node.item.modified,
                    who: node.item.editor(),
                    path: node.path.clone(),
                    id: node.item.id.clone(),
                });
            } else {
                self.ambiguous.push(Ambiguous {
                    item: node.item.clone(),
                    path: node.path.clone(),
                    copies: left.iter().map(|copy| copy.id.clone()).collect(),
                });
            }
        }
        Vec::new()
    }

    /// `node`, which has no copy, and everything below it: `added`, and `edited` when it was
    /// modified after its creation.
    fn added(&mut self, node: &Node) {
        for node in node.all() {
            if node.item.unsupported().is_some() {
                self.unsupported.push(node.clone());
                continue;
            }
            if !node.item.is_folder() && !node.item.is_native() {
                if let Some(md5) = &node.item.md5 {
                    self.added_blobs.push(AddedBlob {
                        id: node.item.id.clone(),
                        path: node.path.clone(),
                        size: node.item.size,
                        md5: md5.clone(),
                        created: node.item.created,
                    });
                }
            }
            self.changes.extend(new_item(node));
        }
    }

    /// Reads the revisions of the files of the copy that the ambiguous origin files are compared
    /// with, `jobs` at once. An origin file whose present content a revision holds is what was
    /// copied; any other `differs`.
    async fn settle(&mut self, copy: &Hub, jobs: usize) {
        let ambiguous = std::mem::take(&mut self.ambiguous);
        let settled = stream::iter(ambiguous)
            .map(|ambiguous| async move {
                let mut copied_from = false;
                let mut read = 0_u64;
                for id in &ambiguous.copies {
                    read += 1;
                    let revisions = revisions(copy, id).await;
                    if revisions.is_some_and(|revisions| copied(&ambiguous.item, &revisions)) {
                        copied_from = true;
                        break;
                    }
                }
                (ambiguous, copied_from, read)
            })
            .buffer_unordered(jobs.max(1))
            .collect::<Vec<_>>()
            .await;
        for (ambiguous, copied_from, read) in settled {
            self.revisions_read += read;
            if copied_from {
                continue;
            }
            self.unsettled += 1;
            self.changes.push(Change {
                kind: Kind::Differs,
                when: ambiguous.item.modified,
                who: ambiguous.item.editor(),
                path: ambiguous.path,
                id: ambiguous.item.id,
            });
        }
    }

    /// Turns each `added` blob that has the size, md5 and creation time of a file only in the
    /// copy into `moved`, by path, each file of the copy at most once; its `edited` row goes with
    /// it.
    fn find_moves(&mut self) {
        let mut pool = std::mem::take(&mut self.only_in_copy);
        pool.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
        let mut seen = HashSet::new();
        pool.retain(|file| seen.insert(file.id.clone()));
        let mut used = vec![false; pool.len()];
        let mut blobs = std::mem::take(&mut self.added_blobs);
        blobs.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
        let mut moved = Vec::new();
        for blob in blobs {
            let found = pool.iter().enumerate().position(|(n, file)| {
                !used[n]
                    && matches!((file.size, blob.size), (Some(one), Some(other)) if one == other)
                    && file.md5.eq_ignore_ascii_case(&blob.md5)
                    && same_time(file.created, blob.created)
            });
            if let Some(n) = found {
                used[n] = true;
                moved.push((blob.id, pool[n].path.clone()));
            }
        }
        for (id, was) in moved {
            self.changes
                .retain(|change| !(change.id == id && change.kind == Kind::Edited));
            if let Some(change) = self
                .changes
                .iter_mut()
                .find(|change| change.id == id && change.kind == Kind::Added)
            {
                change.kind = Kind::Moved { was };
                change.when = None;
                change.who = Who::Unknown;
            }
        }
    }
}

/// Every revision of the file `id` of the copy, page by page; `None` when they cannot all be
/// read (a 403 for a file the account may not edit, a 404, or any other failure).
async fn revisions(hub: &Hub, id: &str) -> Option<Vec<Revision>> {
    let mut all = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let token = page_token.as_deref();
        let (_, list) = retry_read(hub.retrier(), "revisions.list", |mut delegate| async move {
            let mut call = hub
                .revisions()
                .list(id)
                .page_size(REVISION_PAGE_SIZE)
                .param("fields", REVISION_FIELDS)
                .add_scope(google_drive3::api::Scope::Full);
            if let Some(token) = token {
                call = call.page_token(token);
            }
            call.delegate(&mut delegate).doit().await
        })
        .await
        .ok()?;
        all.extend(list.revisions.unwrap_or_default());
        page_token = list.next_page_token;
        if page_token.is_none() {
            return Some(all);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use google_drive3::api::Revision;

    use super::{copied, newer, same, CopyFile};
    use crate::files::diverged::origin::Item;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn blob(md5: Option<&str>, modified: &str) -> Item {
        Item {
            id: String::from("o"),
            name: String::from("a.bin"),
            mime_type: String::from("application/octet-stream"),
            size: md5.map(|_| 4),
            md5: md5.map(str::to_owned),
            modified: Some(at(modified)),
            ..Item::default()
        }
    }

    fn doc(modified: Option<&str>) -> Item {
        Item {
            mime_type: String::from("application/vnd.google-apps.document"),
            modified: modified.map(at),
            ..blob(None, "2026-01-01T00:00:00Z")
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
            created: None,
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
    fn the_same_content_needs_both_checksums_or_both_times() {
        let item = blob(Some("aa"), "2026-11-02T00:00:00Z");
        assert!(same(&item, &copy(Some("AA"), Some("2026-12-01T00:00:00Z"))));
        assert!(!same(
            &item,
            &copy(Some("bb"), Some("2026-11-02T00:00:00Z"))
        ));
        // A blob without a checksum is no evidence, on either side.
        assert!(!same(
            &blob(None, "2026-11-02T00:00:00Z"),
            &copy(None, None)
        ));
        assert!(!same(&item, &copy(None, None)));

        let item = doc(Some("2026-11-02T00:00:00.123456Z"));
        assert!(same(&item, &copy(None, Some("2026-11-02T00:00:00.123Z"))));
        assert!(!same(&item, &copy(None, Some("2026-11-02T00:00:00.124Z"))));
        assert!(!same(&doc(None), &copy(None, None)));
    }

    #[test]
    fn the_origin_is_newer_only_than_files_with_a_time() {
        let item = blob(Some("aa"), "2026-11-02T00:00:00.000900Z");
        let older = copy(Some("bb"), Some("2026-11-01T00:00:00Z"));
        let newer_copy = copy(Some("cc"), Some("2026-11-03T00:00:00Z"));
        let timeless = copy(Some("dd"), None);
        let same_ms = copy(Some("ee"), Some("2026-11-02T00:00:00Z"));
        assert!(newer(&item, &[&older]));
        assert!(!newer(&item, &[&older, &newer_copy]));
        assert!(!newer(&item, &[&timeless]));
        assert!(
            !newer(&item, &[&same_ms]),
            "the same millisecond is not later"
        );
    }

    #[test]
    fn a_revision_with_the_origins_present_content_is_evidence_of_the_copy() {
        let item = blob(Some("aa"), "2026-11-05T00:00:00Z");
        assert!(copied(
            &item,
            &[
                revision(Some("bb"), Some("2026-11-06T00:00:00Z")),
                revision(Some("AA"), Some("2026-10-01T00:00:00Z")),
            ]
        ));
        assert!(!copied(
            &item,
            &[revision(Some("bb"), Some("2026-11-01T00:00:00Z"))]
        ));
        assert!(!copied(
            &item,
            &[revision(None, Some("2026-11-05T00:00:00Z"))]
        ));
        assert!(!copied(&item, &[]));

        let item = doc(Some("2026-11-05T00:00:00.123456Z"));
        assert!(copied(
            &item,
            &[revision(None, Some("2026-11-05T00:00:00.123Z"))]
        ));
        assert!(!copied(
            &item,
            &[revision(None, Some("2026-11-03T00:00:00Z"))]
        ));
        assert!(!copied(&item, &[revision(None, None)]));
        assert!(!copied(&doc(None), &[revision(None, None)]));
    }
}
