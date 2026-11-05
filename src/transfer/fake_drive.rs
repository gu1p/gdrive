//! A fake Drive `files.list` endpoint for walk tests, with media downloads for the recursive
//! download.
//!
//! Unlike `common::mock_server`, which answers requests in order with canned responses, this
//! server holds a set of files and answers every query from them, so it works however many
//! requests are in flight and in whatever order they arrive. It understands the queries the walk
//! sends: `'<id>' in parents` terms joined by `or`, and `trashed = false`. It also records every
//! request and the most requests it had in flight at once. `files.get` of a stored file returns
//! the fields its `fields` mask names, so a test can check what a metadata request asks for;
//! with `alt=media` it returns the content of a blob ([`FakeFile::blob`]), counted apart from
//! the other requests, with a strong `ETag` (the md5 of the content), and with
//! `Range: bytes=<first>-` the rest of it from that byte (206, or 416 past the end) unless an
//! `If-Range` names another `ETag` ([`FakeDrive::media_ranges`]). `files.export` of a Workspace
//! document ([`FakeFile::native`]) returns its content in whatever format is asked for, without
//! `Content-Length` as Drive sends exports, and records the format ([`FakeDrive::exports`]).
//! `about.get` names the account ([`Behavior::account_email`]).
//!
//! For recursive uploads it answers `files.generateIds`, multipart `files.create` (the uploads
//! of small files and the creation of folders, [`Created`]) and resumable sessions (a `POST` with
//! `uploadType=resumable`, then `PUT`s of chunks at the next offset, 308 with `Range` until the
//! last). Like Drive, a create fails with 404 when a parent does not exist yet. The
//! `modifiedTime` and `createdTime` of a create's metadata are the new file's.
//!
//! For syncs it answers multipart `files.update` of a stored file's content (a `PATCH` to the
//! upload path, [`FakeDrive::updates`]), metadata `files.update` (a `PATCH` of JSON, such as
//! `{"trashed": true}`, [`FakeDrive::patches`]) and records `files.delete`
//! ([`FakeDrive::deletes`]). The stored files do not change: a listing after an update still
//! returns what the test started the fake with. Every request that is not a `GET` is also in
//! [`FakeDrive::writes`], which is empty for an account that was only read.
//!
//! For `files diverged` it lists the children of folders that are in the trash
//! (`trashed = true`), with `explicitlyTrashed`, `trashedTime` and `trashingUser`
//! ([`FakeFile::trashed_by`]), returns `lastModifyingUser` and `owners` ([`FakeFile::modified_by`],
//! [`FakeFile::owned_by`]), and answers `revisions.list` of a stored file from
//! [`Behavior::revisions`] ([`FakeDrive::revision_requests`]); [`Behavior::revisions_refused`]
//! makes it fail and [`Behavior::unreadable_trash`] fails the listings of the trash.
//!
//! For `files activity` it answers `permissions.list` of a stored file from
//! [`Behavior::permissions`] (paged with `pageSize` and `pageToken`), and the audit log of the
//! Admin SDK Reports API (`GET /admin/reports/v1/activity/users/all/applications/drive`) from
//! the records of [`Behavior::audit_log`]: newest first, within `startTime` (included, unless
//! [`Bounds::BothExcluded`]) and `endTime` (excluded), with `filters` of `name==value`
//! terms over event parameters (a record matches when one of its events has all of them), paged
//! with `maxResults` and `pageToken`. Both record their requests ([`FakeDrive::audit_requests`],
//! [`FakeDrive::permission_requests`]); [`Behavior::audit_error`],
//! [`Behavior::audit_error_after_first_page`] and [`Behavior::audit_rate_limited`] make the
//! audit log fail.
//!
//! A test that checks how many requests were in flight at once does not rely on delays: the
//! fake holds the first requests of a kind until all of them have arrived ([`Rendezvous`]), or a
//! listing until a download has started, so the overlap happens however slow the machine is.

use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use bytes::Bytes;
use futures::{stream, StreamExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::{body::Incoming, service::service_fn, Method, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Map, Value};
use tokio::{
    net::TcpListener,
    sync::{watch, Barrier},
    task::JoinHandle,
    time::Instant,
};

use crate::{common::drive_file::MIME_TYPE_DRIVE_FOLDER, hub::Hub};

const LIST_PATH: &str = "/drive/v3/files";
const ABOUT_PATH: &str = "/drive/v3/about";
const GENERATE_IDS_PATH: &str = "/drive/v3/files/generateIds";
const UPLOAD_PATH: &str = "/upload/drive/v3/files";
const SESSION_PATH: &str = "/upload-session/";
const AUDIT_PATH: &str = "/admin/reports/v1/activity/users/all/applications/drive";
const PERMISSIONS_SUFFIX: &str = "/permissions";
const REVISIONS_SUFFIX: &str = "/revisions";

/// A user as Drive names one in `lastModifyingUser`, `owners` and `trashingUser`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FakeUser {
    pub(crate) display_name: Option<String>,
    pub(crate) email: Option<String>,
}

impl FakeUser {
    /// A user with a display name and an email address.
    pub(crate) fn new(display_name: &str, email: &str) -> Self {
        Self {
            display_name: Some(display_name.to_owned()),
            email: Some(email.to_owned()),
        }
    }

    /// A user whose email address Drive does not show.
    pub(crate) fn named(display_name: &str) -> Self {
        Self {
            display_name: Some(display_name.to_owned()),
            email: None,
        }
    }

    fn to_json(&self) -> Value {
        let mut user = Map::new();
        user.insert("kind".into(), json!("drive#user"));
        if let Some(name) = &self.display_name {
            user.insert("displayName".into(), json!(name));
        }
        if let Some(email) = &self.email {
            user.insert("emailAddress".into(), json!(email));
        }
        Value::Object(user)
    }
}

/// A file stored by the fake.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeFile {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) mime_type: String,
    pub(crate) parents: Vec<String>,
    pub(crate) size: Option<u64>,
    pub(crate) md5: Option<String>,
    pub(crate) trashed: bool,
    /// `explicitlyTrashed`, sent only when set: `Some(false)` for an item in a trashed folder.
    pub(crate) explicitly_trashed: Option<bool>,
    pub(crate) drive_id: Option<String>,
    /// Target id and target MIME type of a shortcut.
    pub(crate) shortcut_target: Option<(String, String)>,
    pub(crate) target_resource_key: Option<String>,
    pub(crate) resource_key: Option<String>,
    /// What a media download of a blob, or an export of a Workspace document, returns.
    pub(crate) content: Option<String>,
    /// `modifiedTime`, RFC 3339.
    pub(crate) modified_time: Option<String>,
    /// `createdTime`, RFC 3339.
    pub(crate) created_time: Option<String>,
    /// `lastModifyingUser`, sent only when set.
    pub(crate) last_modifying_user: Option<FakeUser>,
    /// `owners`, sent only when there are some.
    pub(crate) owners: Vec<FakeUser>,
    /// `trashedTime`, RFC 3339, sent only when set.
    pub(crate) trashed_time: Option<String>,
    /// `trashingUser`, sent only when set.
    pub(crate) trashing_user: Option<FakeUser>,
}

impl FakeFile {
    pub(crate) fn folder(id: &str, name: &str, parent: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            mime_type: MIME_TYPE_DRIVE_FOLDER.to_owned(),
            parents: vec![parent.to_owned()],
            ..Self::default()
        }
    }

    /// A binary file with `content`'s size and md5.
    pub(crate) fn blob(id: &str, name: &str, parent: &str, content: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            mime_type: String::from("text/plain"),
            parents: vec![parent.to_owned()],
            size: Some(content.len() as u64),
            md5: Some(format!("{:x}", md5::compute(content))),
            content: Some(content.to_owned()),
            ..Self::default()
        }
    }

    /// A Google Docs document: no size, no md5.
    pub(crate) fn document(id: &str, name: &str, parent: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            mime_type: String::from("application/vnd.google-apps.document"),
            parents: vec![parent.to_owned()],
            ..Self::default()
        }
    }

    /// A Workspace document of `mime_type` whose exports return `content`.
    pub(crate) fn native(
        id: &str,
        name: &str,
        parent: &str,
        mime_type: &str,
        content: &str,
    ) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            mime_type: mime_type.to_owned(),
            parents: vec![parent.to_owned()],
            content: Some(content.to_owned()),
            ..Self::default()
        }
    }

    /// With `modifiedTime` (RFC 3339).
    pub(crate) fn modified(self, time: &str) -> Self {
        Self {
            modified_time: Some(time.to_owned()),
            ..self
        }
    }

    /// With `createdTime` (RFC 3339).
    pub(crate) fn created(self, time: &str) -> Self {
        Self {
            created_time: Some(time.to_owned()),
            ..self
        }
    }

    /// Last modified by `user` (`lastModifyingUser`).
    pub(crate) fn modified_by(self, user: FakeUser) -> Self {
        Self {
            last_modifying_user: Some(user),
            ..self
        }
    }

    /// Owned by `user` (`owners`).
    pub(crate) fn owned_by(self, user: FakeUser) -> Self {
        Self {
            owners: vec![user],
            ..self
        }
    }

    /// Moved to the trash by `user` (`None`: Drive does not say, as outside shared drives) at
    /// `time` (RFC 3339; `None`: Drive does not say): `trashed` and `explicitlyTrashed`.
    pub(crate) fn trashed_by(self, user: Option<FakeUser>, time: Option<&str>) -> Self {
        Self {
            trashed: true,
            explicitly_trashed: Some(true),
            trashing_user: user,
            trashed_time: time.map(str::to_owned),
            ..self
        }
    }

    pub(crate) fn shortcut(
        id: &str,
        name: &str,
        parent: &str,
        target_id: &str,
        target_mime_type: &str,
    ) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            mime_type: String::from("application/vnd.google-apps.shortcut"),
            parents: vec![parent.to_owned()],
            shortcut_target: Some((target_id.to_owned(), target_mime_type.to_owned())),
            ..Self::default()
        }
    }

    pub(crate) fn trashed(self) -> Self {
        Self {
            trashed: true,
            ..self
        }
    }

    /// In the trash because a folder above it is, as Drive reports such an item: `trashed`, not
    /// `explicitlyTrashed`.
    pub(crate) fn in_trashed_folder(self) -> Self {
        Self {
            trashed: true,
            explicitly_trashed: Some(false),
            ..self
        }
    }

    pub(crate) fn in_drive(self, drive_id: &str) -> Self {
        Self {
            drive_id: Some(drive_id.to_owned()),
            ..self
        }
    }

    pub(crate) fn with_resource_key(self, resource_key: &str) -> Self {
        Self {
            resource_key: Some(resource_key.to_owned()),
            ..self
        }
    }

    /// For a shortcut: the target's resource key.
    pub(crate) fn with_target_resource_key(self, resource_key: &str) -> Self {
        Self {
            target_resource_key: Some(resource_key.to_owned()),
            ..self
        }
    }

    pub(crate) fn with_parents(self, parents: &[&str]) -> Self {
        Self {
            parents: parents.iter().map(|&parent| parent.to_owned()).collect(),
            ..self
        }
    }

    fn to_json(&self, with_parents: bool) -> Value {
        let mut file = Map::new();
        file.insert("id".into(), json!(self.id));
        file.insert("name".into(), json!(self.name));
        file.insert("mimeType".into(), json!(self.mime_type));
        file.insert("trashed".into(), json!(self.trashed));
        if let Some(explicitly) = self.explicitly_trashed {
            file.insert("explicitlyTrashed".into(), json!(explicitly));
        }
        if with_parents {
            file.insert("parents".into(), json!(self.parents));
        }
        if let Some(size) = self.size {
            file.insert("size".into(), json!(size.to_string()));
        }
        if let Some(md5) = &self.md5 {
            file.insert("md5Checksum".into(), json!(md5));
        }
        if let Some(drive_id) = &self.drive_id {
            file.insert("driveId".into(), json!(drive_id));
        }
        if let Some(modified_time) = &self.modified_time {
            file.insert("modifiedTime".into(), json!(modified_time));
        }
        if let Some(created_time) = &self.created_time {
            file.insert("createdTime".into(), json!(created_time));
        }
        if let Some(user) = &self.last_modifying_user {
            file.insert("lastModifyingUser".into(), user.to_json());
        }
        if !self.owners.is_empty() {
            let owners = self.owners.iter().map(FakeUser::to_json).collect();
            file.insert("owners".into(), Value::Array(owners));
        }
        if let Some(trashed_time) = &self.trashed_time {
            file.insert("trashedTime".into(), json!(trashed_time));
        }
        if let Some(user) = &self.trashing_user {
            file.insert("trashingUser".into(), user.to_json());
        }
        if let Some(resource_key) = &self.resource_key {
            file.insert("resourceKey".into(), json!(resource_key));
        }
        if let Some((target_id, target_mime_type)) = &self.shortcut_target {
            let mut details = json!({"targetId": target_id, "targetMimeType": target_mime_type});
            if let Some(resource_key) = &self.target_resource_key {
                details["targetResourceKey"] = json!(resource_key);
            }
            file.insert("shortcutDetails".into(), details);
        }
        Value::Object(file)
    }
}

