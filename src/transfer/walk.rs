//! Incremental, breadth-first listing of a Drive folder subtree, exposed as a [`Stream`] of
//! [`WalkItem`]s that a consumer can act on while the walk is still running.
//!
//! Milestone 2 items 2 and 3 of docs/triage/README.md: page-by-page discovery with a fixed
//! `fields` mask, up to 50 parent ids per query, a separate limit for listing requests, a
//! bounded queue towards the consumer, and `incompleteSearch` honored.
//!
//! # Design
//!
//! ```text
//!   Walk::start(Arc<Hub>, WalkRoot, WalkOptions)
//!       |
//!       | tokio::spawn (one task; requests are futures it owns, never spawned)
//!       v
//!  +--------------------------------- producer task ---------------------------------+
//!  |                                                                                 |
//!  |  pending: VecDeque<PendingFolder>           in_flight: FuturesUnordered<page>   |
//!  |  breadth-first; split batches go to the     at most `listing_jobs` requests     |
//!  |  front and are listed one folder each                                           |
//!  |                                                                                 |
//!  |  [root] [a] [b] [c] ..  -- next_batch -->   files.list                          |
//!  |     ^    up to 50 ids, same corpus,          q=('a' in parents or 'b' in        |
//!  |     |    distinct ids                          parents ..) and trashed = false  |
//!  |     |                                        pageSize=1000 [pageToken] fields   |
//!  |     |                                        supportsAllDrives,                 |
//!  |     |                                        includeItemsFromAllDrives,         |
//!  |     |                                        [corpora=drive&driveId]            |
//!  |     |                                        [X-Goog-Drive-Resource-Keys]       |
//!  |     |                                                  |                        |
//!  |     |                                                  | pages, in completion   |
//!  |     |                                                  v order                  |
//!  |     +-- subfolders and followed      handle_page: route each file by `parents`  |
//!  |         shortcut targets, each       to its folder(s), build WalkEntry; next    |
//!  |         with a new FolderKey         page of the batch in the same slot; on an  |
//!  |                                      error or an unroutable result, split       |
//!  |                                                  |                              |
//!  +--------------------------------------------------|------------------------------+
//!                                                     | sender.send(item).await:
//!                                                     | waits while the channel is full
//!                                                     v
//!                            bounded mpsc channel (`buffer` items)
//!                                                     |
//!                                                     v
//!            consumer: Walk as Stream<Item = WalkItem>; dropping it aborts the task
//! ```
//!
//! The producer loop: start requests while a slot is free and folders are pending, wait for any
//! page, send its items one by one (each send may wait for the consumer), queue the next page or
//! the folders it found, repeat. The walk ends when nothing is pending or in flight; the
//! channel then closes and the stream returns `None`.
//!
//! # Invariants
//!
//! - At most `listing_jobs` list requests are in flight. A batch's next page takes the slot of
//!   the page that just finished.
//! - Backpressure: items reach the consumer only through the bounded channel. While the producer
//!   waits for room it starts no request, so it runs ahead of the consumer by at most `buffer`
//!   items plus the pages already in flight. Beyond that it holds the queue of discovered,
//!   not yet listed folders (one small record per folder) and, for every batch in flight, the
//!   ids it emitted so far.
//! - Order: the entry of a folder (with [`Children::Listed`]) is sent before any item about its
//!   children: their entries, its [`IncompleteSearch`], [`WalkItem::FolderDone`] or
//!   [`FolderError`]. Every listed folder location, the root included, ends with exactly one
//!   `FolderDone` or `FolderError`, and nothing names it afterwards. Across folders the order
//!   is the completion order of concurrent requests; within a folder it is Drive's order.
//! - A file id is emitted at most once per folder location, also when Drive repeats it on a
//!   later page or when a batch is split and its folders are listed again; so a subfolder is
//!   queued once per location. A `FolderError` may follow some entries of that folder when a
//!   later page failed.
//! - Trashed files are never emitted: `trashed = false` is in every query and a trashed result
//!   is dropped anyway. `incompleteSearch` is never dropped: it becomes an
//!   [`IncompleteSearch`] item naming the folders of that query, before the page's entries.
//! - Termination: a folder that is its own ancestor is not listed again, and a shortcut target
//!   is listed only if its id was not listed or queued before ([`Children::Cycle`]), so every
//!   folder id is listed at most once through shortcuts plus once per real location.
//! - Cancellation: dropping the [`Walk`] aborts the producer task, which drops the requests in
//!   flight with it; no request is sent afterwards. The stream ends only after the producer
//!   returned; a panic in the producer is resumed in the consumer, so a crash never looks like
//!   a complete walk.
//!
//! # Batching
//!
//! One query lists up to [`MAX_PARENTS_PER_QUERY`] folders, and every result is routed by its
//! `parents` field to the folders of the batch it names; a file with several parents in the
//! batch is emitted under each, as one query per folder would return it. The alternative, one
//! query per parent whenever a file could have several parents, would give up batching
//! entirely, since any file could. A query only combines folders with the same corpus and
//! distinct ids: a folder reached twice (a legacy multi-parent folder) waits for the next
//! batch. Fallbacks:
//! - a result without `parents` in a batch of several folders cannot be placed, so the batch is
//!   split and each folder is listed by a query of its own, skipping the ids already emitted;
//! - an error of a batch of several folders cannot be attributed, so the batch is split the
//!   same way, and only the folder that fails again yields a [`FolderError`]. Transient errors
//!   never get that far: every page request is retried per the hub's retry policy and throttle
//!   (`retry_read`, which also repeats a page whose reply broke off), so a split is only for
//!   errors that remain.
//!
//! Batches are filled greedily, up to the limit, rather than spread over free slots: it sends
//! the fewest requests (each `files.list` costs quota), and the latency of a walk is bound by
//! the depth of the tree anyway. Limits: the pages of one query come one after the other, so a
//! batch whose folders hold many thousand children is paged in a single slot; splitting costs
//! one extra request per folder of the failed batch; a small tree uses one slot per level.
//!
//! Shared drives: a root with a `driveId` is searched in that drive's corpus (`corpora=drive`,
//! `driveId`). Its subfolders inherit the corpus; a followed shortcut's target is searched in
//! the user's corpus, because it may be in another drive.
//!
//! Resource keys: every queued folder keeps its `resourceKey` (for a followed shortcut, the
//! `targetResourceKey`; for the root, [`WalkRoot::resource_key`]), and every page request of a
//! batch with keys carries them in the [`RESOURCE_KEYS_HEADER`]. google-drive3 cannot add a
//! header, so those requests are built on the hub's client and credentials with the same
//! parameters and error mapping; requests without keys go through google-drive3.
//!
//! # Consumers
//!
//! The recursive download (`files::download::recursive`, through `common::file_tree_drive`)
//! consumes the walk while the scheduler ([`super::scheduler`]) transfers: local names are
//! assigned per folder on [`WalkItem::FolderDone`], a [`FolderError`] is a failed outcome that
//! cancels only that folder's subtree (unknown, not empty), an [`IncompleteSearch`] is a
//! warning and a non-zero exit, and the walk lists with its own `listing_jobs` while the
//! scheduler's bounded queue paces it.
//!
//! Still to do:
//!
//! - A file with a `resourceKey` (shared by link only) must be downloaded with the
//!   [`RESOURCE_KEYS_HEADER`] naming it, as the walk does for folders. Add
//!   `capabilities/canDownload` to [`FIELDS`] when a transfer needs it. Skip and update modes
//!   need `modifiedTime`.
//! - Shortcuts to files are not resolved: the shortcut entry carries `shortcutDetails` (target
//!   id, MIME type and resource key); a consumer that follows shortcuts gets the target's
//!   metadata with `files.get` and the same fields, within the listing limit.
//! - A [`FolderError`] or an [`IncompleteSearch`] must suppress any deletion or sync phase for
//!   those folders and their ancestors.

