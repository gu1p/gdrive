use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use hyper_util::client::legacy::{connect::Connect, Client};

use crate::{
    app_config::{
        self,
        account_type::{AccountType, LoadAccountType},
        AppConfig,
    },
    auth::{
        self, adc,
        service::{self, ServiceCredentials},
        stored::StoredCredentials,
    },
    hub::{self, Hub},
};

/// A Drive hub for the current account, authorized with its stored token.
///
/// This never starts an interactive authorization: when the stored token cannot be used or
/// refreshed, the error is a [`auth::ReauthRequired`] that tells the user to log in again.
pub(crate) async fn get_hub() -> Result<Hub, GetHubError> {
    let app_cfg = AppConfig::load_current_account().map_err(GetHubError::LoadCurrentAccount)?;
    get_hub_for(&app_cfg).await
}

/// A Drive hub for the account `app_cfg`, of any type: a user's stored token, a service account
/// or Application Default Credentials, with the same checks as [`get_hub`]. The token is got (or
/// refreshed) before the command starts working, so that an expired or revoked authorization is
/// reported up front; an interactive authorization is never started.
pub(crate) async fn get_hub_for(app_cfg: &AppConfig) -> Result<Hub, GetHubError> {
    match app_cfg
        .account_type()
        .map_err(GetHubError::LoadAccountType)?
    {
        AccountType::User => {}
        AccountType::ServiceAccount { subject } => {
            return service_account_hub(app_cfg, subject).await;
        }
        AccountType::ApplicationDefaultCredentials => return adc_hub().await,
    }

    let secret = app_cfg.load_secret().map_err(GetHubError::LoadSecret)?;
    let client = hub::token_http_client().map_err(GetHubError::Hub)?;
    let credentials = StoredCredentials::load(
        client,
        &app_cfg.account.name,
        &secret,
        app_cfg.tokens_path(),
    )
    .await
    .map_err(GetHubError::Auth)?;

    // Get (or refresh) the access token before the command starts working, so that an expired
    // or revoked authorization is reported up front.
    credentials
        .access_token(&auth::DRIVE_SCOPES)
        .await
        .map_err(GetHubError::Auth)?;

    let hub = Hub::new(credentials).map_err(GetHubError::Hub)?;

    Ok(hub)
}

/// A hub for the account `app_cfg` whose credentials can give tokens for
/// [`auth::AUDIT_LOG_SCOPE`], besides the Drive scopes. Fails, before any request to the Reports
/// API, when the account was not authorized for the audit log (`GetHubError::AuditLogNotGranted`,
/// whose message names `gdrive account reauth <ACCOUNT> --audit-log`) or is not a user account
/// (`GetHubError::AuditLogUnsupported`: service accounts and Application Default Credentials are
/// not supported yet). Never starts an interactive authorization.
pub(crate) async fn get_audit_hub_for(app_cfg: &AppConfig) -> Result<Hub, GetHubError> {
    audit_hub_with(app_cfg, hub::token_http_client).await
}

/// [`get_audit_hub_for`] with the token endpoint client that `client` creates, once the account
/// is known to be a user account.
///
/// The checks are those of [`get_hub_for`] for a user account, with two more before any token
/// is asked for: the account type, and the grant that `tokens.json` records. The grant must be
/// checked first: asked for a scope set that no stored token covers, yup-oauth2 refreshes, so a
/// request for a grant the account does not have would go to Google.
async fn audit_hub_with<C>(
    app_cfg: &AppConfig,
    client: impl FnOnce() -> io::Result<Client<C, String>>,
) -> Result<Hub, GetHubError>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let account = &app_cfg.account.name;
    match app_cfg
        .account_type()
        .map_err(GetHubError::LoadAccountType)?
    {
        AccountType::User => {}
        account_type => {
            return Err(GetHubError::AuditLogUnsupported {
                account: account.clone(),
                account_type,
            });
        }
    }

    let secret = app_cfg.load_secret().map_err(GetHubError::LoadSecret)?;
    let client = client().map_err(GetHubError::Hub)?;
    let credentials = StoredCredentials::load(client, account, &secret, app_cfg.tokens_path())
        .await
        .map_err(GetHubError::Auth)?;
    if !credentials.audit_log_granted() {
        return Err(GetHubError::AuditLogNotGranted(account.clone()));
    }

    // Get (or refresh) the access token before the command starts working, so that an expired
    // or revoked authorization is reported up front. Asked for the scope set of the grant, the
    // token serves both the Drive requests and the audit log, and a refresh replaces the
    // grant's own entry in `tokens.json` instead of adding one.
    credentials
        .access_token(&auth::DRIVE_AND_AUDIT_LOG_SCOPES)
        .await
        .map_err(GetHubError::Auth)?;

    Hub::new(credentials).map_err(GetHubError::Hub)
}

