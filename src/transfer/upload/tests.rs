//! The uploader against the protocol mock of `mock_drive.rs`. Chunks are 256 KiB, the smallest
//! Google allows, so that the uploads stay small; the hub's retry policy is the one for tests
//! (4 attempts, millisecond backoff).

use std::{
    io::Cursor,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use hyper::{header::HeaderMap, Method, StatusCode};
use serde_json::json;

use super::{
    mock_drive::{Faults, Logged, MockDrive},
    next_offset, upload_path, upload_reader, Failure, Step, UploadError, UploadOptions,
    UploadTarget, CHUNK_ALIGNMENT, FILE_FIELDS,
};
use crate::common::{
    drive_file::MIME_TYPE_DRIVE_DOCUMENT,
    retry::{RetryPolicy, StopReason},
};

const CHUNK: u64 = CHUNK_ALIGNMENT;
/// Five chunks: four full ones and a short final one.
const SIZE: u64 = 4 * CHUNK + 12_345;

fn test_data(size: u64) -> Vec<u8> {
    (0..size)
        .map(|i| u8::try_from((i * 7 + i / 4096) % 251).expect("below 251"))
        .collect()
}

/// A local file to upload.
struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    data: Vec<u8>,
}

fn fixture(size: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.bin");
    let data = test_data(size);
    std::fs::write(&path, &data).unwrap();
    Fixture {
        _dir: dir,
        path,
        data,
    }
}

/// Resumable uploads (no multipart threshold) in 256 KiB chunks.
fn options() -> UploadOptions {
    UploadOptions {
        chunk_size: CHUNK,
        multipart_threshold: 0,
        verify_md5: true,
        print_chunk_errors: true,
        print_chunk_info: true,
        session_dir: None,
        session_identity: None,
    }
}

fn target() -> UploadTarget {
    UploadTarget {
        parents: vec![String::from("folder-id")],
        name: String::from("data.bin"),
        mime: Some(mime::APPLICATION_OCTET_STREAM),
        ..UploadTarget::default()
    }
}

async fn upload(
    mock: &MockDrive,
    fixture: &Fixture,
    target: &UploadTarget,
    options: &UploadOptions,
) -> Result<google_drive3::api::File, UploadError> {
    upload_with_progress(mock, fixture, target, options, None).await
}

async fn upload_with_progress(
    mock: &MockDrive,
    fixture: &Fixture,
    target: &UploadTarget,
    options: &UploadOptions,
    progress: Option<&(dyn Fn(u64) + Send + Sync)>,
) -> Result<google_drive3::api::File, UploadError> {
    let hub = mock.hub();
    let call = upload_path(&hub, &fixture.path, target, options, progress);
    let Ok(result) = tokio::time::timeout(Duration::from_secs(60), call).await else {
        panic!("the upload did not finish:\n{}", mock.log())
    };
    result
}

/// The upload is reported as a success with the pre-generated id, Drive holds exactly the local
/// bytes, one file was created from one session, and the session requests were PUTs without the
/// access token.
fn assert_uploaded_once(
    mock: &MockDrive,
    data: &[u8],
    result: Result<google_drive3::api::File, UploadError>,
) {
    let file = result.unwrap_or_else(|err| {
        panic!(
            "the upload must be reported as a success, got: {err}\n{}",
            mock.log()
        )
    });
    assert_eq!(file.id.as_deref(), Some("gen-id-1"), "{}", mock.log());

    let state = mock.state();
    assert_eq!(state.files.len(), 1, "exactly one file must be created");
    let stored = &state.files["gen-id-1"];
    assert!(
        stored.content == data,
        "Drive must hold the uploaded bytes unchanged: {} of {} bytes\n{}",
        stored.content.len(),
        data.len(),
        state.log_text()
    );
    assert_eq!(state.sessions.len(), 1, "one session must be started");
    for request in state.session_log() {
        assert_eq!(
            request.method,
            Method::PUT,
            "chunk and status requests use PUT"
        );
        assert_eq!(
            request.header("authorization"),
            None,
            "the session URL authorizes the request"
        );
    }
}