#[cfg(test)]
mod tests;

use std::{
    collections::{HashSet, VecDeque},
    error,
    fmt::{self, Display, Formatter},
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use google_apis_common::{to_body, to_bytes};
use google_drive3::api::{File, FileList, Scope};
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH, USER_AGENT};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    common::{
        drive_file,
        retry::{retry_read, ApiDelegate},
    },
    files::list::ListQuery,
    hub::Hub,
};

/// The largest page `files.list` returns.
pub(crate) const PAGE_SIZE: i32 = 1000;

/// The most folders OR-ed into one `files.list` query. About 2.5 KB of `q` with Drive ids.
pub(crate) const MAX_PARENTS_PER_QUERY: usize = 50;

/// The `fields` mask of every list request: what the recursive download, the scheduler and a
/// migration read (`parents` routes the results of a batched query, `driveId` tells shared-drive
/// items apart, `resourceKey` opens link-shared folders and files, `createdTime` is what a
/// migrated file gets as its own).
pub(crate) const FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,size,\
    md5Checksum,modifiedTime,createdTime,parents,driveId,trashed,resourceKey,\
    shortcutDetails(targetId,targetMimeType,targetResourceKey))";

/// The header that names the resource keys of link-shared items a request reads: comma-separated
/// `<id>/<resourceKey>` pairs.
pub(crate) const RESOURCE_KEYS_HEADER: &str = "X-Goog-Drive-Resource-Keys";

