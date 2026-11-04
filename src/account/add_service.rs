//! `gdrive account add --service-account` and `--adc`: accounts that authenticate with a service
//! account key or with Application Default Credentials.

use std::{
    error,
    fmt::{Display, Formatter},
    io,
    path::PathBuf,
};

use hyper_util::client::legacy::{connect::Connect, Client};

use crate::{
    app_config::{
        self,
        account_type::{AccountType, InstallAccount},
        AppConfig,
    },
    auth::{
        adc::{self, AdcSource},
        service::{self, ServiceCredentials},
    },
    hub,
};

#[derive(Debug, Clone)]
pub(crate) struct ServiceAccountConfig {
    pub(crate) key_path: PathBuf,
    /// The Google Workspace user to act for (domain-wide delegation).
    pub(crate) subject: Option<String>,
    /// The account name; the `client_email` of the key by default.
    pub(crate) name: Option<String>,
}

/// Checks the key by getting a token with it, stores it as an account and switches to that
/// account. Nothing is stored when the key does not work.
pub(crate) async fn add_service_account(config: &ServiceAccountConfig) -> Result<(), Box<Error>> {
    let base_path = AppConfig::default_base_path().map_err(Error::DefaultBasePath)?;
    let client = hub::token_http_client().map_err(Error::HttpClient)?;
    let app_cfg = add_service_account_in(base_path, client, config).await?;

    println!(
        "Saved the service account key in {}",
        app_cfg.account_base_path().display()
    );
    println!(
        "Keep it safe! Anyone who gets it can access everything the service account can access."
    );

    app_config::switch_account(&app_cfg).map_err(Error::SwitchAccount)?;
    println!();
    println!("Logged in as {}", app_cfg.account.name);
    if config.subject.is_none() {
        println!(
            "A service account has no Drive storage of its own: upload into a shared drive that \
             it is a member of. See docs/service_accounts.md."
        );
    }
    Ok(())
}

/// The account name of `account add --adc` without --name.
const DEFAULT_ADC_NAME: &str = "adc";

/// Looks up Application Default Credentials and gets a token with them, then stores an account
/// that looks them up again on every run, and switches to it.
pub(crate) async fn add_adc(name: Option<String>) -> Result<(), Box<Error>> {
    let base_path = AppConfig::default_base_path().map_err(Error::DefaultBasePath)?;
    let client = hub::token_http_client().map_err(Error::HttpClient)?;
    let source = adc::resolve().await.map_err(Error::FindAdc)?;
    // Only now is it known that the metadata server, the last place to look, exists.
    let found = source.to_string();
    let app_cfg = add_adc_in(base_path, client, source, name).await?;
    println!("Found Application Default Credentials: {found}");

    app_config::switch_account(&app_cfg).map_err(Error::SwitchAccount)?;
    println!();
    println!("Logged in as {}", app_cfg.account.name);
    println!(
        "gdrive stores no credentials for this account: it looks up the Application Default \
         Credentials again on every run."
    );
    Ok(())
}

async fn add_adc_in<C>(
    base_path: PathBuf,
    client: Client<C, String>,
    source: AdcSource,
    name: Option<String>,
) -> Result<AppConfig, Box<Error>>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let name = name.unwrap_or_else(|| String::from(DEFAULT_ADC_NAME));
    let app_cfg = AppConfig::named_account_in(base_path, &name).ok_or(Error::InvalidName(name))?;
    app_cfg.check_replaceable().map_err(Error::Install)?;

    let credentials = ServiceCredentials::application_default(client, source)
        .await
        .map_err(Error::AdcCredentials)?;
    credentials
        .access_token()
        .await
        .map_err(Error::AdcCredentials)?;

    app_cfg
        .install(&AccountType::ApplicationDefaultCredentials, None, None)
        .await
        .map_err(Error::Install)?;
    Ok(app_cfg)
}