#[tokio::test]
async fn resumable_upload_streams_chunks_to_a_session() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults::default()).await;
    let reported = Mutex::new(Vec::new());
    let progress = |offset| reported.lock().unwrap().push(offset);

    let result =
        upload_with_progress(&mock, &fixture, &target(), &options(), Some(&progress)).await;

    assert_uploaded_once(&mock, &fixture.data, result);
    let state = mock.state();
    assert_eq!(state.generated_ids, 1);

    let start = state.uploads()[0];
    assert_eq!(start.method, Method::POST);
    assert_eq!(start.path, "/upload/drive/v3/files");
    assert_eq!(
        start.query_param("uploadType").as_deref(),
        Some("resumable")
    );
    assert_eq!(
        start.query_param("supportsAllDrives").as_deref(),
        Some("true")
    );
    assert_eq!(start.query_param("fields").as_deref(), Some(FILE_FIELDS));
    assert_eq!(start.header("authorization"), Some("Bearer mock-token"));
    assert_eq!(
        start.header("x-upload-content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(
        start.header("x-upload-content-length"),
        Some(SIZE.to_string().as_str())
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&start.body).unwrap(),
        json!({
            "id": "gen-id-1",
            "name": "data.bin",
            "mimeType": "application/octet-stream",
            "parents": ["folder-id"],
        })
    );

    let ranges: Vec<_> = state
        .session_log()
        .iter()
        .map(|request| request.header("content-range").unwrap().to_owned())
        .collect();
    let last = SIZE - 1;
    assert_eq!(
        ranges,
        [
            format!("bytes 0-{}/{SIZE}", CHUNK - 1),
            format!("bytes {CHUNK}-{}/{SIZE}", 2 * CHUNK - 1),
            format!("bytes {}-{}/{SIZE}", 2 * CHUNK, 3 * CHUNK - 1),
            format!("bytes {}-{}/{SIZE}", 3 * CHUNK, 4 * CHUNK - 1),
            format!("bytes {}-{last}/{SIZE}", 4 * CHUNK),
        ]
    );
    for request in state.session_log() {
        assert_eq!(
            request.header("content-type"),
            Some("application/octet-stream")
        );
    }
    assert_eq!(
        *reported.lock().unwrap(),
        [CHUNK, 2 * CHUNK, 3 * CHUNK, 4 * CHUNK, SIZE]
    );
}

#[tokio::test]
async fn transient_503_on_second_chunk_resends_that_chunk() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        fail_chunk_at: Some(CHUNK),
        ..Faults::default()
    })
    .await;

    let result = upload(&mock, &fixture, &target(), &options()).await;

    assert_uploaded_once(&mock, &fixture.data, result);
    let state = mock.state();
    let log = state.session_log();
    let failed = log
        .iter()
        .position(|request| request.status == Some(StatusCode::SERVICE_UNAVAILABLE))
        .expect("the second chunk fails once");
    assert_eq!(log[failed].chunk_start(), Some(CHUNK));
    assert!(
        log[failed + 1].is_status_query(),
        "the session is asked where it stands"
    );
    assert_eq!(
        log[failed + 2].chunk_start(),
        Some(CHUNK),
        "then the chunk is sent again"
    );
}

#[tokio::test]
async fn lost_final_reply_is_resolved_by_a_status_query() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        cut_final_reply: true,
        ..Faults::default()
    })
    .await;

    let result = upload(&mock, &fixture, &target(), &options()).await;

    assert_uploaded_once(&mock, &fixture.data, result);
    let state = mock.state();
    let last = state.session_log().last().copied().unwrap().clone();
    assert!(last.is_status_query(), "{}", state.log_text());
    assert_eq!(last.status, Some(StatusCode::OK));
}

#[tokio::test]
async fn partially_committed_chunk_resumes_at_the_range_reported_by_308() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        short_commit: Some((CHUNK, 100 * 1024)),
        ..Faults::default()
    })
    .await;

    let result = upload(&mock, &fixture, &target(), &options()).await;

    assert_uploaded_once(&mock, &fixture.data, result);
    let starts = mock.state().chunk_starts();
    assert_eq!(starts[..3], [0, CHUNK, CHUNK + 100 * 1024]);
}

#[tokio::test]
async fn repeated_308_without_progress_fails_within_the_retry_budget() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        stall_at: Some(CHUNK),
        ..Faults::default()
    })
    .await;

    let error = upload(&mock, &fixture, &target(), &options())
        .await
        .expect_err("an upload that makes no progress must fail");

    assert!(
        matches!(
            &error,
            UploadError::Failed {
                step: Step::Chunk,
                failure,
                reason: StopReason::GaveUp { .. },
            } if matches!(**failure, Failure::NoProgress)
        ),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("the server accepted none of the chunk"),
        "{error}"
    );
    let state = mock.state();
    let stalled = state
        .chunk_starts()
        .iter()
        .filter(|start| **start == CHUNK)
        .count();
    let max_attempts = RetryPolicy::for_tests().max_attempts as usize;
    assert!(
        (2..=max_attempts).contains(&stalled),
        "the stalled chunk must be sent again with backoff, at most {max_attempts} times, was \
         sent {stalled} times\n{}",
        state.log_text()
    );
    assert_eq!(state.sessions[0].received.len() as u64, CHUNK);
    assert!(state.files.is_empty());
}

