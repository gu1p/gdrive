//! The relay between two in-process servers: a source Drive written here ([`SourceDrive`]),
//! which serves the content of its files generated on the fly, with `Range`, `If-Range` and a
//! strong `ETag`, and can fail a request, break a reply off or change a file while it is being
//! read; and as the destination, the strict mock of the upload protocol of the uploader's tests
//! (`upload/mock_drive.rs`). Both hubs use the retry policy for tests. The relays between two
//! `fake_drive` instances, which the command's tests use, are at the end.

use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::Duration,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::{body::Frame, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::{net::TcpSocket, task::JoinHandle};

use super::{check_supported, export_of, relay_file, RelayError, Unsupported, EXPORT_LIMIT};
use crate::{
    common::{
        drive_file::{
            DocType, MIME_TYPE_DOCX, MIME_TYPE_DRIVE_DOCUMENT, MIME_TYPE_DRIVE_DRAWING,
            MIME_TYPE_DRIVE_FOLDER, MIME_TYPE_DRIVE_PRESENTATION, MIME_TYPE_DRIVE_SHORTCUT,
            MIME_TYPE_DRIVE_SPREADSHEET, MIME_TYPE_PPTX, MIME_TYPE_XLSX,
        },
        media_client::MediaError,
        retry::StopReason,
    },
    hub::Hub,
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile},
        upload::{
            mock_drive::{Faults, Logged, MockDrive},
            UploadError, UploadOptions, UploadTarget, CHUNK_ALIGNMENT,
        },
    },
};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const CHUNK: u64 = CHUNK_ALIGNMENT;

const MODIFIED: &str = "2024-05-06T07:08:09.123Z";
const CREATED: &str = "2023-01-02T03:04:05.000Z";
const SOURCE_EMAIL: &str = "source@example.com";

// ---------------------------------------------------------------------------------------------
// Content

/// The period of the test content: a prime, so that no chunk or piece size is a multiple of it
/// and a byte sent at the wrong offset is noticed.
const PERIOD: usize = 1_000_003;

fn table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..PERIOD)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 56) as u8
            })
            .collect()
    })
}

/// The test content at `offset`, as much as `buf` holds: the table, repeated.
fn fill(offset: u64, buf: &mut [u8]) {
    let table = table();
    let mut at = usize::try_from(offset % PERIOD as u64).unwrap();
    let mut done = 0;
    while done < buf.len() {
        let len = (PERIOD - at).min(buf.len() - done);
        buf[done..done + len].copy_from_slice(&table[at..at + len]);
        done += len;
        at = 0;
    }
}

fn content(offset: u64, len: u64) -> Vec<u8> {
    let mut content = vec![0; usize::try_from(len).unwrap()];
    fill(offset, &mut content);
    content
}

fn md5_of(offset: u64, len: u64) -> String {
    format!("{:x}", md5::compute(content(offset, len)))
}

fn time(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

// ---------------------------------------------------------------------------------------------
// The source

/// Where the export of a Workspace file starts in the test content, so that it differs from
/// the blobs.
const EXPORT_OFFSET: u64 = 7;

/// A file of the source Drive. A blob's content is the test content from `seed` (0 unless it
/// changed), an export's from [`EXPORT_OFFSET`].
#[derive(Debug, Clone)]
struct SourceFile {
    id: String,
    name: String,
    mime: String,
    /// The size of a blob, of the export of a Workspace file.
    size: u64,
    /// The `md5Checksum` the metadata reports: the content's, another one, or none.
    md5: Option<String>,
    version: i64,
    seed: u64,
    /// The `resourceKey` of a file shared by link only: every request about it must name it.
    resource_key: Option<String>,
}

impl SourceFile {
    fn blob(id: &str, size: u64) -> Self {
        SourceFile {
            id: id.to_owned(),
            name: format!("{id}.bin"),
            mime: String::from("application/x-test"),
            size,
            md5: Some(md5_of(0, size)),
            version: 1,
            seed: 0,
            resource_key: None,
        }
    }

    /// A blob with no md5 in its metadata, which is too large to hash quickly in a test.
    fn large_blob(id: &str, size: u64) -> Self {
        SourceFile {
            md5: None,
            ..SourceFile::blob(id, 0)
        }
        .sized(size)
    }

    fn sized(self, size: u64) -> Self {
        SourceFile { size, ..self }
    }

    fn workspace(id: &str, mime: &str, export_size: u64) -> Self {
        SourceFile {
            id: id.to_owned(),
            name: format!("{id} document"),
            mime: mime.to_owned(),
            size: export_size,
            md5: None,
            version: 1,
            seed: 0,
            resource_key: None,
        }
    }

    fn is_blob(&self) -> bool {
        !self.mime.starts_with("application/vnd.google-apps.")
    }

    fn etag(&self) -> String {
        format!("\"{}-v{}\"", self.id, self.version)
    }

    /// The file resource of `files.get`.
    fn resource(&self) -> Value {
        let mut resource = json!({
            "id": self.id,
            "name": self.name,
            "mimeType": self.mime,
            "version": self.version.to_string(),
            "modifiedTime": MODIFIED,
            "createdTime": CREATED,
        });
        if self.is_blob() {
            resource["size"] = self.size.to_string().into();
        }
        if let Some(md5) = &self.md5 {
            resource["md5Checksum"] = md5.clone().into();
        }
        if let Some(key) = &self.resource_key {
            resource["resourceKey"] = key.clone().into();
        }
        resource
    }

    /// The file with another content of the same size: a new seed, version and `ETag`, and the
    /// md5 of the new content if it had one.
    fn changed(&mut self) {
        self.version += 1;
        self.seed += 1;
        if self.md5.is_some() {
            self.md5 = Some(md5_of(self.seed, self.size));
        }
    }

    /// What a listing has of the file: all of the resource.
    fn listing(&self) -> google_drive3::api::File {
        serde_json::from_value(self.resource()).unwrap()
    }
}

/// How the source deviates from a well-behaved Drive.
#[expect(clippy::struct_excessive_bools, reason = "independent faults")]
#[derive(Debug, Default, Clone)]
struct SourceFaults {
    /// Break the first media reply off once it has sent the content up to this offset.
    cut_at: Option<u64>,
    /// Answer the first media request with a 503.
    fail_first: bool,
    /// Once a media reply has sent the content up to this offset, the file changes (its md5,
    /// version and `ETag`) and the reply breaks off.
    change_at: Option<u64>,
    /// Answer a `Range` request with the whole content, like a server that ignores `Range`.
    ignore_range: bool,
    /// Change the file's `ETag` on every media reply, as if it changed with every request
    /// although its content does not.
    unstable_etag: bool,
    /// Break the first export reply off half-way.
    cut_export: bool,
    /// The file changes (another content of the same size, a new version and `ETag`) just
    /// before the media request with this number (from 0) is answered: after the relay checked
    /// the metadata, before it gets the content.
    change_before_media: Option<u32>,
    /// Send media replies without an `ETag`.
    no_etag: bool,
}

/// A request the source received.
#[derive(Debug, Clone)]
struct SourceRequest {
    path: String,
    query: String,
    range: Option<String>,
    if_range: Option<String>,
    resource_keys: Option<String>,
}

impl SourceRequest {
    fn param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    fn is_media(&self) -> bool {
        self.param("alt").as_deref() == Some("media")
    }

    /// A `files.get` of the metadata.
    fn is_metadata(&self) -> bool {
        self.path.starts_with("/drive/v3/files/") && !self.is_media() && !self.is_export()
    }

    fn is_export(&self) -> bool {
        self.path.ends_with("/export")
    }
}

#[derive(Debug)]
struct SourceState {
    files: Vec<SourceFile>,
    faults: SourceFaults,
    email: String,
    log: Vec<SourceRequest>,
    media_replies: u32,
    export_replies: u32,
}

type Body = UnsyncBoxBody<Bytes, io::Error>;

/// The send buffer of the source's connections.
const SEND_BUFFER: u32 = 64 * 1024;

/// The source Drive.
struct SourceDrive {
    addr: SocketAddr,
    state: Arc<Mutex<SourceState>>,
    /// Bytes of content the media and export replies have produced.
    served: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Drop for SourceDrive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SourceDrive {
    fn start(files: Vec<SourceFile>, faults: SourceFaults) -> Self {
        Self::start_as(files, faults, SOURCE_EMAIL)
    }

    fn start_as(files: Vec<SourceFile>, faults: SourceFaults, email: &str) -> Self {
        // A small send buffer (accepted connections inherit it), so that the bytes the source
        // has produced are close to the bytes the relay has read.
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_send_buffer_size(SEND_BUFFER).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(64).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(SourceState {
            files,
            faults,
            email: email.to_owned(),
            log: Vec::new(),
            media_replies: 0,
            export_replies: 0,
        }));
        let served = Arc::new(AtomicU64::new(0));
        let (accept_state, accept_served) = (Arc::clone(&state), Arc::clone(&served));
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (state, served) = (Arc::clone(&accept_state), Arc::clone(&accept_served));
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let (state, served) = (Arc::clone(&state), Arc::clone(&served));
                        async move { Ok::<_, io::Error>(handle(&state, &served, &request)) }
                    });
                    // Connections end with an error when a reply is broken off.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        SourceDrive {
            addr,
            state,
            served,
            task,
        }
    }

    fn hub(&self) -> Hub {
        Hub::for_mock_server(&format!("http://{}/", self.addr)).unwrap()
    }

    fn state(&self) -> MutexGuard<'_, SourceState> {
        self.state.lock().unwrap()
    }

    fn log(&self) -> Vec<SourceRequest> {
        self.state().log.clone()
    }

    fn media(&self) -> Vec<SourceRequest> {
        self.log()
            .into_iter()
            .filter(SourceRequest::is_media)
            .collect()
    }

    fn served(&self) -> u64 {
        self.served.load(Ordering::SeqCst)
    }

    fn file(&self, id: &str) -> SourceFile {
        self.state()
            .files
            .iter()
            .find(|file| file.id == id)
            .cloned()
            .unwrap()
    }
}

