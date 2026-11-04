//! Finding Application Default Credentials (#37).
//!
//! The places are those of Google's client libraries, in their order:
//!
//! 1. the file named by `GOOGLE_APPLICATION_CREDENTIALS`;
//! 2. gcloud's file `application_default_credentials.json`, which `gcloud auth
//!    application-default login` writes into `$CLOUDSDK_CONFIG`, `%APPDATA%\gcloud` (Windows) or
//!    `~/.config/gcloud`;
//! 3. the metadata server of Compute Engine, Cloud Run, GKE and the like, at
//!    `$GCE_METADATA_HOST` or `metadata.google.internal`.
//!
//! The files may hold a service account key (`"type": "service_account"`) or user credentials
//! (`"type": "authorized_user"`). yup-oauth2's `ApplicationDefaultCredentialsAuthenticator` only
//! reads a service account key from `GOOGLE_APPLICATION_CREDENTIALS` and knows nothing about
//! gcloud's file, so gdrive looks the credentials up itself and uses yup-oauth2's authenticators
//! for each kind. Whether the metadata server exists shows only when it is asked for a token.

use std::{
    error::Error,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use serde_json::Value;
use yup_oauth2::{authorized_user::AuthorizedUserSecret, ServiceAccountKey};

use super::service::{parse_key, KeyError};
use crate::common::json_error::RedactedJsonError;

const ENV_VAR: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const GCLOUD_FILE_NAME: &str = "application_default_credentials.json";
const DEFAULT_METADATA_HOST: &str = "metadata.google.internal";
const METADATA_TOKEN_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/token";

/// Where a credentials file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    EnvVar,
    GcloudFile,
}

/// The Application Default Credentials that were found.
#[derive(Debug, Clone)]
pub(crate) enum AdcSource {
    ServiceAccountKey {
        origin: Origin,
        path: PathBuf,
        key: ServiceAccountKey,
    },
    AuthorizedUser {
        origin: Origin,
        path: PathBuf,
        secret: AuthorizedUserSecret,
    },
    MetadataServer {
        url: String,
        host: String,
        /// The gcloud file that does not exist, if there is a place for it.
        gcloud_file: Option<PathBuf>,
    },
}

impl Display for AdcSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let from = |origin: &Origin| match origin {
            Origin::EnvVar => " named by GOOGLE_APPLICATION_CREDENTIALS",
            Origin::GcloudFile => " of gcloud",
        };
        match self {
            AdcSource::ServiceAccountKey { origin, path, key } => write!(
                f,
                "the key of service account '{}' in '{}'{}",
                key.client_email,
                path.display(),
                from(origin)
            ),
            AdcSource::AuthorizedUser { origin, path, .. } => write!(
                f,
                "the user credentials in '{}'{}",
                path.display(),
                from(origin)
            ),
            AdcSource::MetadataServer { host, .. } => {
                write!(f, "the service account of the metadata server at {host}")
            }
        }
    }
}

/// Looks up Application Default Credentials in the environment of this process.
pub(crate) async fn resolve() -> Result<AdcSource, AdcError> {
    resolve_with(&|name| std::env::var_os(name)).await
}

/// Looks up Application Default Credentials with `env` as the environment.
pub(crate) async fn resolve_with(
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
) -> Result<AdcSource, AdcError> {
    if let Some(path) = env(ENV_VAR) {
        if path.is_empty() {
            return Err(AdcError::EmptyEnvVar);
        }
        let path = PathBuf::from(path);
        return match tokio::fs::read(&path).await {
            Ok(content) => parse_file(Origin::EnvVar, path, &content),
            Err(source) => Err(AdcError::Read {
                origin: Origin::EnvVar,
                path,
                source,
            }),
        };
    }

    let gcloud_file = gcloud_dir(env).map(|dir| dir.join(GCLOUD_FILE_NAME));
    if let Some(path) = &gcloud_file {
        match tokio::fs::read(path).await {
            Ok(content) => return parse_file(Origin::GcloudFile, path.clone(), &content),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(AdcError::Read {
                    origin: Origin::GcloudFile,
                    path: path.clone(),
                    source,
                })
            }
        }
    }

    let host = env("GCE_METADATA_HOST")
        .and_then(|host| host.into_string().ok())
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_METADATA_HOST));
    Ok(AdcSource::MetadataServer {
        url: format!("http://{host}{METADATA_TOKEN_PATH}"),
        host,
        gcloud_file,
    })
}