#[tokio::test]
async fn rate_limited_chunk_waits_for_retry_after_and_holds_back_the_throttle() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        rate_limit_chunk_at: Some((CHUNK, 1)),
        ..Faults::default()
    })
    .await;
    let hub = mock.hub();
    let other_worker = hub.retrier().throttle.clone();

    // Another worker sharing the hub's throttle looks at it once the client has seen the 429.
    let held_back = async {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let remaining = other_worker.remaining();
            if !remaining.is_zero() {
                return remaining;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Duration::ZERO
    };
    let (target, options) = (target(), options());
    let (result, held_back) = tokio::join!(
        upload_path(&hub, &fixture.path, &target, &options, None),
        held_back
    );

    assert_uploaded_once(&mock, &fixture.data, result);
    assert!(
        held_back > Duration::from_millis(500),
        "a rate limit must hold back every request sharing the throttle, held back {held_back:?}"
    );
    let state = mock.state();
    let log = state.session_log();
    let limited = log
        .iter()
        .position(|request| request.status == Some(StatusCode::TOO_MANY_REQUESTS))
        .expect("the chunk is rate limited once");
    assert_eq!(log[limited].chunk_start(), Some(CHUNK));
    let next = log[limited + 1];
    let waited = next.at - log[limited].at;
    // The policy's own backoff is at most 10 ms here: only Retry-After explains a second.
    assert!(
        waited >= Duration::from_millis(950),
        "Retry-After: 1 must be honored, the next request came after {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "waited far longer than asked: {waited:?}"
    );
    assert_eq!(
        hub.retrier().throttle.remaining(),
        Duration::ZERO,
        "the hold is over"
    );
}

#[tokio::test]
async fn the_throttle_is_waited_for_before_every_chunk() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults::default()).await;
    let hub = mock.hub();
    let throttle = hub.retrier().throttle.clone();
    // Another worker hits a rate limit while the first chunk is confirmed.
    let progress = move |offset| {
        if offset == CHUNK {
            throttle.signal(Duration::from_millis(300));
        }
    };

    let result = upload_path(&hub, &fixture.path, &target(), &options(), Some(&progress)).await;

    assert_uploaded_once(&mock, &fixture.data, result);
    let state = mock.state();
    let log = state.session_log();
    let waited = log[1].at - log[0].at;
    assert!(
        waited >= Duration::from_millis(290),
        "the second chunk must wait for the throttle, it came after {waited:?}"
    );
}

#[tokio::test]
async fn an_expired_session_is_replaced_by_a_new_one() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        expire_session_at: Some(2 * CHUNK),
        ..Faults::default()
    })
    .await;

    let file = upload(&mock, &fixture, &target(), &options())
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", mock.log()));

    assert_eq!(file.id.as_deref(), Some("gen-id-1"));
    let state = mock.state();
    assert_eq!(state.sessions.len(), 2, "{}", state.log_text());
    assert!(state.sessions[0].expired);
    assert_eq!(
        state.sessions[1].received, fixture.data,
        "all of it in the new session"
    );
    assert_eq!(state.files.len(), 1);
    assert!(
        state
            .log
            .iter()
            .any(|request| request.path == "/drive/v3/files/gen-id-1"
                && request.status == Some(StatusCode::NOT_FOUND)),
        "before starting again, the file is looked up by its id"
    );
}

#[tokio::test]
async fn small_files_go_up_in_one_multipart_request() {
    let fixture = fixture(100_000);
    let mock = MockDrive::start(Faults::default()).await;
    let options = UploadOptions {
        chunk_size: CHUNK,
        ..UploadOptions::default()
    };
    let reported = Mutex::new(Vec::new());
    let progress = |offset| reported.lock().unwrap().push(offset);

    let file = upload_with_progress(&mock, &fixture, &target(), &options, Some(&progress))
        .await
        .unwrap();

    assert_eq!(file.id.as_deref(), Some("gen-id-1"));
    assert!(file.md5_checksum.is_some(), "the md5 was checked");
    let state = mock.state();
    let uploads = state.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].method, Method::POST);
    assert_eq!(
        uploads[0].query_param("uploadType").as_deref(),
        Some("multipart")
    );
    assert_eq!(
        uploads[0].header("authorization"),
        Some("Bearer mock-token")
    );
    let stored = &state.files["gen-id-1"];
    assert_eq!(stored.content, fixture.data);
    assert_eq!(stored.content_type, "application/octet-stream");
    assert_eq!(
        stored.metadata,
        json!({
            "id": "gen-id-1",
            "name": "data.bin",
            "mimeType": "application/octet-stream",
            "parents": ["folder-id"],
        })
    );
    assert!(state.session_log().is_empty());
    assert_eq!(*reported.lock().unwrap(), [100_000]);
}

#[tokio::test]
async fn the_multipart_threshold_does_not_depend_on_the_chunk_size() {
    // Larger than a chunk, at most the threshold: multipart.
    let fixture_small = fixture(3 * CHUNK + 1);
    let mock = MockDrive::start(Faults::default()).await;
    let options = UploadOptions {
        chunk_size: CHUNK,
        multipart_threshold: 4 * CHUNK,
        ..UploadOptions::default()
    };
    upload(&mock, &fixture_small, &target(), &options)
        .await
        .unwrap();
    assert!(mock.state().session_log().is_empty());

    // Larger than the threshold, at most a chunk: resumable, in one chunk.
    let fixture_large = fixture(2 * CHUNK);
    let mock = MockDrive::start(Faults::default()).await;
    let options = UploadOptions {
        chunk_size: 4 * CHUNK,
        multipart_threshold: CHUNK,
        ..UploadOptions::default()
    };
    upload(&mock, &fixture_large, &target(), &options)
        .await
        .unwrap();
    assert_eq!(mock.state().chunk_starts(), [0]);
}

