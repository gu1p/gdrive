//! Protocol tests for the media requests of `files download`: parameters, and resuming a
//! download that breaks off (`resume.rs`).

use std::path::Path;

use http_body_util::BodyExt;
use serde_json::json;

use super::{
    download_with_hub, file_content,
    resume::{Identity, Revision},
    save_to_file, Config, Destination, ExistingFileAction, Expected, Next, ResumableDownload,
};
use crate::common::{
    drive_file::{MIME_TYPE_DRIVE_FOLDER, MIME_TYPE_DRIVE_SHORTCUT},
    mock_server::{MockServer, Response},
    retry::RetryPolicy,
};

async fn media_request(acknowledge_abuse: bool) -> crate::common::mock_server::Request {
    let server = MockServer::start(vec![Response::raw(200, "content")]).await;
    let hub = server.hub();

    let source = file_content(&hub, "file-id", acknowledge_abuse, String::from("test")).unwrap();
    let mut download = ResumableDownload::new(source, hub.retrier().state(), Expected::default());
    let Next::Body { body, restart } = download.next().await.unwrap() else {
        panic!("expected a body");
    };
    assert!(!restart);
    let bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"content");

    let mut requests = server.requests();
    assert_eq!(requests.len(), 1);
    requests.remove(0)
}

#[tokio::test]
async fn acknowledge_abuse_is_sent_with_the_media_request() {
    let request = media_request(true).await;
    assert_eq!(request.path, "/drive/v3/files/file-id");
    assert_eq!(request.query_param("alt"), Some("media"));
    assert_eq!(request.query_param("acknowledgeAbuse"), Some("true"));
    assert_eq!(request.query_param("supportsAllDrives"), Some("true"));
    assert_eq!(
        request.header("range"),
        None,
        "the first request has no Range"
    );
}

#[tokio::test]
async fn abuse_is_not_acknowledged_by_default() {
    let request = media_request(false).await;
    assert_eq!(request.query_param("acknowledgeAbuse"), Some("false"));
}

fn config(file_id: &str, destination: &std::path::Path) -> Config {
    Config {
        file_id: file_id.to_owned(),
        existing_file_action: ExistingFileAction::Abort,
        follow_shortcuts: false,
        download_directories: false,
        destination: Destination::Path(destination.to_owned()),
        acknowledge_abuse: true,
        transfer: crate::transfer::scheduler::Options::default(),
    }
}

/// The requests that fetched file content, as (path, acknowledgeAbuse).
fn media_requests(server: &MockServer) -> Vec<(String, Option<String>)> {
    server
        .requests()
        .into_iter()
        .filter(|request| request.query_param("alt") == Some("media"))
        .map(|request| {
            let acknowledge_abuse = request.query_param("acknowledgeAbuse").map(str::to_owned);
            (request.path, acknowledge_abuse)
        })
        .collect()
}

#[tokio::test]
async fn acknowledge_abuse_reaches_a_followed_shortcut_target() {
    let server = MockServer::start(vec![
        Response::json(
            200,
            &json!({
                "id": "shortcut", "name": "link", "mimeType": MIME_TYPE_DRIVE_SHORTCUT,
                "shortcutDetails": {"targetId": "target", "targetMimeType": "text/plain"},
            }),
        ),
        Response::json(
            200,
            &json!({"id": "target", "name": "a.txt", "mimeType": "text/plain"}),
        ),
        Response::raw(200, "hello"),
    ])
    .await;
    let destination = tempfile::tempdir().unwrap();

    let config = Config {
        follow_shortcuts: true,
        ..config("shortcut", destination.path())
    };
    download_with_hub(&server.hub(), config).await.unwrap();

    assert_eq!(
        media_requests(&server),
        [(
            String::from("/drive/v3/files/target"),
            Some(String::from("true"))
        )]
    );
    let content = std::fs::read(destination.path().join("a.txt")).unwrap();
    assert_eq!(content, b"hello");
}

