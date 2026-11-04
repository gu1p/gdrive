use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
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
    match app_cfg
        .account_type()
        .map_err(GetHubError::LoadAccountType)?
    {
        AccountType::User => {}
        AccountType::ServiceAccount { subject } => {
            return service_account_hub(&app_cfg, subject).await;
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
            GetHubError::LoadSecret(_) => "unable to load secret",
            GetHubError::LoadAccountType(_) => "unable to load the account type",
            GetHubError::Auth(_) | GetHubError::Service(_) => "unable to authenticate",
            GetHubError::Hub(_) => "unable to create Google Drive hub",
        };

        f.write_str(s)
    }
}