/// How the fake deviates from a well-behaved, instant Drive.
#[derive(Debug, Clone, Default)]
pub(crate) struct Behavior {
    /// The most files per page; `None` honors `pageSize`. Drive may return fewer than asked.
    pub(crate) page_size: Option<usize>,
    /// How long every request takes, so that requests overlap.
    pub(crate) delay: Duration,
    /// A query naming one of these folders fails with 404, like a folder the user cannot read.
    pub(crate) unreadable: HashSet<String>,
    /// A query naming one of these folders fails with 500 on every page after the first.
    pub(crate) fail_after_first_page: HashSet<String>,
    /// A query naming one of these folders reports `incompleteSearch`.
    pub(crate) incomplete: HashSet<String>,
    /// Leave `parents` out of the returned files.
    pub(crate) omit_parents: bool,
    /// Return trashed files even when the query excludes them.
    pub(crate) ignore_trashed_filter: bool,
    /// Start every page after the first with the last file of the previous page, as Drive may
    /// when the folder changes during the listing.
    pub(crate) repeat_across_pages: bool,
    /// Folders shared by link only: a query naming one fails with 404 unless the
    /// `X-Goog-Drive-Resource-Keys` header has `<id>/<key>` for it.
    pub(crate) resource_keys: HashMap<String, String>,
    /// A query naming one of these folders fails with 503 the given number of times (counted
    /// per folder) before it is answered, like a backend that is briefly unavailable.
    pub(crate) unavailable_first: HashMap<String, usize>,
    /// The `Retry-After` (seconds) of those 503 replies.
    pub(crate) unavailable_retry_after: Option<u64>,
    /// How long every media download takes before its reply.
    pub(crate) media_delay: Duration,
    /// Media downloads of these file ids fail with 404, after the given time instead of
    /// `media_delay`.
    pub(crate) media_missing: HashMap<String, Duration>,
    /// Media downloads of these file ids send half of the content and then nothing more, like
    /// a transfer that is still running.
    pub(crate) media_stalled: HashSet<String>,
    /// Exports of these file ids fail with Drive's 403 for exports over 10 MB.
    pub(crate) export_too_large: HashSet<String>,
    /// How long the create of a file (not a folder) takes before its reply.
    pub(crate) upload_delay: Duration,
    /// Creates of files with these names fail with 400.
    pub(crate) upload_rejected: HashSet<String>,
    /// Content updates of these file ids fail with 400.
    pub(crate) update_rejected: HashSet<String>,
    /// Metadata updates (a trash, a rename) of these file ids fail with 403.
    pub(crate) patch_rejected: HashSet<String>,
    /// How long every chunk of a resumable session takes before its reply.
    pub(crate) chunk_delay: Duration,
    /// The chunk of the file with this name that starts at this offset fails with 503 once,
    /// committing nothing.
    pub(crate) chunk_unavailable: HashMap<String, u64>,
    /// Chunks that start at or after this offset get no reply, like a transfer that hangs.
    /// [`FakeDrive::wait_for_stalled_chunks`] waits until that many have arrived.
    pub(crate) chunks_stalled_from: Option<u64>,
    /// The first this many file uploads (multipart creates of files, first chunks of resumable
    /// sessions) are held until all of them have arrived, so they are in flight together.
    pub(crate) upload_rendezvous: usize,
    /// The first this many media downloads are held until all of them have arrived.
    pub(crate) media_rendezvous: usize,
    /// Every media download and export waits for a permit of this semaphore before it is
    /// answered (and takes it), so that a test acts while the request is in flight.
    pub(crate) content_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// The first queries naming one of these folders, as many as there are folders, are held
    /// until all of them have arrived.
    pub(crate) list_rendezvous: HashSet<String>,
    /// A query naming one of these folders is answered only once a media download has arrived.
    pub(crate) list_after_media: HashSet<String>,
    /// The email address `about.get` reports; `fake@example.com` without one.
    pub(crate) account_email: Option<String>,
    /// The records of the audit log, as the Reports API returns them in `items`.
    pub(crate) audit_log: Vec<Value>,
    /// The most audit records per page; `None` honors `maxResults`.
    pub(crate) audit_page_size: Option<usize>,
    /// Whether `startTime` takes a record of that very time.
    pub(crate) audit_bounds: Bounds,
    /// Every audit log request is answered with this status and JSON body.
    pub(crate) audit_error: Option<(u16, Value)>,
    /// Every audit log request for a page after the first (with a `pageToken`) is answered
    /// with this status and JSON body.
    pub(crate) audit_error_after_first_page: Option<(u16, Value)>,
    /// The first this many audit log requests are answered with 429, a rate limit.
    pub(crate) audit_rate_limited: usize,
    /// The first this many audit log requests are held until all of them have arrived.
    pub(crate) audit_rendezvous: usize,
    /// The sharing list of these file ids: the permissions `permissions.list` returns.
    pub(crate) permissions: HashMap<String, Vec<Value>>,
    /// `permissions.list` of these file ids fails with 403.
    pub(crate) permissions_unreadable: HashSet<String>,
    /// A query for the trash (`trashed = true`) naming one of these folders fails with 404.
    pub(crate) unreadable_trash: HashSet<String>,
    /// The revisions `revisions.list` returns of these file ids, oldest first; a stored file
    /// that is not here has none.
    pub(crate) revisions: HashMap<String, Vec<Value>>,
    /// `revisions.list` of these file ids fails with this status (403, 404, ...).
    pub(crate) revisions_refused: HashMap<String, u16>,
}

/// Which ends of its time range an audit log request includes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Bounds {
    /// `startTime` is included, `endTime` is not.
    #[default]
    StartIncluded,
    /// Neither is.
    BothExcluded,
}

/// How long a request held by [`Rendezvous`] or [`Behavior::list_after_media`] waits at most.
/// Then it goes on, so that a test whose requests do not overlap fails on its check of what was
/// in flight at once instead of hanging.
const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Holds the first `parties` requests that join it until all of them have arrived; later ones
/// pass. A test sees these requests in flight together without depending on how long anything
/// takes.
struct Rendezvous {
    parties: usize,
    joined: AtomicUsize,
    barrier: Barrier,
}

impl Rendezvous {
    fn new(parties: usize) -> Self {
        Self {
            parties,
            joined: AtomicUsize::new(0),
            barrier: Barrier::new(parties),
        }
    }

    async fn join(&self) {
        if self.joined.fetch_add(1, Ordering::SeqCst) < self.parties {
            // A party that gives up still counts: the others are not held longer.
            let _ = tokio::time::timeout(RENDEZVOUS_TIMEOUT, self.barrier.wait()).await;
        }
    }
}

/// A chunk or status request of a resumable session, as the fake answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    /// The name of the file the session uploads.
    pub(crate) name: String,
    /// The first byte; `None` for a status request.
    pub(crate) first: Option<u64>,
    pub(crate) status: u16,
}

/// A resumable session.
#[derive(Debug)]
struct Session {
    metadata: Value,
    /// The stored file a resumable update replaces the content of; `None` for a create.
    update: Option<String>,
    total: u64,
    received: Vec<u8>,
    /// See [`Created::arrived`].
    arrived: usize,
    /// The reply that completed the session.
    done: Option<Value>,
}

/// A `files.create` the fake answered.
#[derive(Debug, Clone)]
pub(crate) struct Created {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) mime_type: String,
    pub(crate) parents: Vec<String>,
    pub(crate) content: Vec<u8>,
    /// The `modifiedTime` and `createdTime` of the metadata.
    pub(crate) modified_time: Option<String>,
    pub(crate) creation_time: Option<String>,
    /// The position of the request's arrival among all arrivals and replies of creates.
    pub(crate) arrived: usize,
    /// The position of its reply.
    pub(crate) answered: usize,
}

/// A list request as the fake saw it, with percent-decoded parameters.
#[derive(Debug, Clone)]
pub(crate) struct ListRequest {
    pub(crate) params: Vec<(String, String)>,
    /// The folder ids named in `q`, in order.
    pub(crate) parents: Vec<String>,
    /// The `X-Goog-Drive-Resource-Keys` header.
    pub(crate) resource_keys: Option<String>,
}

