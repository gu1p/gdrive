//! An in-process mock of Drive's upload endpoints for the uploader's tests. It follows
//! <https://developers.google.com/workspace/drive/api/guides/manage-uploads>, like the mock of
//! google-apis-common's helper in `tests/resumable_upload.rs`:
//! - `POST /upload/drive/v3/files` (a new file) and `PATCH /upload/drive/v3/files/{id}` (an
//!   update) with `uploadType=resumable` start a session at `/upload-session/{n}`; with
//!   `uploadType=multipart` they upload a whole file;
//! - a chunk is accepted only at the next uncommitted offset; anything else gets the 503 that
//!   Google sends in that case;
//! - a 308 carries `Range: bytes=0-N` for the committed bytes, and no `Range` when nothing is
//!   committed yet;
//! - an empty request with `Content-Range: bytes */TOTAL` asks for the status, and a finished
//!   session answers it with 200 and the file resource;
//! - what Google does with more data sent to a finished session is undocumented, so the mock
//!   rejects it with 400;
//! - a new file whose id exists already gets a 409;
//! - `GET /drive/v3/files/generateIds` and `GET /drive/v3/files/{id}` give ids and files.

use std::{
    collections::BTreeMap,
    fmt::Write,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Empty, Full, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
    service::service_fn,
    HeaderMap, Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::hub::Hub;

type MockBody = UnsyncBoxBody<Bytes, io::Error>;

/// Faults the mock injects. Unless said otherwise, each happens once.
#[expect(clippy::struct_excessive_bools, reason = "independent faults")]
#[derive(Debug, Default, Clone)]
pub(super) struct Faults {
    /// Answer the chunk that starts at this offset with a 503 and commit none of it.
    pub(super) fail_chunk_at: Option<u64>,
    /// Commit only the given number of bytes of the chunk that starts at the given offset.
    pub(super) short_commit: Option<(u64, usize)>,
    /// Cut the body of the reply that completes a session, after its head.
    pub(super) cut_final_reply: bool,
    /// Answer every chunk that starts at this offset with a 308 that repeats the current
    /// Range, committing nothing (this one never goes away).
    pub(super) stall_at: Option<u64>,
    /// Answer the chunk that starts at the given offset with a 429 rateLimitExceeded and
    /// `Retry-After: <seconds>`, committing none of it.
    pub(super) rate_limit_chunk_at: Option<(u64, u64)>,
    /// Answer the chunk that starts at this offset with a 404, and every request to its
    /// session after it: the session expired.
    pub(super) expire_session_at: Option<u64>,
    /// After this many accepted chunks (over all sessions), close every connection without a
    /// reply until [`MockDrive::revive`]: the network or the process is gone.
    pub(super) die_after_chunks: Option<u32>,
    /// Report an md5 that is not the one of the content.
    pub(super) wrong_md5: bool,
    /// Answer the first multipart upload with a 503 without storing it.
    pub(super) fail_multipart: bool,
    /// Store the first multipart upload but cut the body of its reply.
    pub(super) cut_multipart_reply: bool,
    /// Answer every session start with this status and JSON error (never goes away).
    pub(super) reject_start: Option<(u16, Value)>,
}

/// A request the mock received, and how it answered (`None`: it closed the connection).
#[derive(Debug, Clone)]
pub(super) struct Logged {
    pub(super) method: Method,
    pub(super) path: String,
    pub(super) query: String,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
    pub(super) status: Option<StatusCode>,
    pub(super) at: Instant,
}

impl Logged {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// A query parameter, percent-decoded.
    pub(super) fn query_param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    pub(super) fn is_session_request(&self) -> bool {
        self.path.starts_with("/upload-session/")
    }

    /// The first byte of a chunk request; `None` for a status request.
    pub(super) fn chunk_start(&self) -> Option<u64> {
        let range = self.header("content-range")?.strip_prefix("bytes ")?;
        range.split_once('-')?.0.parse().ok()
    }

    pub(super) fn is_status_query(&self) -> bool {
        self.header("content-range")
            .is_some_and(|range| range.starts_with("bytes */"))
    }
}

/// A resumable session.
#[derive(Debug)]
pub(super) struct Session {
    pub(super) metadata: Value,
    pub(super) content_type: String,
    pub(super) total: u64,
    pub(super) received: Vec<u8>,
    pub(super) complete: bool,
    pub(super) expired: bool,
    pub(super) file_id: String,
}

/// A file on the mock Drive.
#[derive(Debug, Clone)]
pub(super) struct StoredFile {
    pub(super) metadata: Value,
    pub(super) content: Vec<u8>,
    pub(super) content_type: String,
}

#[derive(Debug, Default)]
pub(super) struct State {
    pub(super) faults: Faults,
    pub(super) sessions: Vec<Session>,
    pub(super) files: BTreeMap<String, StoredFile>,
    pub(super) log: Vec<Logged>,
    pub(super) dead: bool,
    pub(super) accepted_chunks: u32,
    pub(super) generated_ids: u32,
    next_file: u32,
}

impl State {
    /// The last requests, for failure messages.
    pub(super) fn log_text(&self) -> String {
        const LINES: usize = 40;
        let skipped = self.log.len().saturating_sub(LINES);
        let mut text = if skipped > 0 {
            format!("({skipped} earlier requests not shown)\n")
        } else {
            String::new()
        };
        for request in &self.log[skipped..] {
            let _ = writeln!(
                text,
                "{} {} Content-Range: {} ({} bytes) -> {}",
                request.method,
                request.path,
                request.header("content-range").unwrap_or("-"),
                request.body.len(),
                request.status.map_or_else(
                    || String::from("closed"),
                    |status| status.as_u16().to_string()
                ),
            );
        }
        text
    }

    /// The requests to upload sessions.
    pub(super) fn session_log(&self) -> Vec<&Logged> {
        self.log
            .iter()
            .filter(|request| request.is_session_request())
            .collect()
    }

    /// The first bytes of the chunk requests, in order.
    pub(super) fn chunk_starts(&self) -> Vec<u64> {
        self.log.iter().filter_map(Logged::chunk_start).collect()
    }

    /// The session starts and multipart uploads.
    pub(super) fn uploads(&self) -> Vec<&Logged> {
        self.log
            .iter()
            .filter(|request| request.path.starts_with("/upload/drive/v3/files"))
            .collect()
    }

    fn route(
        &mut self,
        method: &Method,
        path: &str,
        query: &str,
        headers: &HeaderMap,
        body: &[u8],
        root: &str,
    ) -> Response<MockBody> {
        let upload_type = url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "uploadType")
            .map(|(_, value)| value.into_owned());

        if *method == Method::GET && path == "/drive/v3/files/generateIds" {
            self.generated_ids += 1;
            let id = format!("gen-id-{}", self.generated_ids);
            return json_reply(
                StatusCode::OK,
                &json!({"kind": "drive#generatedIds", "space": "drive", "ids": [id]}),
            );
        }
        if *method == Method::GET {
            if let Some(id) = path.strip_prefix("/drive/v3/files/") {
                return match self.files.get(id) {
                    Some(file) => json_reply(StatusCode::OK, &self.resource(id, file)),
                    None => google_error(StatusCode::NOT_FOUND, "notFound", "File not found."),
                };
            }
        }

        let update_id = path
            .strip_prefix("/upload/drive/v3/files/")
            .map(str::to_owned);
        let is_upload = (*method == Method::POST && path == "/upload/drive/v3/files")
            || (*method == Method::PATCH && update_id.is_some());
        if is_upload {
            return match upload_type.as_deref() {
                Some("resumable") => self.start_session(update_id, headers, body, root),
                Some("multipart") => self.multipart(update_id, headers, body),
                _ => text(StatusCode::BAD_REQUEST, "unknown uploadType"),
            };
        }

        if let Some(index) = path.strip_prefix("/upload-session/") {
            let Ok(index) = index.parse::<usize>() else {
                return text(StatusCode::NOT_FOUND, "unknown session");
            };
            let content_range = headers
                .get("content-range")
                .and_then(|value| value.to_str().ok());
            return self.session_request(index, content_range, body);
        }

        text(StatusCode::NOT_FOUND, "unknown path")
    }

    /// The id of a new file: its pre-generated one, or the next mock id.
    fn new_file_id(&mut self, metadata: &Value) -> String {
        if let Some(id) = metadata["id"].as_str() {
            return id.to_owned();
        }
        self.next_file += 1;
        format!("mock-file-{}", self.next_file)
    }

    /// The id of the file an upload writes: the updated one, or a new one (`None` when a new
    /// file's id exists already).
    fn file_id(&mut self, update_id: Option<String>, metadata: &Value) -> Option<String> {
        if let Some(id) = update_id {
            return Some(id);
        }
        let id = self.new_file_id(metadata);
        (!self.files.contains_key(&id)).then_some(id)
    }

    fn start_session(
        &mut self,
        update_id: Option<String>,
        headers: &HeaderMap,
        body: &[u8],
        root: &str,
    ) -> Response<MockBody> {
        if let Some((status, error)) = &self.faults.reject_start {
            let status = StatusCode::from_u16(*status).expect("valid status");
            return json_reply(status, error);
        }
        let Ok(metadata) = serde_json::from_slice::<Value>(body) else {
            return text(StatusCode::BAD_REQUEST, "the metadata is not JSON");
        };
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let Some(total) = header("x-upload-content-length").and_then(|v| v.parse().ok()) else {
            return text(StatusCode::BAD_REQUEST, "missing X-Upload-Content-Length");
        };
        let content_type = header("x-upload-content-type").unwrap_or_default();
        let Some(file_id) = self.file_id(update_id, &metadata) else {
            return conflict();
        };

        self.sessions.push(Session {
            metadata,
            content_type,
            total,
            received: Vec::new(),
            complete: false,
            expired: false,
            file_id,
        });
        Response::builder()
            .status(StatusCode::OK)
            .header(
                LOCATION,
                format!("{root}upload-session/{}", self.sessions.len() - 1),
            )
            .body(empty())
            .expect("valid session start reply")
    }

    fn multipart(
        &mut self,
        update_id: Option<String>,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Response<MockBody> {
        let Some((metadata, content_type, content)) = parse_multipart(headers, body) else {
            return text(StatusCode::BAD_REQUEST, "malformed multipart body");
        };
        if std::mem::take(&mut self.faults.fail_multipart) {
            return text(StatusCode::SERVICE_UNAVAILABLE, "injected backend error");
        }
        let Some(id) = self.file_id(update_id, &metadata) else {
            return conflict();
        };
        let file = StoredFile {
            metadata,
            content,
            content_type,
        };
        let resource = self.resource(&id, &file);
        self.files.insert(id, file);
        let cut = std::mem::take(&mut self.faults.cut_multipart_reply);
        file_reply(&resource, cut)
    }

    fn session_request(
        &mut self,
        index: usize,
        content_range: Option<&str>,
        body: &[u8],
    ) -> Response<MockBody> {
        let Some((range, total)) = content_range
            .and_then(|value| value.strip_prefix("bytes "))
            .and_then(|value| value.split_once('/'))
        else {
            return text(
                StatusCode::BAD_REQUEST,
                "missing or malformed Content-Range",
            );
        };
        let Ok(total) = total.parse::<u64>() else {
            return text(StatusCode::BAD_REQUEST, "malformed total length");
        };
        let faults = &mut self.faults;
        let Some(session) = self.sessions.get_mut(index) else {
            return text(StatusCode::NOT_FOUND, "unknown session");
        };
        if session.expired {
            return google_error(StatusCode::NOT_FOUND, "notFound", "Session expired.");
        }
        if total != session.total {
            return text(StatusCode::BAD_REQUEST, "the total length changed");
        }

        if range == "*" {
            return if session.complete {
                let file = self.files[&session.file_id].clone();
                let id = session.file_id.clone();
                file_reply(&self.resource(&id, &file), false)
            } else {
                resume_incomplete(session)
            };
        }
        if session.complete {
            return text(StatusCode::BAD_REQUEST, "the upload is already complete");
        }

        let Some((first, last)) = range.split_once('-').and_then(|(first, last)| {
            Some((first.parse::<u64>().ok()?, last.parse::<u64>().ok()?))
        }) else {
            return text(StatusCode::BAD_REQUEST, "malformed byte range");
        };
        let committed = session.received.len() as u64;
        if first != committed {
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "Invalid request.  According to the Content-Range header, the upload offset \
                     is {first} byte(s), which exceeds already uploaded size of {committed} byte(s)."
                ),
            );
        }
        if (last + 1).checked_sub(first) != Some(body.len() as u64) {
            return text(
                StatusCode::BAD_REQUEST,
                "Content-Range does not match the body",
            );
        }
        if let Some(response) = chunk_fault(faults, session, first) {
            return response;
        }

        let accepted = match faults.short_commit {
            Some((at, len)) if at == first => {
                faults.short_commit = None;
                len
            }
            _ => body.len(),
        };
        session.received.extend_from_slice(&body[..accepted]);
        self.accepted_chunks += 1;
        if faults.die_after_chunks == Some(self.accepted_chunks) {
            self.dead = true;
        }

        if session.received.len() as u64 == total {
            session.complete = true;
            let file = StoredFile {
                metadata: session.metadata.clone(),
                content: session.received.clone(),
                content_type: session.content_type.clone(),
            };
            let id = session.file_id.clone();
            let cut = std::mem::take(&mut faults.cut_final_reply);
            let resource = self.resource(&id, &file);
            self.files.insert(id, file);
            file_reply(&resource, cut)
        } else {
            resume_incomplete(session)
        }
    }

    /// The file resource Drive returns for `file`. Google Workspace files have no size and md5.
    fn resource(&self, id: &str, file: &StoredFile) -> Value {
        let mime_type = file.metadata["mimeType"]
            .as_str()
            .unwrap_or(&file.content_type)
            .to_owned();
        let mut resource = json!({
            "kind": "drive#file",
            "id": id,
            "name": file.metadata["name"],
            "mimeType": mime_type,
        });
        if !mime_type.starts_with("application/vnd.google-apps.") {
            let md5 = if self.faults.wrong_md5 {
                md5::compute(b"something else")
            } else {
                md5::compute(&file.content)
            };
            resource["size"] = file.content.len().to_string().into();
            resource["md5Checksum"] = format!("{md5:x}").into();
        }
        resource
    }
}

