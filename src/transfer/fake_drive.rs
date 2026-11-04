//! A fake Drive `files.list` endpoint for walk tests.
//!
//! Unlike `common::mock_server`, which answers requests in order with canned responses, this
//! server holds a set of files and answers every query from them, so it works however many
//! requests are in flight and in whatever order they arrive. It understands the queries the walk
//! sends: `'<id>' in parents` terms joined by `or`, and `trashed = false`. It also records every
//! request and the most requests it had in flight at once.

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
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn, Method, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Map, Value};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{common::drive_file::MIME_TYPE_DRIVE_FOLDER, hub::Hub};

const LIST_PATH: &str = "/drive/v3/files";

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
    pub(crate) drive_id: Option<String>,
    /// Target id and target MIME type of a shortcut.
    pub(crate) shortcut_target: Option<(String, String)>,
    pub(crate) target_resource_key: Option<String>,
    pub(crate) resource_key: Option<String>,
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

struct State {
    files: Vec<FakeFile>,
    behavior: Behavior,
    requests: Mutex<Vec<ListRequest>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
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
        let state = Arc::new(State {
            files,
            behavior,
            requests: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
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

    pub(crate) fn request_count(&self) -> usize {
        self.state.requests.lock().expect("requests lock").len()
    }

    /// The most requests that were being answered at the same time.
    pub(crate) fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }
}

impl Drop for FakeDrive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Counts a request as in flight until dropped, also when the client goes away.
struct InFlight<'a>(&'a State);

impl<'a> InFlight<'a> {
    fn new(state: &'a State) -> Self {
        let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Self(state)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

type FakeResponse = hyper::Response<Full<Bytes>>;

async fn handle(
    state: Arc<State>,
    request: hyper::Request<Incoming>,
) -> Result<FakeResponse, Infallible> {
    let _in_flight = InFlight::new(&state);

    if request.method() != Method::GET || request.uri().path() != LIST_PATH {
        return Ok(error(StatusCode::NOT_FOUND, "unexpected request"));
    }

    let params = request
        .uri()
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
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

    tokio::time::sleep(state.behavior.delay).await;

    Ok(list(&state, &list_request, &q))
}

fn list(state: &State, request: &ListRequest, q: &str) -> FakeResponse {
    let behavior = &state.behavior;
    let names = |set: &HashSet<String>| request.parents.iter().any(|id| set.contains(id));

    if names(&behavior.unreadable) {
        return error(StatusCode::NOT_FOUND, "File not found.");
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
    let matching = state
        .files
        .iter()
        .filter(|file| file.parents.iter().any(|id| request.parents.contains(id)))
        .filter(|file| !(exclude_trashed && file.trashed))
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
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("valid response")
}

#[cfg(test)]
mod tests {
    use super::parent_ids;

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