/// The MIME type prefix of Google Workspace types (Docs, Sheets, folders, shortcuts, ...).
const GOOGLE_APPS_MIME_PREFIX: &str = "application/vnd.google-apps.";

/// How a walk runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalkOptions {
    /// The most `files.list` requests in flight at once; values below 1 mean 1.
    pub(crate) listing_jobs: usize,
    /// How many items the walk buffers ahead of the consumer before it stops listing; values
    /// below 1 mean 1.
    pub(crate) buffer: usize,
    /// The most folders listed by one query, 1 to [`MAX_PARENTS_PER_QUERY`] (values outside are
    /// clamped). 1 lists every folder with its own query.
    pub(crate) max_parents_per_query: usize,
    /// List the target folder of a shortcut under the shortcut's path. Shortcuts to files are
    /// never resolved by the walk.
    pub(crate) follow_shortcuts: bool,
}

impl WalkOptions {
    /// Two listings at once, as the plan's starting point for the scheduler.
    pub(crate) const DEFAULT_LISTING_JOBS: usize = 2;
    /// About one full page.
    pub(crate) const DEFAULT_BUFFER: usize = 1000;

    fn normalized(self) -> Self {
        Self {
            listing_jobs: self.listing_jobs.max(1),
            buffer: self.buffer.max(1),
            max_parents_per_query: self.max_parents_per_query.clamp(1, MAX_PARENTS_PER_QUERY),
            follow_shortcuts: self.follow_shortcuts,
        }
    }
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            listing_jobs: Self::DEFAULT_LISTING_JOBS,
            buffer: Self::DEFAULT_BUFFER,
            max_parents_per_query: MAX_PARENTS_PER_QUERY,
            follow_shortcuts: false,
        }
    }
}

/// The folder a walk starts from. The root itself is not emitted; its children have depth 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalkRoot {
    pub(crate) id: String,
    /// The root's Drive name, reported in errors about the root.
    pub(crate) name: String,
    /// The shared drive the root is in (the root's `driveId`). The subtree is then searched in
    /// that drive's corpus (`corpora=drive`, `driveId`) instead of the user's.
    pub(crate) drive_id: Option<String>,
    /// The root's `resourceKey`, needed to list a folder that is shared by link only.
    pub(crate) resource_key: Option<String>,
}

/// Identifies one listed folder location in one walk. A folder reached on two paths (a shortcut
/// and its real place) gets two keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FolderKey(u64);

impl FolderKey {
    /// The key of the walk's root folder.
    pub(crate) const ROOT: FolderKey = FolderKey(0);

    /// Key number `key`, for tests that build walk items by hand.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_tests(key: u64) -> Self {
        FolderKey(key)
    }
}

/// What a listed Drive file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Folder,
    /// Content stored as bytes, downloadable with `alt=media`.
    Blob,
    /// A Google Workspace document (Docs, Sheets, Slides, ...): only exportable.
    Native,
    Shortcut,
}

impl EntryKind {
    /// Classifies a file by its MIME type. A file without a MIME type counts as a blob.
    #[must_use]
    pub(crate) fn of(file: &File) -> Self {
        if drive_file::is_directory(file) {
            Self::Folder
        } else if drive_file::is_shortcut(file) {
            Self::Shortcut
        } else if file
            .mime_type
            .as_deref()
            .is_some_and(|mime| mime.starts_with(GOOGLE_APPS_MIME_PREFIX))
        {
            Self::Native
        } else {
            Self::Blob
        }
    }
}

/// Whether the walk lists the children of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Children {
    /// Nothing is listed: a blob, a native document, a shortcut that is not followed (following
    /// is off, or the target is not a folder) or a folder without an id.
    None,
    /// The folder, or the target folder of a followed shortcut, is listed: every child entry has
    /// `parent` set to this key.
    Listed(FolderKey),
    /// Not listed, so that a cycle cannot repeat: a folder that is its own ancestor, or a
    /// shortcut whose target folder was already listed or queued in this walk.
    Cycle,
}

/// One file found by the walk.
#[derive(Debug, Clone)]
pub(crate) struct WalkEntry {
    /// The raw Drive names from the root's child down to this file; the root is not included.
    pub(crate) components: Vec<String>,
    /// The file as `files.list` returned it, with the [`FIELDS`] mask. Boxed: a `File` is about
    /// 2 KB, most of it fields the mask leaves empty.
    pub(crate) file: Box<File>,
    /// `components.len()`: 1 for a child of the root.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the recursive download orders by listing, not by depth"
        )
    )]
    pub(crate) depth: usize,
    pub(crate) kind: EntryKind,
    /// The folder location whose listing returned this file.
    pub(crate) parent: FolderKey,
    pub(crate) children: Children,
}

