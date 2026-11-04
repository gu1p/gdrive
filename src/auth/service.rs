//! Credentials of service accounts (#49).
//!
//! A service account authenticates with the private key of its JSON key file: yup-oauth2's
//! `ServiceAccountAuthenticator` signs a JWT with it and exchanges that at the key's `token_uri`
//! for an access token (RFC 7523). There is no refresh token and nothing interactive: when the
//! cached token expires, a new JWT is signed. With a subject, the token is issued for that
//! Google Workspace user (domain-wide delegation), which the domain's administrator has to allow.
//!
//! Every token is requested for [`SERVICE_SCOPES`], whatever scope a Drive call asks for: the
//! full Drive scope covers all of them, and domain-wide delegation must be granted for each scope
//! a service account asks for, so one scope keeps that setup to one line.

use std::{
    error,
    fmt::{self, Display, Formatter, Write as _},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use hyper_util::client::legacy::{connect::Connect, Client};
use serde_json::{Map, Value};
use yup_oauth2::{
    authenticator::Authenticator, error::AuthErrorCode, storage::TokenInfo, AccessToken,
    CustomHyperClientBuilder, ServiceAccountAuthenticator, ServiceAccountKey,
};

use super::store::{TokenFile, TokenStore};
use crate::{
    app_config::{account_type::AccountType, AppConfig},
    common::json_error::RedactedJsonError,
};

/// The scopes of every token of a service account.
pub(crate) const SERVICE_SCOPES: [&str; 1] = ["https://www.googleapis.com/auth/drive"];

/// How long one token request may take before gdrive gives up.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(60);

/// Checks the content of a service account key file and returns the key.
///
/// Only the fields gdrive needs are checked: `"type": "service_account"`, `client_email`,
/// `private_key` and `token_uri`, which must be an https URL (plain http only on the loopback
/// interface, for local test servers). Whether the private key is a usable RSA key shows when
/// the authenticator is built. Errors never quote the file: it holds the private key.
pub(crate) fn parse_key(content: &[u8]) -> Result<ServiceAccountKey, KeyError> {
    let value: Value = serde_json::from_slice(content)
        .map_err(|error| KeyError::Json(RedactedJsonError::from(&error)))?;
    let Value::Object(fields) = value else {
        return Err(KeyError::NotAnObject);
    };

    match fields.get("type") {
        Some(Value::String(key_type)) if key_type == "service_account" => {}
        Some(Value::String(key_type)) => return Err(KeyError::WrongType(known_type(key_type))),
        _ if fields.contains_key("installed") || fields.contains_key("web") => {
            return Err(KeyError::OAuthClient);
        }
        _ => return Err(KeyError::WrongType(None)),
    }
    for field in ["client_email", "private_key", "token_uri"] {
        if string_field(&fields, field).is_none_or(|value| value.trim().is_empty()) {
            return Err(KeyError::MissingField(field));
        }
    }
    if !is_acceptable_token_uri(string_field(&fields, "token_uri").unwrap_or_default()) {
        return Err(KeyError::TokenUri);
    }

    serde_json::from_value(Value::Object(fields)).map_err(|_| KeyError::OptionalFieldType)
}

fn string_field<'a>(fields: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    fields.get(name).and_then(Value::as_str)
}

/// The value of a `"type"` that is not `service_account`, if it is one of the credential types
/// Google writes; anything else is not repeated in messages.
fn known_type(key_type: &str) -> Option<String> {
    const KNOWN: [&str; 4] = [
        "authorized_user",
        "external_account",
        "external_account_authorized_user",
        "impersonated_service_account",
    ];
    KNOWN.contains(&key_type).then(|| key_type.to_owned())
}

fn is_acceptable_token_uri(token_uri: &str) -> bool {
    let Ok(url) = url::Url::parse(token_uri) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => match url.host() {
            Some(url::Host::Domain(domain)) => domain == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// Who the tokens are for, for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    client_email: String,
    client_id: Option<String>,
    subject: Option<String>,
}

impl Display for Identity {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "service account '{}'", self.client_email)?;
        if let Some(subject) = &self.subject {
            write!(f, " acting for '{subject}'")?;
        }
        Ok(())
    }
}

/// The credentials of a service account. Implements the Drive API's `GetToken`.
#[derive(Clone)]
pub(crate) struct ServiceCredentials<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    auth: Authenticator<C>,
    identity: Arc<Identity>,
}