/// The metadata, the content type and the content of a multipart/related body.
fn parse_multipart(headers: &HeaderMap, body: &[u8]) -> Option<(Value, String, Vec<u8>)> {
    let content_type = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    let boundary = content_type
        .strip_prefix("multipart/related; boundary=")?
        .trim_matches('"');
    let delimiter = format!("--{boundary}");
    let parts = split(body, delimiter.as_bytes());
    // "", metadata, content, "--\r\n"
    if parts.len() != 4 || !parts[0].is_empty() || !parts[3].starts_with(b"--") {
        return None;
    }
    let part = |raw: &[u8]| -> Option<(String, Vec<u8>)> {
        let raw = raw.strip_prefix(b"\r\n")?.strip_suffix(b"\r\n")?;
        let split_at = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
        let head = String::from_utf8(raw[..split_at].to_vec()).ok()?;
        let content_type = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Type: "))?
            .to_owned();
        Some((content_type, raw[split_at + 4..].to_vec()))
    };
    let (metadata_type, metadata) = part(parts[1])?;
    if !metadata_type.starts_with("application/json") {
        return None;
    }
    let (content_type, content) = part(parts[2])?;
    Some((
        serde_json::from_slice(&metadata).ok()?,
        content_type,
        content,
    ))
}

fn split<'a>(mut bytes: &'a [u8], delimiter: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    while let Some(at) = bytes
        .windows(delimiter.len())
        .position(|window| window == delimiter)
    {
        parts.push(&bytes[..at]);
        bytes = &bytes[at + delimiter.len()..];
    }
    parts.push(bytes);
    parts
}

