//! The account commands with a service account, run as the gdrive binary against a temporary
//! home directory and an in-process mock of the token endpoint. The key's `token_uri` points at
//! the mock, which is what `ServiceAccountAuthenticator` sends the signed JWT to; nothing here
//! talks to Google.

use std::{
    path::{Path, PathBuf},
    process::Output,
};

// `allow`, not `expect`: these tests use only the mock server and the key helpers.
#[allow(
    dead_code,
    unreachable_pub,
    reason = "only part of gdrive's OAuth test helpers is used here"
)]
#[path = "../src/auth/test_support.rs"]
mod test_support;

use test_support::{service_account_key_json, verified_jwt_claims, MockServer, TEST_CLIENT_EMAIL};

const TOKEN_RESPONSE: &str =
    r#"{"access_token":"ya29.integration","expires_in":3599,"token_type":"Bearer"}"#;

/// Runs gdrive with `home` as its home directory (and working directory) and no proxy.
async fn gdrive(home: &Path, args: &[&str]) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_gdrive"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home);
    for var in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        command.env_remove(var);
    }
    command.output().await.expect("gdrive runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn account_dir(home: &Path, name: &str) -> PathBuf {
    home.join(".config").join("gdrive3").join(name)
}

/// A gdrive 3.x user account, which logs in with an OAuth client.
fn add_user_account(home: &Path, name: &str) {
    let user_dir = account_dir(home, name);
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::write(
        user_dir.join("secret.json"),
        r#"{"client_id":"id","client_secret":"secret"}"#,
    )
    .unwrap();
    std::fs::write(user_dir.join("tokens.json"), "[]").unwrap();
}

/// Checks the files of the service account in `dir`: the key as given, mode 0600, and the subject.
fn assert_service_account_files(dir: &Path, key: &str) {
    assert_eq!(
        std::fs::read_to_string(dir.join("service_account.json")).unwrap(),
        key
    );
    let account_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("account.json")).unwrap()).unwrap();
    assert_eq!(
        account_json,
        serde_json::json!({"type": "service_account", "subject": "user@example.com"})
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("service_account.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

/// Exports the service account from `home`, imports it into a new home and checks it there.
async fn assert_export_and_import(home: &Path, key: &str) {
    let exported = gdrive(home, &["account", "export", TEST_CLIENT_EMAIL]).await;
    assert!(exported.status.success(), "{}", stderr(&exported));
    let archive = std::fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "tar"))
        .expect("export writes a .tar file");

    let other_home = tempfile::tempdir().unwrap();
    let other_home = other_home.path();
    let imported = gdrive(
        other_home,
        &["account", "import", archive.to_str().unwrap()],
    )
    .await;
    assert!(imported.status.success(), "{}", stderr(&imported));
    let list = gdrive(other_home, &["account", "list"]).await;
    assert_eq!(stdout(&list), format!("{TEST_CLIENT_EMAIL}\n"));
    let current = gdrive(other_home, &["account", "current"]).await;
    assert_eq!(stdout(&current), format!("{TEST_CLIENT_EMAIL}\n"));
    assert_service_account_files(&account_dir(other_home, TEST_CLIENT_EMAIL), key);
}

#[tokio::test]
async fn a_service_account_is_added_listed_exported_imported_and_removed() {
    let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let key = service_account_key_json(&server.url("/token"));
    std::fs::write(home.join("key.json"), &key).unwrap();

    add_user_account(home, "me@example.com");

    let added = gdrive(
        home,
        &[
            "account",
            "add",
            "--service-account",
            "key.json",
            "--subject",
            "user@example.com",
        ],
    )
    .await;
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(
        stdout(&added).contains(&format!("Logged in as {TEST_CLIENT_EMAIL}")),
        "{}",
        stdout(&added)
    );
    assert!(!stdout(&added).contains("ya29.integration"));

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one token request checks the key");
    let claims = verified_jwt_claims(&requests[0].form("assertion").unwrap());
    assert_eq!(claims["sub"], "user@example.com");

    let dir = account_dir(home, TEST_CLIENT_EMAIL);
    assert_service_account_files(&dir, &key);

    let list = gdrive(home, &["account", "list"]).await;
    assert_eq!(
        stdout(&list),
        format!("me@example.com\n{TEST_CLIENT_EMAIL}\n")
    );
    let current = gdrive(home, &["account", "current"]).await;
    assert_eq!(stdout(&current), format!("{TEST_CLIENT_EMAIL}\n"));

    let switched = gdrive(home, &["account", "switch", "me@example.com"]).await;
    assert!(switched.status.success(), "{}", stderr(&switched));
    let switched = gdrive(home, &["account", "switch", TEST_CLIENT_EMAIL]).await;
    assert!(switched.status.success(), "{}", stderr(&switched));

    // A service account needs no reauthorization, and says what to do instead.
    let reauth = gdrive(home, &["account", "reauth"]).await;
    assert!(!reauth.status.success());
    assert!(
        stderr(&reauth).starts_with(&format!(
            "Error: account '{TEST_CLIENT_EMAIL}' is a service account"
        )),
        "{}",
        stderr(&reauth)
    );
    assert!(stderr(&reauth).contains("--service-account KEY_JSON"));

    assert_export_and_import(home, &key).await;

    let removed = gdrive(home, &["account", "remove", TEST_CLIENT_EMAIL]).await;
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!dir.exists());
    let list = gdrive(home, &["account", "list"]).await;
    assert_eq!(stdout(&list), "me@example.com\n");

    // Only the add sent a token request.
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_key_that_does_not_work_adds_nothing() {
    let server = MockServer::start(vec![(
        400,
        String::from(r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#),
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    std::fs::write(
        home.join("key.json"),
        service_account_key_json(&server.url("/token")),
    )
    .unwrap();

    let added = gdrive(home, &["account", "add", "--service-account", "key.json"]).await;
    assert_eq!(added.status.code(), Some(1));
    let message = stderr(&added);
    assert!(
        message.starts_with("Error: the service account key does not work: "),
        "{message}"
    );
    assert!(message.contains("Invalid JWT Signature."), "{message}");
    assert!(!message.contains("BEGIN PRIVATE KEY"), "{message}");
    assert!(!account_dir(home, TEST_CLIENT_EMAIL).exists());
}
