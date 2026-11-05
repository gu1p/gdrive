use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

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

// STUB(activity-auth): replaced at merge
/// A hub for the account `app_cfg` whose credentials can give tokens for
/// [`auth::AUDIT_LOG_SCOPE`], besides the Drive scopes. Fails, before any request to the Reports
/// API, when the account was not authorized for the audit log (`GetHubError::AuditLogNotGranted`,
/// whose message names `gdrive account reauth <ACCOUNT> --audit-log`) or is not a user account
/// (`GetHubError::AuditLogUnsupported`: service accounts and Application Default Credentials are
/// not supported yet). Never starts an interactive authorization.
pub(crate) async fn get_audit_hub_for(app_cfg: &AppConfig) -> Result<Hub, GetHubError> {
    get_hub_for(app_cfg).await
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
}

impl Error for GetHubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            GetHubError::LoadCurrentAccount(source) => Some(source),
            GetHubError::DefaultBasePath(source) => Some(source),
            GetHubError::AccountNotFound(_) => None,
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
        };

        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::{named_account_in, GetHubError};
    use crate::app_config::AppConfig;

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