async fn add_service_account_in<C>(
    base_path: PathBuf,
    client: Client<C, String>,
    config: &ServiceAccountConfig,
) -> Result<AppConfig, Box<Error>>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    if let Some(subject) = &config.subject {
        if !subject.contains('@') {
            return Err(Error::Subject(subject.clone()).into());
        }
    }

    let key_path = &config.key_path;
    let content = tokio::fs::read(key_path)
        .await
        .map_err(|source| Error::ReadKey(key_path.clone(), source))?;
    let key =
        service::parse_key(&content).map_err(|source| Error::Key(key_path.clone(), source))?;

    let name = config.name.as_ref().unwrap_or(&key.client_email).clone();
    let app_cfg = AppConfig::named_account_in(base_path, &name).ok_or(Error::InvalidName(name))?;
    // Before the token request, so that a name clash is reported first.
    app_cfg.check_replaceable().map_err(Error::Install)?;

    let credentials =
        ServiceCredentials::service_account(client, key, config.subject.clone(), None)
            .await
            .map_err(Error::Credentials)?;
    let token = credentials
        .access_token()
        .await
        .map_err(Error::Credentials)?;

    let account_type = AccountType::ServiceAccount {
        subject: config.subject.clone(),
    };
    app_cfg
        .install(
            &account_type,
            Some(&content),
            Some(&service::token_file(&token)),
        )
        .await
        .map_err(Error::Install)?;
    Ok(app_cfg)
}

#[derive(Debug)]
pub(crate) enum Error {
    DefaultBasePath(app_config::errors::DefaultBasePath),
    HttpClient(io::Error),
    Subject(String),
    ReadKey(PathBuf, io::Error),
    Key(PathBuf, service::KeyError),
    InvalidName(String),
    Credentials(service::Error),
    FindAdc(adc::AdcError),
    AdcCredentials(service::Error),
    Install(InstallAccount),
    SwitchAccount(app_config::errors::SaveAccountConfig),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::DefaultBasePath(source) => Some(source),
            Error::HttpClient(source) | Error::ReadKey(_, source) => Some(source),
            Error::Key(_, source) => Some(source),
            Error::FindAdc(source) => Some(source),
            // These are shown as themselves: they say what to do.
            Error::AdcCredentials(source @ service::Error::NoMetadataServer { .. }) => {
                source.source()
            }
            Error::Credentials(source) | Error::AdcCredentials(source) => Some(source),
            Error::Subject(_)
            | Error::InvalidName(_)
            | Error::Install(InstallAccount::UserAccountExists(_)) => None,
            Error::Install(source) => Some(source),
            Error::SwitchAccount(source) => Some(source),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DefaultBasePath(_) => f.write_str("unable to find the config directory"),
            Error::HttpClient(_) => f.write_str("unable to create the HTTP client"),
            Error::Subject(subject) => write!(
                f,
                "--subject needs the email address of a Google Workspace user, not '{subject}'"
            ),
            Error::ReadKey(path, _) => write!(f, "unable to read '{}'", path.display()),
            Error::Key(path, _) => write!(
                f,
                "'{}' is not a usable service account key",
                path.display()
            ),
            Error::InvalidName(name) => write!(
                f,
                "'{name}' cannot be an account name: it must not be empty, '.' or '..', or \
                 contain '/' or '\\'"
            ),
            Error::Credentials(_) => f.write_str("the service account key does not work"),
            Error::FindAdc(_) => f.write_str("unable to use the Application Default Credentials"),
            Error::AdcCredentials(error @ service::Error::NoMetadataServer { .. }) => error.fmt(f),
            Error::AdcCredentials(_) => {
                f.write_str("the Application Default Credentials do not work")
            }
            Error::Install(error @ InstallAccount::UserAccountExists(_)) => error.fmt(f),
            Error::Install(_) => f.write_str("unable to save the account"),
            Error::SwitchAccount(_) => f.write_str("unable to switch to the account"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::auth::{
        service::SERVICE_SCOPES,
        store::TokenFile,
        test_support::{
            service_account_key_json, verified_jwt_claims, MockServer, TEST_CLIENT_EMAIL,
        },
    };

    fn http_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, String> {
        Client::builder(hyper_util::rt::TokioExecutor::new()).build_http()
    }

    const TOKEN_RESPONSE: &str =
        r#"{"access_token":"ya29.added","expires_in":3599,"token_type":"Bearer"}"#;

    struct Setup {
        base: tempfile::TempDir,
        key_path: PathBuf,
        key: String,
        server: MockServer,
    }

    async fn setup(responses: Vec<(u16, String)>) -> Setup {
        let server = MockServer::start(responses).await;
        let base = tempfile::tempdir().unwrap();
        let key = service_account_key_json(&server.url("/token"));
        let key_path = base.path().join("downloaded-key.json");
        fs::write(&key_path, &key).unwrap();
        Setup {
            base,
            key_path,
            key,
            server,
        }
    }

    fn config(setup: &Setup, subject: Option<&str>, name: Option<&str>) -> ServiceAccountConfig {
        ServiceAccountConfig {
            key_path: setup.key_path.clone(),
            subject: subject.map(str::to_owned),
            name: name.map(str::to_owned),
        }
    }

    fn config_root(setup: &Setup) -> PathBuf {
        setup.base.path().join("gdrive3")
    }

    #[tokio::test]
    async fn adding_stores_the_key_the_subject_and_the_first_token() {
        let setup = setup(vec![(200, String::from(TOKEN_RESPONSE))]).await;

        let app_cfg = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, Some("user@example.com"), None),
        )
        .await
        .unwrap();