impl ListRequest {
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// An audit log, `permissions.list` or `revisions.list` request as the fake saw it, with
/// percent-decoded parameters.
#[derive(Debug, Clone)]
pub(crate) struct ApiRequest {
    /// For `permissions.list` and `revisions.list`, the file id; empty for the audit log.
    pub(crate) file_id: String,
    pub(crate) params: Vec<(String, String)>,
    /// The `Authorization` header.
    pub(crate) authorization: Option<String>,
}

impl ApiRequest {
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct State {
    /// The stored files; creates, content updates and metadata updates change them.
    files: Mutex<Vec<FakeFile>>,
    behavior: Behavior,
    requests: Mutex<Vec<ListRequest>>,
    /// The ids of `files.get` requests.
    gets: Mutex<Vec<String>>,
    /// The 503 replies still to send, per folder ([`Behavior::unavailable_first`]).
    unavailable_left: Mutex<HashMap<String, usize>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// The file ids of media downloads.
    media: Mutex<Vec<String>>,
    /// The file ids and `Range` headers of media downloads.
    media_ranges: Mutex<Vec<(String, Option<String>)>>,
    /// The file ids and MIME types of exports.
    exports: Mutex<Vec<(String, String)>>,
    media_in_flight: AtomicUsize,
    media_max_in_flight: AtomicUsize,
    /// When the first media download arrived.
    first_media: Mutex<Option<Instant>>,
    /// When the last list reply was sent.
    last_list: Mutex<Option<Instant>>,
    /// Successful creates, in reply order.
    created: Mutex<Vec<Created>>,
    /// Successful content updates: the file id and the new content.
    updates: Mutex<Vec<(String, Vec<u8>)>>,
    /// Metadata updates: the file id and the JSON body, also the rejected ones.
    patches: Mutex<Vec<(String, Value)>>,
    /// The ids of `files.delete` requests.
    deletes: Mutex<Vec<String>>,
    /// `METHOD PATH` of every request that is not a `GET`, whatever the answer.
    writes: Mutex<Vec<String>>,
    /// Orders the arrivals and replies of creates.
    create_events: AtomicUsize,
    uploads_in_flight: AtomicUsize,
    uploads_max_in_flight: AtomicUsize,
    /// Ids handed out by `files.generateIds`.
    generated: AtomicUsize,
    sessions: Mutex<Vec<Session>>,
    /// Sessions started and not completed, and the most at once.
    sessions_active: AtomicUsize,
    sessions_max_active: AtomicUsize,
    chunks: Mutex<Vec<Chunk>>,
    /// The faults of [`Behavior::chunk_unavailable`] still to inject.
    chunk_faults_left: Mutex<HashMap<String, u64>>,
    upload_rendezvous: Rendezvous,
    media_rendezvous: Rendezvous,
    list_rendezvous: Rendezvous,
    /// Whether a media download has arrived, for [`Behavior::list_after_media`].
    media_arrived: watch::Sender<bool>,
    /// The chunks held without a reply ([`Behavior::chunks_stalled_from`]).
    stalled_chunks: watch::Sender<usize>,
    audit_requests: Mutex<Vec<ApiRequest>>,
    /// The 429 replies still to send ([`Behavior::audit_rate_limited`]).
    audit_rate_limited_left: Mutex<usize>,
    audit_in_flight: AtomicUsize,
    audit_max_in_flight: AtomicUsize,
    audit_rendezvous: Rendezvous,
    permission_requests: Mutex<Vec<ApiRequest>>,
    revision_requests: Mutex<Vec<ApiRequest>>,
}

pub(crate) struct FakeDrive {
    addr: SocketAddr,
    state: Arc<State>,
    task: JoinHandle<()>,
}

impl FakeDrive {
    pub(crate) async fn start(files: Vec<FakeFile>, behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake drive");
        let addr = listener.local_addr().expect("fake drive address");
        let behavior_chunk_faults = behavior.chunk_unavailable.clone();
        let upload_rendezvous = Rendezvous::new(behavior.upload_rendezvous);
        let media_rendezvous = Rendezvous::new(behavior.media_rendezvous);
        let list_rendezvous = Rendezvous::new(behavior.list_rendezvous.len());
        let audit_rendezvous = Rendezvous::new(behavior.audit_rendezvous);
        let audit_rate_limited_left = Mutex::new(behavior.audit_rate_limited);
        let state = Arc::new(State {
            files: Mutex::new(files),
            unavailable_left: Mutex::new(behavior.unavailable_first.clone()),
            behavior,
            requests: Mutex::new(Vec::new()),
            gets: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            media: Mutex::new(Vec::new()),
            media_ranges: Mutex::new(Vec::new()),
            exports: Mutex::new(Vec::new()),
            media_in_flight: AtomicUsize::new(0),
            media_max_in_flight: AtomicUsize::new(0),
            first_media: Mutex::new(None),
            last_list: Mutex::new(None),
            created: Mutex::new(Vec::new()),
            updates: Mutex::new(Vec::new()),
            patches: Mutex::new(Vec::new()),
            deletes: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            create_events: AtomicUsize::new(0),
            uploads_in_flight: AtomicUsize::new(0),
            uploads_max_in_flight: AtomicUsize::new(0),
            generated: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            sessions_active: AtomicUsize::new(0),
            sessions_max_active: AtomicUsize::new(0),
            chunks: Mutex::new(Vec::new()),
            chunk_faults_left: Mutex::new(behavior_chunk_faults),
            upload_rendezvous,
            media_rendezvous,
            list_rendezvous,
            media_arrived: watch::channel(false).0,
            stalled_chunks: watch::channel(0).0,
            audit_requests: Mutex::new(Vec::new()),
            audit_rate_limited_left,
            audit_in_flight: AtomicUsize::new(0),
            audit_max_in_flight: AtomicUsize::new(0),
            audit_rendezvous,
            permission_requests: Mutex::new(Vec::new()),
            revision_requests: Mutex::new(Vec::new()),
        });

        let server_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let service = service_fn(move |request| handle(Arc::clone(&state), request));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        Self { addr, state, task }
    }

    /// A hub whose requests all go to this server.
    pub(crate) fn hub(&self) -> Hub {
        Hub::for_mock_server(&format!("http://{}/", self.addr)).expect("create fake drive hub")
    }

    pub(crate) fn requests(&self) -> Vec<ListRequest> {
        self.state.requests.lock().expect("requests lock").clone()
    }

    /// The file ids of the `files.get` requests, which [`Self::requests`] does not include.
    pub(crate) fn gets(&self) -> Vec<String> {
        self.state.gets.lock().expect("gets lock").clone()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.state.requests.lock().expect("requests lock").len()
    }

    /// The most requests that were being answered at the same time.
    pub(crate) fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }

    /// The file ids of the media downloads, in arrival order.
    pub(crate) fn media(&self) -> Vec<String> {
        self.state.media.lock().expect("media lock").clone()
    }

    /// The file ids and `Range` headers of the media downloads, in arrival order.
    pub(crate) fn media_ranges(&self) -> Vec<(String, Option<String>)> {
        self.state
            .media_ranges
            .lock()
            .expect("media ranges lock")
            .clone()
    }

    /// The file ids and requested MIME types of the exports, in arrival order.
    pub(crate) fn exports(&self) -> Vec<(String, String)> {
        self.state.exports.lock().expect("exports lock").clone()
    }

    /// The most media downloads that were being answered at the same time.
    pub(crate) fn media_max_in_flight(&self) -> usize {
        self.state.media_max_in_flight.load(Ordering::SeqCst)
    }

    /// The successful creates, in reply order.
    pub(crate) fn created(&self) -> Vec<Created> {
        self.state.created.lock().expect("created lock").clone()
    }

    /// The successful content updates (`files.update` with media): file id and new content.
    pub(crate) fn updates(&self) -> Vec<(String, Vec<u8>)> {
        self.state.updates.lock().expect("updates lock").clone()
    }

    /// The metadata updates (`files.update` without media), rejected ones included: file id and
    /// JSON body.
    pub(crate) fn patches(&self) -> Vec<(String, Value)> {
        self.state.patches.lock().expect("patches lock").clone()
    }

    /// The file ids of `files.delete` requests.
    pub(crate) fn deletes(&self) -> Vec<String> {
        self.state.deletes.lock().expect("deletes lock").clone()
    }

    /// `METHOD PATH` of every request that was not a `GET`, also the ones the fake does not
    /// know: empty when the account was only read.
    pub(crate) fn writes(&self) -> Vec<String> {
        self.state.writes.lock().expect("writes lock").clone()
    }

    /// How many ids `files.generateIds` handed out.
    pub(crate) fn generated_ids(&self) -> usize {
        self.state.generated.load(Ordering::SeqCst)
    }

    /// The chunk and status requests of resumable sessions, in arrival order.
    pub(crate) fn chunks(&self) -> Vec<Chunk> {
        self.state.chunks.lock().expect("chunks lock").clone()
    }

    /// The chunks that arrived and are held without a reply ([`Behavior::chunks_stalled_from`]).
    pub(crate) fn stalled_chunks(&self) -> usize {
        *self.state.stalled_chunks.borrow()
    }

    /// Returns once `count` chunks are held without a reply ([`Behavior::chunks_stalled_from`]):
    /// each was sent in full after the chunks before it were on Drive and answered, so the
    /// uploads that sent them are in the middle of their sessions, waiting.
    pub(crate) async fn wait_for_stalled_chunks(&self, count: usize) {
        let mut stalled = self.state.stalled_chunks.subscribe();
        // The sender lives as long as `self`.
        let _ = stalled.wait_for(|stalled| *stalled >= count).await;
    }

    /// The most resumable sessions that were started and not completed at the same time.
    pub(crate) fn sessions_max_active(&self) -> usize {
        self.state.sessions_max_active.load(Ordering::SeqCst)
    }

    /// The most file uploads (creates of non-folders) that were being answered at once.
    pub(crate) fn uploads_max_in_flight(&self) -> usize {
        self.state.uploads_max_in_flight.load(Ordering::SeqCst)
    }

    /// The requests to the audit log, in arrival order.
    pub(crate) fn audit_requests(&self) -> Vec<ApiRequest> {
        self.state
            .audit_requests
            .lock()
            .expect("audit requests lock")
            .clone()
    }

    /// The most audit log requests that were being answered at the same time.
    pub(crate) fn audit_max_in_flight(&self) -> usize {
        self.state.audit_max_in_flight.load(Ordering::SeqCst)
    }

    /// The `permissions.list` requests, in arrival order.
    pub(crate) fn permission_requests(&self) -> Vec<ApiRequest> {
        self.state
            .permission_requests
            .lock()
            .expect("permission requests lock")
            .clone()
    }

    /// The `revisions.list` requests, in arrival order.
    pub(crate) fn revision_requests(&self) -> Vec<ApiRequest> {
        self.state
            .revision_requests
            .lock()
            .expect("revision requests lock")
            .clone()
    }

    /// When the first media download arrived and when the last list reply was sent.
    pub(crate) fn first_media_and_last_list(&self) -> (Option<Instant>, Option<Instant>) {
        (
            *self.state.first_media.lock().expect("first media lock"),
            *self.state.last_list.lock().expect("last list lock"),
        )
    }
}

impl State {
    fn files(&self) -> std::sync::MutexGuard<'_, Vec<FakeFile>> {
        self.files.lock().expect("files lock")
    }

