//! Regression tests for the resumable-upload bugs of google-apis-common 8.0.0 (see
//! docs/triage/README.md section 2 and vendor/google-apis-common/GDRIVE-PATCHES.md), run against
//! an in-process mock of Google's resumable upload protocol.
//!
//! What runs here: gdrive is a binary-only crate, so an integration test cannot call
//! `files::upload::upload_file`, and gdrive's `Hub` type is bound to the OAuth authenticator. The
//! `DriveHub` itself can be pointed at another root URL, so the test compiles gdrive's real
//! `UploadDelegate` from `src/common/delegate.rs` and issues the same `files.create` request chain
//! that `upload_file` builds (`param("fields", ..)`, `add_scope(Full)`, `delegate(..)`,
//! `supports_all_drives(true)`, `upload_resumable(..)`) on a `DriveHub` whose root URL is the mock
//! server. That covers google-drive3's session initiation and final JSON decoding, the vendored
//! google-apis-common resumable loop, and gdrive's retry decisions. Only the backoff timings are
//! shorter than the ones `upload.rs` uses, so that the unpatched crate fails within a second
//! instead of retrying for hours.
//!
//! The mock follows <https://developers.google.com/workspace/drive/api/guides/manage-uploads>:
//! - a chunk is accepted only at the next uncommitted offset; anything else gets the 503 that
//!   Google sends in that case (Byron/google-apis-rs#438);
//! - a 308 carries `Range: bytes=0-N` for the committed bytes, and no `Range` when nothing is
//!   committed yet;
//! - an empty request with `Content-Range: bytes */TOTAL` asks for the status, and a finished
//!   session answers it with 200 and the file resource;
//! - what Google does with more data sent to a finished session is undocumented, so the mock
//!   rejects it with 400.

use std::{
    convert::Infallible,
    io::{self, Cursor},
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use bytes::Bytes;
use futures::StreamExt;
use google_drive3::DriveHub;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Empty, Full, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::{
    client::legacy::connect::HttpConnector,
    rt::{TokioExecutor, TokioIo},
};
use tokio::net::TcpListener;

// `allow`, not `expect`: whether the unused parts count as dead code differs between rustc
// releases (1.88, the MSRV, does not report them).
#[allow(
    dead_code,
    reason = "only the upload delegate of this gdrive module is used by the tests"
)]
#[path = "../src/common/delegate.rs"]
mod delegate;
#[path = "../src/common/human_size.rs"]
mod human_size;

use delegate::{BackoffConfig, ChunkSize, UploadDelegate, UploadDelegateConfig};

const FILE_ID: &str = "mock-file-id";
const SESSION_PATH: &str = "/upload-session";
const CREATE_PATH: &str = "/resumable/upload/drive/v3/files";
const MIB: u64 = 1 << 20;
/// Four chunks with the 1 MiB chunk size: three full ones and a short final one.
const UPLOAD_SIZE: u64 = 3 * MIB + 12_345;
/// The `fields` mask of `upload_file` in src/files/upload.rs.
const FIELDS: &str = "id,name,size,createdTime,modifiedTime,md5Checksum,mimeType,parents,shared,\
                      description,webContentLink,webViewLink";

type MockBody = UnsyncBoxBody<Bytes, io::Error>;
type UploadResult = Result<google_drive3::api::File, Box<google_drive3::Error>>;

/// Faults the mock injects, each at most once.
#[derive(Default)]
struct Faults {
    /// Answer the chunk that starts at this offset with a 503 and commit none of it.
    fail_chunk_at: Option<u64>,
    /// Commit only the given number of bytes of the chunk that starts at the given offset.
    short_commit: Option<(u64, usize)>,
    /// Cut the body of the reply that completes the upload, after its status line and headers.
    cut_final_reply: bool,
    /// Answer every chunk that starts at this offset with a 308 that repeats the current Range,
    /// committing nothing (unlike the faults above, this one never goes away).
    stall_at: Option<u64>,
}

#[derive(Default)]
struct MockState {
    faults: Faults,
    /// Number of `files.create` session initiations, i.e. files that would exist on Drive.
    creates: u32,
    received: Vec<u8>,
    complete: bool,
    session_methods: Vec<Method>,
    stalled_chunks: u32,
    log: Vec<String>,
}