/// A folder location the walk lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FolderRef {
    pub(crate) key: FolderKey,
    /// The Drive id that is listed: for a followed shortcut, the target's id.
    pub(crate) id: String,
    /// The Drive name (for a followed shortcut, the shortcut's name).
    pub(crate) name: String,
    /// As in [`WalkEntry::components`]; empty for the root.
    pub(crate) components: Vec<String>,
}

impl FolderRef {
    /// The components joined with `/`, or the name for the root. For messages only: Drive names
    /// may contain `/`.
    #[must_use]
    pub(crate) fn display_path(&self) -> String {
        if self.components.is_empty() {
            self.name.clone()
        } else {
            self.components.join("/")
        }
    }
}

/// A folder that could not be listed. The walk continues with the other folders.
#[derive(Debug)]
pub(crate) struct FolderError {
    pub(crate) folder: FolderRef,
    pub(crate) error: Box<google_drive3::Error>,
}

impl Display for FolderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unable to list folder '{}' with id '{}'",
            self.folder.display_path(),
            self.folder.id
        )
    }
}

impl error::Error for FolderError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Drive answered a query with `incompleteSearch`: some children of these folders may be missing
/// from the walk. Emitted before the entries of that page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncompleteSearch {
    pub(crate) folders: Vec<FolderRef>,
}

/// One item of a [`Walk`].
#[derive(Debug)]
pub(crate) enum WalkItem {
    Entry(WalkEntry),
    /// Every child of this folder location has been emitted: its listing is complete. Sent
    /// once per listed folder, the root included, unless the folder ends with a
    /// [`FolderError`] instead.
    FolderDone(FolderKey),
    FolderError(FolderError),
    IncompleteSearch(IncompleteSearch),
}

/// A running walk. Dropping it stops the walk: the producer task is aborted and no further
/// request is sent.
pub(crate) struct Walk {
    items: mpsc::Receiver<WalkItem>,
    /// `None` once the producer finished and was awaited.
    producer: Option<JoinHandle<()>>,
}

impl Walk {
    /// Starts walking the subtree of `root` on the current Tokio runtime.
    #[must_use]
    pub(crate) fn start(hub: Arc<Hub>, root: WalkRoot, options: WalkOptions) -> Walk {
        Self::start_with_fields(hub, root, options, FIELDS)
    }

    /// [`Self::start`] with the `fields` mask `fields` instead of [`FIELDS`], for a consumer
    /// that reads more of each file. It must name every field of [`FIELDS`].
    #[must_use]
    pub(crate) fn start_with_fields(
        hub: Arc<Hub>,
        root: WalkRoot,
        options: WalkOptions,
        fields: &'static str,
    ) -> Walk {
        let options = options.normalized();
        let (sender, items) = mpsc::channel(options.buffer);

        let WalkRoot {
            id,
            name,
            drive_id,
            resource_key,
        } = root;
        let root = PendingFolder {
            folder: FolderRef {
                key: FolderKey::ROOT,
                id: id.clone(),
                name,
                components: Vec::new(),
            },
            corpus: drive_id.map_or(Corpus::User, Corpus::Drive),
            resource_key,
            ancestors: Arc::new(Ancestors {
                id: id.clone(),
                parent: None,
            }),
            emitted: HashSet::new(),
            alone: false,
        };

        let producer = Producer {
            hub,
            options,
            fields,
            sender,
            pending: VecDeque::from([root]),
            visited: HashSet::from([id]),
            last_key: FolderKey::ROOT,
            in_flight: FuturesUnordered::new(),
        };

        Walk {
            items,
            producer: Some(tokio::spawn(producer.run())),
        }
    }
}

impl Stream for Walk {
    type Item = WalkItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<WalkItem>> {
        let this = &mut *self;

        match this.items.poll_recv(cx) {
            Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => {}
        }

        // The channel is closed and drained: the producer returned or panicked.
        let Some(producer) = this.producer.as_mut() else {
            return Poll::Ready(None);
        };
        let Poll::Ready(result) = Pin::new(producer).poll(cx) else {
            return Poll::Pending;
        };
        this.producer = None;
        match result {
            // A panic in the producer is a bug; it must not look like a complete walk.
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Ok(()) | Err(_) => Poll::Ready(None),
        }
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        if let Some(producer) = &self.producer {
            producer.abort();
        }
    }
}

/// The corpus a folder's query searches.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Corpus {
    /// No `corpora`: the default `user` corpus, with items from all drives.
    User,
    /// `corpora=drive` and this `driveId`.
    Drive(String),
}