    /// The stored file `id`.
    fn file(&self, id: &str) -> Option<FakeFile> {
        self.files().iter().find(|file| file.id == id).cloned()
    }
}

impl Drop for FakeDrive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Counts a request as in flight until dropped, also when the client goes away.
struct InFlight<'a> {
    now: &'a AtomicUsize,
}

impl<'a> InFlight<'a> {
    fn new(now: &'a AtomicUsize, max: &AtomicUsize) -> Self {
        let count = now.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(count, Ordering::SeqCst);
        Self { now }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }
}

type FakeResponse = hyper::Response<BoxBody<Bytes, Infallible>>;

/// Notes a request that is not a `GET` for [`FakeDrive::writes`].
fn record_write(state: &State, method: &Method, path: &str) {
    if method != Method::GET {
        state
            .writes
            .lock()
            .expect("writes lock")
            .push(format!("{method} {path}"));
    }
}

async fn handle(
    state: Arc<State>,
    request: hyper::Request<Incoming>,
) -> Result<FakeResponse, Infallible> {
    let _in_flight = InFlight::new(&state.in_flight, &state.max_in_flight);

    let params = request
        .uri()
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let path = request.uri().path().to_owned();
    record_write(&state, request.method(), &path);
    if let Some(response) = activity_route(&state, &request, &path, &params).await {
        return Ok(response);
    }
    if request.method() == Method::POST && path == UPLOAD_PATH {
        let resumable = params
            .iter()
            .any(|(key, value)| key == "uploadType" && value == "resumable");
        return Ok(if resumable {
            start_session(&state, request, None).await
        } else {
            create(&state, request).await
        });
    }
    if matches!(*request.method(), Method::PATCH | Method::DELETE) {
        return Ok(modify(&state, &path, request).await);
    }
    if request.method() == Method::PUT {
        if let Some(session) = path
            .strip_prefix(SESSION_PATH)
            .and_then(|n| n.parse::<usize>().ok())
        {
            return Ok(session_request(&state, session, request).await);
        }
    }
    if request.method() != Method::GET {
        return Ok(error(StatusCode::NOT_FOUND, "unexpected request"));
    }
    if path == GENERATE_IDS_PATH {
        return Ok(generate_ids(&state, &params));
    }
    if let Some(id) = path
        .as_str()
        .strip_prefix(LIST_PATH)
        .and_then(|id| id.strip_prefix('/'))
    {
        if let Some(id) = id.strip_suffix("/export") {
            return Ok(export(&state, id, &params).await);
        }
        if params
            .iter()
            .any(|(key, value)| key == "alt" && value == "media")
        {
            return Ok(media(&state, id, request.headers()).await);
        }
        state.gets.lock().expect("gets lock").push(id.to_owned());
        return Ok(get(&state, id, &params));
    }
    if path == ABOUT_PATH {
        return Ok(about(&state));
    }
    if path != LIST_PATH {
        return Ok(error(StatusCode::NOT_FOUND, "unexpected request"));
    }

    let q = params
        .iter()
        .find(|(key, _)| key == "q")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let list_request = ListRequest {
        parents: parent_ids(&q),
        params,
        resource_keys: request
            .headers()
            .get("x-goog-drive-resource-keys")
            .and_then(|value| value.to_str().ok())
            .map(String::from),
    };
    state
        .requests
        .lock()
        .expect("requests lock")
        .push(list_request.clone());

    let names = |set: &HashSet<String>| list_request.parents.iter().any(|id| set.contains(id));
    if names(&state.behavior.list_rendezvous) {
        state.list_rendezvous.join().await;
    }
    if names(&state.behavior.list_after_media) {
        let mut media_arrived = state.media_arrived.subscribe();
        let arrived = media_arrived.wait_for(|arrived| *arrived);
        let _ = tokio::time::timeout(RENDEZVOUS_TIMEOUT, arrived).await;
    }
    tokio::time::sleep(state.behavior.delay).await;

    let response = list(&state, &list_request, &q);
    *state.last_list.lock().expect("last list lock") = Some(Instant::now());
    Ok(response)
}

/// `files.generateIds`: `count` ids that no other call returned.
fn generate_ids(state: &State, params: &[(String, String)]) -> FakeResponse {
    let count = params
        .iter()
        .find(|(key, _)| key == "count")
        .and_then(|(_, count)| count.parse::<usize>().ok())
        .unwrap_or(10);
    let first = state.generated.fetch_add(count, Ordering::SeqCst);
    let ids = (first..first + count)
        .map(|n| format!("gen-{n}"))
        .collect::<Vec<_>>();
    json_response(
        StatusCode::OK,
        &json!({"kind": "drive#generatedIds", "space": "drive", "ids": ids}),
    )
}

/// A multipart `files.create`: the metadata part and the content part.
async fn create(state: &State, request: hyper::Request<Incoming>) -> FakeResponse {
    let arrived = state.create_events.fetch_add(1, Ordering::SeqCst);
    let boundary = request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once("boundary="))
        .map(|(_, boundary)| boundary.trim_matches('"').to_owned());
    let Ok(body) = request.into_body().collect().await else {
        return error(StatusCode::BAD_REQUEST, "unreadable body");
    };
    let Some((metadata, content)) = boundary.and_then(|b| multipart(&body.to_bytes(), &b)) else {
        return error(StatusCode::BAD_REQUEST, "not a multipart upload");
    };
    let is_folder = metadata["mimeType"].as_str() == Some(MIME_TYPE_DRIVE_FOLDER);
    let _upload =
        (!is_folder).then(|| InFlight::new(&state.uploads_in_flight, &state.uploads_max_in_flight));
    if !is_folder {
        state.upload_rendezvous.join().await;
        tokio::time::sleep(state.behavior.upload_delay).await;
    }

    finish_create(state, &metadata, content, arrived)
}

/// The end of a create with all its content: Drive's checks, the record, the file resource.
fn finish_create(
    state: &State,
    metadata: &Value,
    content: Vec<u8>,
    arrived: usize,
) -> FakeResponse {
    let text = |key: &str| metadata[key].as_str().unwrap_or_default().to_owned();
    let mime_type = text("mimeType");
    let parents = metadata["parents"]
        .as_array()
        .map(|parents| {
            parents
                .iter()
                .filter_map(|parent| parent.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let name = text("name");
    if state.behavior.upload_rejected.contains(&name) {
        return error(StatusCode::BAD_REQUEST, "Invalid upload.");
    }
    let missing_parent = {
        let created = state.created.lock().expect("created lock");
        parents.iter().find(|parent| {
            parent.as_str() != "root"
                && !created.iter().any(|folder| {
                    &folder.id == *parent && folder.mime_type == MIME_TYPE_DRIVE_FOLDER
                })
                && !state.files().iter().any(|file| &file.id == *parent)
        })
    };
    if let Some(parent) = missing_parent {
        return error(StatusCode::NOT_FOUND, &format!("File not found: {parent}."));
    }

    let time = |key: &str| metadata[key].as_str().map(String::from);
    let created = Created {
        id: text("id"),
        name,
        mime_type,
        parents,
        content,
        modified_time: time("modifiedTime"),
        creation_time: time("createdTime"),
        arrived,
        answered: state.create_events.fetch_add(1, Ordering::SeqCst),
    };
    let reply = created_reply(&created);
    state.files().push(FakeFile {
        id: created.id.clone(),
        name: created.name.clone(),
        mime_type: created.mime_type.clone(),
        parents: created.parents.clone(),
        size: (!is_folder_type(&created.mime_type)).then_some(created.content.len() as u64),
        md5: (!is_folder_type(&created.mime_type))
            .then(|| format!("{:x}", md5::compute(&created.content))),
        content: Some(String::from_utf8_lossy(&created.content).into_owned()),
        modified_time: created.modified_time.clone(),
        created_time: created.creation_time.clone(),
        ..FakeFile::default()
    });
    state.created.lock().expect("created lock").push(created);
    json_response(StatusCode::OK, &reply)
}

/// The file resource of a create.
fn created_reply(created: &Created) -> Value {
    let mut reply = json!({
        "kind": "drive#file",
        "id": created.id,
        "name": created.name,
        "mimeType": created.mime_type,
        "parents": created.parents,
        "size": created.content.len().to_string(),
        "md5Checksum": format!("{:x}", md5::compute(&created.content)),
    });
    if let Some(modified_time) = &created.modified_time {
        reply["modifiedTime"] = json!(modified_time);
    }
    if let Some(creation_time) = &created.creation_time {
        reply["createdTime"] = json!(creation_time);
    }
    reply
}

fn is_folder_type(mime_type: &str) -> bool {
    mime_type == MIME_TYPE_DRIVE_FOLDER
}

/// A `PATCH` (`files.update` of the content or of the metadata) or a `DELETE` of a file.
async fn modify(state: &State, path: &str, request: hyper::Request<Incoming>) -> FakeResponse {
    let id_after = |prefix: &str| {
        path.strip_prefix(prefix)
            .and_then(|id| id.strip_prefix('/'))
            .map(String::from)
    };
    match (
        request.method().clone(),
        id_after(UPLOAD_PATH),
        id_after(LIST_PATH),
    ) {
        (Method::PATCH, Some(id), _) => {
            let resumable = request.uri().query().is_some_and(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .any(|(key, value)| key == "uploadType" && value == "resumable")
            });
            if resumable {
                start_session(state, request, Some(id)).await
            } else {
                update(state, &id, request).await
            }
        }
        (Method::PATCH, None, Some(id)) => patch(state, &id, request).await,
        (Method::DELETE, None, Some(id)) => {
            state.deletes.lock().expect("deletes lock").push(id);
            hyper::Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Full::new(Bytes::new()).boxed())
                .expect("valid response")
        }
        _ => error(StatusCode::NOT_FOUND, "unexpected request"),
    }
}

/// A multipart `files.update` of a stored file: its new content. A resumable update is a
/// session ([`start_session`]).
async fn update(state: &State, id: &str, request: hyper::Request<Incoming>) -> FakeResponse {
    let multipart_upload = request.uri().query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "uploadType" && value == "multipart")
    });
    if !multipart_upload {
        return error(StatusCode::BAD_REQUEST, "only multipart updates");
    }
    let boundary = request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once("boundary="))
        .map(|(_, boundary)| boundary.trim_matches('"').to_owned());
    let Ok(body) = request.into_body().collect().await else {
        return error(StatusCode::BAD_REQUEST, "unreadable body");
    };
    let Some((_, content)) = boundary.and_then(|b| multipart(&body.to_bytes(), &b)) else {
        return error(StatusCode::BAD_REQUEST, "not a multipart upload");
    };
    match apply_update(state, id, content) {
        Ok(reply) => json_response(StatusCode::OK, &reply),
        Err(response) => *response,
    }
}

