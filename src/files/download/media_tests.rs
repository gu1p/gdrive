//! Protocol tests for the media request of `files download`.

use http_body_util::BodyExt;
use serde_json::json;

use super::{download_file, download_with_hub, Config, Destination, ExistingFileAction};
use crate::common::{
    drive_file::{MIME_TYPE_DRIVE_FOLDER, MIME_TYPE_DRIVE_SHORTCUT},
    mock_server::{MockServer, Response},
};

async fn media_request(acknowledge_abuse: bool) -> crate::common::mock_server::Request {
    let server = MockServer::start(vec![Response::json(200, &serde_json::json!("content"))]).await;

    let body = download_file(&server.hub(), "file-id", acknowledge_abuse)
        .await
        .unwrap();
    let bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"\"content\"");

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