fn handle(
    state: &Arc<Mutex<SourceState>>,
    served: &Arc<AtomicU64>,
    request: &Request<hyper::body::Incoming>,
) -> Response<Body> {
    let header = |name| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(String::from)
    };
    let logged = SourceRequest {
        path: request.uri().path().to_owned(),
        query: request.uri().query().unwrap_or_default().to_owned(),
        range: header(hyper::header::RANGE),
        if_range: header(hyper::header::IF_RANGE),
        resource_keys: header(hyper::header::HeaderName::from_static(
            "x-goog-drive-resource-keys",
        )),
    };
    let mut guard = state.lock().unwrap();
    guard.log.push(logged.clone());
    if request.method() != Method::GET {
        return error(StatusCode::NOT_FOUND, "unexpected request");
    }
    if logged.path == "/drive/v3/about" {
        return json_reply(
            StatusCode::OK,
            &json!({"user": {"emailAddress": guard.email}}),
        );
    }
    let Some(id) = logged.path.strip_prefix("/drive/v3/files/") else {
        return error(StatusCode::NOT_FOUND, "unexpected request");
    };
    let id = id.trim_end_matches("/export");
    let Some(file) = guard.files.iter().find(|file| file.id == id).cloned() else {
        return error(StatusCode::NOT_FOUND, "File not found.");
    };
    // Like Drive, a file shared by link only is not found without its resource key.
    if let Some(key) = &file.resource_key {
        if logged.resource_keys.as_deref() != Some(format!("{id}/{key}").as_str()) {
            return error(StatusCode::NOT_FOUND, "File not found.");
        }
    }

    if logged.is_export() {
        let cut = guard.faults.cut_export && guard.export_replies == 0;
        guard.export_replies += 1;
        drop(guard);
        let stop = cut.then_some((file.size / 2, Stop::Cut));
        let content = Generated {
            id: file.id.clone(),
            base: EXPORT_OFFSET,
            start: 0,
            end: file.size,
            stop,
        };
        // No Content-Length, as Drive sends exports.
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", MIME_TYPE_DOCX)
            .body(content.body(state, served))
            .unwrap();
    }
    if !logged.is_media() {
        return json_reply(StatusCode::OK, &file.resource());
    }

    media_reply(state, served, guard, &file, &logged)
}

/// A media download of `file`, which the log holds already (`guard`).
fn media_reply(
    state: &Arc<Mutex<SourceState>>,
    served: &Arc<AtomicU64>,
    mut guard: MutexGuard<'_, SourceState>,
    file: &SourceFile,
    logged: &SourceRequest,
) -> Response<Body> {
    let id = file.id.as_str();
    let reply = guard.media_replies;
    guard.media_replies += 1;
    let faults = guard.faults.clone();
    let mut file = file.clone();
    if let Some(stored) = guard.files.iter_mut().find(|stored| stored.id == id) {
        if faults.change_before_media == Some(reply) {
            stored.changed();
            file = stored.clone();
        }
        if faults.unstable_etag {
            stored.version += 1;
        }
    }
    drop(guard);
    if faults.fail_first && reply == 0 {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Backend Error");
    }
    let etag = file.etag();
    let first = logged
        .range
        .as_deref()
        .and_then(|range| range.strip_prefix("bytes="))
        .and_then(|range| range.strip_suffix('-'))
        .and_then(|first| first.parse::<u64>().ok())
        .filter(|_| !faults.ignore_range)
        .filter(|_| logged.if_range.as_ref().is_none_or(|tag| *tag == etag) || faults.no_etag);
    let mut builder = Response::builder().header("content-type", file.mime.as_str());
    if !faults.no_etag {
        builder = builder.header("etag", etag.as_str());
    }
    let start = match first {
        Some(first) if first >= file.size => {
            return builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{}", file.size))
                .body(empty())
                .unwrap();
        }
        Some(first) => {
            builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
                "content-range",
                format!("bytes {first}-{}/{}", file.size - 1, file.size),
            );
            first
        }
        None => {
            builder = builder.status(StatusCode::OK);
            0
        }
    };
    let stop = [
        faults
            .cut_at
            .filter(|&at| reply == 0 && at > start)
            .map(|at| (at, Stop::Cut)),
        faults
            .change_at
            .filter(|&at| at > start && at < file.size)
            .map(|at| (at, Stop::Change)),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|(at, _)| *at);
    let content = Generated {
        id: file.id.clone(),
        base: file.seed,
        start,
        end: file.size,
        stop,
    };
    builder
        .header("content-length", file.size - start)
        .body(content.body(state, served))
        .unwrap()
}

/// How a generated body ends early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// It breaks off.
    Cut,
    /// The file changes, then it breaks off.
    Change,
}

/// Bytes per frame of a generated body.
const FRAME: u64 = 64 * KIB;

/// A body of generated content.
struct Generated {
    /// The file it is the content of.
    id: String,
    /// Where the content starts in the test content.
    base: u64,
    /// The offsets in the content of the first byte and past the last one.
    start: u64,
    end: u64,
    /// Where the body breaks off, and how.
    stop: Option<(u64, Stop)>,
}