/// The new `content` of the stored file `id`, recorded and stored; the file resource, or the
/// error reply.
fn apply_update(state: &State, id: &str, content: Vec<u8>) -> Result<Value, Box<FakeResponse>> {
    if state.behavior.update_rejected.contains(id) {
        return Err(Box::new(error(StatusCode::BAD_REQUEST, "Invalid upload.")));
    }
    let mut files = state.files();
    let Some(file) = files.iter_mut().find(|file| file.id == id) else {
        let message = format!("File not found: {id}.");
        return Err(Box::new(error(StatusCode::NOT_FOUND, &message)));
    };
    file.size = Some(content.len() as u64);
    file.md5 = Some(format!("{:x}", md5::compute(&content)));
    file.content = Some(String::from_utf8_lossy(&content).into_owned());
    let reply = json!({
        "kind": "drive#file",
        "id": id,
        "name": file.name,
        "mimeType": file.mime_type,
        "parents": file.parents,
        "size": content.len().to_string(),
        "md5Checksum": format!("{:x}", md5::compute(&content)),
    });
    drop(files);
    state
        .updates
        .lock()
        .expect("updates lock")
        .push((id.to_owned(), content));
    Ok(reply)
}

/// A metadata `files.update` (`PATCH` of JSON) of a stored file: `trashed` and `name` are
/// stored.
async fn patch(state: &State, id: &str, request: hyper::Request<Incoming>) -> FakeResponse {
    let Ok(body) = request.into_body().collect().await else {
        return error(StatusCode::BAD_REQUEST, "unreadable body");
    };
    let metadata = serde_json::from_slice::<Value>(&body.to_bytes()).unwrap_or(Value::Null);
    state
        .patches
        .lock()
        .expect("patches lock")
        .push((id.to_owned(), metadata.clone()));
    if state.behavior.patch_rejected.contains(id) {
        return error(
            StatusCode::FORBIDDEN,
            "The user does not have sufficient permissions for this file.",
        );
    }
    let mut files = state.files();
    let Some(file) = files.iter_mut().find(|file| file.id == id) else {
        return error(StatusCode::NOT_FOUND, &format!("File not found: {id}."));
    };
    if let Some(trashed) = metadata["trashed"].as_bool() {
        file.trashed = trashed;
    }
    if let Some(name) = metadata["name"].as_str() {
        name.clone_into(&mut file.name);
    }
    let mut reply = file.to_json(true);
    drop(files);
    if let (Some(reply), Some(changes)) = (reply.as_object_mut(), metadata.as_object()) {
        for (key, value) in changes {
            reply.insert(key.clone(), value.clone());
        }
    }
    json_response(StatusCode::OK, &reply)
}

/// `POST ...?uploadType=resumable`: a new session for the metadata in the body.
async fn start_session(
    state: &State,
    request: hyper::Request<Incoming>,
    update: Option<String>,
) -> FakeResponse {
    let arrived = state.create_events.fetch_add(1, Ordering::SeqCst);
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(String::from)
    };
    let (Some(host), Some(total)) = (
        header("host"),
        header("x-upload-content-length").and_then(|total| total.parse::<u64>().ok()),
    ) else {
        return error(
            StatusCode::BAD_REQUEST,
            "no Host or X-Upload-Content-Length",
        );
    };
    let Ok(body) = request.into_body().collect().await else {
        return error(StatusCode::BAD_REQUEST, "unreadable body");
    };
    let Ok(metadata) = serde_json::from_slice::<Value>(&body.to_bytes()) else {
        return error(StatusCode::BAD_REQUEST, "no JSON metadata");
    };
    if state
        .behavior
        .upload_rejected
        .contains(metadata["name"].as_str().unwrap_or_default())
    {
        return error(StatusCode::BAD_REQUEST, "Invalid upload.");
    }

    let session = {
        let mut sessions = state.sessions.lock().expect("sessions lock");
        sessions.push(Session {
            metadata,
            update,
            total,
            received: Vec::new(),
            arrived,
            done: None,
        });
        sessions.len() - 1
    };
    let active = state.sessions_active.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .sessions_max_active
        .fetch_max(active, Ordering::SeqCst);
    hyper::Response::builder()
        .status(StatusCode::OK)
        .header("location", format!("http://{host}{SESSION_PATH}{session}"))
        .body(Full::new(Bytes::new()).boxed())
        .expect("valid response")
}

/// A `PUT` to a session: a chunk at `Content-Range: bytes FIRST-LAST/TOTAL`, or a status
/// request with `bytes */TOTAL`.
async fn session_request(
    state: &State,
    session: usize,
    request: hyper::Request<Incoming>,
) -> FakeResponse {
    let range = request
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "))
        .and_then(|value| value.split_once('/'))
        .map(|(range, _)| range.to_owned());
    let Ok(body) = request.into_body().collect().await else {
        return error(StatusCode::BAD_REQUEST, "unreadable body");
    };
    let body = body.to_bytes();
    let Some(range) = range else {
        return error(StatusCode::BAD_REQUEST, "no Content-Range");
    };
    let first = (range != "*")
        .then(|| {
            let (first, last) = range.split_once('-')?;
            Some((first.parse::<u64>().ok()?, last.parse::<u64>().ok()?))
        })
        .flatten();
    if range != "*" && first.is_none() {
        return error(StatusCode::BAD_REQUEST, "malformed Content-Range");
    }
    hold_chunk(state, first.map(|(first, _)| first)).await;

    let mut sessions = state.sessions.lock().expect("sessions lock");
    let Some(this) = sessions.get_mut(session) else {
        return error(StatusCode::NOT_FOUND, "no such session");
    };
    let name = this.metadata["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let committed = this.received.len() as u64;
    let response = if let Some(done) = &this.done {
        json_response(StatusCode::OK, done)
    } else if let Some((first, last)) = first {
        let fault = {
            let mut faults = state.chunk_faults_left.lock().expect("faults lock");
            let fault = faults.get(&name) == Some(&first);
            if fault {
                faults.remove(&name);
            }
            fault
        };
        if first != committed || fault {
            error(StatusCode::SERVICE_UNAVAILABLE, "Backend Error")
        } else if last + 1 - first != body.len() as u64 {
            error(
                StatusCode::BAD_REQUEST,
                "Content-Range does not match the body",
            )
        } else {
            this.received.extend_from_slice(&body);
            if this.received.len() as u64 == this.total {
                let metadata = this.metadata.clone();
                let content = this.received.clone();
                let arrived = this.arrived;
                let update = this.update.clone();
                drop(sessions);
                let response = match update {
                    Some(id) => finish_update(state, session, &id, content),
                    None => finish_session_create(state, session, &metadata, content, arrived),
                };
                record_chunk(state, &name, Some(first), response.status());
                return response;
            }
            resume_incomplete(this.received.len())
        }
    } else {
        resume_incomplete(this.received.len())
    };
    drop(sessions);
    record_chunk(
        state,
        &name,
        first.map(|(first, _)| first),
        response.status(),
    );
    response
}

/// The last chunk of a resumable create arrived: the file is created.
fn finish_session_create(
    state: &State,
    session: usize,
    metadata: &Value,
    content: Vec<u8>,
    arrived: usize,
) -> FakeResponse {
    let response = finish_create(state, metadata, content, arrived);
    if response.status() == StatusCode::OK {
        let reply = state
            .created
            .lock()
            .expect("created lock")
            .last()
            .map(created_reply);
        session_done(state, session, reply);
    }
    response
}

/// The last chunk of a resumable update arrived: the stored file gets `content`.
fn finish_update(state: &State, session: usize, id: &str, content: Vec<u8>) -> FakeResponse {
    match apply_update(state, id, content) {
        Ok(reply) => {
            session_done(state, session, Some(reply.clone()));
            json_response(StatusCode::OK, &reply)
        }
        Err(response) => *response,
    }
}

/// The session `session` completed with `reply`.
fn session_done(state: &State, session: usize, reply: Option<Value>) {
    state.sessions_active.fetch_sub(1, Ordering::SeqCst);
    if let Some(this) = state
        .sessions
        .lock()
        .expect("sessions lock")
        .get_mut(session)
    {
        this.done = reply;
    }
}

/// What a request of a session waits for before it is answered: [`Behavior::chunk_delay`]; a
/// chunk from [`Behavior::chunks_stalled_from`] on, forever (counted as stalled once its body
/// has arrived); and the first chunk of a session, for those of the others
/// ([`Behavior::upload_rendezvous`]). `first` is the chunk's first byte.
async fn hold_chunk(state: &State, first: Option<u64>) {
    if let Some(first) = first {
        if state
            .behavior
            .chunks_stalled_from
            .is_some_and(|from| first >= from)
        {
            state.stalled_chunks.send_modify(|stalled| *stalled += 1);
            std::future::pending::<()>().await;
        }
        if first == 0 {
            state.upload_rendezvous.join().await;
        }
    }
    tokio::time::sleep(state.behavior.chunk_delay).await;
}

fn record_chunk(state: &State, name: &str, first: Option<u64>, status: StatusCode) {
    state.chunks.lock().expect("chunks lock").push(Chunk {
        name: name.to_owned(),
        first,
        status: status.as_u16(),
    });
}

/// A 308 for `committed` bytes: `Range: bytes=0-<last>`, none when nothing is committed.
fn resume_incomplete(committed: usize) -> FakeResponse {
    let mut response = hyper::Response::builder().status(StatusCode::PERMANENT_REDIRECT);
    if committed > 0 {
        response = response.header("range", format!("bytes=0-{}", committed - 1));
    }
    response
        .body(Full::new(Bytes::new()).boxed())
        .expect("valid response")
}

/// The JSON metadata and the content of a `multipart/related` body with two parts.
fn multipart(body: &[u8], boundary: &str) -> Option<(Value, Vec<u8>)> {
    let delimiter = format!("--{boundary}");
    let mut parts = split(body, delimiter.as_bytes()).into_iter().skip(1);
    let mut part = || -> Option<Vec<u8>> {
        let part = parts.next()?;
        let part = part.strip_prefix(b"\r\n")?;
        let start = part.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
        let content = &part[start..];
        Some(content.strip_suffix(b"\r\n").unwrap_or(content).to_vec())
    };
    let metadata = serde_json::from_slice(&part()?).ok()?;
    let content = part()?;
    Some((metadata, content))
}

/// `haystack` split at every `delimiter`.
fn split<'a>(haystack: &'a [u8], delimiter: &[u8]) -> Vec<&'a [u8]> {
    let mut pieces = Vec::new();
    let mut rest = haystack;
    while let Some(at) = rest
        .windows(delimiter.len())
        .position(|window| window == delimiter)
    {
        pieces.push(&rest[..at]);
        rest = &rest[at + delimiter.len()..];
    }
    pieces.push(rest);
    pieces
}

/// `about.get`: the account of [`Behavior::account_email`].
fn about(state: &State) -> FakeResponse {
    let email = state
        .behavior
        .account_email
        .as_deref()
        .unwrap_or("fake@example.com");
    json_response(StatusCode::OK, &json!({"user": {"emailAddress": email}}))
}