#[tokio::test]
async fn an_empty_file_is_a_multipart_upload() {
    let fixture = fixture(0);
    let mock = MockDrive::start(Faults::default()).await;

    let file = upload(&mock, &fixture, &target(), &UploadOptions::default())
        .await
        .unwrap();

    assert_eq!(file.size, Some(0));
    assert_eq!(mock.state().files["gen-id-1"].content, b"");
}

#[tokio::test]
async fn a_wrong_md5_is_reported_with_the_file_id() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        wrong_md5: true,
        ..Faults::default()
    })
    .await;

    let error = upload(&mock, &fixture, &target(), &options())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, UploadError::Md5Mismatch { file_id, .. } if file_id == "gen-id-1"),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("gen-id-1"), "{message}");
    assert!(
        message.contains(&format!("{:x}", md5::compute(&fixture.data))),
        "{message}"
    );

    // Without verification, the same upload succeeds.
    let mock = MockDrive::start(Faults {
        wrong_md5: true,
        ..Faults::default()
    })
    .await;
    let options = UploadOptions {
        verify_md5: false,
        ..options()
    };
    upload(&mock, &fixture, &target(), &options).await.unwrap();
}

#[tokio::test]
async fn a_lost_multipart_reply_is_resolved_by_the_pre_generated_id() {
    let fixture = fixture(1000);
    let mock = MockDrive::start(Faults {
        cut_multipart_reply: true,
        ..Faults::default()
    })
    .await;

    let file = upload(&mock, &fixture, &target(), &UploadOptions::default())
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", mock.log()));

    assert_eq!(file.id.as_deref(), Some("gen-id-1"));
    let state = mock.state();
    assert_eq!(state.files.len(), 1, "no second file");
    let statuses: Vec<_> = state
        .log
        .iter()
        .map(|request| (request.method.clone(), request.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (Method::GET, Some(StatusCode::OK)),
            (Method::POST, Some(StatusCode::OK)),
            (Method::GET, Some(StatusCode::OK)),
        ],
        "generateIds, the upload whose reply broke off, the lookup by id"
    );
}

/// A create whose id exists already gets a 409: the file is looked up, not created again.
#[tokio::test]
async fn a_repeated_create_is_resolved_by_its_id() {
    let fixture = fixture(1000);
    let mock = MockDrive::start(Faults::default()).await;
    let target = UploadTarget {
        new_file_id: Some(String::from("earlier-id")),
        ..target()
    };
    upload(&mock, &fixture, &target, &UploadOptions::default())
        .await
        .unwrap();

    let file = upload(&mock, &fixture, &target, &UploadOptions::default())
        .await
        .unwrap();

    assert_eq!(file.id.as_deref(), Some("earlier-id"));
    let state = mock.state();
    assert_eq!(state.files.len(), 1);
    let statuses: Vec<_> = state.log.iter().map(|request| request.status).collect();
    assert_eq!(
        statuses,
        [
            Some(StatusCode::OK),
            Some(StatusCode::CONFLICT),
            Some(StatusCode::OK)
        ],
        "{}",
        state.log_text()
    );
}

#[tokio::test]
async fn a_failed_multipart_request_is_sent_again() {
    let fixture = fixture(1000);
    let mock = MockDrive::start(Faults {
        fail_multipart: true,
        ..Faults::default()
    })
    .await;

    upload(&mock, &fixture, &target(), &UploadOptions::default())
        .await
        .unwrap();

    let state = mock.state();
    assert_eq!(state.uploads().len(), 2);
    assert_eq!(state.files["gen-id-1"].content, fixture.data);
}

#[tokio::test]
async fn an_update_patches_the_file_without_a_new_id() {
    for options in [options(), UploadOptions::default()] {
        let fixture = fixture(SIZE);
        let mock = MockDrive::start(Faults::default()).await;
        let target = UploadTarget {
            existing_file_id: Some(String::from("existing-id")),
            ..target()
        };

        let file = upload(&mock, &fixture, &target, &options).await.unwrap();

        assert_eq!(file.id.as_deref(), Some("existing-id"));
        let state = mock.state();
        assert_eq!(state.generated_ids, 0);
        let upload = state.uploads()[0];
        assert_eq!(upload.method, Method::PATCH);
        assert_eq!(upload.path, "/upload/drive/v3/files/existing-id");
        assert_eq!(
            state.files["existing-id"].metadata,
            json!({"name": "data.bin"})
        );
        assert_eq!(state.files["existing-id"].content, fixture.data);
    }
}