impl<C> ServiceCredentials<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Credentials for `key`, acting for `subject` if there is one. With `tokens_path`, tokens are
    /// cached in that `tokens.json`; an unreadable cache counts as empty. Sends no request.
    pub(crate) async fn service_account(
        client: Client<C, String>,
        key: ServiceAccountKey,
        subject: Option<String>,
        tokens_path: Option<&Path>,
    ) -> Result<Self, Error> {
        let identity = Arc::new(Identity {
            client_email: key.client_email.clone(),
            client_id: key.client_id.clone(),
            subject: subject.clone(),
        });

        let mut builder =
            ServiceAccountAuthenticator::with_client(key, CustomHyperClientBuilder::from(client));
        if let Some(subject) = subject {
            builder = builder.subject(subject);
        }
        if let Some(path) = tokens_path {
            let cached = TokenFile::load(path)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            builder =
                builder.with_storage(Box::new(TokenStore::new(path.to_path_buf(), cached, None)));
        }
        // Parses the private key, which can fail.
        let auth = builder.build().await.map_err(|source| Error::PrivateKey {
            identity: Arc::clone(&identity),
            source,
        })?;

        Ok(ServiceCredentials { auth, identity })
    }

    /// The credentials of the service account `app_cfg`, from its stored key, with tokens cached
    /// in its `tokens.json`.
    pub(crate) async fn load(
        client: Client<C, String>,
        app_cfg: &AppConfig,
        subject: Option<String>,
    ) -> Result<Self, Error> {
        let path = app_cfg.service_account_key_path();
        let content = match tokio::fs::read(&path).await {
            Ok(content) => content,
            Err(source) => return Err(Error::ReadKey { path, source }),
        };
        let key = parse_key(&content).map_err(|source| Error::Key { path, source })?;
        Self::service_account(client, key, subject, Some(app_cfg.tokens_path())).await
    }

    /// An access token for [`SERVICE_SCOPES`]: the cached one while it is valid, otherwise a new
    /// one, which is cached.
    pub(crate) async fn access_token(&self) -> Result<AccessToken, Error> {
        let result = tokio::time::timeout(TOKEN_TIMEOUT, self.auth.token(&SERVICE_SCOPES))
            .await
            .map_err(|_| Error::Timeout {
                identity: Arc::clone(&self.identity),
            })?;
        result.map_err(|source| Error::Token {
            identity: Arc::clone(&self.identity),
            source,
        })
    }
}

/// The tokens file for a token obtained when the account was added.
pub(crate) fn token_file(token: &AccessToken) -> TokenFile {
    let mut file = TokenFile::default();
    file.set(
        &SERVICE_SCOPES,
        TokenInfo {
            access_token: token.token().map(str::to_owned),
            refresh_token: None,
            expires_at: token.expiration_time(),
            id_token: None,
        },
    );
    file
}

type GetTokenOutput<'a> = super::stored::GetTokenOutput<'a>;

impl<C> google_apis_common::GetToken for ServiceCredentials<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Ignores `scopes`: see [`SERVICE_SCOPES`].
    fn get_token<'a>(&'a self, _scopes: &'a [&str]) -> GetTokenOutput<'a> {
        Box::pin(async move {
            let token = self.access_token().await?;
            Ok(token.token().map(str::to_owned))
        })
    }
}

/// What to do about a token error of `identity`, if gdrive knows.
fn token_hint(identity: &Identity, error: &yup_oauth2::Error) -> Option<String> {
    let yup_oauth2::Error::AuthError(error) = error else {
        return None;
    };
    match (&error.error, &identity.subject) {
        (AuthErrorCode::UnauthorizedClient, Some(_)) => {
            let client_id = identity
                .client_id
                .as_deref()
                .unwrap_or("the client ID of the service account");
            Some(format!(
                "domain-wide delegation is not allowed for this service account. A Google \
                 Workspace administrator has to add client ID {client_id} with the scope {} in \
                 the Admin console (Security > Access and data control > API controls > \
                 Domain-wide delegation).",
                SERVICE_SCOPES[0]
            ))
        }
        (AuthErrorCode::InvalidGrant, subject) => {
            let mut hint = String::from(
                "Google refuses the key when it was deleted or disabled, or when this computer's \
                 clock is off by more than a few minutes",
            );
            if let Some(subject) = subject {
                let _ = write!(
                    hint,
                    ", or when '{subject}' is not a user of the Workspace domain"
                );
            }
            hint.push_str(
                ". To use a new key, run `gdrive account add --service-account KEY_JSON` again.",
            );
            Some(hint)
        }
        _ => None,
    }
}