/// `files.get?alt=media`: the content of a blob, from the first byte of its `Range`
/// (`bytes=<first>-`) unless an `If-Range` is not the content's `ETag`.
async fn media(state: &State, id: &str, headers: &hyper::HeaderMap) -> FakeResponse {
    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(String::from)
    };
    let range = header(hyper::header::RANGE);
    let if_range = header(hyper::header::IF_RANGE);
    let _media = InFlight::new(&state.media_in_flight, &state.media_max_in_flight);
    state.media.lock().expect("media lock").push(id.to_owned());
    state
        .media_ranges
        .lock()
        .expect("media ranges lock")
        .push((id.to_owned(), range.clone()));
    state
        .first_media
        .lock()
        .expect("first media lock")
        .get_or_insert_with(Instant::now);
    state.media_arrived.send_replace(true);
    state.media_rendezvous.join().await;
    pass_gate(state).await;

    let behavior = &state.behavior;
    if let Some(delay) = behavior.media_missing.get(id) {
        tokio::time::sleep(*delay).await;
        return error(StatusCode::NOT_FOUND, "File not found.");
    }
    tokio::time::sleep(behavior.media_delay).await;
    let content = state
        .files()
        .iter()
        .find(|file| file.id == id)
        .and_then(|file| file.content.clone());
    let Some(content) = content else {
        return error(StatusCode::NOT_FOUND, "File not found.");
    };

    let total = content.len();
    let etag = format!("\"{:x}\"", md5::compute(&content));
    let first = range
        .as_deref()
        .and_then(|range| range.strip_prefix("bytes="))
        .and_then(|range| range.strip_suffix('-'))
        .and_then(|first| first.parse::<usize>().ok())
        .filter(|_| if_range.as_ref().is_none_or(|tag| *tag == etag));
    let mut response = hyper::Response::builder()
        .header("content-type", "application/octet-stream")
        .header("etag", etag.as_str());
    let content = match first {
        Some(first) if first >= total => {
            return response
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{total}"))
                .body(Full::new(Bytes::new()).boxed())
                .expect("valid response");
        }
        Some(first) => {
            response = response.status(StatusCode::PARTIAL_CONTENT).header(
                "content-range",
                format!("bytes {first}-{}/{total}", total - 1),
            );
            Bytes::from(content).slice(first..)
        }
        None => {
            response = response.status(StatusCode::OK);
            Bytes::from(content)
        }
    };

    let length = content.len();
    let body = if behavior.media_stalled.contains(id) {
        let half = content.slice(..length / 2);
        let frames = stream::iter([Ok::<_, Infallible>(hyper::body::Frame::data(half))])
            .chain(stream::pending());
        BodyExt::boxed(StreamBody::new(frames))
    } else {
        Full::new(content).boxed()
    };
    response
        .header("content-length", length)
        .body(body)
        .expect("valid response")
}

/// `files.export`: the document's content, sent in chunks without `Content-Length`.
/// Waits for a permit of [`Behavior::content_gate`], if there is one.
async fn pass_gate(state: &State) {
    if let Some(gate) = &state.behavior.content_gate {
        if let Ok(permit) = gate.acquire().await {
            permit.forget();
        }
    }
}

async fn export(state: &State, id: &str, params: &[(String, String)]) -> FakeResponse {
    let mime_type = params
        .iter()
        .find(|(key, _)| key == "mimeType")
        .map(|(_, mime_type)| mime_type.clone())
        .unwrap_or_default();
    state
        .exports
        .lock()
        .expect("exports lock")
        .push((id.to_owned(), mime_type));
    pass_gate(state).await;

    if state.behavior.export_too_large.contains(id) {
        let message = "This file is too large to be exported.";
        let body = json!({"error": {
            "code": 403,
            "message": message,
            "errors": [{"domain": "global", "reason": "exportSizeLimitExceeded", "message": message}],
        }});
        return json_response(StatusCode::FORBIDDEN, &body);
    }
    let content = state
        .files()
        .iter()
        .find(|file| file.id == id && file.mime_type.starts_with("application/vnd.google-apps."))
        .and_then(|file| file.content.clone());
    let Some(content) = content else {
        return error(StatusCode::NOT_FOUND, "File not found.");
    };
    let frames = stream::iter([Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from(
        content,
    )))]);
    hyper::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(BodyExt::boxed(StreamBody::new(frames)))
        .expect("valid response")
}

fn list(state: &State, request: &ListRequest, q: &str) -> FakeResponse {
    let behavior = &state.behavior;
    let names = |set: &HashSet<String>| request.parents.iter().any(|id| set.contains(id));

    let only_trashed = q.contains("trashed = true");
    if names(&behavior.unreadable) || (only_trashed && names(&behavior.unreadable_trash)) {
        return error(StatusCode::NOT_FOUND, "File not found.");
    }
    let unavailable = {
        let mut left = state.unavailable_left.lock().expect("unavailable lock");
        request.parents.iter().any(|id| match left.get_mut(id) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        })
    };
    if unavailable {
        let mut response = error(StatusCode::SERVICE_UNAVAILABLE, "Backend Error");
        if let Some(seconds) = behavior.unavailable_retry_after {
            response
                .headers_mut()
                .insert("retry-after", hyper::header::HeaderValue::from(seconds));
        }
        return response;
    }
    let sent_keys = request
        .resource_keys
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .collect::<Vec<_>>();
    let missing_key = request.parents.iter().any(|id| {
        behavior
            .resource_keys
            .get(id)
            .is_some_and(|key| !sent_keys.contains(&format!("{id}/{key}").as_str()))
    });
    if missing_key {
        return error(StatusCode::NOT_FOUND, "File not found.");
    }

    let offset = match request.param("pageToken") {
        None => 0,
        Some(token) => match parse_token(token, q) {
            Some(offset) => offset,
            None => return error(StatusCode::BAD_REQUEST, "Invalid page token."),
        },
    };
    if offset > 0 && names(&behavior.fail_after_first_page) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Error");
    }

    let exclude_trashed = q.contains("trashed = false") && !behavior.ignore_trashed_filter;
    let drive = (request.param("corpora") == Some("drive"))
        .then(|| request.param("driveId"))
        .flatten();
    let files = state.files();
    let matching = files
        .iter()
        .filter(|file| file.parents.iter().any(|id| request.parents.contains(id)))
        .filter(|file| !(exclude_trashed && file.trashed))
        .filter(|file| !only_trashed || file.trashed)
        .filter(|file| drive.is_none_or(|drive| file.drive_id.as_deref() == Some(drive)))
        .collect::<Vec<_>>();

    let requested = request
        .param("pageSize")
        .and_then(|size| size.parse::<usize>().ok())
        .unwrap_or(100);
    let page_size = behavior.page_size.unwrap_or(requested).min(requested);
    let start = if behavior.repeat_across_pages {
        offset.saturating_sub(1)
    } else {
        offset
    };
    let end = (start + page_size).min(matching.len());
    let files = matching
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .map(|file| file.to_json(!behavior.omit_parents))
        .collect::<Vec<_>>();

    let mut body = json!({"kind": "drive#fileList", "files": files});
    if end < matching.len() {
        body["nextPageToken"] = json!(token(end, q));
    }
    if names(&behavior.incomplete) {
        body["incompleteSearch"] = json!(true);
    }
    json_response(StatusCode::OK, &body)
}

/// The `Authorization` header of a request.
fn authorization(headers: &hyper::HeaderMap) -> Option<String> {
    headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(String::from)
}

/// `permissions.list` of the stored file `id`: its [`Behavior::permissions`], `pageSize` (100
/// without one) at a time.
fn permissions(
    state: &State,
    id: &str,
    params: Vec<(String, String)>,
    headers: &hyper::HeaderMap,
) -> FakeResponse {
    let request = ApiRequest {
        file_id: id.to_owned(),
        params,
        authorization: authorization(headers),
    };
    state
        .permission_requests
        .lock()
        .expect("permission requests lock")
        .push(request.clone());
    if state.file(id).is_none() {
        return error(StatusCode::NOT_FOUND, &format!("File not found: {id}."));
    }
    if state.behavior.permissions_unreadable.contains(id) {
        return error(
            StatusCode::FORBIDDEN,
            "The user does not have sufficient permissions for this file.",
        );
    }
    let all = state
        .behavior
        .permissions
        .get(id)
        .cloned()
        .unwrap_or_default();
    let offset = match request.param("pageToken") {
        None => 0,
        Some(token) => match parse_token(token, id) {
            Some(offset) => offset,
            None => return error(StatusCode::BAD_REQUEST, "Invalid page token."),
        },
    };
    let requested = request
        .param("pageSize")
        .and_then(|size| size.parse::<usize>().ok())
        .unwrap_or(100);
    let page_size = state
        .behavior
        .page_size
        .unwrap_or(requested)
        .min(requested)
        .max(1);
    let end = (offset + page_size).min(all.len());
    let mut body = json!({
        "kind": "drive#permissionList",
        "permissions": all.get(offset..end).unwrap_or_default(),
    });
    if end < all.len() {
        body["nextPageToken"] = json!(token(end, id));
    }
    json_response(StatusCode::OK, &body)
}

/// The routes of `files activity` and `files diverged`: the audit log, `permissions.list` and
/// `revisions.list`.
async fn activity_route(
    state: &State,
    request: &hyper::Request<Incoming>,
    path: &str,
    params: &[(String, String)],
) -> Option<FakeResponse> {
    if request.method() != Method::GET {
        return None;
    }
    if path == AUDIT_PATH {
        return Some(audit(state, params.to_vec(), request.headers()).await);
    }
    let id = path.strip_prefix(LIST_PATH)?.strip_prefix('/')?;
    if let Some(id) = id.strip_suffix(REVISIONS_SUFFIX) {
        return Some(revisions(state, id, params.to_vec(), request.headers()));
    }
    let id = id.strip_suffix(PERMISSIONS_SUFFIX)?;
    Some(permissions(state, id, params.to_vec(), request.headers()))
}