#[tokio::test]
async fn a_conversion_asks_for_the_google_type_and_sends_the_source_type() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults::default()).await;
    let target = UploadTarget {
        name: String::from("notes.md"),
        mime: Some("text/markdown".parse().unwrap()),
        convert_to: Some(MIME_TYPE_DRIVE_DOCUMENT.parse().unwrap()),
        parents: Vec::new(),
        ..UploadTarget::default()
    };

    let file = upload(&mock, &fixture, &target, &options()).await.unwrap();

    assert_eq!(file.mime_type.as_deref(), Some(MIME_TYPE_DRIVE_DOCUMENT));
    let state = mock.state();
    assert_eq!(state.generated_ids, 0, "Drive takes no id for a conversion");
    let start = state.uploads()[0];
    assert_eq!(start.header("x-upload-content-type"), Some("text/markdown"));
    let mut metadata = serde_json::from_slice::<serde_json::Value>(&start.body).unwrap();
    let marker = metadata
        .as_object_mut()
        .unwrap()
        .remove("appProperties")
        .expect("a conversion is marked");
    assert_eq!(marker[super::UPLOAD_MARKER].as_str().unwrap().len(), 32);
    assert_eq!(
        metadata,
        json!({"name": "notes.md", "mimeType": MIME_TYPE_DRIVE_DOCUMENT})
    );
    for request in state.session_log() {
        assert_eq!(request.header("content-type"), Some("text/markdown"));
    }
}

/// Changes the file at `path` once the server confirmed the first chunk.
fn after_first_chunk(path: PathBuf, change: fn(&std::fs::File)) -> impl Fn(u64) + Send + Sync {
    move |offset| {
        if offset == CHUNK {
            // Open for writing rather than appending: Windows refuses set_len on an
            // append-only handle. Seek to the end so writes still append.
            let mut file = std::fs::File::options().write(true).open(&path).unwrap();
            std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0)).unwrap();
            change(&file);
        }
    }
}

#[tokio::test]
async fn a_source_that_gets_shorter_fails_without_retrying() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults::default()).await;
    let progress = after_first_chunk(fixture.path.clone(), |file| {
        file.set_len(2 * CHUNK + 100).unwrap();
    });

    let error = upload_with_progress(&mock, &fixture, &target(), &options(), Some(&progress))
        .await
        .unwrap_err();

    assert!(
        matches!(&error, UploadError::Read(source) if source.kind() == std::io::ErrorKind::UnexpectedEof),
        "{error:?}"
    );
    let state = mock.state();
    assert_eq!(state.sessions[0].received.len() as u64, 2 * CHUNK);
    assert!(state.files.is_empty());
}

#[tokio::test]
async fn a_source_that_grows_during_the_upload_is_an_error() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults::default()).await;
    let progress = after_first_chunk(fixture.path.clone(), |mut file| {
        std::io::Write::write_all(&mut file, b"more").unwrap();
    });

    let error = upload_with_progress(&mock, &fixture, &target(), &options(), Some(&progress))
        .await
        .unwrap_err();

    assert!(
        matches!(
            &error,
            UploadError::SourceChanged { file_id, uploaded: SIZE, now }
                if file_id == "gen-id-1" && *now == SIZE + 4
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("gen-id-1"), "{error}");
}