#[tokio::test]
async fn acknowledge_abuse_reaches_every_file_of_a_recursive_download() {
    let content = "hello";
    let md5 = format!("{:x}", md5::compute(content));
    let server = MockServer::start(vec![
        Response::json(
            200,
            &json!({"id": "dir", "name": "folder", "mimeType": MIME_TYPE_DRIVE_FOLDER}),
        ),
        Response::json(
            200,
            &json!({"files": [{
                "id": "f1", "name": "a.txt", "mimeType": "text/plain",
                "size": content.len().to_string(), "md5Checksum": md5,
            }]}),
        ),
        Response::raw(200, content),
    ])
    .await;
    let destination = tempfile::tempdir().unwrap();

    let config = Config {
        download_directories: true,
        ..config("dir", destination.path())
    };
    download_with_hub(&server.hub(), config).await.unwrap();

    assert_eq!(
        media_requests(&server),
        [(
            String::from("/drive/v3/files/f1"),
            Some(String::from("true"))
        )]
    );
    let downloaded = std::fs::read(destination.path().join("folder").join("a.txt")).unwrap();
    assert_eq!(downloaded, content.as_bytes());
}

/// 64 KiB of distinct bytes, so that a misplaced resume shows.
fn content() -> String {
    (0..65_536_u32)
        .map(|i| char::from(b'a' + u8::try_from((i * 7 + i / 251) % 26).unwrap()))
        .collect()
}

/// Downloads `file-id` of `size` bytes with md5 `md5` into `dir/out.bin` from `server`.
async fn download(
    server: &MockServer,
    dir: &Path,
    size: Option<u64>,
    md5: Option<md5::Digest>,
) -> Result<(), super::errors::SaveBodyToFile> {
    let hub = server.hub();
    let source = file_content(&hub, "file-id", false, String::from("test download")).unwrap();
    let mut download =
        ResumableDownload::new(source, hub.retrier().state(), Expected { md5, size });
    let first = download
        .next()
        .await
        .map_err(|err| super::errors::SaveBodyToFile::Request(Box::new(err)))?;
    save_to_file(download, first, &dir.join("out.bin"), "file-id").await
}