impl Generated {
    /// The body, produced as it is sent and counted in `served`.
    fn body(self, state: &Arc<Mutex<SourceState>>, served: &Arc<AtomicU64>) -> Body {
        let (state, served) = (Arc::clone(state), Arc::clone(served));
        let Generated {
            id,
            base,
            start,
            end,
            stop,
        } = self;
        let frames = futures::stream::unfold(Some(start), move |position| {
            let (state, served, id) = (Arc::clone(&state), Arc::clone(&served), id.clone());
            async move {
                let position = position?;
                if let Some((at, how)) = stop.filter(|(at, _)| *at == position) {
                    if how == Stop::Change {
                        let mut state = state.lock().unwrap();
                        if let Some(file) = state.files.iter_mut().find(|file| file.id == id) {
                            file.version += 1;
                            file.md5 = Some(md5_of(1, 1));
                        }
                    }
                    // Let the frames before go out first.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    return Some((Err(io::Error::other(format!("cut at {at}"))), None));
                }
                if position >= end {
                    return None;
                }
                let limit = stop.map_or(end, |(at, _)| at.min(end));
                let len = FRAME.min(limit - position);
                let mut data = vec![0; usize::try_from(len).unwrap()];
                fill(base + position, &mut data);
                served.fetch_add(len, Ordering::SeqCst);
                Some((Ok(Frame::data(Bytes::from(data))), Some(position + len)))
            }
        });
        StreamBody::new(frames).boxed_unsync()
    }
}