/// The size is the reader's when the upload starts, whatever an earlier look at the file said.
#[tokio::test]
async fn a_reader_is_uploaded_whole() {
    let mock = MockDrive::start(Faults::default()).await;
    let hub = mock.hub();
    let data = test_data(SIZE);

    upload_reader(
        &hub,
        Cursor::new(data.clone()),
        &target(),
        &options(),
        RetryPolicy::for_tests(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(mock.state().files["gen-id-1"].content, data);
}

#[tokio::test]
async fn a_rejected_start_keeps_the_drive_error_for_the_hints() {
    let fixture = fixture(SIZE);
    let body = json!({"error": {
        "code": 403,
        "message": "Service Accounts do not have storage quota.",
        "errors": [{"domain": "usageLimits", "reason": "storageQuotaExceeded"}],
    }});
    let mock = MockDrive::start(Faults {
        reject_start: Some((403, body.clone())),
        ..Faults::default()
    })
    .await;

    let error = upload(&mock, &fixture, &target(), &options())
        .await
        .unwrap_err();

    assert!(
        matches!(
            &error,
            UploadError::Failed {
                step: Step::Start,
                reason: StopReason::Quota { .. },
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "starting the upload failed: Drive answered 403 Forbidden (quota exceeded \
         (storageQuotaExceeded))"
    );
    let source = std::error::Error::source(&error).expect("the Drive error");
    assert!(matches!(
        source.downcast_ref::<google_drive3::Error>(),
        Some(google_drive3::Error::BadRequest(json)) if *json == body
    ));
    assert_eq!(mock.state().uploads().len(), 1, "not retried");
    assert!(matches!(
        error.into_drive_error(),
        google_drive3::Error::BadRequest(json) if json == body
    ));
}

#[test]
fn the_offset_after_a_308_follows_its_range() {
    let headers = |range: &str| {
        let mut headers = HeaderMap::new();
        headers.insert("range", range.parse().unwrap());
        headers
    };
    assert_eq!(next_offset(&HeaderMap::new(), 100), Some(0));
    assert_eq!(next_offset(&headers("bytes=0-42"), 100), Some(43));
    assert_eq!(next_offset(&headers("bytes 0-99"), 100), Some(100));
    assert_eq!(next_offset(&headers("bytes=0-100"), 100), None);
    assert_eq!(next_offset(&headers("bytes=5-42"), 100), None);
    assert_eq!(next_offset(&headers("items=0-42"), 100), None);
}

#[test]
fn chunks_are_multiples_of_256_kib() {
    let chunk_len = |chunk_size| {
        UploadOptions {
            chunk_size,
            ..UploadOptions::default()
        }
        .chunk_len()
    };
    assert_eq!(chunk_len(0), CHUNK);
    assert_eq!(chunk_len(CHUNK + 1), CHUNK);
    assert_eq!(chunk_len(3 * CHUNK + CHUNK - 1), 3 * CHUNK);
    assert_eq!(chunk_len(32 << 20), 32 << 20);
}

#[test]
fn errors_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<UploadError>();
}

/// A scheduler spawns uploads on the runtime's worker threads.
#[tokio::test]
async fn an_upload_can_be_spawned() {
    fn assert_send<T: Send>(_: &T) {}
    let mock = MockDrive::start(Faults::default()).await;
    let hub = mock.hub();
    let path = PathBuf::from("missing");
    let (target, options) = (target(), options());
    let progress = |_| {};
    let upload = upload_path(&hub, &path, &target, &options, Some(&progress));
    assert_send(&upload);
    let reader = upload_reader(
        &hub,
        Cursor::new(Vec::new()),
        &target,
        &options,
        RetryPolicy::for_tests(),
        None,
    );
    assert_send(&reader);
}

/// Session records in the `uploads` directory of a temporary account directory.
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
            ..options()
        }
    }

    fn files(&self) -> Vec<PathBuf> {
        match std::fs::read_dir(&self.uploads) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn assert_none(&self) {
        assert_eq!(self.files(), Vec::<PathBuf>::new(), "the record is deleted");
    }

    /// The one record there is.
    fn record(&self) -> serde_json::Value {
        let files = self.files();
        assert_eq!(files.len(), 1, "{files:?}");
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap()
    }

    fn rewrite(&self, change: impl FnOnce(&mut serde_json::Value)) {
        let mut record = self.record();
        change(&mut record);
        std::fs::write(&self.files()[0], record.to_string()).unwrap();
    }
}

/// A first run that loses the connection to Drive after `chunks` accepted chunks, and fails.
async fn interrupted_run(fixture: &Fixture, records: &Records, chunks: u32) -> MockDrive {
    let mock = MockDrive::start(Faults {
        die_after_chunks: Some(chunks),
        ..Faults::default()
    })
    .await;
    let error = upload(&mock, fixture, &target(), &records.options())
        .await
        .expect_err("the first run is interrupted");
    assert!(
        matches!(
            error,
            UploadError::Failed {
                reason: StopReason::GaveUp { .. },
                ..
            }
        ),
        "{error:?}"
    );
    mock.revive();
    mock
}

#[tokio::test]
async fn an_interrupted_upload_continues_in_the_next_run() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let mock = interrupted_run(&fixture, &records, 2).await;

    let record = records.record();
    assert_eq!(record["offset"], 2 * CHUNK);
    assert_eq!(record["chunk_size"], CHUNK);
    assert_eq!(record["source"]["size"], SIZE);
    assert_eq!(
        record["source"]["path"],
        std::fs::canonicalize(&fixture.path)
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert!(record["source"]["mtime"]["secs"].as_u64().unwrap() > 0);
    assert_eq!(record["target"]["name"], "data.bin");
    assert_eq!(record["target"]["parents"], json!(["folder-id"]));
    assert_eq!(record["target"]["file_id"], "gen-id-1");
    assert_eq!(record["identity"], "user-a@example.com");
    assert!(record["session_url"]
        .as_str()
        .unwrap()
        .ends_with("/upload-session/0"));
    let age = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - record["created"].as_u64().unwrap();
    assert!(age < 60, "{age}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&records.files()[0])
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let before = mock.state().log.len();
    let reported = Mutex::new(Vec::new());
    let progress = |offset| reported.lock().unwrap().push(offset);
    let file = upload_with_progress(
        &mock,
        &fixture,
        &target(),
        &records.options(),
        Some(&progress),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}\n{}", mock.log()));

    assert_eq!(file.id.as_deref(), Some("gen-id-1"));
    let state = mock.state();
    let second_run = &state.log[before..];
    assert_eq!(second_run[0].path, "/drive/v3/about", "who the account is");
    assert_eq!(
        second_run[0].query_param("fields").as_deref(),
        Some("user(emailAddress)")
    );
    assert!(second_run[1].is_status_query(), "{}", state.log_text());
    let starts: Vec<_> = second_run.iter().filter_map(Logged::chunk_start).collect();
    assert_eq!(
        starts,
        [2 * CHUNK, 3 * CHUNK, 4 * CHUNK],
        "only the rest is sent"
    );
    assert_eq!(state.sessions.len(), 1, "the session is continued");
    assert_eq!(state.generated_ids, 1);
    assert_eq!(state.files.len(), 1);
    assert_eq!(state.files["gen-id-1"].content, fixture.data);
    assert_eq!(
        *reported.lock().unwrap(),
        [2 * CHUNK, 3 * CHUNK, 4 * CHUNK, SIZE]
    );
    records.assert_none();
}

#[tokio::test]
async fn a_changed_source_is_uploaded_from_the_start() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let mock = interrupted_run(&fixture, &records, 2).await;

    let changed: Vec<u8> = fixture
        .data
        .iter()
        .map(|byte| byte.wrapping_add(1))
        .collect();
    std::fs::write(&fixture.path, &changed).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&fixture.path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    let before = mock.state().log.len();

    let file = upload(&mock, &fixture, &target(), &records.options())
        .await
        .unwrap();

    let state = mock.state();
    assert!(
        state.log[before..]
            .iter()
            .all(|request| request.path != "/upload-session/0"),
        "the old session is left alone"
    );
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(
        state.sessions[1].received, changed,
        "all of it, from byte 0"
    );
    assert_eq!(file.id.as_deref(), Some("gen-id-2"));
    assert_eq!(state.files["gen-id-2"].content, changed);
    records.assert_none();
}

#[tokio::test]
async fn a_record_older_than_a_week_is_not_continued() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let mock = interrupted_run(&fixture, &records, 2).await;
    records.rewrite(|record| {
        let created = record["created"].as_u64().unwrap();
        record["created"] = (created - 8 * 24 * 60 * 60).into();
    });
    let before = mock.state().log.len();

    upload(&mock, &fixture, &target(), &records.options())
        .await
        .unwrap();

    let state = mock.state();
    assert!(state.log[before..]
        .iter()
        .all(|request| request.path != "/upload-session/0"));
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.sessions[1].received, fixture.data);
    records.assert_none();
}

