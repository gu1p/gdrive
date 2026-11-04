//! Incremental, breadth-first listing of a Drive folder subtree, exposed as a [`Stream`] of
//! [`WalkItem`]s that a consumer can act on while the walk is still running.

#[cfg(test)]
mod tests;

use std::{
    collections::{HashSet, VecDeque},
    error,
    fmt::{self, Display, Formatter},
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use google_drive3::api::{File, FileList, Scope};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{common::drive_file, hub::Hub};

/// The largest page `files.list` returns.
pub(crate) const PAGE_SIZE: i32 = 1000;

/// The most folders OR-ed into one `files.list` query. About 2.5 KB of `q` with Drive ids.
pub(crate) const MAX_PARENTS_PER_QUERY: usize = 50;

/// The `fields` mask of every list request: what the recursive download and the scheduler read
/// (`parents` routes the results of a batched query, `driveId` tells shared-drive items apart).
pub(crate) const FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,size,\
    md5Checksum,modifiedTime,parents,driveId,trashed,\
    shortcutDetails(targetId,targetMimeType,targetResourceKey))";

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
}

/// Identifies one listed folder location in one walk. A folder reached on two paths (a shortcut
/// and its real place) gets two keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FolderKey(u64);

impl FolderKey {
    /// The key of the walk's root folder.
    pub(crate) const ROOT: FolderKey = FolderKey(0);
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
        let options = options.normalized();
        let (sender, items) = mpsc::channel(options.buffer);

        let WalkRoot { id, name, drive_id } = root;
        let root = PendingFolder {
            folder: FolderRef {
                key: FolderKey::ROOT,
                id: id.clone(),
                name,
                components: Vec::new(),
            },
            corpus: drive_id.map_or(Corpus::User, Corpus::Drive),
            ancestors: Arc::new(Ancestors {
                id: id.clone(),
                parent: None,
            }),
            skip: HashSet::new(),
            alone: false,
        };

        let producer = Producer {
            hub,
            options,
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
    /// This folder's id and its ancestors' ids.
    ancestors: Arc<Ancestors>,
    /// Ids already emitted under this folder by a batch that was split.
    skip: HashSet<String>,
    /// List this folder with a query of its own: the batch it was in failed or could not be
    /// routed.
    alone: bool,
}

/// The folders listed by one query, and the state of their pagination.
#[derive(Debug)]
struct Batch {
    folders: Vec<PendingFolder>,
    /// Per folder: the ids emitted under it so far, kept for batches of several folders so that
    /// a split does not emit them again.
    emitted: Vec<HashSet<String>>,
    q: String,
}

impl Batch {
    fn new(folders: Vec<PendingFolder>) -> Self {
        let q = parents_query(folders.iter().map(|folder| folder.folder.id.as_str()));
        let emitted = if folders.len() > 1 {
            folders.iter().map(|_| HashSet::new()).collect()
        } else {
            Vec::new()
        };
        Self {
            folders,
            emitted,
            q,
        }
    }

    fn corpus(&self) -> &Corpus {
        // Every batch has at least one folder.
        self.folders
            .first()
            .map_or(&Corpus::User, |folder| &folder.corpus)
    }

    /// Whether `file` is new under folder `index`: not emitted by an earlier, split batch. Records
    /// it as emitted.
    fn newly_emitted(&mut self, index: usize, file: &File) -> bool {
        let Some(id) = &file.id else {
            return true;
        };
        if self.folders[index].skip.contains(id) {
            return false;
        }
        if let Some(emitted) = self.emitted.get_mut(index) {
            emitted.insert(id.clone());
        }
        true
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

/// The `q` of a batch: `('a' in parents or 'b' in parents) and trashed = false`.
fn parents_query<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let parents = ids
        .map(|id| format!("{} in parents", quote(id)))
        .collect::<Vec<_>>();
    if let [parent] = parents.as_slice() {
        format!("{parent} and trashed = false")
    } else {
        format!("({}) and trashed = false", parents.join(" or "))
    }
}

/// Quotes a value as a string literal of the Drive query language: `'` and `\` are escaped
/// with a backslash.
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
        self.in_flight.push(Box::pin(async move {
            let result = list_page(&hub, &batch, page_token.as_deref()).await;
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
        let emitted = emitted
            .into_iter()
            .chain(std::iter::repeat_with(HashSet::new));
        let folders = folders.into_iter().zip(emitted).collect::<Vec<_>>();
        for (mut folder, emitted) in folders.into_iter().rev() {
            folder.skip.extend(emitted);
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
        Children::Listed(self.queue(parent, id, components, corpus))
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
        Children::Listed(self.queue(parent, target_id, components, Corpus::User))
    }

    fn queue(
        &mut self,
        parent: &PendingFolder,
        id: &str,
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
            ancestors: Arc::new(Ancestors {
                id: id.to_owned(),
                parent: Some(Arc::clone(&parent.ancestors)),
            }),
            skip: HashSet::new(),
            alone: false,
        });
        key
    }

    /// Waits for room in the channel: this is the walk's backpressure.
    async fn send(&mut self, item: WalkItem) -> Result<(), Closed> {
        self.sender.send(item).await.map_err(|_| Closed)
    }
}

/// Requests one page of a batch's query.
async fn list_page(
    hub: &Hub,
    batch: &Batch,
    page_token: Option<&str>,
) -> Result<FileList, Box<google_drive3::Error>> {
    let mut call = hub
        .files()
        .list()
        .q(&batch.q)
        .page_size(PAGE_SIZE)
        .supports_all_drives(true)
        .include_items_from_all_drives(true)
        .param("fields", FIELDS)
        .add_scope(Scope::Full);
    if let Some(token) = page_token {
        call = call.page_token(token);
    }
    if let Corpus::Drive(drive_id) = batch.corpus() {
        call = call.corpora("drive").drive_id(drive_id);
    }

    // TODO(retry): wrap with retry_api once merged
    let (_, list) = call.doit().await.map_err(Box::new)?;
    Ok(list)
}