fn empty() -> Body {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn json_reply(status: StatusCode, body: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(body.to_string()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    json_reply(
        status,
        &json!({"error": {"code": status.as_u16(), "message": message}}),
    )
}

// ---------------------------------------------------------------------------------------------
// Relays

/// Resumable uploads in chunks of `chunk` bytes.
fn options(chunk: u64) -> UploadOptions {
    UploadOptions {
        chunk_size: chunk,
        multipart_threshold: 0,
        ..UploadOptions::default()
    }
}

fn target() -> UploadTarget {
    UploadTarget {
        parents: vec![String::from("dest-folder")],
        ..UploadTarget::default()
    }
}

async fn relay(
    source: &SourceDrive,
    dest: &MockDrive,
    file: &google_drive3::api::File,
    target: &UploadTarget,
    options: &UploadOptions,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<google_drive3::api::File, RelayError> {
    let (source_hub, dest_hub) = (source.hub(), dest.hub());
    let call = relay_file(&source_hub, &dest_hub, file, target, options, progress);
    match tokio::time::timeout(Duration::from_secs(120), call).await {
        Ok(result) => result,
        Err(elapsed) => panic!(
            "the relay did not finish ({elapsed})\nsource: {:?}\ndestination:\n{}",
            source.log(),
            dest.log()
        ),
    }
}

/// The relay succeeded and the destination holds exactly the source's bytes in one file.
fn assert_copied(
    dest: &MockDrive,
    result: Result<google_drive3::api::File, RelayError>,
    size: u64,
) -> google_drive3::api::File {
    let file = result.unwrap_or_else(|error| panic!("{error:?}\n{}", dest.log()));
    let state = dest.state();
    assert_eq!(state.files.len(), 1, "{}", state.log_text());
    let id = file.id.clone().unwrap();
    let stored = &state.files[&id];
    assert!(
        stored.content == content(0, size),
        "the copy must hold the source's bytes: {} of {size} bytes\n{}",
        stored.content.len(),
        state.log_text()
    );
    file
}

// ---------------------------------------------------------------------------------------------
// What can be relayed

fn file_of(mime: Option<&str>) -> google_drive3::api::File {
    google_drive3::api::File {
        id: Some(String::from("id")),
        mime_type: mime.map(String::from),
        ..google_drive3::api::File::default()
    }
}

#[test]
fn blobs_and_exportable_workspace_files_are_supported() {
    for mime in [
        Some("application/pdf"),
        Some("image/jpeg"),
        Some("application/octet-stream"),
        Some(MIME_TYPE_DRIVE_DOCUMENT),
        Some(MIME_TYPE_DRIVE_SPREADSHEET),
        Some(MIME_TYPE_DRIVE_PRESENTATION),
        // The relay fetches the type first.
        None,
    ] {
        assert_eq!(check_supported(&file_of(mime)), Ok(()), "{mime:?}");
    }
}

#[test]
fn shortcuts_and_workspace_types_without_export_are_not() {
    assert_eq!(
        check_supported(&file_of(Some(MIME_TYPE_DRIVE_SHORTCUT))),
        Err(Unsupported::Shortcut)
    );
    for mime in [
        // Drive converts no export back to a drawing.
        MIME_TYPE_DRIVE_DRAWING,
        "application/vnd.google-apps.form",
        "application/vnd.google-apps.site",
        "application/vnd.google-apps.map",
        "application/vnd.google-apps.jam",
        "application/vnd.google-apps.script",
        "application/vnd.google-apps.unknown-new-type",
        MIME_TYPE_DRIVE_FOLDER,
    ] {
        assert_eq!(
            check_supported(&file_of(Some(mime))),
            Err(Unsupported::NativeType(mime.to_owned())),
            "{mime}"
        );
    }
}

#[test]
fn docs_sheets_and_slides_are_exported_as_office_files_and_converted_back() {
    let formats: Vec<_> = DocType::ALL
        .iter()
        .filter_map(|doc_type| {
            let export = export_of(doc_type.mime().as_ref())?;
            assert_eq!(export.doc_type, *doc_type);
            Some((doc_type.mime().to_string(), export.format.to_string()))
        })
        .collect();
    assert_eq!(
        formats,
        [
            (
                MIME_TYPE_DRIVE_DOCUMENT.to_owned(),
                MIME_TYPE_DOCX.to_owned()
            ),
            (
                MIME_TYPE_DRIVE_SPREADSHEET.to_owned(),
                MIME_TYPE_XLSX.to_owned()
            ),
            (
                MIME_TYPE_DRIVE_PRESENTATION.to_owned(),
                MIME_TYPE_PPTX.to_owned()
            ),
        ],
        "drawings are not exported"
    );
    assert!(export_of("application/pdf").is_none());
}

#[test]
fn messages_are_short_phrases_without_urls_or_tokens() {
    let messages = [
        (
            Unsupported::Shortcut.to_string(),
            "shortcuts have no content",
        ),
        (
            Unsupported::NativeType(String::from("application/vnd.google-apps.form")).to_string(),
            "Google Forms cannot be exported",
        ),
        (
            Unsupported::NativeType(String::from(MIME_TYPE_DRIVE_DRAWING)).to_string(),
            "Google Drawings cannot be copied: Drive converts no export back to a drawing",
        ),
        (
            Unsupported::NativeType(String::from("application/vnd.google-apps.new")).to_string(),
            "files of type application/vnd.google-apps.new cannot be exported",
        ),
        (
            RelayError::Unsupported(Unsupported::Shortcut).to_string(),
            "shortcuts have no content",
        ),
        (
            RelayError::Source(MediaError::Resume {
                reason: StopReason::NotFound,
            })
            .to_string(),
            "unable to read the source file",
        ),
        (
            RelayError::ExportTooLarge {
                limit: EXPORT_LIMIT,
            }
            .to_string(),
            "the exported file is larger than the 10.5 MB that the Drive API exports",
        ),
        (
            RelayError::Upload(UploadError::SessionsExpired { sessions: 4 }).to_string(),
            "unable to upload the copy",
        ),
        (
            RelayError::Md5Mismatch {
                expected: String::from("aaaa"),
                actual: String::from("bbbb"),
            }
            .to_string(),
            "the copy does not match the source: its md5 is bbbb, the source's is aaaa",
        ),
        (
            RelayError::SourceChanged.to_string(),
            "the source file changed while it was being copied; copy it again",
        ),
        (
            RelayError::SourceMetadata(Box::new(google_drive3::Error::Cancelled)).to_string(),
            "unable to read the metadata of the source file",
        ),
    ];
    for (message, expected) in messages {
        assert_eq!(message, expected);
    }
    assert_eq!(
        RelayError::Unsupported(Unsupported::Shortcut).unsupported(),
        Some(&Unsupported::Shortcut)
    );
    assert_eq!(RelayError::SourceChanged.unsupported(), None);
}

/// The message `gdrive` prints for an error: it and its sources.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[tokio::test]
async fn a_failed_source_is_reported_without_its_url_or_token() {
    let source = SourceDrive::start(Vec::new(), SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;
    let listing = SourceFile::blob("gone", 1000).listing();

    let error = relay(&source, &dest, &listing, &target(), &options(CHUNK), None)
        .await
        .unwrap_err();

    assert!(matches!(error, RelayError::Source(_)), "{error:?}");
    let message = chain(&error);
    assert_eq!(
        message,
        "unable to read the source file: Drive answered 404 Not Found: File not found."
    );
    for secret in ["http", "127.0.0.1", "token", "Bearer"] {
        assert!(!message.contains(secret), "{message}");
    }
    assert!(dest.state().files.is_empty());
}

#[tokio::test]
async fn drive_error_replies_of_the_destination_stay_in_the_error_chain() {
    for (status, reason, message) in [
        (400, "badRequest", "Invalid value for parents."),
        (404, "notFound", "File not found: dest-folder."),
    ] {
        let file = SourceFile::blob("blob", 2 * CHUNK);
        let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
        let reply = json!({"error": {
            "code": status,
            "message": message,
            "errors": [{"domain": "global", "reason": reason, "message": message}],
        }});
        let dest = MockDrive::start(Faults {
            reject_start: Some((status, reply)),
            ..Faults::default()
        })
        .await;

        let error = relay(
            &source,
            &dest,
            &file.listing(),
            &target(),
            &options(CHUNK),
            None,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, RelayError::Upload(_)), "{error:?}");
        let message_chain = chain(&error);
        assert!(
            message_chain.starts_with(&format!(
                "unable to upload the copy: starting the upload failed: Drive answered {status}"
            )),
            "{message_chain}"
        );
        assert!(message_chain.contains(message), "{message_chain}");
        // Google's error as google-drive3 reports one, which the account hints look for.
        let mut cause = std::error::Error::source(&error);
        let mut google = None;
        while let Some(error) = cause {
            if let Some(google_drive3::Error::BadRequest(json)) =
                error.downcast_ref::<google_drive3::Error>()
            {
                google = Some(json.clone());
            }
            cause = error.source();
        }
        assert_eq!(google.unwrap()["error"]["errors"][0]["reason"], reason);
    }
}

#[test]
fn relay_errors_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<RelayError>();
}

#[tokio::test]
async fn a_relay_can_be_spawned() {
    fn assert_send<T: Send>(_: &T) {}
    let source = SourceDrive::start(Vec::new(), SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;
    let (source_hub, dest_hub) = (source.hub(), dest.hub());
    let listing = SourceFile::blob("a", 10).listing();
    let (target, options) = (target(), options(CHUNK));
    let call = relay_file(&source_hub, &dest_hub, &listing, &target, &options, None);
    assert_send(&call);
}

// ---------------------------------------------------------------------------------------------
// Blobs

/// 100 MiB in chunks of 8 MiB: the destination compares what it gets with the content instead
/// of keeping it, the source produces it as it is sent, so the test holds no copy of it. The
/// source never gets ahead of what the destination committed by more than the chunk in flight
/// and what the loopback connections buffer: the relay holds no more than a few pieces. The
/// md5s are left out (`verify_md5`, a source without md5), which take seconds over 100 MiB in a
/// debug build; the other tests check them.
#[tokio::test]
async fn a_large_blob_streams_through_in_chunks_without_being_held() {
    const SIZE: u64 = 100 * MIB + 12_345;
    const LARGE_CHUNK: u64 = 8 * MIB;
    let source = SourceDrive::start(
        vec![SourceFile::large_blob("large", SIZE)],
        SourceFaults::default(),
    );
    let dest = MockDrive::start_checking(Faults::default(), fill).await;
    let served = Arc::clone(&source.served);
    let lead = AtomicU64::new(0);
    let reported = Mutex::new(Vec::new());
    let progress = |committed: u64| {
        lead.fetch_max(
            served.load(Ordering::SeqCst).saturating_sub(committed),
            Ordering::SeqCst,
        );
        reported.lock().unwrap().push(committed);
    };
    let options = UploadOptions {
        verify_md5: false,
        ..options(LARGE_CHUNK)
    };

    // The listing has no md5: the relay asks for the metadata first.
    let listing = google_drive3::api::File {
        md5_checksum: None,
        ..source.file("large").listing()
    };
    let file = relay(
        &source,
        &dest,
        &listing,
        &target(),
        &options,
        Some(&progress),
    )
    .await
    .unwrap_or_else(|error| panic!("{error:?}\n{}", dest.log()));

    let state = dest.state();
    let id = file.id.unwrap();
    assert_eq!(state.files[&id].checked_size, Some(SIZE));
    assert_eq!(state.sessions.len(), 1);
    assert_eq!(state.sessions[0].committed, SIZE);
    assert_eq!(
        state.sessions[0].first_mismatch, None,
        "the copy holds the source's bytes"
    );
    let starts = state.chunk_starts();
    assert_eq!(starts.len(), 13, "{}", state.log_text());
    assert!(starts
        .iter()
        .enumerate()
        .all(|(n, start)| *start == n as u64 * LARGE_CHUNK));
    drop(state);

    let media = source.media();
    assert_eq!(media.len(), 1, "read in one go: {media:?}");
    assert_eq!(media[0].range, None);
    assert_eq!(source.served(), SIZE, "every byte is read once");
    let gets: Vec<_> = source
        .log()
        .into_iter()
        .filter(SourceRequest::is_metadata)
        .collect();
    assert_eq!(gets.len(), 1);
    assert!(gets[0]
        .param("fields")
        .is_some_and(|fields| fields.contains("md5Checksum") && fields.contains("size")));

    let lead = lead.load(Ordering::SeqCst);
    assert!(
        lead <= 3 * LARGE_CHUNK,
        "the source got {lead} bytes ahead of the destination"
    );
    let reported = reported.into_inner().unwrap();
    assert_eq!(reported.len(), 13);
    assert_eq!(reported.last(), Some(&SIZE));
}

#[tokio::test]
async fn a_failed_destination_chunk_is_read_again_from_the_committed_offset() {
    let size = 6 * CHUNK + 999;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults {
        fail_chunk_at: Some(2 * CHUNK),
        ..Faults::default()
    })
    .await;

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await;

    let copy = assert_copied(&dest, result, size);
    assert_eq!(copy.md5_checksum, file.md5);
    assert_eq!(
        dest.state().chunk_starts(),
        [
            0,
            CHUNK,
            2 * CHUNK,
            2 * CHUNK,
            3 * CHUNK,
            4 * CHUNK,
            5 * CHUNK,
            6 * CHUNK
        ]
    );
    let log = source.log();
    let media: Vec<_> = log.iter().filter(|request| request.is_media()).collect();
    assert_eq!(media.len(), 2, "{log:?}");
    assert_eq!(media[0].range, None);
    assert_eq!(media[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(media[1].if_range.as_deref(), Some("\"blob-v1\""));
    let checked_before = log
        .iter()
        .take_while(|request| request.range.is_none())
        .filter(|request| request.is_metadata())
        .count();
    assert_eq!(
        checked_before, 1,
        "the metadata is checked before reading again"
    );
    assert!(log
        .iter()
        .all(
            |request| request.param("supportsAllDrives").as_deref() == Some("true")
                || !request.is_media()
        ));
}

#[tokio::test]
async fn a_source_reply_that_breaks_off_is_resumed_with_range_and_if_range() {
    let size = 5 * CHUNK + 77;
    let cut = 1_234_567;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(
        vec![file.clone()],
        SourceFaults {
            cut_at: Some(cut),
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await;

    assert_copied(&dest, result, size);
    let media = source.media();
    assert_eq!(media.len(), 2, "{media:?}");
    assert_eq!(media[1].range, Some(format!("bytes={cut}-")));
    assert_eq!(media[1].if_range.as_deref(), Some("\"blob-v1\""));
    assert_eq!(
        dest.state().chunk_starts(),
        [0, CHUNK, 2 * CHUNK, 3 * CHUNK, 4 * CHUNK, 5 * CHUNK],
        "the destination saw nothing of it"
    );
}

#[tokio::test]
async fn a_failed_source_request_is_retried() {
    let size = 2 * CHUNK;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(
        vec![file.clone()],
        SourceFaults {
            fail_first: true,
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await;

    assert_copied(&dest, result, size);
    assert_eq!(source.media().len(), 2);
}

#[tokio::test]
async fn a_reply_with_all_of_the_content_to_a_range_request_is_read_up_to_the_offset() {
    let size = 3 * MIB;
    let cut = MIB + 5;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(
        vec![file.clone()],
        SourceFaults {
            cut_at: Some(cut),
            ignore_range: true,
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await;

    assert_copied(&dest, result, size);
    let media = source.media();
    assert_eq!(media.len(), 2, "{media:?}");
    assert_eq!(media[1].range, Some(format!("bytes={cut}-")));
    assert_eq!(
        source.served(),
        cut + size,
        "the start is read again and dropped"
    );
}

#[tokio::test]
async fn far_from_the_start_a_failed_if_range_is_asked_again_with_the_range_alone() {
    let size = 6 * MIB;
    let cut = 5 * MIB;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(
        vec![file.clone()],
        SourceFaults {
            cut_at: Some(cut),
            unstable_etag: true,
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(MIB),
        None,
    )
    .await;

    assert_copied(&dest, result, size);
    let media = source.media();
    let ranges: Vec<_> = media
        .iter()
        .map(|request| (request.range.clone(), request.if_range.clone()))
        .collect();
    assert_eq!(
        ranges,
        [
            (None, None),
            (
                Some(format!("bytes={cut}-")),
                Some(String::from("\"blob-v1\""))
            ),
            (Some(format!("bytes={cut}-")), None),
        ]
    );
}

#[tokio::test]
async fn a_source_that_changes_while_it_is_read_fails_with_source_changed() {
    let size = 4 * CHUNK;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(
        vec![file.clone()],
        SourceFaults {
            change_at: Some(600_000),
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;

    let error = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, RelayError::SourceChanged), "{error:?}");
    assert_eq!(source.media().len(), 1, "the changed file is not read on");
    assert!(source.log().last().unwrap().is_metadata());
    let state = dest.state();
    assert!(state.files.is_empty(), "{}", state.log_text());
    assert!(state.sessions[0].committed < size);
}

#[tokio::test]
async fn a_source_that_changes_between_the_check_and_the_resume_fails_with_source_changed() {
    for md5 in [false, true] {
        let size = 4 * CHUNK;
        let blob = SourceFile::blob("blob", size);
        let file = SourceFile {
            md5: blob.md5.clone().filter(|_| md5),
            ..blob
        };
        let source = SourceDrive::start(
            vec![file.clone()],
            SourceFaults {
                cut_at: Some(600_000),
                // After the metadata check before the resume: the same size, other bytes.
                change_before_media: Some(1),
                ..SourceFaults::default()
            },
        );
        let dest = MockDrive::start(Faults::default()).await;

        let error = relay(
            &source,
            &dest,
            &file.listing(),
            &target(),
            &options(CHUNK),
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, RelayError::SourceChanged),
            "md5 {md5}: {error:?}"
        );
        let log = source.log();
        let media: Vec<_> = log.iter().filter(|request| request.is_media()).collect();
        assert_eq!(media.len(), 2, "{log:?}");
        assert_eq!(media[1].range.as_deref(), Some("bytes=600000-"));
        assert_eq!(media[1].if_range.as_deref(), Some("\"blob-v1\""));
        let checked = log
            .iter()
            .take_while(|request| request.range.is_none())
            .filter(|request| request.is_metadata())
            .count();
        // Without an md5 in the listing, the metadata is fetched first too.
        assert_eq!(checked, if md5 { 1 } else { 2 }, "{log:?}");
        let state = dest.state();
        assert!(state.files.is_empty(), "no copy: {}", state.log_text());
    }
}

#[tokio::test]
async fn a_source_without_md5_or_etag_is_not_resumed() {
    let size = 4 * CHUNK;
    let faults = SourceFaults {
        cut_at: Some(600_000),
        no_etag: true,
        ..SourceFaults::default()
    };
    let unproven = SourceFile {
        md5: None,
        ..SourceFile::blob("blob", size)
    };
    let source = SourceDrive::start(vec![unproven.clone()], faults.clone());
    let dest = MockDrive::start(Faults::default()).await;

    let error = relay(
        &source,
        &dest,
        &unproven.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, RelayError::SourceChanged), "{error:?}");
    assert_eq!(source.media().len(), 1, "no resume is asked for");
    assert!(dest.state().files.is_empty());

    // With an md5, the metadata proves the content the same: resumed without If-Range.
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(vec![file.clone()], faults);
    let dest = MockDrive::start(Faults::default()).await;
    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await;
    assert_copied(&dest, result, size);
    let media = source.media();
    assert_eq!(media.len(), 2);
    assert_eq!(media[1].range.as_deref(), Some("bytes=600000-"));
    assert_eq!(media[1].if_range, None);
}

#[tokio::test]
async fn a_file_shared_by_link_is_read_with_its_resource_key() {
    let size = 3 * CHUNK;
    let blob = SourceFile {
        resource_key: Some(String::from("key-1")),
        ..SourceFile::blob("blob", size)
    };
    let doc = SourceFile {
        resource_key: Some(String::from("key-2")),
        ..SourceFile::workspace("doc", MIME_TYPE_DRIVE_DOCUMENT, 10 * KIB)
    };
    let source = SourceDrive::start(
        vec![blob.clone(), doc.clone()],
        SourceFaults {
            cut_at: Some(400_000),
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;
    let options = UploadOptions {
        multipart_threshold: 100 * KIB,
        ..options(CHUNK)
    };

    // The listing has no md5: the metadata is fetched as well as the content and a resume.
    let listing = google_drive3::api::File {
        md5_checksum: None,
        ..blob.listing()
    };
    let result = relay(&source, &dest, &listing, &target(), &options, None).await;
    assert_copied(&dest, result, size);
    let copy = relay(&source, &dest, &doc.listing(), &target(), &options, None)
        .await
        .unwrap_or_else(|error| panic!("{error:?}\n{:?}", source.log()));
    assert_eq!(
        dest.state().files[copy.id.as_deref().unwrap()].content,
        content(EXPORT_OFFSET, 10 * KIB)
    );

    let log = source.log();
    let count = |what: fn(&SourceRequest) -> bool| log.iter().filter(|r| what(r)).count();
    assert_eq!(count(SourceRequest::is_media), 2, "{log:?}");
    assert_eq!(
        count(SourceRequest::is_metadata),
        2,
        "a fetch and a check: {log:?}"
    );
    assert_eq!(count(SourceRequest::is_export), 1);
    for request in &log {
        let keys = if request.path.contains("/doc") {
            "doc/key-2"
        } else {
            "blob/key-1"
        };
        assert_eq!(request.resource_keys.as_deref(), Some(keys), "{request:?}");
    }

    // Without its key, Drive does not find the file.
    let keyless = google_drive3::api::File {
        resource_key: None,
        ..blob.listing()
    };
    let error = relay(&source, &dest, &keyless, &target(), &options, None)
        .await
        .unwrap_err();
    assert_eq!(
        chain(&error),
        "unable to read the source file: Drive answered 404 Not Found: File not found."
    );
}

#[tokio::test]
async fn a_source_changed_since_its_listing_fails_with_source_changed() {
    let size = 2 * CHUNK;
    let listed = SourceFile {
        md5: Some(md5_of(3, size)),
        ..SourceFile::blob("blob", size)
    };
    let source = SourceDrive::start(
        vec![SourceFile::blob("blob", size)],
        SourceFaults::default(),
    );
    let dest = MockDrive::start(Faults::default()).await;

    let error = relay(
        &source,
        &dest,
        &listed.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, RelayError::SourceChanged), "{error:?}");
}

#[tokio::test]
async fn a_copy_whose_md5_is_not_the_sources_fails_with_md5_mismatch() {
    let size = 2 * CHUNK + 1;
    let wrong = md5_of(3, size);
    let file = SourceFile {
        md5: Some(wrong.clone()),
        ..SourceFile::blob("blob", size)
    };
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;

    let error = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await
    .unwrap_err();

    match error {
        RelayError::Md5Mismatch { expected, actual } => {
            assert_eq!(expected, wrong);
            assert_eq!(actual, md5_of(0, size));
        }
        error => panic!("{error:?}"),
    }
}

#[tokio::test]
async fn the_uploaders_own_md5_check_is_kept() {
    let size = 2 * CHUNK + 1;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults {
        wrong_md5: true,
        ..Faults::default()
    })
    .await;

    let error = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await
    .unwrap_err();

    match error {
        RelayError::Md5Mismatch { expected, actual } => {
            assert_eq!(expected, md5_of(0, size), "the md5 of the bytes read");
            assert_eq!(actual, format!("{:x}", md5::compute(b"something else")));
        }
        error => panic!("{error:?}"),
    }
}

#[tokio::test]
async fn a_small_blob_goes_up_in_one_multipart_request_with_the_sources_type_and_times() {
    let size = 100 * KIB;
    let file = SourceFile {
        mime: String::from("image/jpeg"),
        ..SourceFile::blob("photo", size)
    };
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;
    let target = UploadTarget {
        new_file_id: Some(String::from("pre-generated")),
        // Ignored: the copy has the source's type.
        mime: Some(mime::TEXT_PLAIN),
        convert_to: Some(mime::TEXT_PLAIN),
        existing_file_id: Some(String::from("ignored")),
        ..target()
    };
    let reported = Mutex::new(Vec::new());
    let progress = |offset| reported.lock().unwrap().push(offset);

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target,
        &UploadOptions::default(),
        Some(&progress),
    )
    .await;

    let copy = assert_copied(&dest, result, size);
    assert_eq!(copy.id.as_deref(), Some("pre-generated"));
    assert_eq!(copy.md5_checksum, file.md5);
    assert_eq!(copy.size, Some(i64::try_from(size).unwrap()));
    assert_eq!(copy.modified_time, Some(time(MODIFIED)));
    assert_eq!(copy.created_time, Some(time(CREATED)));
    let state = dest.state();
    let uploads = state.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].method, Method::POST);
    assert_eq!(
        uploads[0].query_param("uploadType").as_deref(),
        Some("multipart")
    );
    let stored = &state.files["pre-generated"];
    assert_eq!(stored.content_type, "image/jpeg");
    assert_eq!(
        stored.metadata,
        json!({
            "id": "pre-generated",
            "name": "photo.bin",
            "mimeType": "image/jpeg",
            "parents": ["dest-folder"],
            "modifiedTime": MODIFIED,
            "createdTime": CREATED,
        })
    );
    assert_eq!(state.generated_ids, 0);
    drop(state);
    assert_eq!(*reported.lock().unwrap(), [size]);
    let log = source.log();
    assert_eq!(log.len(), 1, "the listing is complete: {log:?}");
    assert!(log[0].is_media() && log[0].range.is_none());
}

#[tokio::test]
async fn a_resumable_copy_gets_the_sources_name_type_and_times_too() {
    let size = 3 * CHUNK;
    let file = SourceFile::blob("blob", size);
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;
    let target = UploadTarget {
        name: Some(String::from("renamed.bin")),
        new_file_id: Some(String::from("pre-generated")),
        ..target()
    };

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target,
        &options(CHUNK),
        None,
    )
    .await;

    let copy = assert_copied(&dest, result, size);
    assert_eq!(copy.id.as_deref(), Some("pre-generated"));
    assert_eq!(copy.size, Some(i64::try_from(size).unwrap()));
    assert_eq!(copy.modified_time, Some(time(MODIFIED)));
    assert_eq!(copy.created_time, Some(time(CREATED)));
    let state = dest.state();
    assert_eq!(state.generated_ids, 0, "the pre-generated id is used");
    assert_eq!(
        state.sessions[0].metadata,
        json!({
            "id": "pre-generated",
            "name": "renamed.bin",
            "mimeType": "application/x-test",
            "parents": ["dest-folder"],
            "modifiedTime": MODIFIED,
            "createdTime": CREATED,
        })
    );
    assert_eq!(state.sessions[0].content_type, "application/x-test");
}

#[tokio::test]
async fn an_empty_blob_is_copied_without_reading_it() {
    let file = SourceFile::blob("empty", 0);
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;

    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &options(CHUNK),
        None,
    )
    .await;

    assert_copied(&dest, result, 0);
    assert!(source.log().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Workspace files

#[tokio::test]
async fn a_workspace_document_is_exported_and_converted_back() {
    let size = 50 * KIB;
    let file = SourceFile::workspace("doc", MIME_TYPE_DRIVE_DOCUMENT, size);
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;
    let target = UploadTarget {
        new_file_id: Some(String::from("not-for-conversions")),
        ..target()
    };

    let copy = relay(
        &source,
        &dest,
        &file.listing(),
        &target,
        &UploadOptions::default(),
        None,
    )
    .await
    .unwrap_or_else(|error| panic!("{error:?}\n{}", dest.log()));

    let exports: Vec<_> = source
        .log()
        .into_iter()
        .filter(SourceRequest::is_export)
        .collect();
    assert_eq!(exports.len(), 1);
    assert_eq!(
        exports[0].param("mimeType").as_deref(),
        Some(MIME_TYPE_DOCX)
    );
    assert_eq!(copy.mime_type.as_deref(), Some(MIME_TYPE_DRIVE_DOCUMENT));
    let state = dest.state();
    let stored = &state.files[copy.id.as_deref().unwrap()];
    assert_eq!(stored.content, content(EXPORT_OFFSET, size));
    assert_eq!(stored.content_type, MIME_TYPE_DOCX);
    assert_eq!(stored.metadata["mimeType"], MIME_TYPE_DRIVE_DOCUMENT);
    assert_eq!(stored.metadata["name"], "doc document");
    assert_eq!(stored.metadata["modifiedTime"], MODIFIED);
    assert!(
        stored.metadata.get("id").is_none(),
        "Drive takes no id for a conversion"
    );
    assert!(stored.metadata["appProperties"]["gdriveUploadId"].is_string());
}

#[tokio::test]
async fn a_large_export_is_resumable_and_one_that_breaks_off_is_exported_again() {
    let size = 3 * MIB;
    let file = SourceFile::workspace("sheet", MIME_TYPE_DRIVE_SPREADSHEET, size);
    let source = SourceDrive::start(
        vec![file.clone()],
        SourceFaults {
            cut_export: true,
            ..SourceFaults::default()
        },
    );
    let dest = MockDrive::start(Faults::default()).await;
    let options = UploadOptions {
        session_dir: Some(PathBuf::from("/nonexistent/never-written")),
        ..options(MIB)
    };

    let copy = relay(&source, &dest, &file.listing(), &target(), &options, None)
        .await
        .unwrap_or_else(|error| panic!("{error:?}\n{}", dest.log()));

    let state = dest.state();
    assert_eq!(state.sessions.len(), 1);
    assert_eq!(state.sessions[0].content_type, MIME_TYPE_XLSX);
    assert_eq!(
        state.files[copy.id.as_deref().unwrap()].content,
        content(EXPORT_OFFSET, size)
    );
    drop(state);
    let log = source.log();
    assert_eq!(log.iter().filter(|request| request.is_export()).count(), 2);
    assert!(
        log.iter().all(|request| request.path != "/drive/v3/about"),
        "an export is not recorded: {log:?}"
    );
}

#[tokio::test]
async fn an_export_over_the_limit_fails_as_soon_as_it_is_too_large() {
    let file = SourceFile::workspace("slides", MIME_TYPE_DRIVE_PRESENTATION, 64 * MIB);
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults::default()).await;

    let error = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &UploadOptions::default(),
        None,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, RelayError::ExportTooLarge { limit } if limit == EXPORT_LIMIT),
        "{error:?}"
    );
    let served = source.served();
    assert!(
        served > EXPORT_LIMIT && served < EXPORT_LIMIT + 16 * MIB,
        "reading stops past the limit: {served} bytes"
    );
    assert!(dest.state().log.is_empty(), "nothing is uploaded");
}

// ---------------------------------------------------------------------------------------------
// Sessions across runs

/// Session records in a temporary directory.
struct Records {
    _dir: tempfile::TempDir,
    uploads: PathBuf,
}

impl Records {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        Records { _dir: dir, uploads }
    }

    fn options(&self) -> UploadOptions {
        UploadOptions {
            session_dir: Some(self.uploads.clone()),
            ..options(CHUNK)
        }
    }

    fn files(&self) -> Vec<PathBuf> {
        match std::fs::read_dir(&self.uploads) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn record(&self) -> Value {
        let files = self.files();
        assert_eq!(files.len(), 1, "{files:?}");
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap()
    }
}

const SESSION_SIZE: u64 = 5 * CHUNK + 4321;

/// A first relay that loses the destination after two chunks, and fails.
async fn interrupted_relay(records: &Records, file: &SourceFile) -> (SourceDrive, MockDrive) {
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults {
        die_after_chunks: Some(2),
        ..Faults::default()
    })
    .await;
    let error = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &records.options(),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            RelayError::Upload(UploadError::Failed {
                reason: StopReason::GaveUp { .. },
                ..
            })
        ),
        "{error:?}"
    );
    dest.revive();
    (source, dest)
}

