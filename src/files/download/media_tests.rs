//! Protocol tests for the media request of `files download`.

use http_body_util::BodyExt;

use super::download_file;
use crate::common::mock_server::{MockServer, Response};

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
