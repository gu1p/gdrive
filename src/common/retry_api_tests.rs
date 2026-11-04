//! Metadata calls retry per the policy of `retry.rs`, against the mock Drive server.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::{
    common::mock_server::{MockServer, Request, Response},
    files::{
        self,
        list::{ListFilesConfig, ListQuery, ListSortOrder},
    },
};

fn google_error(code: u16, reason: &str, message: &str) -> serde_json::Value {
    json!({"error": {
        "code": code,
        "message": message,
        "errors": [{"domain": "global", "reason": reason, "message": message}],
    }})
}

fn file_page(ids: &[&str]) -> Response {
    let files: Vec<_> = ids
        .iter()
        .map(|id| json!({"id": id, "name": format!("name of {id}")}))
        .collect();
    Response::json(200, &json!({ "files": files }))
}

async fn list(server: &MockServer) -> Result<Vec<String>, files::list::Error> {
    let query = ListQuery::files_in_folder(String::from("folder-id"));
    let files = files::list::list_files(
        &server.hub(),
        ListFilesConfig {
            query: &query,
            order_by: &ListSortOrder::default(),
            max_files: 100,
        },
    )
    .await?;
    Ok(files.into_iter().filter_map(|file| file.id).collect())
}

/// The request as far as a retry must repeat it.
fn signature(request: &Request) -> (String, String, Vec<(String, String)>) {
    (
        request.method.clone(),
        request.path.clone(),
        request.query.clone(),
    )
}

#[tokio::test]
async fn listing_succeeds_after_a_503() {
    let server = MockServer::start(vec![
        Response::json(503, &google_error(503, "backendError", "Backend Error")),
        file_page(&["a", "b"]),
    ])
    .await;

    let ids = list(&server).await.expect("the listing must be retried");

    assert_eq!(ids, ["a", "b"]);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        signature(&requests[0]),
        signature(&requests[1]),
        "the retry repeats the same request"
    );
    assert_eq!(requests[1].path, "/drive/v3/files");
}

#[tokio::test]
async fn rate_limited_listing_waits_for_retry_after() {
    let server = MockServer::start(vec![
        Response::json(
            403,
            &google_error(403, "rateLimitExceeded", "Rate Limit Exceeded"),
        )
        .with_header("Retry-After", "1"),
        file_page(&["a"]),
    ])
    .await;
    let hub = server.hub();
    let query = ListQuery::files_in_folder(String::from("folder-id"));

    let start = Instant::now();
    let files = files::list::list_files(
        &hub,
        ListFilesConfig {
            query: &query,
            order_by: &ListSortOrder::default(),
            max_files: 100,
        },
    )
    .await
    .expect("a rate-limited listing must be retried");
    let elapsed = start.elapsed();

    assert_eq!(files.len(), 1);
    assert_eq!(server.requests().len(), 2);
    // The test policy's own backoff is at most 10 ms: only Retry-After explains a second.
    assert!(
        elapsed >= Duration::from_millis(950),
        "Retry-After: 1 must be honored, retried after {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    assert_eq!(
        hub.retrier().throttle.remaining(),
        Duration::ZERO,
        "the hold is over"
    );
}

#[tokio::test]
async fn a_rate_limit_holds_back_the_next_call_of_the_hub() {
    let server = MockServer::start(vec![
        Response::json(
            429,
            &google_error(429, "rateLimitExceeded", "Rate Limit Exceeded"),
        )
        .with_header("Retry-After", "1"),
        file_page(&["a"]),
    ])
    .await;
    let hub = server.hub();

    // The throttle is shared by the hub's calls: a call that starts while another one waits
    // out a rate limit waits as well. Here the second call starts right after the first one
    // has seen the 429.
    let throttle = hub.retrier().throttle.clone();
    let started = Instant::now();
    let query = ListQuery::files_in_folder(String::from("folder-id"));
    let order_by = ListSortOrder::default();
    let first = files::list::list_files(
        &hub,
        ListFilesConfig {
            query: &query,
            order_by: &order_by,
            max_files: 100,
        },
    );
    let second_waits = async {
        while throttle.remaining().is_zero() {
            if started.elapsed() > Duration::from_secs(5) {
                return Duration::ZERO;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let waiting = Instant::now();
        throttle.wait().await;
        waiting.elapsed()
    };
    let (first, waited) = tokio::join!(first, second_waits);

    first.expect("the listing must be retried");
    assert!(
        waited >= Duration::from_millis(900),
        "a second request must wait for the shared throttle, waited {waited:?}"
    );
}

#[tokio::test]
async fn not_found_is_not_retried() {
    let server = MockServer::start(vec![Response::json(
        404,
        &google_error(404, "notFound", "File not found: missing."),
    )])
    .await;

    let error = files::info::get_file(&server.hub(), "missing")
        .await
        .expect_err("a missing file is an error");

    assert!(
        matches!(&error, google_drive3::Error::BadRequest(body)
            if body["error"]["code"] == 404),
        "the error is returned unchanged: {error:?}"
    );
    assert_eq!(server.requests().len(), 1, "a 404 is not retried");
}

#[tokio::test]
async fn permission_denied_is_not_retried() {
    let server = MockServer::start(vec![Response::json(
        403,
        &google_error(
            403,
            "insufficientFilePermissions",
            "The user does not have sufficient permissions for this file.",
        ),
    )])
    .await;

    let result = list(&server).await;

    assert!(result.is_err());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn unauthorized_is_not_retried_and_points_at_reauth() {
    let server = MockServer::start(vec![Response::json(
        401,
        &google_error(401, "authError", "Invalid Credentials"),
    )])
    .await;

    let error = files::info::get_file(&server.hub(), "file-id")
        .await
        .expect_err("a 401 is an error");

    assert_eq!(server.requests().len(), 1, "a 401 is not retried");
    assert_eq!(
        crate::error_message(&error),
        "Error: Token retrieval failed: Google did not accept the access token (Invalid \
         Credentials); run 'gdrive account reauth' to authorize the current account again"
    );
}

#[tokio::test]
async fn a_persistent_503_gives_up_after_the_budget() {
    let attempts = crate::common::retry::RetryPolicy::for_tests().max_attempts;
    let replies = (0..attempts + 2)
        .map(|_| Response::json(503, &google_error(503, "backendError", "Backend Error")))
        .collect();
    let server = MockServer::start(replies).await;

    let error = files::info::get_file(&server.hub(), "file-id")
        .await
        .expect_err("the budget must end the retries");

    assert!(matches!(error, google_drive3::Error::BadRequest(_)));
    assert_eq!(
        server.requests().len(),
        usize::try_from(attempts).unwrap(),
        "exactly max_attempts attempts"
    );
}