#[tokio::test]
async fn an_interrupted_relay_continues_from_the_committed_offset_in_the_next_run() {
    let records = Records::new();
    let file = SourceFile::blob("blob", SESSION_SIZE);
    let (first_source, dest) = interrupted_relay(&records, &file).await;
    assert!(first_source
        .log()
        .iter()
        .any(|request| request.path == "/drive/v3/about"));

    let record = records.record();
    assert_eq!(record["offset"], 2 * CHUNK);
    assert_eq!(record["identity"], "user-a@example.com");
    assert_eq!(
        record["source"]["remote"],
        json!({
            "account": SOURCE_EMAIL,
            "file_id": "blob",
            "md5": file.md5,
            "version": null,
        })
    );
    assert_eq!(record["source"]["size"], SESSION_SIZE);
    assert_eq!(record["target"]["modified_time"], MODIFIED);
    assert_eq!(record["target"]["created_time"], CREATED);
    assert_eq!(record["target"]["mime"], "application/x-test");

    // The next run, from a new process: a new source server for the same account.
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let before = dest.state().log.len();
    let reported = Mutex::new(Vec::new());
    let progress = |offset| reported.lock().unwrap().push(offset);
    let result = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &records.options(),
        Some(&progress),
    )
    .await;

    let copy = assert_copied(&dest, result, SESSION_SIZE);
    assert_eq!(copy.id.as_deref(), Some("gen-id-1"));
    let state = dest.state();
    assert_eq!(state.sessions.len(), 1, "the session is continued");
    let starts: Vec<_> = state.log[before..]
        .iter()
        .filter_map(Logged::chunk_start)
        .collect();
    assert_eq!(starts, [2 * CHUNK, 3 * CHUNK, 4 * CHUNK, 5 * CHUNK]);
    drop(state);
    let media = source.media();
    assert_eq!(media.len(), 1, "{media:?}");
    assert_eq!(
        media[0].range,
        Some(format!("bytes={}-", 2 * CHUNK)),
        "what the session holds is not read again"
    );
    assert_eq!(source.served(), SESSION_SIZE - 2 * CHUNK);
    assert_eq!(
        *reported.lock().unwrap(),
        [2 * CHUNK, 3 * CHUNK, 4 * CHUNK, 5 * CHUNK, SESSION_SIZE]
    );
    assert!(records.files().is_empty(), "the record is deleted");
}