/// The file holds exactly `expected`; on failure, says where it differs instead of printing it.
pub(crate) fn assert_content(path: &Path, expected: &str) {
    let actual = std::fs::read(path).unwrap();
    let first_difference = actual
        .iter()
        .zip(expected.as_bytes())
        .position(|(actual, expected)| actual != expected);
    assert_eq!(
        (actual.len(), first_difference),
        (expected.len(), None),
        "{} must be byte-identical to the content (length, first differing offset)",
        path.display()
    );
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn ranges(server: &MockServer) -> Vec<Option<String>> {
    server
        .requests()
        .iter()
        .map(|request| request.header("range").map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_cut_body_is_resumed_with_a_range_request() {
    let content = content();
    let cut = 20_000;
    let total = content.len();
    let server = MockServer::start(vec![
        Response::raw(200, &content).cut_after(cut),
        Response::raw(206, &content[cut..]).with_header(
            "Content-Range",
            &format!("bytes {cut}-{}/{total}", total - 1),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download(
        &server,
        dir.path(),
        Some(total as u64),
        Some(md5::compute(&content)),
    )
    .await
    .unwrap();

    assert_eq!(ranges(&server), [None, Some(format!("bytes={cut}-"))]);
    assert_content(&dir.path().join("out.bin"), &content);
    assert_eq!(dir_entries(dir.path()), ["out.bin"]);
}

#[tokio::test]
async fn a_body_cut_several_times_is_resumed_each_time() {
    // Each attempt brings new bytes, so the budget (4 attempts in tests) starts over each time.
    let content = content();
    let total = content.len();
    let cuts = [10_000, 20_000, 30_000, 40_000, 50_000];
    let mut replies = vec![Response::raw(200, &content).cut_after(cuts[0])];
    for pair in cuts.windows(2) {
        replies.push(
            Response::raw(206, &content[pair[0]..])
                .with_header(
                    "Content-Range",
                    &format!("bytes {}-{}/{total}", pair[0], total - 1),
                )
                .cut_after(pair[1] - pair[0]),
        );
    }
    let last = cuts[cuts.len() - 1];
    replies.push(Response::raw(206, &content[last..]).with_header(
        "Content-Range",
        &format!("bytes {last}-{}/{total}", total - 1),
    ));
    let server = MockServer::start(replies).await;
    let dir = tempfile::tempdir().unwrap();

    download(
        &server,
        dir.path(),
        Some(total as u64),
        Some(md5::compute(&content)),
    )
    .await
    .unwrap();

    assert_eq!(server.requests().len(), cuts.len() + 1);
    assert_content(&dir.path().join("out.bin"), &content);
}

#[tokio::test]
async fn a_200_reply_to_a_range_request_starts_over() {
    let content = content();
    let cut = 30_000;
    let server = MockServer::start(vec![
        Response::raw(200, &content).cut_after(cut),
        // The server ignores the Range header and sends everything again.
        Response::raw(200, &content),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download(
        &server,
        dir.path(),
        Some(content.len() as u64),
        Some(md5::compute(&content)),
    )
    .await
    .unwrap();

    assert_eq!(ranges(&server), [None, Some(format!("bytes={cut}-"))]);
    assert_content(&dir.path().join("out.bin"), &content);
    assert_eq!(
        dir_entries(dir.path()),
        ["out.bin"],
        "the discarded temporary file is removed"
    );
}

#[tokio::test]
async fn a_206_that_starts_elsewhere_is_not_appended() {
    let content = content();
    let total = content.len();
    let cut = 30_000;
    let server = MockServer::start(vec![
        Response::raw(200, &content).cut_after(cut),
        // Starts 1000 bytes too early: appending it would duplicate bytes.
        Response::raw(206, &content[cut - 1000..]).with_header(
            "Content-Range",
            &format!("bytes {}-{}/{total}", cut - 1000, total - 1),
        ),
        Response::raw(200, &content),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download(&server, dir.path(), Some(total as u64), None)
        .await
        .unwrap();

    assert_eq!(
        ranges(&server),
        [None, Some(format!("bytes={cut}-")), None],
        "after a wrong Content-Range the content is requested from the start"
    );
    assert_content(&dir.path().join("out.bin"), &content);
}

#[tokio::test]
async fn a_416_with_another_size_starts_over() {
    let content = content();
    let total = content.len();
    let cut = 30_000;
    let server = MockServer::start(vec![
        Response::raw(200, &content).cut_after(cut),
        Response::raw(416, "").with_header("Content-Range", &format!("bytes */{total}")),
        Response::raw(200, &content),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download(&server, dir.path(), Some(total as u64), None)
        .await
        .unwrap();

    assert_eq!(ranges(&server), [None, Some(format!("bytes={cut}-")), None]);
    assert_content(&dir.path().join("out.bin"), &content);
}

#[tokio::test]
async fn a_416_for_a_file_that_shrank_is_a_size_mismatch() {
    let content = content();
    let cut = 30_000;
    let server = MockServer::start(vec![
        Response::raw(200, &content).cut_after(cut),
        // The file is now exactly as long as what was received, but Drive's metadata said more.
        Response::raw(416, "").with_header("Content-Range", &format!("bytes */{cut}")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    let result = download(&server, dir.path(), Some(content.len() as u64), None).await;

    assert!(
        matches!(
            result,
            Err(super::errors::SaveBodyToFile::SizeMismatch { expected, actual })
                if expected == content.len() as u64 && actual == cut as u64
        ),
        "{result:?}"
    );
    assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn a_persistent_failure_removes_the_temp_file_and_keeps_the_destination() {
    let content = content();
    let attempts = RetryPolicy::for_tests().max_attempts;
    let mut replies = vec![Response::raw(200, &content).cut_after(10_000)];
    replies.extend((0..attempts + 2).map(|_| {
        Response::json(
            503,
            &json!({"error": {"code": 503, "message": "Backend Error"}}),
        )
    }));
    let server = MockServer::start(replies).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("out.bin"), b"old").unwrap();

    let result = download(
        &server,
        dir.path(),
        Some(content.len() as u64),
        Some(md5::compute(&content)),
    )
    .await;

    let Err(error) = result else {
        panic!("a download that cannot be resumed must fail");
    };
    assert!(
        matches!(error, super::errors::SaveBodyToFile::Request(_)),
        "{error:?}"
    );
    let message = crate::error_message(&error);
    assert!(
        message.contains("unable to resume the download")
            && message.contains("Drive answered 503")
            && message.contains("gave up after 4 attempts"),
        "{message}"
    );
    assert_eq!(
        server.requests().len(),
        usize::try_from(attempts).unwrap(),
        "the attempt that broke off and the resumes make max_attempts attempts"
    );
    assert_eq!(std::fs::read(dir.path().join("out.bin")).unwrap(), b"old");
    assert_eq!(
        dir_entries(dir.path()),
        ["out.bin"],
        "no temporary file is left"
    );
}

#[tokio::test]
async fn a_body_that_keeps_breaking_without_progress_gives_up() {
    let content = content();
    let attempts = RetryPolicy::for_tests().max_attempts;
    let total = content.len();
    let cut = 10_000;
    let mut replies = vec![Response::raw(200, &content).cut_after(cut)];
    // Every resume breaks off before its first byte.
    replies.extend((0..attempts + 2).map(|_| {
        Response::raw(206, &content[cut..])
            .with_header(
                "Content-Range",
                &format!("bytes {cut}-{}/{total}", total - 1),
            )
            .cut_after(0)
    }));
    let server = MockServer::start(replies).await;
    let dir = tempfile::tempdir().unwrap();

    let result = download(&server, dir.path(), Some(total as u64), None).await;

    assert!(
        matches!(result, Err(super::errors::SaveBodyToFile::ReadChunk(_))),
        "{result:?}"
    );
    assert_eq!(
        server.requests().len(),
        usize::try_from(attempts).unwrap(),
        "the attempt that broke off and the resumes make max_attempts attempts"
    );
    assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn the_byte_count_is_checked_without_md5() {
    // Without an md5 (exports, Workspace files) the byte count is checked.
    let server = MockServer::start(vec![Response::raw(200, "all of it")]).await;
    let dir = tempfile::tempdir().unwrap();
    download(&server, dir.path(), Some(9), None).await.unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("out.bin")).unwrap(),
        b"all of it"
    );

    // A reply that is complete by its Content-Length but shorter than Drive's size.
    let server = MockServer::start(vec![Response::raw(200, "only part")]).await;
    let dir = tempfile::tempdir().unwrap();
    let result = download(&server, dir.path(), Some(100), None).await;
    assert!(
        matches!(
            result,
            Err(super::errors::SaveBodyToFile::SizeMismatch {
                expected: 100,
                actual: 9
            })
        ),
        "{result:?}"
    );
    assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn a_missing_file_is_not_retried() {
    let server = MockServer::start(vec![Response::json(
        404,
        &json!({"error": {"code": 404, "message": "File not found: file-id."}}),
    )])
    .await;
    let dir = tempfile::tempdir().unwrap();

    let result = download(&server, dir.path(), None, None).await;

    assert!(result.is_err());
    assert_eq!(server.requests().len(), 1);
    assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
}

/// Downloads `file-id` like [`download`], resuming only while the file is still at `revision`.
async fn download_revision(
    server: &MockServer,
    dir: &Path,
    revision: Revision,
) -> Result<(), super::errors::SaveBodyToFile> {
    let hub = server.hub();
    let source = file_content(&hub, "file-id", false, String::from("test download")).unwrap();
    let mut download = ResumableDownload::new(source, hub.retrier().state(), revision.expected())
        .with_identity(Identity::new(&hub, "file-id", revision));
    let first = download
        .next()
        .await
        .map_err(|err| super::errors::SaveBodyToFile::Request(Box::new(err)))?;
    save_to_file(download, first, &dir.join("out.bin"), "file-id").await
}

/// A `files.get` reply with the fields of a [`Revision`].
fn revision_reply(version: u64, md5: Option<md5::Digest>, size: usize, modified: &str) -> Response {
    let mut file = json!({
        "id": "file-id",
        "version": version.to_string(),
        "size": size.to_string(),
        "modifiedTime": modified,
    });
    if let Some(md5) = md5 {
        file["md5Checksum"] = format!("{md5:x}").into();
    }
    Response::json(200, &file)
}

fn revision(version: i64, md5: Option<md5::Digest>, size: usize, modified: &str) -> Revision {
    Revision {
        version: Some(version),
        md5,
        size: Some(size as u64),
        modified_time: Some(modified.parse().unwrap()),
    }
}

const BEFORE: &str = "2026-11-01T10:00:00.000Z";
const AFTER: &str = "2026-11-01T10:05:00.000Z";

#[tokio::test]
async fn a_file_replaced_by_one_of_the_same_size_is_downloaded_again_from_the_start() {
    // Version B replaces version A between the break and the resume: same size, other bytes.
    let a = content();
    let b = a.to_uppercase();
    assert_eq!(a.len(), b.len());
    let cut = 20_000;
    let server = MockServer::start(vec![
        Response::raw(200, &a).cut_after(cut),
        // The check before the resume sees the new revision.
        revision_reply(8, Some(md5::compute(&b)), b.len(), AFTER),
        Response::raw(200, &b),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download_revision(
        &server,
        dir.path(),
        revision(7, Some(md5::compute(&a)), a.len(), BEFORE),
    )
    .await
    .expect("the new revision is downloaded and checked against its own md5");

    assert_content(&dir.path().join("out.bin"), &b);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].path, "/drive/v3/files/file-id");
    assert_eq!(
        requests[1].query_param("fields"),
        Some("id%2Cversion%2Cmd5Checksum%2Csize%2CmodifiedTime")
    );
    assert_eq!(
        requests[2].header("range"),
        None,
        "a changed file is requested from the start, never resumed"
    );
    assert_eq!(dir_entries(dir.path()), ["out.bin"]);
}

#[tokio::test]
async fn a_change_that_the_metadata_does_not_show_yet_is_caught_by_if_range() {
    // No md5 and unchanged metadata: only the ETag tells the versions apart.
    let a = content();
    let b = a.to_uppercase();
    let cut = 20_000;
    let server = MockServer::start(vec![
        Response::raw(200, &a)
            .with_header("ETag", "\"rev-a\"")
            .cut_after(cut),
        revision_reply(7, None, a.len(), BEFORE),
        // The server answers the If-Range of an outdated ETag with all of the new content.
        Response::raw(200, &b).with_header("ETag", "\"rev-b\""),
        revision_reply(8, None, b.len(), AFTER),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download_revision(&server, dir.path(), revision(7, None, a.len(), BEFORE))
        .await
        .unwrap();

    assert_content(&dir.path().join("out.bin"), &b);
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[2].header("range"),
        Some(format!("bytes={cut}-").as_str())
    );
    assert_eq!(requests[2].header("if-range"), Some("\"rev-a\""));
    assert_eq!(dir_entries(dir.path()), ["out.bin"]);
}

#[tokio::test]
async fn an_unchanged_file_is_resumed_with_its_etag() {
    let content = content();
    let total = content.len();
    let cut = 20_000;
    let server = MockServer::start(vec![
        Response::raw(200, &content)
            .with_header("ETag", "\"rev-a\"")
            .cut_after(cut),
        revision_reply(7, Some(md5::compute(&content)), total, BEFORE),
        Response::raw(206, &content[cut..]).with_header(
            "Content-Range",
            &format!("bytes {cut}-{}/{total}", total - 1),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download_revision(
        &server,
        dir.path(),
        revision(7, Some(md5::compute(&content)), total, BEFORE),
    )
    .await
    .unwrap();

    assert_content(&dir.path().join("out.bin"), &content);
    let requests = server.requests();
    assert_eq!(
        requests[2].header("range"),
        Some(format!("bytes={cut}-").as_str())
    );
    assert_eq!(requests[2].header("if-range"), Some("\"rev-a\""));
}

#[tokio::test]
async fn a_weak_etag_is_not_sent_as_if_range() {
    let content = content();
    let total = content.len();
    let cut = 20_000;
    let server = MockServer::start(vec![
        Response::raw(200, &content)
            .with_header("ETag", "W/\"rev-a\"")
            .cut_after(cut),
        Response::raw(206, &content[cut..]).with_header(
            "Content-Range",
            &format!("bytes {cut}-{}/{total}", total - 1),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    download(&server, dir.path(), Some(total as u64), None)
        .await
        .unwrap();

    assert_eq!(server.requests()[1].header("if-range"), None);
}

#[tokio::test]
async fn a_file_deleted_before_the_resume_is_a_clean_error() {
    let content = content();
    let server = MockServer::start(vec![
        Response::raw(200, &content).cut_after(20_000),
        Response::json(
            404,
            &json!({"error": {"code": 404, "message": "File not found: file-id."}}),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    let result = download_revision(
        &server,
        dir.path(),
        revision(7, None, content.len(), BEFORE),
    )
    .await;

    let Err(error) = result else {
        panic!("a file that is gone cannot be resumed");
    };
    let message = crate::error_message(&error);
    assert!(
        message.contains("unable to check whether the file changed on Drive"),
        "{message}"
    );
    assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
}