/// Writes the messages of `error` and its sources, separated by `: `.
///
/// The Drive client wraps token errors in `MissingToken`, whose `source()` is `None`, so the
/// messages of the sources would be lost; they are part of the message of [`Error`] instead.
fn write_chain(f: &mut Formatter<'_>, error: &(dyn error::Error + 'static)) -> fmt::Result {
    write!(f, "{}", error.to_string().trim_end())?;
    let mut source = error.source();
    while let Some(cause) = source {
        write!(f, ": {}", cause.to_string().trim_end())?;
        source = cause.source();
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    ReadKey {
        path: PathBuf,
        source: io::Error,
    },
    Key {
        path: PathBuf,
        source: KeyError,
    },
    PrivateKey {
        identity: Arc<Identity>,
        source: io::Error,
    },
    Token {
        identity: Arc<Identity>,
        source: yup_oauth2::Error,
    },
    Timeout {
        identity: Arc<Identity>,
    },
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::ReadKey { path, .. } => {
                write!(
                    f,
                    "unable to read the service account key '{}'",
                    path.display()
                )
            }
            Error::Key { path, .. } => write!(
                f,
                "'{}' is not a usable service account key",
                path.display()
            ),
            Error::PrivateKey { identity, source } => {
                write!(f, "the private key of {identity} cannot be used: ")?;
                write_chain(f, source)
            }
            Error::Token { identity, source } => {
                write!(f, "unable to get an access token for {identity}: ")?;
                write_chain(f, source)?;
                if let Some(hint) = token_hint(identity, source) {
                    write!(f, "\nHint: {hint}")?;
                }
                Ok(())
            }
            Error::Timeout { identity } => write!(
                f,
                "no answer to the token request of {identity} within {} seconds",
                TOKEN_TIMEOUT.as_secs()
            ),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::ReadKey { source, .. } => Some(source),
            Error::Key { source, .. } => Some(source),
            // Part of the message, see `write_chain`.
            Error::PrivateKey { .. } | Error::Token { .. } | Error::Timeout { .. } => None,
        }
    }
}

/// A service account key file that cannot be used. Never quotes the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyError {
    Json(RedactedJsonError),
    NotAnObject,
    OAuthClient,
    /// The `"type"` is missing or not `service_account`; holds it if it is a known type.
    WrongType(Option<String>),
    MissingField(&'static str),
    TokenUri,
    OptionalFieldType,
}

impl Display for KeyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Json(error) => write!(f, "the file is not valid JSON ({error})"),
            KeyError::NotAnObject => f.write_str("the file holds no JSON object"),
            KeyError::OAuthClient => f.write_str(
                "this is an OAuth client file, not a service account key\nTo add a Google \
                 account with this client, run `gdrive account add` and enter its client ID and \
                 secret.",
            ),
            KeyError::WrongType(Some(key_type)) => write!(
                f,
                "this is not a service account key: its \"type\" is \"{key_type}\", not \
                 \"service_account\""
            ),
            KeyError::WrongType(None) => f.write_str(
                "this is not a service account key: \"type\" is not \"service_account\"",
            ),
            KeyError::MissingField(field) => write!(f, "the key has no \"{field}\""),
            KeyError::TokenUri => f.write_str("the \"token_uri\" of the key is not an https URL"),
            KeyError::OptionalFieldType => {
                f.write_str("a field of the key has the wrong type (all of them are strings)")
            }
        }
    }
}

impl error::Error for KeyError {}

/// The hint for a Drive error that comes from a service account having no storage of its own,
/// if `error` is one and the account is a service account.
pub(crate) fn storage_hint(
    error: &(dyn error::Error + 'static),
    account_type: &AccountType,
) -> Option<&'static str> {
    let AccountType::ServiceAccount { subject } = account_type else {
        return None;
    };
    if !find_drive_error(error).is_some_and(is_storage_or_ownership_error) {
        return None;
    }
    Some(if subject.is_some() {
        "Hint: files a service account creates are owned by the user it acts for (--subject); \
         that user's storage may be full, or the files may have to go into a shared drive."
    } else {
        "Hint: service accounts have no Drive storage of their own and cannot own files. Upload \
         into a shared drive that the service account is a member of (--parent with a folder of \
         that drive), or add the account again with --subject USER to act for a Google Workspace \
         user (domain-wide delegation)."
    })
}