/// The ids of a folder and its ancestors, to find a folder that is its own ancestor.
#[derive(Debug)]
struct Ancestors {
    id: String,
    parent: Option<Arc<Ancestors>>,
}

impl Ancestors {
    fn contains(&self, id: &str) -> bool {
        let mut current = Some(self);
        while let Some(ancestors) = current {
            if ancestors.id == id {
                return true;
            }
            current = ancestors.parent.as_deref();
        }
        false
    }
}

/// A folder location that is waiting to be listed.
#[derive(Debug)]
struct PendingFolder {
    folder: FolderRef,
    corpus: Corpus,
    /// The folder's `resourceKey` (for a followed shortcut, the target's), sent with every page
    /// request of the folder.
    resource_key: Option<String>,
    /// This folder's id and its ancestors' ids.
    ancestors: Arc<Ancestors>,
    /// Ids already emitted under this folder, by a batch that was split.
    emitted: HashSet<String>,
    /// List this folder with a query of its own: the batch it was in failed or could not be
    /// routed.
    alone: bool,
}

/// The folders listed by one query, and the state of their pagination.
#[derive(Debug)]
struct Batch {
    folders: Vec<PendingFolder>,
    /// Per folder: the ids emitted under it so far, by this batch and by the batch it was split
    /// from, so that an id repeated on a later page or after a split is not emitted again.
    emitted: Vec<HashSet<String>>,
    q: String,
}

impl Batch {
    fn new(mut folders: Vec<PendingFolder>) -> Self {
        let q = parents_query(folders.iter().map(|folder| folder.folder.id.as_str()));
        let emitted = folders
            .iter_mut()
            .map(|folder| std::mem::take(&mut folder.emitted))
            .collect();
        Self {
            folders,
            emitted,
            q,
        }
    }

    /// The value of [`RESOURCE_KEYS_HEADER`] for the folders of this batch that have a resource
    /// key, if any has one.
    fn resource_keys(&self) -> Option<String> {
        let keys = self
            .folders
            .iter()
            .filter_map(|folder| {
                let key = folder.resource_key.as_deref()?;
                Some(format!("{}/{key}", folder.folder.id))
            })
            .collect::<Vec<_>>();
        (!keys.is_empty()).then(|| keys.join(","))
    }

    fn corpus(&self) -> &Corpus {
        // Every batch has at least one folder.
        self.folders
            .first()
            .map_or(&Corpus::User, |folder| &folder.corpus)
    }

    /// Whether `file` is new under folder `index`: not emitted before, on an earlier page (Drive
    /// can repeat an id across pages when the folder changes during the listing) or by the batch
    /// this one was split from. Records it as emitted. A file without an id is always new.
    fn newly_emitted(&mut self, index: usize, file: &File) -> bool {
        match (&file.id, self.emitted.get_mut(index)) {
            (Some(id), Some(emitted)) => emitted.insert(id.clone()),
            (None, _) | (_, None) => true,
        }
    }

    /// The folders of this batch a listed file belongs to. With one folder that is the folder;
    /// otherwise the folders named in the file's `parents` (several for a file with more than
    /// one parent in the batch, none if `parents` is missing).
    fn route(&self, file: &File) -> Vec<usize> {
        if self.folders.len() == 1 {
            return vec![0];
        }
        let Some(parents) = &file.parents else {
            return Vec::new();
        };
        self.folders
            .iter()
            .enumerate()
            .filter(|(_, folder)| parents.contains(&folder.folder.id))
            .map(|(index, _)| index)
            .collect()
    }
}

/// The `q` of a batch: `('a' in parents or 'b' in parents) and trashed = false`. One folder is
/// listed with the query of `files list --parent`: `'a' in parents and trashed = false`.
fn parents_query<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    children_query(ids, false)
}

/// The `q` that lists the children of the folders `ids` that are in the trash (`trashed`) or
/// not: `('a' in parents or 'b' in parents) and trashed = <trashed>`, for one folder
/// `'a' in parents and trashed = <trashed>` (without the trash, the query of
/// `files list --parent`).
pub(crate) fn children_query<'a>(ids: impl Iterator<Item = &'a str>, trashed: bool) -> String {
    let ids = ids.collect::<Vec<_>>();
    if let [id] = ids.as_slice() {
        if !trashed {
            return ListQuery::files_in_folder((*id).to_owned()).q();
        }
        return format!("{} in parents and trashed = true", quote(id));
    }
    let parents = ids
        .iter()
        .map(|id| format!("{} in parents", quote(id)))
        .collect::<Vec<_>>();
    format!("({}) and trashed = {trashed}", parents.join(" or "))
}