/// The account named `account_name`, one of those `gdrive account list` prints. Fails with
/// [`GetHubError::AccountNotFound`], which names it and the commands that list and add
/// accounts, when there is no such account.
pub(crate) fn load_named_account(account_name: &str) -> Result<AppConfig, GetHubError> {
    let base_path = AppConfig::default_base_path().map_err(GetHubError::DefaultBasePath)?;
    named_account_in(base_path, account_name)
}

/// [`load_named_account`] in the config root `base_path`.
fn named_account_in(base_path: PathBuf, account_name: &str) -> Result<AppConfig, GetHubError> {
    AppConfig::named_account_in(base_path, account_name)
        .filter(AppConfig::exists)
        .ok_or_else(|| GetHubError::AccountNotFound(account_name.to_owned()))
}

/// A Drive hub for the service account `app_cfg`, with a token got (or taken from the cache)
/// before the command starts working.
async fn service_account_hub(
    app_cfg: &AppConfig,
    subject: Option<String>,
) -> Result<Hub, GetHubError> {
    let client = hub::token_http_client().map_err(GetHubError::Hub)?;
    let credentials = ServiceCredentials::load(client, app_cfg, subject)
        .await
        .map_err(GetHubError::Service)?;
    credentials
        .access_token()
        .await
        .map_err(GetHubError::Service)?;
    Hub::new(credentials).map_err(GetHubError::Hub)
}

/// A Drive hub with Application Default Credentials, which are looked up again on every run.
async fn adc_hub() -> Result<Hub, GetHubError> {
    let client = hub::token_http_client().map_err(GetHubError::Hub)?;
    let source = adc::resolve()
        .await
        .map_err(|error| GetHubError::Service(service::Error::Adc(error)))?;
    let credentials = ServiceCredentials::application_default(client, source)
        .await
        .map_err(GetHubError::Service)?;
    credentials
        .access_token()
        .await
        .map_err(GetHubError::Service)?;
    Hub::new(credentials).map_err(GetHubError::Hub)
}

/// The hint for a failed command of the current account, if it is a service account or uses
/// Application Default Credentials and ran into a Drive error typical for those.
pub(crate) fn account_hint(error: &(dyn Error + 'static)) -> Option<String> {
    let account_type = AppConfig::load_current_account()
        .ok()?
        .account_type()
        .ok()?;
    auth::service::account_hint(error, &account_type)
}

#[derive(Debug)]
pub(crate) enum GetHubError {
    LoadCurrentAccount(app_config::errors::LoadCurrentAccount),
    DefaultBasePath(app_config::errors::DefaultBasePath),
    /// No account of this name ([`load_named_account`]).
    AccountNotFound(String),
    LoadSecret(app_config::errors::LoadSecret),
    LoadAccountType(LoadAccountType),
    Auth(auth::stored::Error),
    Service(auth::service::Error),
    Hub(io::Error),
    /// The stored tokens of this user account do not record read access to the audit log
    /// ([`get_audit_hub_for`]).
    AuditLogNotGranted(String),
    /// [`get_audit_hub_for`] for an account that is not a user account.
    AuditLogUnsupported {
        account: String,
        account_type: AccountType,
    },
}

impl Error for GetHubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            GetHubError::LoadCurrentAccount(source) => Some(source),
            GetHubError::DefaultBasePath(source) => Some(source),
            GetHubError::AccountNotFound(_)
            | GetHubError::AuditLogNotGranted(_)
            | GetHubError::AuditLogUnsupported { .. } => None,
            GetHubError::LoadSecret(source) => Some(source),
            GetHubError::LoadAccountType(source) => Some(source),
            GetHubError::Auth(source) => Some(source),
            GetHubError::Service(source) => Some(source),
            GetHubError::Hub(source) => Some(source),
        }
    }
}