#[tokio::test]
async fn a_session_that_is_gone_is_replaced_by_a_new_one() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let mock = interrupted_run(&fixture, &records, 2).await;
    mock.state().sessions[0].expired = true;

    let file = upload(&mock, &fixture, &target(), &records.options())
        .await
        .unwrap();

    let state = mock.state();
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.sessions[1].received, fixture.data);
    assert_eq!(
        file.id.as_deref(),
        Some("gen-id-1"),
        "the new session creates the file with the same id"
    );
    assert_eq!(state.generated_ids, 1);
    records.assert_none();
}

#[tokio::test]
async fn a_session_that_completed_is_found_by_its_record() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    // The last chunk completes the session, but its reply is lost and so is the connection.
    let mock = MockDrive::start(Faults {
        die_after_chunks: Some(5),
        cut_final_reply: true,
        ..Faults::default()
    })
    .await;
    upload(&mock, &fixture, &target(), &records.options())
        .await
        .unwrap_err();
    mock.revive();
    assert_eq!(records.record()["offset"], 4 * CHUNK);
    let before = mock.state().log.len();

    let file = upload(&mock, &fixture, &target(), &records.options())
        .await
        .unwrap();

    assert_eq!(file.id.as_deref(), Some("gen-id-1"));
    let state = mock.state();
    let second_run = &state.log[before..];
    assert_eq!(second_run.len(), 2, "{}", state.log_text());
    assert!(second_run[1].is_status_query());
    assert_eq!(state.files.len(), 1);
    records.assert_none();
}

#[tokio::test]
async fn without_a_session_dir_nothing_is_recorded() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let mock = MockDrive::start(Faults {
        die_after_chunks: Some(2),
        ..Faults::default()
    })
    .await;
    upload(&mock, &fixture, &target(), &options())
        .await
        .unwrap_err();
    assert!(!records.uploads.exists());
}

/// The failure messages of the tests are formatted from the guard they hold: taking the lock
/// again for them would deadlock on a real failure.
#[tokio::test]
async fn failure_logs_come_from_the_held_guard() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults::default()).await;
    upload(&mock, &fixture, &target(), &options())
        .await
        .unwrap();

    let state = mock.state();
    let text = state.log_text();
    assert!(text.contains("POST /upload/drive/v3/files"), "{text}");
    assert!(
        text.contains(&format!(
            "PUT /upload-session/0 Content-Range: bytes 0-{}",
            CHUNK - 1
        )),
        "{text}"
    );
    drop(state);
    assert_eq!(mock.log(), text);
}

fn conversion() -> UploadTarget {
    UploadTarget {
        parents: vec![String::from("folder-id")],
        name: String::from("notes.md"),
        mime: Some("text/markdown".parse().unwrap()),
        convert_to: Some(MIME_TYPE_DRIVE_DOCUMENT.parse().unwrap()),
        ..UploadTarget::default()
    }
}