/// `revisions.list` of the stored file `id`: its [`Behavior::revisions`] in their order,
/// `pageSize` (200 without one) at a time, or the status of [`Behavior::revisions_refused`].
fn revisions(
    state: &State,
    id: &str,
    params: Vec<(String, String)>,
    headers: &hyper::HeaderMap,
) -> FakeResponse {
    let request = ApiRequest {
        file_id: id.to_owned(),
        params,
        authorization: authorization(headers),
    };
    state
        .revision_requests
        .lock()
        .expect("revision requests lock")
        .push(request.clone());
    if state.file(id).is_none() {
        return error(StatusCode::NOT_FOUND, &format!("File not found: {id}."));
    }
    if let Some(&status) = state.behavior.revisions_refused.get(id) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
        return error(
            status,
            "The user does not have sufficient permissions for this file.",
        );
    }
    let all = state
        .behavior
        .revisions
        .get(id)
        .cloned()
        .unwrap_or_default();
    let offset = match request.param("pageToken") {
        None => 0,
        Some(token) => match parse_token(token, id) {
            Some(offset) => offset,
            None => return error(StatusCode::BAD_REQUEST, "Invalid page token."),
        },
    };
    let requested = request
        .param("pageSize")
        .and_then(|size| size.parse::<usize>().ok())
        .unwrap_or(200);
    let page_size = state
        .behavior
        .page_size
        .unwrap_or(requested)
        .min(requested)
        .max(1);
    let end = (offset + page_size).min(all.len());
    let mut body = json!({
        "kind": "drive#revisionList",
        "revisions": all.get(offset..end).unwrap_or_default(),
    });
    if end < all.len() {
        body["nextPageToken"] = json!(token(end, id));
    }
    json_response(StatusCode::OK, &body)
}

/// What an audit log request selects.
struct AuditQuery {
    start: Option<chrono::DateTime<chrono::Utc>>,
    end: Option<chrono::DateTime<chrono::Utc>>,
    max_results: usize,
    /// The `name==value` terms of `filters`.
    terms: Vec<(String, String)>,
    /// What a page token is bound to.
    digest: String,
}

impl AuditQuery {
    /// The query of `request`, or the reply to a request Google would refuse.
    fn of(request: &ApiRequest) -> Result<Self, Box<FakeResponse>> {
        let invalid = |name: &str| {
            Box::new(error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid value '{name}'"),
            ))
        };
        let time = |name: &str| {
            request
                .param(name)
                .map(|value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .map(|time| time.with_timezone(&chrono::Utc))
                        .map_err(|_| invalid(name))
                })
                .transpose()
        };
        let (start, end) = (time("startTime")?, time("endTime")?);
        if let (Some(start), Some(end)) = (start, end) {
            if start > end {
                return Err(Box::new(error(
                    StatusCode::BAD_REQUEST,
                    "Start time must be less than or equal to the end time.",
                )));
            }
        }
        let max_results = match request.param("maxResults").map(str::parse::<usize>) {
            None => 1000,
            Some(Ok(max)) if (1..=1000).contains(&max) => max,
            Some(_) => return Err(invalid("maxResults")),
        };
        let filters = request.param("filters").unwrap_or_default();
        let terms = filters
            .split(',')
            .filter(|term| !term.is_empty())
            .map(|term| {
                term.split_once("==")
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .ok_or_else(|| invalid("filters"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let digest = format!(
            "{:?} {:?} {filters} {max_results}",
            request.param("startTime"),
            request.param("endTime")
        );
        Ok(AuditQuery {
            start,
            end,
            max_results,
            terms,
            digest,
        })
    }

    /// Whether the query selects `record`: its time is in the range (a record without a
    /// readable time always is), and one of its events has every term.
    fn selects(&self, record: &Value, bounds: Bounds) -> bool {
        let in_range = record_time(record).is_none_or(|time| {
            let after_start = self.start.is_none_or(|start| match bounds {
                Bounds::StartIncluded => time >= start,
                Bounds::BothExcluded => time > start,
            });
            after_start && self.end.is_none_or(|end| time < end)
        });
        let has = |event: &Value, (name, value): &(String, String)| {
            event["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|parameter| {
                    parameter["name"] == name.as_str() && parameter["value"] == value.as_str()
                })
        };
        in_range
            && (self.terms.is_empty()
                || record["events"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|event| self.terms.iter().all(|term| has(event, term))))
    }
}

/// The `id.time` of an audit record.
fn record_time(record: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    record["id"]["time"]
        .as_str()
        .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
        .map(|time| time.with_timezone(&chrono::Utc))
}

/// The audit log of the Reports API: the records of [`Behavior::audit_log`] that the request's
/// time range and `filters` select, newest first, `maxResults` (1 to 1000) at a time.
async fn audit(
    state: &State,
    params: Vec<(String, String)>,
    headers: &hyper::HeaderMap,
) -> FakeResponse {
    let _in_flight = InFlight::new(&state.audit_in_flight, &state.audit_max_in_flight);
    let request = ApiRequest {
        file_id: String::new(),
        params,
        authorization: authorization(headers),
    };
    state
        .audit_requests
        .lock()
        .expect("audit requests lock")
        .push(request.clone());
    state.audit_rendezvous.join().await;
    tokio::time::sleep(state.behavior.delay).await;

    let behavior = &state.behavior;
    let later_page = request.param("pageToken").is_some();
    let failure = behavior.audit_error.as_ref().or(behavior
        .audit_error_after_first_page
        .as_ref()
        .filter(|_| later_page));
    if let Some((status, body)) = failure {
        let status = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return json_response(status, body);
    }
    let limited = {
        let mut left = state
            .audit_rate_limited_left
            .lock()
            .expect("rate limit lock");
        let limited = *left > 0;
        *left = left.saturating_sub(1);
        limited
    };
    if limited {
        let message = "Quota exceeded for quota metric 'Queries' of service \
                       'admin.googleapis.com'.";
        return json_response(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({"error": {
                "code": 429,
                "message": message,
                "errors": [{"domain": "global", "reason": "rateLimitExceeded", "message": message}],
            }}),
        );
    }

    let query = match AuditQuery::of(&request) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let mut records = behavior
        .audit_log
        .iter()
        .filter(|record| query.selects(record, behavior.audit_bounds))
        .collect::<Vec<_>>();
    records.sort_by_key(|record| std::cmp::Reverse(record_time(record)));

    let offset = match request.param("pageToken") {
        None => 0,
        Some(token) => match parse_token(token, &query.digest) {
            Some(offset) => offset,
            None => return error(StatusCode::BAD_REQUEST, "Invalid page token."),
        },
    };
    let page_size = behavior
        .audit_page_size
        .unwrap_or(query.max_results)
        .min(query.max_results);
    let end = (offset + page_size).min(records.len());
    let items = records.get(offset..end).unwrap_or_default();
    let mut body = json!({"kind": "admin#reports#activities"});
    // Google leaves `items` out of an empty page.
    if !items.is_empty() {
        body["items"] = json!(items);
    }
    if end < records.len() {
        body["nextPageToken"] = json!(token(end, &query.digest));
    }
    json_response(StatusCode::OK, &body)
}

/// `files.get`: the stored file with the top-level fields of the `fields` mask (all without one).
fn get(state: &State, id: &str, params: &[(String, String)]) -> FakeResponse {
    let Some(file) = state.file(id) else {
        return error(StatusCode::NOT_FOUND, "File not found.");
    };
    let mut file = file.to_json(true);
    let fields = params
        .iter()
        .find(|(key, _)| key == "fields")
        .map(|(_, fields)| top_level_fields(fields));
    if let (Some(fields), Some(object)) = (fields, file.as_object_mut()) {
        object.retain(|key, _| fields.contains(key));
    }
    json_response(StatusCode::OK, &file)
}

/// The top-level names of a fields mask: `a,b(c,d),e` gives `a`, `b`, `e`.
fn top_level_fields(mask: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut depth = 0_usize;
    for c in mask.chars().chain([',']) {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => fields.push(std::mem::take(&mut field).trim().to_owned()),
            c if depth == 0 => field.push(c),
            _ => {}
        }
    }
    fields
}

/// A page token is the offset and a digest of the query, so a token sent with another query is
/// rejected as Drive would.
fn token(offset: usize, q: &str) -> String {
    format!("{offset}-{:x}", md5::compute(q))
}

fn parse_token(token: &str, q: &str) -> Option<usize> {
    let (offset, digest) = token.split_once('-')?;
    (digest == format!("{:x}", md5::compute(q)))
        .then(|| offset.parse().ok())
        .flatten()
}

/// The ids of the `'<id>' in parents` terms of a query, unescaped.
pub(crate) fn parent_ids(q: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut chars = q.char_indices();
    while let Some((_, c)) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut literal = String::new();
        let mut end = q.len();
        while let Some((index, c)) = chars.next() {
            match c {
                '\\' => literal.extend(chars.next().map(|(_, escaped)| escaped)),
                '\'' => {
                    end = index + 1;
                    break;
                }
                c => literal.push(c),
            }
        }
        if q[end..].trim_start().starts_with("in parents") {
            ids.push(literal);
        }
    }
    ids
}

fn error(status: StatusCode, message: &str) -> FakeResponse {
    json_response(
        status,
        &json!({"error": {"code": status.as_u16(), "message": message}}),
    )
}