#[tokio::test]
async fn a_session_that_completed_before_the_run_ended_is_taken_without_reading_the_source() {
    let records = Records::new();
    let file = SourceFile::blob("blob", SESSION_SIZE);
    let first = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    // The last chunk completes the session, but its reply is lost and Drive is gone after it.
    let dest = MockDrive::start(Faults {
        die_after_chunks: Some(6),
        cut_final_reply: true,
        ..Faults::default()
    })
    .await;
    let error = relay(
        &first,
        &dest,
        &file.listing(),
        &target(),
        &records.options(),
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, RelayError::Upload(_)), "{error:?}");
    assert!(dest.state().sessions[0].complete);
    assert_eq!(records.files().len(), 1, "the record is left");
    dest.revive();

    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let copy = relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &records.options(),
        None,
    )
    .await
    .unwrap_or_else(|error| panic!("{error:?}\n{}", dest.log()));

    assert_eq!(copy.id.as_deref(), Some("gen-id-1"));
    assert_eq!(
        copy.md5_checksum, file.md5,
        "checked against the source's md5"
    );
    assert!(
        source.media().is_empty(),
        "the source is not read: {:?}",
        source.log()
    );
    let state = dest.state();
    assert_eq!(state.sessions.len(), 1);
    assert_eq!(state.files["gen-id-1"].content, content(0, SESSION_SIZE));
    drop(state);
    assert!(records.files().is_empty(), "the record is deleted");
}