/// A conversion has no pre-generated id: when Drive created it but the whole reply was lost, the
/// file is found by its marker instead of being uploaded a second time.
#[tokio::test]
async fn a_conversion_whose_reply_is_lost_is_found_by_its_marker() {
    let fixture = fixture(1000);
    let mock = MockDrive::start(Faults {
        drop_multipart_reply: true,
        ..Faults::default()
    })
    .await;

    let file = upload(&mock, &fixture, &conversion(), &UploadOptions::default())
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", mock.log()));

    let state = mock.state();
    assert_eq!(
        state.files.len(),
        1,
        "no second document\n{}",
        state.log_text()
    );
    let (id, stored) = state.files.iter().next().unwrap();
    assert_eq!(file.id.as_ref(), Some(id));
    let requests: Vec<_> = state
        .log
        .iter()
        .map(|request| {
            (
                request.method.clone(),
                request.path.as_str(),
                request.status,
            )
        })
        .collect();
    assert_eq!(
        requests,
        [
            (Method::POST, "/upload/drive/v3/files", None),
            (Method::GET, "/drive/v3/files", Some(StatusCode::OK)),
        ],
        "the upload whose reply was dropped, then the search"
    );
    let search = state.log[1].query_param("q").unwrap();
    let marker = stored.metadata["appProperties"][super::UPLOAD_MARKER]
        .as_str()
        .unwrap();
    assert_eq!(
        search,
        format!(
            "appProperties has {{ key='gdriveUploadId' and value='{marker}' }} and trashed = \
             false and 'folder-id' in parents"
        )
    );
}

/// When the search finds nothing, the request did not create the file: it is sent again.
#[tokio::test]
async fn a_failed_conversion_is_sent_again_when_no_file_has_its_marker() {
    let fixture = fixture(1000);
    let mock = MockDrive::start(Faults {
        fail_multipart: true,
        ..Faults::default()
    })
    .await;

    upload(&mock, &fixture, &conversion(), &UploadOptions::default())
        .await
        .unwrap();

    let state = mock.state();
    assert_eq!(state.files.len(), 1);
    assert_eq!(state.uploads().len(), 2, "{}", state.log_text());
    assert!(state
        .log
        .iter()
        .any(|request| request.path == "/drive/v3/files"));
}

/// A resumable conversion whose session expired is searched for before a new session starts.
#[tokio::test]
async fn an_expired_conversion_session_is_replaced_once_its_marker_is_not_found() {
    let fixture = fixture(SIZE);
    let mock = MockDrive::start(Faults {
        expire_session_at: Some(CHUNK),
        ..Faults::default()
    })
    .await;

    upload(&mock, &fixture, &conversion(), &options())
        .await
        .unwrap();

    let state = mock.state();
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.files.len(), 1);
    assert_eq!(
        state.sessions[0].metadata["appProperties"], state.sessions[1].metadata["appProperties"],
        "the same marker in both sessions"
    );
}

/// The account directory does not say who the credentials are (Application Default Credentials
/// can be another account on the next run): a session started as one account is not continued
/// as another.
#[tokio::test]
async fn a_session_of_another_account_is_not_continued() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let mock = interrupted_run(&fixture, &records, 2).await;
    mock.state().email = String::from("user-b@example.com");
    let before = mock.state().log.len();

    let file = upload(&mock, &fixture, &target(), &records.options())
        .await
        .unwrap();

    let state = mock.state();
    assert!(
        state.log[before..]
            .iter()
            .all(|request| request.path != "/upload-session/0"),
        "the other account's session is left alone\n{}",
        state.log_text()
    );
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.sessions[1].received, fixture.data);
    assert_eq!(
        file.id.as_deref(),
        Some("gen-id-2"),
        "not the other account's id"
    );
    records.assert_none();
}

/// A service account is known by its key and subject, without asking Drive.
#[tokio::test]
async fn a_service_account_is_identified_without_a_request() {
    let fixture = fixture(SIZE);
    let records = Records::new();
    let service_account = |identity: &str| UploadOptions {
        session_identity: Some(identity.to_owned()),
        ..records.options()
    };
    let mock = MockDrive::start(Faults {
        die_after_chunks: Some(2),
        ..Faults::default()
    })
    .await;
    upload(
        &mock,
        &fixture,
        &target(),
        &service_account("service account a"),
    )
    .await
    .unwrap_err();
    mock.revive();
    assert_eq!(records.record()["identity"], "service account a");

    let record = records.files().remove(0);
    let saved = std::fs::read(&record).unwrap();

    // Another subject: not continued.
    upload(
        &mock,
        &fixture,
        &target(),
        &service_account("service account a as b"),
    )
    .await
    .unwrap();
    assert_eq!(mock.state().sessions.len(), 2);

    // The same service account: continued.
    std::fs::write(&record, saved).unwrap();
    upload(
        &mock,
        &fixture,
        &target(),
        &service_account("service account a"),
    )
    .await
    .unwrap();
    let state = mock.state();
    assert_eq!(state.sessions.len(), 2, "no new session");
    assert!(state.sessions[0].complete);
    assert!(
        state
            .log
            .iter()
            .all(|request| request.path != "/drive/v3/about"),
        "{}",
        state.log_text()
    );
}