fn json_response(status: StatusCode, body: &Value) -> FakeResponse {
    hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())).boxed())
        .expect("valid response")
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use hyper::{Request, StatusCode};

    use serde_json::json;

    use super::{parent_ids, Behavior, Bounds, FakeDrive, FakeFile, FakeUser};
    use crate::common::media_client::MediaClient;

    #[tokio::test]
    async fn media_downloads_follow_range_and_if_range() {
        let blob = FakeFile::blob("b", "b.txt", "root", "0123456789");
        let drive = FakeDrive::start(vec![blob], Behavior::default()).await;
        let hub = drive.hub();
        let client = MediaClient::new(&hub);
        let url = client
            .drive_url(&["files", "b"], &[("alt", "media")])
            .unwrap();
        let send = |range: Option<&str>, if_range: Option<&str>| {
            let mut request = Request::get(url.as_str());
            if let Some(range) = range {
                request = request.header("range", range);
            }
            if let Some(if_range) = if_range {
                request = request.header("if-range", if_range);
            }
            let request = request
                .body(google_apis_common::to_body::<String>(None))
                .unwrap();
            let client = client.clone();
            async move {
                let response = client.send(request).await.unwrap();
                let (parts, body) = response.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                (parts.status, parts.headers, body)
            }
        };

        let (status, headers, body) = send(None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"0123456789");
        let etag = headers["etag"].to_str().unwrap().to_owned();
        assert_eq!(etag, format!("\"{:x}\"", md5::compute("0123456789")));

        let (status, headers, body) = send(Some("bytes=4-"), Some(&etag)).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers["content-range"], "bytes 4-9/10");
        assert_eq!(headers["content-length"], "6");
        assert_eq!(&body[..], b"456789");

        let (status, _, body) = send(Some("bytes=4-"), Some("\"other\"")).await;
        assert_eq!(status, StatusCode::OK, "another ETag: the whole content");
        assert_eq!(&body[..], b"0123456789");

        let (status, headers, _) = send(Some("bytes=10-"), None).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(headers["content-range"], "bytes */10");

        assert_eq!(
            drive.media_ranges(),
            [
                (String::from("b"), None),
                (String::from("b"), Some(String::from("bytes=4-"))),
                (String::from("b"), Some(String::from("bytes=4-"))),
                (String::from("b"), Some(String::from("bytes=10-"))),
            ]
        );
    }

    #[tokio::test]
    async fn about_names_the_account() {
        let drive = FakeDrive::start(
            Vec::new(),
            Behavior {
                account_email: Some(String::from("me@example.com")),
                ..Behavior::default()
            },
        )
        .await;
        let hub = drive.hub();
        let (_, about) = hub
            .about()
            .get()
            .param("fields", "user(emailAddress)")
            .doit()
            .await
            .unwrap();
        assert_eq!(
            about.user.and_then(|user| user.email_address).as_deref(),
            Some("me@example.com")
        );
    }

    /// The `items` of the audit log pages for `query`, one page per request, and the requests'
    /// statuses.
    async fn audit_pages(drive: &FakeDrive, query: &[(&str, &str)]) -> (Vec<Vec<String>>, u16) {
        let hub = drive.hub();
        let client = MediaClient::new(&hub);
        let mut pages = Vec::new();
        let mut token = None::<String>;
        loop {
            let mut url = url::Url::parse(hub.reports_base_url())
                .unwrap()
                .join("activity/users/all/applications/drive")
                .unwrap();
            url.query_pairs_mut().extend_pairs(query);
            if let Some(token) = &token {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let request = Request::get(url.as_str())
                .body(google_apis_common::to_body::<String>(None))
                .unwrap();
            let response = client.send(request).await.unwrap();
            let status = response.status().as_u16();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if status != 200 {
                return (pages, status);
            }
            let times = body["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| item["id"]["time"].as_str().unwrap().to_owned())
                .collect();
            pages.push(times);
            token = body["nextPageToken"].as_str().map(String::from);
            if token.is_none() {
                return (pages, status);
            }
        }
    }

    #[tokio::test]
    async fn the_audit_log_is_selected_by_time_and_parameters_and_paged() {
        let record = |time: &str, doc: &str, drive: &str| {
            json!({
                "id": {"time": time, "uniqueQualifier": "1"},
                "actor": {"email": "ana@example.com"},
                "events": [{"name": "view", "parameters": [
                    {"name": "doc_id", "value": doc},
                    {"name": "shared_drive_id", "value": drive},
                ]}],
            })
        };
        let behavior = Behavior {
            audit_log: vec![
                record("2026-11-01T00:00:00Z", "a", "d1"),
                record("2026-11-03T00:00:00Z", "a", "d2"),
                record("2026-11-02T00:00:00Z", "b", "d1"),
            ],
            audit_page_size: Some(1),
            ..Behavior::default()
        };
        let drive = FakeDrive::start(Vec::new(), behavior.clone()).await;
        let range = [
            ("startTime", "2026-11-01T00:00:00Z"),
            ("endTime", "2026-11-03T00:00:00Z"),
        ];

        let (pages, status) = audit_pages(&drive, &range).await;
        assert_eq!(status, 200);
        assert_eq!(
            pages,
            [["2026-11-02T00:00:00Z"], ["2026-11-01T00:00:00Z"]],
            "newest first, the end excluded"
        );
        let (pages, _) = audit_pages(&drive, &[("filters", "doc_id==a")]).await;
        assert_eq!(pages, [["2026-11-03T00:00:00Z"], ["2026-11-01T00:00:00Z"]]);
        let (pages, _) = audit_pages(&drive, &[("filters", "shared_drive_id==d2")]).await;
        assert_eq!(pages, [["2026-11-03T00:00:00Z"]]);
        let (pages, _) = audit_pages(&drive, &[("filters", "doc_id==none")]).await;
        assert_eq!(pages, [Vec::<String>::new()]);
        for bad in [
            &[("filters", "doc_id<>a")][..],
            &[("maxResults", "1001")],
            &[
                ("startTime", "2026-11-02T00:00:00Z"),
                ("endTime", "2026-11-01T00:00:00Z"),
            ],
            &[("startTime", "yesterday")],
        ] {
            assert_eq!(audit_pages(&drive, bad).await.1, 400, "{bad:?}");
        }
        assert_eq!(drive.audit_requests().len(), 2 + 2 + 1 + 1 + 4);
        assert_eq!(
            drive.audit_requests()[0].authorization.as_deref(),
            Some("Bearer mock-token")
        );
        assert_eq!(drive.writes(), Vec::<String>::new());

        let exclusive = FakeDrive::start(
            Vec::new(),
            Behavior {
                audit_bounds: Bounds::BothExcluded,
                ..behavior
            },
        )
        .await;
        let (pages, _) = audit_pages(&exclusive, &range).await;
        assert_eq!(pages, [["2026-11-02T00:00:00Z"]]);
    }

    #[tokio::test]
    async fn the_audit_log_answers_the_errors_it_is_given() {
        let drive = FakeDrive::start(
            Vec::new(),
            Behavior {
                audit_rate_limited: 1,
                ..Behavior::default()
            },
        )
        .await;
        assert_eq!(audit_pages(&drive, &[]).await.1, 429);
        assert_eq!(audit_pages(&drive, &[]).await.1, 200);

        let body = json!({"error": {"code": 403, "message": "No."}});
        let drive = FakeDrive::start(
            Vec::new(),
            Behavior {
                audit_error: Some((403, body)),
                ..Behavior::default()
            },
        )
        .await;
        assert_eq!(audit_pages(&drive, &[]).await.1, 403);
    }

    #[tokio::test]
    async fn sharing_lists_are_paged() {
        let permission =
            |email: &str| json!({"type": "user", "role": "reader", "emailAddress": email});
        let drive = FakeDrive::start(
            vec![
                FakeFile::folder("f", "f", "root"),
                FakeFile::folder("g", "g", "root"),
            ],
            Behavior {
                permissions: [(
                    String::from("f"),
                    vec![permission("a@x"), permission("b@x"), permission("c@x")],
                )]
                .into(),
                permissions_unreadable: [String::from("g")].into(),
                ..Behavior::default()
            },
        )
        .await;
        let hub = drive.hub();
        let (_, first) = hub
            .permissions()
            .list("f")
            .page_size(2)
            .doit()
            .await
            .unwrap();
        let emails = |list: &google_drive3::api::PermissionList| {
            list.permissions
                .iter()
                .flatten()
                .map(|permission| permission.email_address.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(emails(&first), ["a@x", "b@x"]);
        let token = first.next_page_token.unwrap();
        let (_, second) = hub
            .permissions()
            .list("f")
            .page_size(2)
            .page_token(&token)
            .doit()
            .await
            .unwrap();
        assert_eq!(emails(&second), ["c@x"]);
        assert!(second.next_page_token.is_none());
        assert!(hub.permissions().list("g").doit().await.is_err());
        assert!(hub.permissions().list("missing").doit().await.is_err());
        let requests = drive.permission_requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1].file_id, "f");
        assert_eq!(requests[1].param("pageToken"), Some(token.as_str()));
        assert!(drive.gets().is_empty(), "not a files.get");
    }

    #[tokio::test]
    async fn the_trash_users_and_revisions_are_answered() {
        let ana = FakeUser::new("Ana", "ana@example.com");
        let bob = FakeUser::named("Bob");
        let drive = FakeDrive::start(
            vec![
                FakeFile::folder("f", "f", "root"),
                FakeFile::blob("a", "a.txt", "f", "a")
                    .created("2026-11-01T00:00:00Z")
                    .modified_by(ana.clone())
                    .owned_by(bob),
                FakeFile::blob("t", "t.txt", "f", "t")
                    .trashed_by(Some(ana), Some("2026-11-02T00:00:00Z")),
                FakeFile::blob("r", "r.txt", "f", "r"),
                FakeFile::blob("x", "x.txt", "f", "x"),
            ],
            Behavior {
                revisions: [(
                    String::from("a"),
                    vec![json!({"id": "1", "modifiedTime": "2026-11-01T00:00:00Z"})],
                )]
                .into(),
                revisions_refused: [(String::from("r"), 403)].into(),
                unreadable_trash: [String::from("g")].into(),
                ..Behavior::default()
            },
        )
        .await;
        let hub = drive.hub();
        let list = |q: &'static str| {
            let hub = hub.clone();
            async move {
                let (_, list) = hub.files().list().q(q).doit().await.unwrap();
                list.files.unwrap_or_default()
            }
        };
        let ids = |files: &[google_drive3::api::File]| {
            files
                .iter()
                .map(|file| file.id.clone().unwrap())
                .collect::<Vec<_>>()
        };

        let kept = list("'f' in parents and trashed = false").await;
        assert_eq!(ids(&kept), ["a", "r", "x"]);
        let a = &kept[0];
        let user = a.last_modifying_user.as_ref().unwrap();
        assert_eq!(user.email_address.as_deref(), Some("ana@example.com"));
        assert_eq!(user.display_name.as_deref(), Some("Ana"));
        let owner = &a.owners.as_ref().unwrap()[0];
        assert_eq!(
            (
                owner.display_name.as_deref(),
                owner.email_address.as_deref()
            ),
            (Some("Bob"), None)
        );
        assert!(a.created_time.is_some() && a.trashing_user.is_none());

        let trashed = list("'f' in parents and trashed = true").await;
        assert_eq!(ids(&trashed), ["t"]);
        assert_eq!(trashed[0].explicitly_trashed, Some(true));
        assert!(trashed[0].trashed_time.is_some());
        assert_eq!(
            trashed[0]
                .trashing_user
                .as_ref()
                .unwrap()
                .email_address
                .as_deref(),
            Some("ana@example.com")
        );
        assert!(hub
            .files()
            .list()
            .q("'g' in parents and trashed = true")
            .doit()
            .await
            .is_err());
        assert!(hub
            .files()
            .list()
            .q("'g' in parents and trashed = false")
            .doit()
            .await
            .is_ok());

        let (_, revisions) = hub.revisions().list("a").doit().await.unwrap();
        assert_eq!(revisions.revisions.unwrap().len(), 1);
        let (_, none) = hub.revisions().list("x").doit().await.unwrap();
        assert!(none.revisions.unwrap_or_default().is_empty());
        assert!(hub.revisions().list("r").doit().await.is_err());
        assert!(hub.revisions().list("missing").doit().await.is_err());
        let requests = drive.revision_requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].file_id, "a");
        assert!(drive.gets().is_empty(), "not a files.get");
        assert_eq!(drive.writes(), Vec::<String>::new());
    }

    #[test]
    fn top_level_fields_skip_nested_masks() {
        assert_eq!(
            super::top_level_fields("id, name,shortcutDetails(targetId,targetMimeType),driveId"),
            ["id", "name", "shortcutDetails", "driveId"]
        );
    }

    #[test]
    fn parent_ids_reads_quoted_and_escaped_ids() {
        assert_eq!(
            parent_ids(r"('a' in parents or 'b\'c' in parents) and trashed = false"),
            ["a", "b'c"]
        );
        assert_eq!(
            parent_ids("name = 'x' and 'p' in parents"),
            ["p"],
            "other literals are not parents"
        );
    }
}