/// gcloud's configuration directory.
fn gcloud_dir(env: &(dyn Fn(&str) -> Option<OsString> + Sync)) -> Option<PathBuf> {
    let non_empty = |name| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = non_empty("CLOUDSDK_CONFIG") {
        return Some(dir);
    }
    if cfg!(windows) {
        non_empty("APPDATA").map(|dir| dir.join("gcloud"))
    } else {
        non_empty("HOME")
            .or_else(home::home_dir)
            .map(|home| home.join(".config").join("gcloud"))
    }
}

fn parse_file(origin: Origin, path: PathBuf, content: &[u8]) -> Result<AdcSource, AdcError> {
    let invalid = |path, source| AdcError::Invalid {
        origin,
        path,
        source,
    };
    let value: Value = match serde_json::from_slice(content) {
        Ok(value) => value,
        Err(error) => {
            return Err(invalid(
                path,
                KeyError::Json(RedactedJsonError::from(&error)),
            ))
        }
    };

    match value.get("type").and_then(Value::as_str) {
        Some("service_account") => match parse_key(content) {
            Ok(key) => Ok(AdcSource::ServiceAccountKey { origin, path, key }),
            Err(source) => Err(invalid(path, source)),
        },
        Some("authorized_user") => {
            for field in ["client_id", "client_secret", "refresh_token"] {
                let value = value.get(field).and_then(Value::as_str);
                if value.is_none_or(|value| value.trim().is_empty()) {
                    return Err(invalid(path, KeyError::MissingField(field)));
                }
            }
            match serde_json::from_value(value) {
                Ok(secret) => Ok(AdcSource::AuthorizedUser {
                    origin,
                    path,
                    secret,
                }),
                Err(_) => Err(invalid(path, KeyError::OptionalFieldType)),
            }
        }
        Some(
            key_type @ ("external_account"
            | "external_account_authorized_user"
            | "impersonated_service_account"),
        ) => Err(AdcError::Unsupported {
            origin,
            path,
            key_type: key_type.to_owned(),
        }),
        _ => Err(AdcError::UnknownType { origin, path }),
    }
}

#[derive(Debug)]
pub(crate) enum AdcError {
    EmptyEnvVar,
    Read {
        origin: Origin,
        path: PathBuf,
        source: io::Error,
    },
    Invalid {
        origin: Origin,
        path: PathBuf,
        source: KeyError,
    },
    Unsupported {
        origin: Origin,
        path: PathBuf,
        key_type: String,
    },
    UnknownType {
        origin: Origin,
        path: PathBuf,
    },
    /// `GOOGLE_APPLICATION_CREDENTIALS` was set while gdrive was looking.
    Changed,
}

/// `'PATH'` and where it comes from.
struct Named<'a>(Origin, &'a PathBuf);

impl Display for Named<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "'{}'", self.1.display())?;
        if self.0 == Origin::EnvVar {
            f.write_str(", named by GOOGLE_APPLICATION_CREDENTIALS,")?;
        }
        Ok(())
    }
}

impl Display for AdcError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            AdcError::EmptyEnvVar => f.write_str(
                "GOOGLE_APPLICATION_CREDENTIALS is set but empty\nUnset it, or set it to the \
                 path of a service account key file.",
            ),
            AdcError::Read { origin, path, .. } => {
                write!(f, "unable to read {}", Named(*origin, path))
            }
            AdcError::Invalid { origin, path, .. } => write!(
                f,
                "{} holds no usable Application Default Credentials",
                Named(*origin, path)
            ),
            AdcError::Unsupported {
                origin,
                path,
                key_type,
            } => write!(
                f,
                "{} holds credentials of type \"{key_type}\", which gdrive does not support; use a \
                 service account key or gcloud user credentials",
                Named(*origin, path)
            ),
            AdcError::UnknownType { origin, path } => write!(
                f,
                "{} holds neither a service account key nor user credentials: its \"type\" is \
                 neither \"service_account\" nor \"authorized_user\"",
                Named(*origin, path)
            ),
            AdcError::Changed => {
                f.write_str("GOOGLE_APPLICATION_CREDENTIALS changed while gdrive was reading it")
            }
        }
    }
}