/// Quotes a value as a string literal of the Drive query language: `'` and `\` are escaped
/// with a backslash, as `files::list` does for `--parent`.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if matches!(c, '\'' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// One answered page request.
struct Page {
    batch: Batch,
    result: Result<FileList, Box<google_drive3::Error>>,
}

/// The consumer dropped the walk.
struct Closed;

/// The task behind a [`Walk`]: it owns the queue of folders to list and the page requests in
/// flight, and sends items to the channel.
struct Producer {
    hub: Arc<Hub>,
    options: WalkOptions,
    /// The `fields` mask of every request.
    fields: &'static str,
    sender: mpsc::Sender<WalkItem>,
    /// Folders waiting to be listed, in breadth-first order (split batches go to the front).
    pending: VecDeque<PendingFolder>,
    /// Every folder id listed or queued, to decide whether to follow a shortcut.
    visited: HashSet<String>,
    last_key: FolderKey,
    /// At most `options.listing_jobs` page requests.
    in_flight: FuturesUnordered<BoxFuture<'static, Page>>,
}

impl Producer {
    async fn run(mut self) {
        loop {
            self.start_requests();
            let Some(page) = self.in_flight.next().await else {
                // Nothing in flight and nothing pending: the walk is complete.
                return;
            };
            if self.handle_page(page).await.is_err() {
                return;
            }
        }
    }

    fn start_requests(&mut self) {
        while self.in_flight.len() < self.options.listing_jobs {
            let Some(batch) = self.next_batch() else {
                break;
            };
            self.request(batch, None);
        }
    }

    /// Takes the next folders from the front of the queue: up to `max_parents_per_query` with
    /// the same corpus and distinct ids, or one folder that must be listed alone.
    fn next_batch(&mut self) -> Option<Batch> {
        let first = self.pending.pop_front()?;
        let alone = first.alone;
        let mut ids = HashSet::from([first.folder.id.clone()]);
        let mut folders = vec![first];

        while !alone && folders.len() < self.options.max_parents_per_query {
            let Some(next) = self.pending.front() else {
                break;
            };
            if next.alone || next.corpus != folders[0].corpus || ids.contains(&next.folder.id) {
                break;
            }
            ids.insert(next.folder.id.clone());
            folders.extend(self.pending.pop_front());
        }

        Some(Batch::new(folders))
    }

    fn request(&mut self, batch: Batch, page_token: Option<String>) {
        let hub = Arc::clone(&self.hub);
        let fields = self.fields;
        self.in_flight.push(Box::pin(async move {
            let result = list_page(&hub, &batch, fields, page_token.as_deref()).await;
            Page { batch, result }
        }));
    }

    async fn handle_page(&mut self, page: Page) -> Result<(), Closed> {
        let Page { mut batch, result } = page;
        let list = match result {
            Ok(list) => list,
            Err(error) => return self.batch_failed(batch, error).await,
        };

        if list.incomplete_search == Some(true) {
            let folders = batch
                .folders
                .iter()
                .map(|folder| folder.folder.clone())
                .collect();
            self.send(WalkItem::IncompleteSearch(IncompleteSearch { folders }))
                .await?;
        }

        let mut unroutable = false;
        for file in list.files.unwrap_or_default() {
            // The query excludes trashed files; never emit one if a server returns it anyway.
            if file.trashed == Some(true) {
                continue;
            }
            let targets = batch.route(&file);
            if targets.is_empty() {
                unroutable = true;
                continue;
            }
            let targets = targets
                .into_iter()
                .filter(|&index| batch.newly_emitted(index, &file))
                .collect::<Vec<_>>();
            let mut file = Some(file);
            for (n, &index) in targets.iter().enumerate() {
                let file = if n + 1 == targets.len() {
                    file.take()
                } else {
                    file.clone()
                };
                let Some(file) = file else {
                    break;
                };
                let entry = self.entry(&batch.folders[index], file);
                self.send(WalkItem::Entry(entry)).await?;
            }
        }

        if unroutable {
            // A result without `parents` cannot be placed: list each folder on its own.
            self.split(batch);
        } else if let Some(token) = list.next_page_token {
            self.request(batch, Some(token));
        } else {
            for folder in &batch.folders {
                self.send(WalkItem::FolderDone(folder.folder.key)).await?;
            }
        }
        Ok(())
    }

    /// A batch of several folders is split into one query per folder, because the error cannot
    /// be attributed to one of them; a single folder is reported.
    async fn batch_failed(
        &mut self,
        batch: Batch,
        error: Box<google_drive3::Error>,
    ) -> Result<(), Closed> {
        if batch.folders.len() > 1 {
            self.split(batch);
            return Ok(());
        }
        if let Some(folder) = batch.folders.into_iter().next() {
            self.send(WalkItem::FolderError(FolderError {
                folder: folder.folder,
                error,
            }))
            .await?;
        }
        Ok(())
    }