impl Display for GetHubError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s = match self {
            GetHubError::LoadCurrentAccount(_) => "unable to load current account",
            GetHubError::DefaultBasePath(_) => "unable to get default base path",
            GetHubError::AccountNotFound(name) => {
                return write!(
                    f,
                    "account '{name}' not found; run 'gdrive account list' to see the \
                     accounts, or 'gdrive account add' to add one"
                );
            }
            GetHubError::LoadSecret(_) => "unable to load secret",
            GetHubError::LoadAccountType(_) => "unable to load the account type",
            GetHubError::Auth(_) | GetHubError::Service(_) => "unable to authenticate",
            GetHubError::Hub(_) => "unable to create Google Drive hub",
            GetHubError::AuditLogNotGranted(name) => {
                return write!(
                    f,
                    "account '{name}' is not authorized to read the audit log of its \
                     organization; run 'gdrive account reauth {name} --audit-log' (the Google \
                     account must be an administrator)"
                );
            }
            GetHubError::AuditLogUnsupported {
                account,
                account_type,
            } => {
                let what = match account_type {
                    AccountType::ServiceAccount { .. } => "is a service account",
                    AccountType::ApplicationDefaultCredentials => {
                        "uses Application Default Credentials"
                    }
                    AccountType::User => "does not log in with an OAuth client",
                };
                return write!(
                    f,
                    "account '{account}' {what}, and service accounts and Application Default \
                     Credentials are not supported for the audit log yet; add the Google account \
                     of an administrator of the organization with 'gdrive account add \
                     --audit-log'"
                );
            }
        };

        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path};

    use hyper_util::client::legacy::Client;
    use time::{Duration, OffsetDateTime};
    use yup_oauth2::storage::TokenInfo;

    use super::{audit_hub_with, get_audit_hub_for, named_account_in, GetHubError};
    use crate::{
        app_config::AppConfig,
        auth::{
            store::{TokenEntry, TokenFile},
            test_support::{redirected_client, MockServer, RedirectConnector},
            ReauthReason, AUDIT_LOG_SCOPE, DRIVE_AND_AUDIT_LOG_SCOPES, DRIVE_SCOPES,
        },
    };

    const ACCOUNT: &str = "me@example.com";

    fn token(access: &str, expires_in: Duration) -> TokenInfo {
        TokenInfo {
            access_token: Some(access.to_owned()),
            refresh_token: Some(String::from("1//secret-refresh")),
            expires_at: Some(OffsetDateTime::now_utc() + expires_in),
            id_token: None,
        }
    }

    /// An OAuth user account in the config root `base`, with `tokens` as its tokens.json.
    async fn user_account(base: &Path, tokens: &TokenFile) -> AppConfig {
        let app_cfg = AppConfig::named_account_in(base.to_path_buf(), ACCOUNT).unwrap();
        std::fs::create_dir_all(app_cfg.account_base_path()).unwrap();
        std::fs::write(
            app_cfg.account_base_path().join("secret.json"),
            r#"{"client_id":"id","client_secret":"secret"}"#,
        )
        .unwrap();
        tokens.save(app_cfg.tokens_path()).await.unwrap();
        app_cfg
    }

    fn granted(token: TokenInfo) -> TokenFile {
        TokenFile::after_login(None, &DRIVE_AND_AUDIT_LOG_SCOPES, token)
    }

    async fn audit_hub(
        app_cfg: &AppConfig,
        server: &MockServer,
    ) -> Result<crate::hub::Hub, GetHubError> {
        let addr = server.addr();
        audit_hub_with(app_cfg, move || Ok(redirected_client(addr))).await
    }

    async fn audit_token(hub: &crate::hub::Hub) -> Option<String> {
        hub.auth.get_token(&[AUDIT_LOG_SCOPE]).await.unwrap()
    }

    #[tokio::test]
    async fn audit_hub_of_a_granted_account_uses_the_stored_token() {
        let server = MockServer::start(vec![(500, String::from("{}"))]).await;
        let dir = tempfile::tempdir().unwrap();
        let app_cfg = user_account(dir.path(), &granted(token("valid", Duration::hours(1)))).await;

        let hub = audit_hub(&app_cfg, &server).await.unwrap();
        assert_eq!(audit_token(&hub).await.as_deref(), Some("valid"));
        let drive = hub.auth.get_token(&DRIVE_SCOPES[..1]).await.unwrap();
        assert_eq!(drive.as_deref(), Some("valid"));
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn audit_hub_refreshes_an_expired_token_before_the_command_starts() {
        let server = MockServer::start(vec![(
            200,
            String::from(r#"{"access_token":"fresh","expires_in":3599,"token_type":"Bearer"}"#),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let app_cfg = user_account(dir.path(), &granted(token("stale", Duration::hours(-1)))).await;

        let hub = audit_hub(&app_cfg, &server).await.unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].form("refresh_token").as_deref(),
            Some("1//secret-refresh")
        );

        // The refreshed token replaced the grant's entry: the grant and the refresh token stay.
        let saved = TokenFile::load(app_cfg.tokens_path())
            .await
            .unwrap()
            .unwrap();
        assert!(saved.audit_log_granted());
        assert_eq!(saved.entries().len(), 1);
        assert_eq!(saved.entries()[0].scopes, DRIVE_AND_AUDIT_LOG_SCOPES);
        assert_eq!(saved.refresh_token(), Some("1//secret-refresh"));

        // The command's requests, for the audit log and for Drive, use it without another one.
        assert_eq!(audit_token(&hub).await.as_deref(), Some("fresh"));
        let drive = hub.auth.get_token(&DRIVE_SCOPES).await.unwrap();
        assert_eq!(drive.as_deref(), Some("fresh"));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn audit_hub_refreshes_with_the_refresh_token_of_the_grant() {
        let server = MockServer::start(vec![(
            200,
            String::from(r#"{"access_token":"fresh","expires_in":3599,"token_type":"Bearer"}"#),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        // A tokens.json as an older process could leave it: the login with the audit log, and
        // a Drive token of the earlier authorization that expires later.
        let entry = |scopes: &[&str], access: &str, refresh: &str, expires_in| TokenEntry {
            scopes: scopes.iter().map(|&scope| scope.to_owned()).collect(),
            token: TokenInfo {
                refresh_token: Some(refresh.to_owned()),
                ..token(access, expires_in)
            },
            credential: None,
        };
        let mixed = serde_json::to_string(&[
            entry(
                &DRIVE_AND_AUDIT_LOG_SCOPES,
                "login",
                "1//grant-refresh",
                Duration::hours(-1),
            ),
            entry(
                &DRIVE_SCOPES,
                "older",
                "1//older-refresh",
                Duration::hours(2),
            ),
        ])
        .unwrap();
        let app_cfg =
            user_account(dir.path(), &TokenFile::from_json(mixed.as_bytes()).unwrap()).await;

        let hub = audit_hub(&app_cfg, &server).await.unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].form("refresh_token").as_deref(),
            Some("1//grant-refresh")
        );
        assert_eq!(audit_token(&hub).await.as_deref(), Some("fresh"));
        // The command's Drive requests do not use the earlier authorization's token either.
        let drive = hub.auth.get_token(&DRIVE_SCOPES).await.unwrap();
        assert_eq!(drive.as_deref(), Some("fresh"));
        assert_eq!(server.requests().len(), 1);

        // The write dropped the earlier authorization's entry.
        let saved = tokio::fs::read_to_string(app_cfg.tokens_path())
            .await
            .unwrap();
        assert!(!saved.contains("older"), "{saved}");
        assert!(TokenFile::from_json(saved.as_bytes())
            .unwrap()
            .audit_log_granted());
    }

    #[tokio::test]
    async fn audit_hub_reports_a_revoked_authorization_before_the_command_starts() {
        let server = MockServer::start(vec![(
            400,
            String::from(r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let app_cfg = user_account(dir.path(), &granted(token("stale", Duration::hours(-1)))).await;

        let Err(error) = audit_hub(&app_cfg, &server).await else {
            panic!("a revoked authorization must fail");
        };
        let reauth = crate::auth::find_reauth_required(&error).unwrap();
        assert!(matches!(
            reauth.reason,
            ReauthReason::RefreshTokenRejected(_)
        ));
        let message = crate::error_message(&error);
        assert!(
            message.starts_with(
                "Error: account 'me@example.com' needs to be authorized again: Google rejected"
            ),
            "{message}"
        );
        assert!(!message.contains("secret"), "{message}");
    }

    #[tokio::test]
    async fn audit_hub_of_an_account_without_the_grant_sends_no_request() {
        let server = MockServer::start(vec![(
            200,
            String::from(r#"{"access_token":"fresh","expires_in":3599,"token_type":"Bearer"}"#),
        )])
        .await;
        for expires_in in [Duration::hours(1), Duration::hours(-1)] {
            let dir = tempfile::tempdir().unwrap();
            let drive_only = TokenFile::after_login(
                None,
                &DRIVE_SCOPES,
                token("ya29.secret-access", expires_in),
            );
            let app_cfg = user_account(dir.path(), &drive_only).await;
            let before = tokio::fs::read(app_cfg.tokens_path()).await.unwrap();

            let Err(error) = audit_hub(&app_cfg, &server).await else {
                panic!("an account without the grant must fail");
            };
            assert!(
                matches!(&error, GetHubError::AuditLogNotGranted(name) if name == ACCOUNT),
                "{error:?}"
            );
            assert_eq!(
                crate::error_message(&error),
                "Error: account 'me@example.com' is not authorized to read the audit log of its \
                 organization; run 'gdrive account reauth me@example.com --audit-log' (the \
                 Google account must be an administrator)"
            );
            assert!(server.requests().is_empty());
            assert_eq!(
                tokio::fs::read(app_cfg.tokens_path()).await.unwrap(),
                before
            );
        }
    }

    #[tokio::test]
    async fn audit_hub_is_refused_for_service_accounts_and_adc() {
        let cases = [
            (
                r#"{"type":"service_account","subject":"user@example.com"}"#,
                "account 'robot' is a service account, and service accounts and Application \
                 Default Credentials are not supported for the audit log yet; add the Google \
                 account of an administrator of the organization with 'gdrive account add \
                 --audit-log'",
            ),
            (
                r#"{"type":"application_default_credentials"}"#,
                "account 'robot' uses Application Default Credentials, and service accounts and \
                 Application Default Credentials are not supported for the audit log yet",
            ),
        ];
        for (account_json, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            let app_cfg = AppConfig::named_account_in(dir.path().to_path_buf(), "robot").unwrap();
            std::fs::create_dir_all(app_cfg.account_base_path()).unwrap();
            std::fs::write(
                app_cfg.account_base_path().join("account.json"),
                account_json,
            )
            .unwrap();

            let Err(error) = get_audit_hub_for(&app_cfg).await else {
                panic!("{account_json} must be refused");
            };
            assert!(
                matches!(&error, GetHubError::AuditLogUnsupported { account, .. } if account == "robot"),
                "{error:?}"
            );
            let message = crate::error_message(&error);
            assert!(
                message.starts_with(&format!("Error: {expected}")),
                "{message}"
            );

            // Refused before a client for the token endpoint is even created.
            let no_client = || -> io::Result<Client<RedirectConnector, String>> {
                panic!("no token client may be created for a {account_json} account")
            };
            let error = audit_hub_with(&app_cfg, no_client).await.err().unwrap();
            assert!(matches!(error, GetHubError::AuditLogUnsupported { .. }));
        }
    }

    #[test]
    fn a_named_account_must_exist_and_the_error_says_how_to_find_one() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        for name in ["nobody", "..", "a/b", ""] {
            let error = named_account_in(base.clone(), name).unwrap_err();
            assert!(
                matches!(&error, GetHubError::AccountNotFound(n) if n == name),
                "{name:?}: {error:?}"
            );
            let message = crate::error_message(&error);
            assert!(
                message.contains(&format!("account '{name}' not found")),
                "{message}"
            );
            assert!(message.contains("'gdrive account list'"), "{message}");
            assert!(message.contains("'gdrive account add'"), "{message}");
        }

        // A directory without tokens.json or account.json is no account either.
        std::fs::create_dir_all(base.join("empty")).unwrap();
        assert!(named_account_in(base.clone(), "empty").is_err());

        std::fs::create_dir_all(base.join("me@example.com")).unwrap();
        std::fs::write(base.join("me@example.com").join("tokens.json"), "[]").unwrap();
        let account = named_account_in(base.clone(), "me@example.com").unwrap();
        assert_eq!(account.account.name, "me@example.com");
        assert_eq!(
            account.account_base_path(),
            AppConfig::named_account_in(base, "me@example.com")
                .unwrap()
                .account_base_path()
        );
    }
}