#[tokio::test]
async fn a_source_without_md5_is_not_recorded() {
    let records = Records::new();
    let file = SourceFile {
        md5: None,
        ..SourceFile::blob("blob", SESSION_SIZE)
    };
    let source = SourceDrive::start(vec![file.clone()], SourceFaults::default());
    let dest = MockDrive::start(Faults {
        die_after_chunks: Some(2),
        ..Faults::default()
    })
    .await;

    relay(
        &source,
        &dest,
        &file.listing(),
        &target(),
        &records.options(),
        None,
    )
    .await
    .unwrap_err();

    assert_eq!(
        records.files(),
        Vec::<PathBuf>::new(),
        "nothing is recorded"
    );
    assert!(
        source
            .log()
            .iter()
            .all(|request| request.path != "/drive/v3/about"),
        "no account is looked up for a record"
    );
}

#[tokio::test]
async fn a_session_is_not_continued_for_another_source_account_or_revision() {
    let records = Records::new();
    let file = SourceFile::blob("blob", SESSION_SIZE);
    let (_, dest) = interrupted_relay(&records, &file).await;

    // The same file id and content, read as another account.
    let other = SourceDrive::start_as(
        vec![file.clone()],
        SourceFaults::default(),
        "someone-else@example.com",
    );
    let result = relay(
        &other,
        &dest,
        &file.listing(),
        &target(),
        &records.options(),
        None,
    )
    .await;

    let copy = result.unwrap_or_else(|error| panic!("{error:?}\n{}", dest.log()));
    assert_eq!(copy.id.as_deref(), Some("gen-id-2"));
    {
        let state = dest.state();
        assert_eq!(state.sessions.len(), 2, "a new session");
        assert_eq!(state.files["gen-id-2"].content, content(0, SESSION_SIZE));
    }
    assert_eq!(other.media()[0].range, None, "read from the start");
    assert_eq!(
        records.files().len(),
        1,
        "the other account's record is left"
    );

    // A changed source of the first account: its record does not match.
    let changed = SourceFile {
        md5: Some(md5_of(3, SESSION_SIZE)),
        ..file
    };
    let source = SourceDrive::start(vec![changed.clone()], SourceFaults::default());
    let _ = relay(
        &source,
        &dest,
        &changed.listing(),
        &target(),
        &records.options(),
        None,
    )
    .await;
    assert_eq!(dest.state().sessions.len(), 3, "a new session");
    assert_eq!(source.media()[0].range, None, "read from the start");
}