    /// Queues every folder of `batch` at the front, to be listed alone, skipping what the batch
    /// already emitted under it.
    fn split(&mut self, batch: Batch) {
        let Batch {
            folders, emitted, ..
        } = batch;
        let folders = folders.into_iter().zip(emitted).collect::<Vec<_>>();
        for (mut folder, emitted) in folders.into_iter().rev() {
            folder.emitted = emitted;
            folder.alone = true;
            self.pending.push_front(folder);
        }
    }

    /// Builds the entry for a file listed in `parent`, queueing it for listing if it is a folder
    /// or a followed shortcut.
    fn entry(&mut self, parent: &PendingFolder, file: File) -> WalkEntry {
        let name = file.name.clone().unwrap_or_default();
        let mut components = parent.folder.components.clone();
        components.push(name);

        let kind = EntryKind::of(&file);
        let children = match kind {
            EntryKind::Folder => self.queue_folder(parent, &file, &components),
            EntryKind::Shortcut => self.queue_shortcut_target(parent, &file, &components),
            EntryKind::Blob | EntryKind::Native => Children::None,
        };

        WalkEntry {
            depth: components.len(),
            components,
            file: Box::new(file),
            kind,
            parent: parent.folder.key,
            children,
        }
    }

    fn queue_folder(
        &mut self,
        parent: &PendingFolder,
        file: &File,
        components: &[String],
    ) -> Children {
        let Some(id) = &file.id else {
            return Children::None;
        };
        if parent.ancestors.contains(id) {
            return Children::Cycle;
        }
        self.visited.insert(id.clone());
        let corpus = parent.corpus.clone();
        let resource_key = file.resource_key.clone();
        Children::Listed(self.queue(parent, id, resource_key, components, corpus))
    }

    fn queue_shortcut_target(
        &mut self,
        parent: &PendingFolder,
        file: &File,
        components: &[String],
    ) -> Children {
        if !self.options.follow_shortcuts {
            return Children::None;
        }
        let Some(details) = &file.shortcut_details else {
            return Children::None;
        };
        let Some(target_id) = &details.target_id else {
            return Children::None;
        };
        if details.target_mime_type.as_deref() != Some(drive_file::MIME_TYPE_DRIVE_FOLDER) {
            return Children::None;
        }
        if self.visited.contains(target_id) || parent.ancestors.contains(target_id) {
            return Children::Cycle;
        }
        self.visited.insert(target_id.clone());
        // The target may be in another drive than the shortcut, so search the user's corpus.
        let resource_key = details.target_resource_key.clone();
        Children::Listed(self.queue(parent, target_id, resource_key, components, Corpus::User))
    }

    fn queue(
        &mut self,
        parent: &PendingFolder,
        id: &str,
        resource_key: Option<String>,
        components: &[String],
        corpus: Corpus,
    ) -> FolderKey {
        self.last_key = FolderKey(self.last_key.0 + 1);
        let key = self.last_key;
        self.pending.push_back(PendingFolder {
            folder: FolderRef {
                key,
                id: id.to_owned(),
                name: components.last().cloned().unwrap_or_default(),
                components: components.to_vec(),
            },
            corpus,
            resource_key,
            ancestors: Arc::new(Ancestors {
                id: id.to_owned(),
                parent: Some(Arc::clone(&parent.ancestors)),
            }),
            emitted: HashSet::new(),
            alone: false,
        });
        key
    }

    /// Waits for room in the channel: this is the walk's backpressure.
    async fn send(&mut self, item: WalkItem) -> Result<(), Closed> {
        self.sender.send(item).await.map_err(|_| Closed)
    }
}

/// Requests one page of a batch's query with the `fields` mask `fields`, through either request
/// path, retried per the hub's policy and sharing its throttle: a rate limit seen by one listing
/// slot holds back the others and the transfers of the same hub.
async fn list_page(
    hub: &Hub,
    batch: &Batch,
    fields: &str,
    page_token: Option<&str>,
) -> Result<FileList, Box<google_drive3::Error>> {
    let resource_keys = batch.resource_keys();
    let drive_id = match batch.corpus() {
        Corpus::User => None,
        Corpus::Drive(drive_id) => Some(drive_id.as_str()),
    };
    let query = PageQuery {
        q: &batch.q,
        fields,
        drive_id,
        resource_keys: resource_keys.as_deref(),
    };
    request_page(hub, &query, page_token).await
}