impl MockState {
    fn session_request(&mut self, content_range: Option<&str>, body: &[u8]) -> Response<MockBody> {
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

        if range == "*" {
            return if self.complete {
                self.file_resource(false)
            } else {
                self.resume_incomplete()
            };
        }

        if self.complete {
            return text(StatusCode::BAD_REQUEST, "the upload is already complete");
        }

        let Some((first, last)) = range.split_once('-').and_then(|(first, last)| {
            Some((first.parse::<u64>().ok()?, last.parse::<u64>().ok()?))
        }) else {
            return text(StatusCode::BAD_REQUEST, "malformed byte range");
        };
        let committed = self.committed();
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
        if self.faults.stall_at == Some(first) {
            self.stalled_chunks += 1;
            return self.resume_incomplete();
        }
        if self.faults.fail_chunk_at == Some(first) {
            self.faults.fail_chunk_at = None;
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "injected transient backend error",
            );
        }

        let accepted = match self.faults.short_commit {
            Some((at, len)) if at == first => {
                self.faults.short_commit = None;
                len
            }
            _ => body.len(),
        };
        self.received.extend_from_slice(&body[..accepted]);

        if self.committed() == total {
            self.complete = true;
            let cut = std::mem::take(&mut self.faults.cut_final_reply);
            self.file_resource(cut)
        } else {
            self.resume_incomplete()
        }
    }

    fn committed(&self) -> u64 {
        self.received.len() as u64
    }

    fn resume_incomplete(&self) -> Response<MockBody> {
        let mut builder = Response::builder().status(StatusCode::PERMANENT_REDIRECT);
        if let Some(last) = self.committed().checked_sub(1) {
            builder = builder.header("Range", format!("bytes=0-{last}"));
        }
        builder
            .body(empty())
            .expect("valid 308 Resume Incomplete reply")
    }

    fn file_resource(&self, cut: bool) -> Response<MockBody> {
        let json = serde_json::json!({
            "kind": "drive#file",
            "id": FILE_ID,
            "name": "data.bin",
            "mimeType": "application/octet-stream",
            "size": self.committed().to_string(),
        })
        .to_string();
        let builder = Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_LENGTH, json.len());

        let body = if cut {
            // Send the head and half of the body, then fail the stream: hyper drops the
            // connection and the client sees a body that ends early.
            let half = Bytes::from(json[..json.len() / 2].to_owned());
            let stream = futures::stream::iter([Ok(Frame::data(half))]).chain(
                futures::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Err(io::Error::other("reply cut by the mock server"))
                }),
            );
            StreamBody::new(stream).boxed_unsync()
        } else {
            full(json)
        };
        builder.body(body).expect("valid file resource reply")
    }
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
struct MockServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl MockServer {
    async fn start(faults: Faults) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock server");
        let server = MockServer {
            addr: listener.local_addr().expect("mock server address"),
            state: Arc::new(Mutex::new(MockState {
                faults,
                ..MockState::default()
            })),
        };

        let accept_server = server.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = accept_server.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let server = server.clone();
                        async move { Ok::<_, Infallible>(server.handle(request).await) }
                    });
                    // Connection errors are expected when the mock cuts a reply.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        server
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<MockBody> {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let content_range = request
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let Ok(body) = request
            .into_body()
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
        else {
            return text(StatusCode::BAD_REQUEST, "unreadable request body");
        };

        let mut state = self.state();
        let response = if method == Method::POST && path == CREATE_PATH {
            state.creates += 1;
            Response::builder()
                .status(StatusCode::OK)
                .header(LOCATION, format!("http://{}{SESSION_PATH}", self.addr))
                .body(empty())
                .expect("valid session initiation reply")
        } else if path == SESSION_PATH {
            state.session_methods.push(method.clone());
            state.session_request(content_range.as_deref(), &body)
        } else {
            text(StatusCode::NOT_FOUND, "unknown path")
        };

        let line = format!(
            "{method} {path} Content-Range: {} ({} bytes) -> {}",
            content_range.as_deref().unwrap_or("-"),
            body.len(),
            response.status().as_u16(),
        );
        state.log.push(line);
        response
    }

    fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state lock")
    }

    fn hub(&self) -> DriveHub<HttpConnector> {
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build_http();
        let mut hub = DriveHub::new(client, String::from("test-access-token"));
        hub.root_url(format!("http://{}/", self.addr));
        hub.base_url(format!("http://{}/drive/v3/", self.addr));
        hub
    }

    fn log(&self) -> String {
        tail(&self.state().log)
    }
}

/// The last lines of the mock's request log, for failure messages.
fn tail(log: &[String]) -> String {
    const LINES: usize = 40;
    let skipped = log.len().saturating_sub(LINES);
    let mut text = if skipped > 0 {
        format!("({skipped} earlier requests not shown)\n")
    } else {
        String::new()
    };
    text.push_str(&log[skipped..].join("\n"));
    text
}