// ---------------------------------------------------------------------------------------------
// Between two fake_drive instances

/// The source and destination fakes of the command's tests, the destination with the folder
/// `dest-folder`.
async fn fakes(files: Vec<FakeFile>, dest: Behavior) -> (FakeDrive, FakeDrive) {
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(
        vec![FakeFile {
            parents: Vec::new(),
            ..FakeFile::folder("dest-folder", "Destination", "")
        }],
        dest,
    )
    .await;
    (source, dest)
}

fn fake_listing(file: &FakeFile) -> google_drive3::api::File {
    google_drive3::api::File {
        id: Some(file.id.clone()),
        name: Some(file.name.clone()),
        mime_type: Some(file.mime_type.clone()),
        size: file.size.map(|size| i64::try_from(size).unwrap()),
        md5_checksum: file.md5.clone(),
        modified_time: file.modified_time.as_deref().map(time),
        created_time: file.created_time.as_deref().map(time),
        ..google_drive3::api::File::default()
    }
}

fn text_content(len: u64) -> String {
    (0..len)
        .map(|n| char::from(b'a' + u8::try_from(n * 7 % 26).unwrap()))
        .collect()
}

#[tokio::test]
async fn blobs_and_documents_relay_between_two_fake_drives() {
    let small = FakeFile::blob("small", "small.txt", "root", "hello").modified(MODIFIED);
    let large = FakeFile {
        created_time: Some(String::from(CREATED)),
        ..FakeFile::blob("large", "large.txt", "root", &text_content(3 * CHUNK + 5))
    };
    let document = FakeFile::native(
        "doc",
        "Notes",
        "root",
        MIME_TYPE_DRIVE_DOCUMENT,
        "exported document",
    );
    let (source, dest) = fakes(
        vec![small.clone(), large.clone(), document.clone()],
        Behavior {
            chunk_unavailable: [(String::from("large.txt"), CHUNK)].into(),
            ..Behavior::default()
        },
    )
    .await;
    let (source_hub, dest_hub) = (source.hub(), dest.hub());
    let options = UploadOptions {
        chunk_size: CHUNK,
        multipart_threshold: CHUNK,
        ..UploadOptions::default()
    };

    let mut copies = Vec::new();
    for file in [&small, &large, &document] {
        // The command pre-generates the ids of blobs; Drive takes none for a conversion.
        let target = UploadTarget {
            new_file_id: Some(format!("copy-of-{}", file.id)),
            ..target()
        };
        let copy = relay_file(
            &source_hub,
            &dest_hub,
            &fake_listing(file),
            &target,
            &options,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{}: {error:?}", file.name));
        copies.push(copy);
    }
    let returned: Vec<_> = copies
        .iter()
        .map(|copy| (copy.id.clone().unwrap_or_default(), copy.size))
        .collect();
    assert_eq!(
        returned,
        [
            (String::from("copy-of-small"), Some(5)),
            (
                String::from("copy-of-large"),
                Some(i64::try_from(3 * CHUNK + 5).unwrap())
            ),
            // The size Drive gives the converted file (the fake's: the export's).
            (String::new(), Some(17)),
        ]
    );

    let created = dest.created();
    let find = |name: &str| created.iter().find(|file| file.name == name).unwrap();
    assert_eq!(find("small.txt").content, b"hello");
    assert_eq!(find("small.txt").mime_type, "text/plain");
    assert_eq!(find("small.txt").parents, ["dest-folder"]);
    assert_eq!(find("small.txt").modified_time.as_deref(), Some(MODIFIED));
    assert_eq!(
        find("large.txt").content,
        large.content.clone().unwrap().into_bytes()
    );
    assert_eq!(find("large.txt").creation_time.as_deref(), Some(CREATED));
    assert_eq!(find("Notes").content, b"exported document");
    assert_eq!(find("Notes").mime_type, MIME_TYPE_DRIVE_DOCUMENT);
    assert_eq!(
        source.exports(),
        [(String::from("doc"), String::from(MIME_TYPE_DOCX))]
    );
    // The chunk the destination failed was read again from its offset.
    assert_eq!(
        source.media_ranges(),
        [
            (String::from("small"), None),
            (String::from("large"), None),
            (String::from("large"), Some(format!("bytes={CHUNK}-"))),
        ]
    );
}

#[tokio::test]
async fn drives_own_export_limit_is_export_too_large() {
    let document = FakeFile::native("doc", "Big", "root", MIME_TYPE_DRIVE_DOCUMENT, "x");
    let source = FakeDrive::start(
        vec![document.clone()],
        Behavior {
            export_too_large: [String::from("doc")].into(),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let error = relay_file(
        &source.hub(),
        &dest.hub(),
        &fake_listing(&document),
        &UploadTarget::default(),
        &UploadOptions::default(),
        None,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, RelayError::ExportTooLarge { .. }),
        "{error:?}"
    );
    assert!(dest.created().is_empty());
}

#[tokio::test]
async fn a_shortcut_is_refused_without_a_request() {
    let shortcut = FakeFile::shortcut("s", "Link", "root", "target", "text/plain");
    let source = FakeDrive::start(vec![shortcut.clone()], Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let error = relay_file(
        &source.hub(),
        &dest.hub(),
        &fake_listing(&shortcut),
        &UploadTarget::default(),
        &UploadOptions::default(),
        None,
    )
    .await
    .unwrap_err();

    assert_eq!(error.unsupported(), Some(&Unsupported::Shortcut));
    assert!(source.media().is_empty() && source.gets().is_empty());
}