impl Error for AdcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AdcError::Read { source, .. } => Some(source),
            AdcError::Invalid { source, .. } => Some(source),
            AdcError::EmptyEnvVar
            | AdcError::Unsupported { .. }
            | AdcError::UnknownType { .. }
            | AdcError::Changed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path};

    use hyper_util::client::legacy::{connect::HttpConnector, Client};
    use serde_json::json;

    use super::*;
    use crate::auth::{
        service::{Error as ServiceError, ServiceCredentials, SERVICE_SCOPES},
        test_support::{
            redirected_client, service_account_key_json, verified_jwt_claims, MockServer,
            TEST_CLIENT_EMAIL,
        },
    };

    const TOKEN_RESPONSE: &str =
        r#"{"access_token":"ya29.adc","expires_in":3599,"token_type":"Bearer"}"#;

    /// An environment of only `vars`, with gcloud's directory in `gcloud_dir` unless set.
    fn env(gcloud_dir: &Path, vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + Sync {
        let mut vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        vars.entry(String::from("CLOUDSDK_CONFIG"))
            .or_insert_with(|| gcloud_dir.as_os_str().to_owned());
        move |name| vars.get(name).cloned()
    }

    fn http_client() -> Client<HttpConnector, String> {
        Client::builder(hyper_util::rt::TokioExecutor::new()).build_http()
    }

    fn user_credentials() -> String {
        json!({
            "account": "",
            "client_id": "gcloud-client.apps.googleusercontent.com",
            "client_secret": "gcloud-client-secret",
            "quota_project_id": "gdrive-test",
            "refresh_token": "1//adc-refresh",
            "type": "authorized_user",
            "universe_domain": "googleapis.com"
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_service_account_key_in_google_application_credentials_is_used_first() {
        let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("key.json");
        std::fs::write(&key_path, service_account_key_json(&server.url("/token"))).unwrap();
        // gcloud's file is there too, but comes second.
        std::fs::write(dir.path().join(GCLOUD_FILE_NAME), user_credentials()).unwrap();

        let source = resolve_with(&env(dir.path(), &[(ENV_VAR, key_path.to_str().unwrap())]))
            .await
            .unwrap();
        let AdcSource::ServiceAccountKey { origin, path, .. } = &source else {
            panic!("expected a service account key, got {source:?}");
        };
        assert_eq!((*origin, path), (Origin::EnvVar, &key_path));
        assert!(source.to_string().contains(TEST_CLIENT_EMAIL), "{source}");

        let credentials = ServiceCredentials::application_default(http_client(), source)
            .await
            .unwrap();
        assert_eq!(
            credentials.access_token().await.unwrap().token(),
            Some("ya29.adc")
        );

        let claims = verified_jwt_claims(&server.requests()[0].form("assertion").unwrap());
        assert_eq!(claims["iss"], TEST_CLIENT_EMAIL);
        assert_eq!(claims["scope"], SERVICE_SCOPES[0]);
    }

    #[tokio::test]
    async fn gcloud_user_credentials_are_refreshed_with_their_refresh_token() {
        let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let dir = tempfile::tempdir().unwrap();
        let gcloud_file = dir.path().join(GCLOUD_FILE_NAME);
        std::fs::write(&gcloud_file, user_credentials()).unwrap();

        let source = resolve_with(&env(dir.path(), &[])).await.unwrap();
        let AdcSource::AuthorizedUser { origin, path, .. } = &source else {
            panic!("expected user credentials, got {source:?}");
        };
        assert_eq!((*origin, path), (Origin::GcloudFile, &gcloud_file));

        // yup-oauth2 posts the refresh to accounts.google.com; the client sends it to the mock.
        let credentials =
            ServiceCredentials::application_default(redirected_client(server.addr()), source)
                .await
                .unwrap();
        assert_eq!(
            credentials.access_token().await.unwrap().token(),
            Some("ya29.adc")
        );

        let request = &server.requests()[0];
        assert_eq!(request.form("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(
            request.form("refresh_token").as_deref(),
            Some("1//adc-refresh")
        );
    }

    #[tokio::test]
    async fn rejected_user_credentials_point_to_gcloud() {
        let server = MockServer::start(vec![(
            400,
            String::from(r#"{"error":"invalid_grant","error_description":"reauth related error (invalid_rapt)"}"#),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(GCLOUD_FILE_NAME), user_credentials()).unwrap();
        let source = resolve_with(&env(dir.path(), &[])).await.unwrap();
        let credentials =
            ServiceCredentials::application_default(redirected_client(server.addr()), source)
                .await
                .unwrap();

        let message = credentials.access_token().await.unwrap_err().to_string();
        assert!(message.contains("invalid_grant"), "{message}");
        assert!(
            message.contains("\nHint: the gcloud credentials expired or were revoked. Run `gcloud auth application-default login --scopes=https://www.googleapis.com/auth/drive,"),
            "{message}"
        );
        assert!(!message.contains("1//adc-refresh"), "{message}");
        assert!(!message.contains("gcloud-client-secret"), "{message}");
    }

    #[tokio::test]
    async fn the_metadata_server_is_asked_when_no_file_is_found() {
        // yup-oauth2 reads GOOGLE_APPLICATION_CREDENTIALS of this process itself before it asks
        // the metadata server; with the variable set, gdrive never gets here.
        if std::env::var_os(ENV_VAR).is_some() {
            eprintln!("skipped: {ENV_VAR} is set for the test process");
            return;
        }
        let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let dir = tempfile::tempdir().unwrap();
        let host = server.addr().to_string();

        let source = resolve_with(&env(dir.path(), &[("GCE_METADATA_HOST", &host)]))
            .await
            .unwrap();
        let AdcSource::MetadataServer {
            url, gcloud_file, ..
        } = &source
        else {
            panic!("expected the metadata server, got {source:?}");
        };
        assert_eq!(url, &format!("http://{host}{METADATA_TOKEN_PATH}"));
        assert_eq!(
            gcloud_file.as_deref(),
            Some(&*dir.path().join(GCLOUD_FILE_NAME))
        );

        let credentials = ServiceCredentials::application_default(http_client(), source)
            .await
            .unwrap();
        assert_eq!(
            credentials.access_token().await.unwrap().token(),
            Some("ya29.adc")
        );

        let request = &server.requests()[0];
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            format!("{METADATA_TOKEN_PATH}?scopes={}", SERVICE_SCOPES[0])
        );
    }

    #[tokio::test]
    async fn no_credentials_anywhere_is_a_clear_error() {
        if std::env::var_os(ENV_VAR).is_some() {
            eprintln!("skipped: {ENV_VAR} is set for the test process");
            return;
        }
        // A port nothing listens on.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = closed.local_addr().unwrap().to_string();
        drop(closed);
        let dir = tempfile::tempdir().unwrap();

        let source = resolve_with(&env(dir.path(), &[("GCE_METADATA_HOST", &host)]))
            .await
            .unwrap();
        let credentials = ServiceCredentials::application_default(http_client(), source)
            .await
            .unwrap();
        let error = credentials.access_token().await.unwrap_err();
        assert!(
            matches!(error, ServiceError::NoMetadataServer { .. }),
            "{error:?}"
        );

        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "no Application Default Credentials were found: GOOGLE_APPLICATION_CREDENTIALS is \
                 not set, '{}' does not exist, and no metadata server answered at {host}",
                dir.path().join(GCLOUD_FILE_NAME).display()
            )),
            "{message}"
        );
        assert!(
            message.contains("\nHint: set GOOGLE_APPLICATION_CREDENTIALS"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn the_default_metadata_host_is_metadata_google_internal() {
        let dir = tempfile::tempdir().unwrap();
        let source = resolve_with(&env(dir.path(), &[])).await.unwrap();
        let AdcSource::MetadataServer { url, host, .. } = source else {
            panic!("expected the metadata server, got {source:?}");
        };
        assert_eq!(host, "metadata.google.internal");
        assert_eq!(
            url,
            "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token"
        );
    }

    #[tokio::test]
    async fn unusable_credential_files_are_named_without_their_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let resolve_file = |content: String| {
            std::fs::write(&path, content).unwrap();
            let env = env(dir.path(), &[(ENV_VAR, path.to_str().unwrap())]);
            async move { resolve_with(&env).await.unwrap_err() }
        };

        let error = resolve_file(
            json!({"type": "external_account", "audience": "//iam.googleapis.com/x"}).to_string(),
        )
        .await;
        assert!(
            matches!(&error, AdcError::Unsupported { key_type, .. } if key_type == "external_account"),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("named by GOOGLE_APPLICATION_CREDENTIALS"),
            "{error}"
        );

        let error = resolve_file(
            json!({"type": "authorized_user", "client_id": "i", "client_secret": "s3cr3t"})
                .to_string(),
        )
        .await;
        let message = crate::error_message(&error);
        assert!(
            message.ends_with("the file has no \"refresh_token\""),
            "{message}"
        );
        assert!(!message.contains("s3cr3t"), "{message}");

        let error = resolve_file(String::from(
            r#"{"type": "authorized_user", "refresh_token": "1//s3cr3t""#,
        ))
        .await;
        assert!(
            matches!(
                error,
                AdcError::Invalid {
                    source: KeyError::Json(_),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!crate::error_message(&error).contains("s3cr3t"));

        let error = resolve_file(json!({"installed": {}}).to_string()).await;
        assert!(matches!(error, AdcError::UnknownType { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_or_empty_google_application_credentials_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let error = resolve_with(&env(dir.path(), &[(ENV_VAR, missing.to_str().unwrap())]))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                AdcError::Read {
                    origin: Origin::EnvVar,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("missing.json"), "{error}");

        let error = resolve_with(&env(dir.path(), &[(ENV_VAR, "")]))
            .await
            .unwrap_err();
        assert!(matches!(error, AdcError::EmptyEnvVar), "{error:?}");
    }
}