        // Named after the client_email by default.
        assert_eq!(app_cfg.account.name, TEST_CLIENT_EMAIL);
        assert_eq!(
            fs::read_to_string(app_cfg.service_account_key_path()).unwrap(),
            setup.key
        );
        assert_eq!(
            app_cfg.account_type().unwrap(),
            AccountType::ServiceAccount {
                subject: Some(String::from("user@example.com"))
            }
        );
        let tokens = TokenFile::load(app_cfg.tokens_path())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tokens.get(&SERVICE_SCOPES).unwrap().access_token.as_deref(),
            Some("ya29.added")
        );

        let requests = setup.server.requests();
        assert_eq!(requests.len(), 1);
        let claims = verified_jwt_claims(&requests[0].form("assertion").unwrap());
        assert_eq!(claims["sub"], "user@example.com");

        // The stored account works without another request while the token is valid.
        let credentials = ServiceCredentials::load(
            http_client(),
            &app_cfg,
            Some(String::from("user@example.com")),
        )
        .await
        .unwrap();
        assert_eq!(
            credentials.access_token().await.unwrap().token(),
            Some("ya29.added")
        );
        assert_eq!(setup.server.requests().len(), 1);
    }

    #[tokio::test]
    async fn name_overrides_the_default_and_is_checked() {
        let setup = setup(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let app_cfg = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, None, Some("robot")),
        )
        .await
        .unwrap();
        assert_eq!(app_cfg.account.name, "robot");
        assert!(config_root(&setup)
            .join("robot/service_account.json")
            .is_file());

        let error = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, None, Some("../elsewhere")),
        )
        .await
        .unwrap_err();
        assert!(matches!(*error, Error::InvalidName(_)), "{error:?}");
    }

    #[tokio::test]
    async fn nothing_is_stored_when_the_key_does_not_work() {
        let setup = setup(vec![(
            400,
            String::from(
                r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#,
            ),
        )])
        .await;

        let error = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, None, None),
        )
        .await
        .unwrap_err();
        assert!(matches!(*error, Error::Credentials(_)), "{error:?}");
        let message = crate::error_message(&*error);
        assert!(
            message.starts_with(
                "Error: the service account key does not work: unable to get an access token"
            ),
            "{message}"
        );
        assert!(message.contains("\nHint: "), "{message}");
        assert!(!config_root(&setup).join(TEST_CLIENT_EMAIL).exists());
    }

    #[tokio::test]
    async fn invalid_keys_and_subjects_are_refused_before_any_request() {
        let setup = setup(vec![(500, String::from("{}"))]).await;

        fs::write(&setup.key_path, r#"{"type": "service_account"}"#).unwrap();
        let error = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, None, None),
        )
        .await
        .unwrap_err();
        let message = crate::error_message(&*error);
        assert!(
            message
                .contains("is not a usable service account key: the file has no \"client_email\""),
            "{message}"
        );

        let error = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, Some("not-an-email"), None),
        )
        .await
        .unwrap_err();
        assert!(matches!(*error, Error::Subject(_)), "{error:?}");

        assert!(setup.server.requests().is_empty());
    }

    #[tokio::test]
    async fn a_user_account_of_the_same_name_is_not_touched() {
        let setup = setup(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let user_dir = config_root(&setup).join("me@example.com");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("tokens.json"), "[]").unwrap();

        let error = add_service_account_in(
            config_root(&setup),
            http_client(),
            &config(&setup, None, Some("me@example.com")),
        )
        .await
        .unwrap_err();
        let message = crate::error_message(&*error);
        assert!(
            message.starts_with("Error: account 'me@example.com' exists already"),
            "{message}"
        );
        assert!(
            message.contains("gdrive account remove me@example.com"),
            "{message}"
        );
        assert!(!user_dir.join("service_account.json").exists());
        assert!(setup.server.requests().is_empty());
    }
}