/// The reply of a fault for the chunk at `first` of `session`, if one applies.
fn chunk_fault(
    faults: &mut Faults,
    session: &mut Session,
    first: u64,
) -> Option<Response<MockBody>> {
    if faults.expire_session_at == Some(first) {
        faults.expire_session_at = None;
        session.expired = true;
        return Some(google_error(
            StatusCode::NOT_FOUND,
            "notFound",
            "Session expired.",
        ));
    }
    if faults.stall_at == Some(first) {
        return Some(resume_incomplete(session));
    }
    if let Some((at, seconds)) = faults.rate_limit_chunk_at {
        if at == first {
            faults.rate_limit_chunk_at = None;
            let body = json!({"error": {
                "code": 429,
                "message": "Rate Limit Exceeded",
                "errors": [{"domain": "usageLimits", "reason": "rateLimitExceeded"}],
            }});
            return Some(
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header(CONTENT_TYPE, "application/json")
                    .header("Retry-After", seconds.to_string())
                    .body(full(body.to_string()))
                    .expect("valid 429 reply"),
            );
        }
    }
    if faults.fail_chunk_at == Some(first) {
        faults.fail_chunk_at = None;
        return Some(text(
            StatusCode::SERVICE_UNAVAILABLE,
            "injected transient backend error",
        ));
    }
    None
}