/// One `files.list` request as the walk sends it: `pageSize` 1000, items of all drives, and the
/// [`RESOURCE_KEYS_HEADER`] when folders shared by link only are named.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageQuery<'a> {
    pub(crate) q: &'a str,
    pub(crate) fields: &'a str,
    /// The shared drive searched (`corpora=drive`, `driveId`); `None`: the user's corpus.
    pub(crate) drive_id: Option<&'a str>,
    /// The value of the [`RESOURCE_KEYS_HEADER`], if any folder needs one.
    pub(crate) resource_keys: Option<&'a str>,
}

/// Requests one page of `query`, retried per the hub's policy and sharing its throttle, through
/// google-drive3 or, with resource keys, through a request of its own.
pub(crate) async fn request_page(
    hub: &Hub,
    query: &PageQuery<'_>,
    page_token: Option<&str>,
) -> Result<FileList, Box<google_drive3::Error>> {
    retry_read(hub.retrier(), "files.list", |mut delegate| async move {
        let page = match query.resource_keys {
            None => list_page_call(hub, query, page_token, &mut delegate).await,
            Some(resource_keys) => {
                list_page_with_resource_keys(hub, query, page_token, resource_keys, &delegate).await
            }
        };
        page.map_err(|error| *error)
    })
    .await
    .map_err(Box::new)
}

/// The page request through google-drive3's `files.list` call.
async fn list_page_call(
    hub: &Hub,
    query: &PageQuery<'_>,
    page_token: Option<&str>,
    delegate: &mut ApiDelegate,
) -> Result<FileList, Box<google_drive3::Error>> {
    let mut call = hub
        .files()
        .list()
        .q(query.q)
        .page_size(PAGE_SIZE)
        .supports_all_drives(true)
        .include_items_from_all_drives(true)
        .param("fields", query.fields)
        .add_scope(Scope::Full);
    if let Some(token) = page_token {
        call = call.page_token(token);
    }
    if let Some(drive_id) = query.drive_id {
        call = call.corpora("drive").drive_id(drive_id);
    }

    let (_, list) = call.delegate(delegate).doit().await.map_err(Box::new)?;
    Ok(list)
}

/// The same request as [`list_page_call`] with the [`RESOURCE_KEYS_HEADER`], which google-drive3
/// cannot send: built on the hub's client and credentials, with google-drive3's error mapping.
/// A failed reply's status and headers go to `delegate`, as google-drive3 hands them to its
/// delegate, so that the retry policy sees its `Retry-After`.
async fn list_page_with_resource_keys(
    hub: &Hub,
    query: &PageQuery<'_>,
    page_token: Option<&str>,
    resource_keys: &str,
    delegate: &ApiDelegate,
) -> Result<FileList, Box<google_drive3::Error>> {
    use google_drive3::Error;

    let page_size = PAGE_SIZE.to_string();
    let mut params = vec![
        ("q", query.q),
        ("pageSize", page_size.as_str()),
        ("supportsAllDrives", "true"),
        ("includeItemsFromAllDrives", "true"),
        ("fields", query.fields),
        ("alt", "json"),
    ];
    if let Some(token) = page_token {
        params.push(("pageToken", token));
    }
    if let Some(drive_id) = query.drive_id {
        params.push(("corpora", "drive"));
        params.push(("driveId", drive_id));
    }
    let url = url::Url::parse_with_params(&format!("{}files", hub.drive_base_url()), &params)
        .map_err(|err| Error::Io(io::Error::other(err)))?;

    let token = hub
        .auth
        .get_token(&[Scope::Full.as_ref()])
        .await
        .map_err(Error::MissingToken)?;
    let mut request = hyper::Request::get(url.as_str())
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header(RESOURCE_KEYS_HEADER, resource_keys)
        .header(CONTENT_LENGTH, 0_u64);
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = request
        .body(to_body::<String>(None))
        .map_err(|err| Error::Io(io::Error::other(err)))?;

    let response = hub
        .client
        .request(request)
        .await
        .map_err(Error::HttpError)?;
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body).await.unwrap_or_default();
    if !parts.status.is_success() {
        delegate.record_failure(parts.status, &parts.headers);
        return Err(Box::new(match serde_json::from_slice(&bytes) {
            Ok(value) => Error::BadRequest(value),
            Err(_) => Error::Failure(hyper::Response::from_parts(parts, to_body(Some(bytes)))),
        }));
    }
    serde_json::from_slice(&bytes).map_err(|err| {
        Box::new(Error::JsonDecodeError(
            String::from_utf8_lossy(&bytes).into_owned(),
            err,
        ))
    })
}

/// The `User-Agent` of requests the walk builds itself.
const USER_AGENT_VALUE: &str = concat!("gdrive/", env!("CARGO_PKG_VERSION"));