fn delegate_config() -> UploadDelegateConfig {
    UploadDelegateConfig {
        chunk_size: ChunkSize::Approx1,
        backoff_config: BackoffConfig {
            max_retries: 5,
            min_sleep: Duration::from_millis(5),
            max_sleep: Duration::from_millis(20),
        },
        print_chunk_errors: true,
        print_chunk_info: true,
    }
}

fn test_data() -> Vec<u8> {
    (0..UPLOAD_SIZE)
        .map(|i| u8::try_from((i * 7 + i / 4096) % 251).expect("below 251"))
        .collect()
}

/// The `files.create` call of `upload_file` in src/files/upload.rs, through gdrive's delegate.
async fn upload(server: &MockServer, data: &[u8]) -> UploadResult {
    let hub = server.hub();
    let config = delegate_config();
    let mut delegate = UploadDelegate::new(&config);
    let dst_file = google_drive3::api::File {
        name: Some(String::from("data.bin")),
        mime_type: Some(String::from("application/octet-stream")),
        ..google_drive3::api::File::default()
    };
    assert!(
        data.len() as u64 > config.chunk_size.in_bytes(),
        "must be a resumable upload"
    );

    let call = hub
        .files()
        .create(dst_file)
        .param("fields", FIELDS)
        .add_scope(google_drive3::api::Scope::Full)
        .delegate(&mut delegate)
        .supports_all_drives(true)
        .upload_resumable(Cursor::new(data.to_vec()), mime::APPLICATION_OCTET_STREAM);

    let Ok(result) = tokio::time::timeout(Duration::from_secs(60), call).await else {
        panic!("upload did not finish:\n{}", server.log())
    };
    result.map(|(_, file)| file).map_err(Box::new)
}

/// The upload is reported as a success, the server holds exactly the local bytes, and exactly
/// one file was created.
fn assert_uploaded_once(server: &MockServer, data: &[u8], result: UploadResult) {
    let file = result.unwrap_or_else(|err| {
        panic!(
            "upload must be reported as a success, got: {err}\n{}",
            server.log()
        )
    });
    assert_eq!(file.id.as_deref(), Some(FILE_ID), "{}", server.log());

    let state = server.state();
    assert!(
        state.received == data,
        "the server must hold the uploaded bytes unchanged: {} of {} bytes received\n{}",
        state.received.len(),
        data.len(),
        tail(&state.log),
    );
    assert_eq!(state.creates, 1, "exactly one file must be created");
    assert!(
        state
            .session_methods
            .iter()
            .all(|method| method == Method::PUT),
        "chunk and status requests must use PUT: {:?}",
        state.session_methods,
    );
}

#[tokio::test]
async fn repeated_308_without_progress_fails_within_the_retry_budget() {
    let data = test_data();
    let server = MockServer::start(Faults {
        stall_at: Some(MIB),
        ..Faults::default()
    })
    .await;

    let result = upload(&server, &data).await;

    let err = result.expect_err("an upload that makes no progress must fail");
    assert!(
        matches!(&*err, google_drive3::Error::Failure(response)
            if response.status() == StatusCode::PERMANENT_REDIRECT),
        "{err}\n{}",
        server.log()
    );
    let state = server.state();
    let max_attempts = delegate_config().backoff_config.max_retries + 1;
    assert!(
        (2..=max_attempts).contains(&state.stalled_chunks),
        "the stalled chunk must be resent with backoff, at most {max_attempts} times, was sent {} \
         times\n{}",
        state.stalled_chunks,
        tail(&state.log),
    );
    assert_eq!(
        state.committed(),
        MIB,
        "nothing past the stall is committed"
    );
    assert_eq!(state.creates, 1, "exactly one file must be created");
}

#[tokio::test]
async fn transient_503_on_second_chunk_resends_that_chunk() {
    let data = test_data();
    let server = MockServer::start(Faults {
        fail_chunk_at: Some(MIB),
        ..Faults::default()
    })
    .await;

    let result = upload(&server, &data).await;

    assert_uploaded_once(&server, &data, result);
}

#[tokio::test]
async fn lost_final_reply_is_still_reported_as_success() {
    let data = test_data();
    let server = MockServer::start(Faults {
        cut_final_reply: true,
        ..Faults::default()
    })
    .await;

    let result = upload(&server, &data).await;

    assert_uploaded_once(&server, &data, result);
}

#[tokio::test]
async fn partially_committed_chunk_resumes_at_the_range_reported_by_308() {
    let data = test_data();
    let server = MockServer::start(Faults {
        short_commit: Some((MIB, 300 * 1024)),
        ..Faults::default()
    })
    .await;

    let result = upload(&server, &data).await;

    assert_uploaded_once(&server, &data, result);
}