fn resume_incomplete(session: &Session) -> Response<MockBody> {
    let mut builder = Response::builder().status(StatusCode::PERMANENT_REDIRECT);
    if let Some(last) = (session.received.len() as u64).checked_sub(1) {
        builder = builder.header("Range", format!("bytes=0-{last}"));
    }
    builder
        .body(empty())
        .expect("valid 308 Resume Incomplete reply")
}

fn file_reply(resource: &Value, cut: bool) -> Response<MockBody> {
    let json = resource.to_string();
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, json.len());
    let body = if cut {
        // Send the head and half of the body, then fail the stream: hyper drops the
        // connection and the client sees a body that ends early.
        let half = Bytes::from(json[..json.len() / 2].to_owned());
        let stream =
            futures::stream::iter([Ok(Frame::data(half))]).chain(futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Err(io::Error::other("reply cut by the mock server"))
            }));
        StreamBody::new(stream).boxed_unsync()
    } else {
        full(json)
    };
    builder.body(body).expect("valid file resource reply")
}

fn conflict() -> Response<MockBody> {
    google_error(
        StatusCode::CONFLICT,
        "duplicate",
        "A file already exists with the provided ID.",
    )
}

fn google_error(status: StatusCode, reason: &str, message: &str) -> Response<MockBody> {
    json_reply(
        status,
        &json!({"error": {
            "code": status.as_u16(),
            "message": message,
            "errors": [{"domain": "global", "reason": reason, "message": message}],
        }}),
    )
}