/// The JSON of a Drive API error response in the source chain of `error`.
fn find_drive_error<'a>(error: &'a (dyn error::Error + 'static)) -> Option<&'a Value> {
    let mut current = Some(error);
    while let Some(error) = current {
        let drive_error = error
            .downcast_ref::<google_apis_common::Error>()
            .or_else(|| {
                error
                    .downcast_ref::<Box<google_apis_common::Error>>()
                    .map(AsRef::as_ref)
            });
        if let Some(google_apis_common::Error::BadRequest(value)) = drive_error {
            return Some(value);
        }
        current = error.source();
    }
    None
}

/// Drive's `storageQuotaExceeded` (what a service account gets for a file it would own), or a
/// 403 about ownership.
fn is_storage_or_ownership_error(value: &Value) -> bool {
    let error = &value["error"];
    let mut reasons = error["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|error| error["reason"].as_str());
    let message = error["message"].as_str().unwrap_or_default().to_lowercase();

    reasons.any(|reason| reason == "storageQuotaExceeded")
        || (error["code"] == 403
            && (message.contains("storage quota") || message.contains("owner")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::auth::test_support::{
        service_account_key_json, verified_jwt_claims, MockServer, TEST_CLIENT_EMAIL,
    };

    fn http_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, String> {
        Client::builder(hyper_util::rt::TokioExecutor::new()).build_http()
    }

    const TOKEN_RESPONSE: &str =
        r#"{"access_token":"ya29.service","expires_in":3599,"token_type":"Bearer"}"#;

    fn key_value(token_uri: &str) -> Value {
        serde_json::from_str(&service_account_key_json(token_uri)).unwrap()
    }

    fn parse(value: &Value) -> Result<ServiceAccountKey, KeyError> {
        parse_key(value.to_string().as_bytes())
    }

    #[test]
    fn a_console_key_file_is_accepted() {
        let key = parse(&key_value("https://oauth2.googleapis.com/token")).unwrap();
        assert_eq!(key.client_email, TEST_CLIENT_EMAIL);
        assert_eq!(key.token_uri, "https://oauth2.googleapis.com/token");
        assert_eq!(key.client_id.as_deref(), Some("123456789012345678901"));
    }

    #[test]
    fn key_files_without_the_needed_fields_are_refused() {
        let complete = key_value("https://oauth2.googleapis.com/token");
        for field in ["client_email", "private_key", "token_uri"] {
            let mut key = complete.clone();
            key.as_object_mut().unwrap().remove(field);
            assert_eq!(parse(&key).unwrap_err(), KeyError::MissingField(field));

            key[field] = json!("  ");
            assert_eq!(parse(&key).unwrap_err(), KeyError::MissingField(field));
        }

        let mut key = complete.clone();
        key.as_object_mut().unwrap().remove("type");
        assert_eq!(parse(&key).unwrap_err(), KeyError::WrongType(None));

        let mut key = complete.clone();
        key["project_id"] = json!(42);
        assert_eq!(parse(&key).unwrap_err(), KeyError::OptionalFieldType);
    }

    #[test]
    fn other_credential_files_are_named_and_refused() {
        let user = json!({"type": "authorized_user", "client_id": "i", "client_secret": "s", "refresh_token": "r"});
        assert_eq!(
            parse(&user).unwrap_err(),
            KeyError::WrongType(Some(String::from("authorized_user")))
        );

        let client = json!({"installed": {"client_id": "i", "client_secret": "s"}});
        let error = parse(&client).unwrap_err();
        assert_eq!(error, KeyError::OAuthClient);
        assert!(error.to_string().contains("gdrive account add"), "{error}");

        // An unknown type is not repeated: it is content of the file.
        let mut odd = key_value("https://oauth2.googleapis.com/token");
        odd["type"] = json!("something-private");
        let error = parse(&odd).unwrap_err();
        assert_eq!(error, KeyError::WrongType(None));
        assert!(!error.to_string().contains("something-private"));

        assert_eq!(parse(&json!([1])).unwrap_err(), KeyError::NotAnObject);
    }

    #[test]
    fn key_errors_do_not_quote_the_file() {
        let secret = "-----BEGIN PRIVATE KEY-----secret";
        for content in [
            format!(r#""{secret}""#),
            format!(r#"{{"type": "service_account", "private_key": "{secret}""#),
            format!(r#"{{"type": "service_account", "private_key": "{secret}"}} trailing"#),
        ] {
            let error = parse_key(content.as_bytes()).unwrap_err();
            assert!(
                matches!(error, KeyError::Json(_) | KeyError::NotAnObject),
                "{error:?}"
            );
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[test]
    fn the_token_uri_must_be_https_except_on_loopback() {
        for uri in [
            "https://oauth2.googleapis.com/token",
            "http://127.0.0.1:8080/token",
            "http://localhost/token",
            "http://[::1]:1/token",
        ] {
            assert!(parse(&key_value(uri)).is_ok(), "{uri}");
        }
        for uri in [
            "http://oauth2.googleapis.com/token",
            "ftp://127.0.0.1/token",
            "oauth2.googleapis.com/token",
            "https://",
        ] {
            assert_eq!(
                parse(&key_value(uri)).unwrap_err(),
                KeyError::TokenUri,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn a_bad_private_key_is_reported_when_the_credentials_are_built() {
        let mut key = key_value("https://oauth2.googleapis.com/token");
        key["private_key"] =
            json!("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n");
        let key = parse(&key).unwrap();

        let error = ServiceCredentials::service_account(http_client(), key, None, None)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, Error::PrivateKey { .. }), "{error:?}");
        assert!(error.to_string().starts_with(&format!(
            "the private key of service account '{TEST_CLIENT_EMAIL}' cannot be used: "
        )));
    }

    #[tokio::test]
    async fn the_token_is_requested_with_a_signed_jwt_at_the_token_uri_of_the_key() {
        let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let token_uri = server.url("/custom/token");
        let key = parse(&key_value(&token_uri)).unwrap();

        let credentials = ServiceCredentials::service_account(
            http_client(),
            key,
            Some(String::from("user@example.com")),
            None,
        )
        .await
        .unwrap();
        let token = credentials.access_token().await.unwrap();
        assert_eq!(token.token(), Some("ya29.service"));
        // Cached in memory: no second request.
        credentials.access_token().await.unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/custom/token");
        assert_eq!(
            requests[0].form("grant_type").as_deref(),
            Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
        );
        let claims = verified_jwt_claims(&requests[0].form("assertion").unwrap());
        assert_eq!(claims["iss"], TEST_CLIENT_EMAIL);
        assert_eq!(claims["aud"], token_uri.as_str());
        assert_eq!(claims["scope"], SERVICE_SCOPES[0]);
        assert_eq!(claims["sub"], "user@example.com");
    }

    #[tokio::test]
    async fn get_token_asks_for_the_full_drive_scope_whatever_the_call_wants() {
        use google_apis_common::GetToken;

        let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let key = parse(&key_value(&server.url("/token"))).unwrap();
        let credentials = ServiceCredentials::service_account(http_client(), key, None, None)
            .await
            .unwrap();

        let token = credentials
            .get_token(&["https://www.googleapis.com/auth/drive.meet.readonly"])
            .await
            .unwrap();
        assert_eq!(token.as_deref(), Some("ya29.service"));

        let claims = verified_jwt_claims(&server.requests()[0].form("assertion").unwrap());
        assert_eq!(claims["scope"], SERVICE_SCOPES[0]);
        assert!(claims.get("sub").is_none_or(Value::is_null), "{claims}");
    }

    #[tokio::test]
    async fn tokens_are_cached_in_tokens_json_and_renewed_when_they_expire() {
        let server = MockServer::start(vec![
            (
                200,
                String::from(r#"{"access_token":"first","expires_in":30,"token_type":"Bearer"}"#),
            ),
            (200, String::from(TOKEN_RESPONSE)),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let tokens_path = dir.path().join("tokens.json");
        let key = || parse(&key_value(&server.url("/token"))).unwrap();

        // Expires within the minute yup-oauth2 keeps in reserve, so the next run renews it.
        let credentials =
            ServiceCredentials::service_account(http_client(), key(), None, Some(&tokens_path))
                .await
                .unwrap();
        assert_eq!(
            credentials.access_token().await.unwrap().token(),
            Some("first")
        );
        let saved = TokenFile::load(&tokens_path).await.unwrap().unwrap();
        assert_eq!(
            saved.get(&SERVICE_SCOPES).unwrap().access_token.as_deref(),
            Some("first")
        );

        let next_run =
            ServiceCredentials::service_account(http_client(), key(), None, Some(&tokens_path))
                .await
                .unwrap();
        assert_eq!(
            next_run.access_token().await.unwrap().token(),
            Some("ya29.service")
        );
        assert_eq!(server.requests().len(), 2);

        // A valid cached token is used without a request.
        let third_run =
            ServiceCredentials::service_account(http_client(), key(), None, Some(&tokens_path))
                .await
                .unwrap();
        assert_eq!(
            third_run.access_token().await.unwrap().token(),
            Some("ya29.service")
        );
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn refused_delegation_names_the_admin_console_setting() {
        let server = MockServer::start(vec![(
            401,
            String::from(
                r#"{"error":"unauthorized_client","error_description":"Client is unauthorized to retrieve access tokens using this method, or client not authorized for any of the scopes requested."}"#,
            ),
        )])
        .await;
        let key = parse(&key_value(&server.url("/token"))).unwrap();
        let credentials = ServiceCredentials::service_account(
            http_client(),
            key,
            Some(String::from("user@example.com")),
            None,
        )
        .await
        .unwrap();

        let error = credentials.access_token().await.unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "unable to get an access token for service account '{TEST_CLIENT_EMAIL}' acting \
                 for 'user@example.com': Server error: unauthorized_client: Client is unauthorized"
            )),
            "{message}"
        );
        assert!(
            message.contains("\nHint: domain-wide delegation is not allowed"),
            "{message}"
        );
        assert!(message.contains("123456789012345678901"), "{message}");
        assert!(message.ends_with("Domain-wide delegation)."), "{message}");
    }

    #[tokio::test]
    async fn a_refused_key_gets_a_hint_and_never_shows_the_assertion() {
        let server = MockServer::start(vec![(
            400,
            String::from(
                r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#,
            ),
        )])
        .await;
        let key = parse(&key_value(&server.url("/token"))).unwrap();
        let credentials = ServiceCredentials::service_account(http_client(), key, None, None)
            .await
            .unwrap();

        let error = credentials.access_token().await.unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("invalid_grant: Invalid JWT Signature."),
            "{message}"
        );
        assert!(
            message.contains("\nHint: Google refuses the key"),
            "{message}"
        );
        let assertion = server.requests()[0].form("assertion").unwrap();
        assert!(!message.contains(&assertion));
        assert!(!format!("{error:?}").contains(&assertion));
    }

    #[test]
    fn storage_errors_of_service_accounts_get_the_shared_drive_hint() {
        let quota = google_apis_common::Error::BadRequest(json!({"error": {
            "code": 403,
            "message": "Service Accounts do not have storage quota. Leverage shared drives (https://developers.google.com/workspace/drive/api/guides/about-shareddrives), or use OAuth delegation (http://support.google.com/a/answer/7281227) instead.",
            "errors": [{"message": "Service Accounts do not have storage quota.", "domain": "usageLimits", "reason": "storageQuotaExceeded"}]
        }}));
        let service_account = AccountType::ServiceAccount { subject: None };

        let hint = storage_hint(&quota, &service_account).unwrap();
        assert!(
            hint.starts_with("Hint: service accounts have no Drive storage"),
            "{hint}"
        );
        assert!(
            hint.contains("shared drive") && hint.contains("--subject"),
            "{hint}"
        );

        // Found through the source chain of a command error, also when boxed.
        let upload = crate::files::upload::Error::Upload(Box::new(quota));
        assert!(storage_hint(&upload, &service_account).is_some());

        // Not for user accounts.
        assert!(storage_hint(&upload, &AccountType::User).is_none());

        let ownership = google_apis_common::Error::BadRequest(json!({"error": {
            "code": 403,
            "message": "Bad Request. User message: \"You can't change the owner of this item.\"",
            "errors": [{"reason": "forbidden"}]
        }}));
        assert!(storage_hint(&ownership, &service_account).is_some());

        let not_found = google_apis_common::Error::BadRequest(json!({"error": {
            "code": 404, "message": "File not found: x.", "errors": [{"reason": "notFound"}]
        }}));
        assert!(storage_hint(&not_found, &service_account).is_none());

        let delegated = AccountType::ServiceAccount {
            subject: Some(String::from("user@example.com")),
        };
        let upload = crate::files::upload::Error::Upload(Box::new(
            google_apis_common::Error::BadRequest(json!({"error": {
                "code": 403, "message": "The user's Drive storage quota has been exceeded.",
                "errors": [{"reason": "storageQuotaExceeded"}]
            }})),
        ));
        let hint = storage_hint(&upload, &delegated).unwrap();
        assert!(hint.contains("acts for"), "{hint}");
    }
}