fn json_reply(status: StatusCode, body: &Value) -> Response<MockBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(full(body.to_string()))
        .expect("valid JSON reply")
}

fn empty() -> MockBody {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

fn full(content: String) -> MockBody {
    Full::new(Bytes::from(content))
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn text(status: StatusCode, message: &str) -> Response<MockBody> {
    Response::builder()
        .status(status)
        .body(full(message.to_owned()))
        .expect("valid text reply")
}

#[derive(Clone)]
pub(super) struct MockDrive {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    task: Arc<JoinHandle<()>>,
}

impl MockDrive {
    pub(super) async fn start(faults: Faults) -> MockDrive {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock server");
        let addr = listener.local_addr().expect("mock server address");
        let state = Arc::new(Mutex::new(State {
            faults,
            ..State::default()
        }));

        let accept_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&accept_state);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let state = Arc::clone(&state);
                        async move { handle(&state, addr, request).await }
                    });
                    // Connection errors are expected when the mock cuts a reply or is down.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        MockDrive {
            addr,
            state,
            task: Arc::new(task),
        }
    }

    /// A hub whose requests all go to this server, with the retry policy for tests.
    pub(super) fn hub(&self) -> Hub {
        Hub::for_mock_server(&format!("http://{}/", self.addr)).expect("create mock hub")
    }

    pub(super) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("mock state lock")
    }

    /// The mock answers again after `die_after_chunks`, with the sessions it had.
    pub(super) fn revive(&self) {
        let mut state = self.state();
        state.dead = false;
        state.faults.die_after_chunks = None;
    }

    /// The last requests, for failure messages. Not while holding [`MockDrive::state`]: use
    /// [`State::log_text`] on the guard then.
    pub(super) fn log(&self) -> String {
        self.state().log_text()
    }
}

impl Drop for MockDrive {
    fn drop(&mut self) {
        if Arc::strong_count(&self.task) == 1 {
            self.task.abort();
        }
    }
}

async fn handle(
    state: &Mutex<State>,
    addr: SocketAddr,
    request: Request<Incoming>,
) -> Result<Response<MockBody>, io::Error> {
    let (parts, body) = request.into_parts();
    let log = |state: &mut State, body: Bytes, status| {
        state.log.push(Logged {
            method: parts.method.clone(),
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().unwrap_or_default().to_owned(),
            headers: parts.headers.clone(),
            body,
            status,
            at: Instant::now(),
        });
    };

    {
        let mut state = state.lock().expect("mock state lock");
        if state.dead {
            log(&mut state, Bytes::new(), None);
            // An error from the service makes hyper close the connection without a reply.
            return Err(io::Error::other("the mock server is down"));
        }
    }

    let body = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            let mut state = state.lock().expect("mock state lock");
            log(&mut state, Bytes::new(), None);
            return Err(io::Error::other(error));
        }
    };

    let mut state = state.lock().expect("mock state lock");
    let response = state.route(
        &parts.method,
        parts.uri.path(),
        parts.uri.query().unwrap_or_default(),
        &parts.headers,
        &body,
        &format!("http://{addr}/"),
    );
    log(&mut state, body, Some(response.status()));
    Ok(response)
}
