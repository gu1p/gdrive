use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
};

use crate::{
    app_config::{self, AppConfig},
    auth::{self, stored::StoredCredentials},
    hub::{self, Hub},
};

/// A Drive hub for the current account, authorized with its stored token.
///
/// This never starts an interactive authorization: when the stored token cannot be used or
/// refreshed, the error is a [`auth::ReauthRequired`] that tells the user to log in again.
pub(crate) async fn get_hub() -> Result<Hub, GetHubError> {
    let app_cfg = AppConfig::load_current_account().map_err(GetHubError::LoadCurrentAccount)?;
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

#[derive(Debug)]
pub(crate) enum GetHubError {
    LoadCurrentAccount(app_config::errors::LoadCurrentAccount),
    LoadSecret(app_config::errors::LoadSecret),
    Auth(auth::stored::Error),
    Hub(io::Error),
}

impl Error for GetHubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            GetHubError::LoadCurrentAccount(source) => Some(source),
            GetHubError::LoadSecret(source) => Some(source),
            GetHubError::Auth(source) => Some(source),
            GetHubError::Hub(source) => Some(source),
        }
    }
}

impl Display for GetHubError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s = match self {
            GetHubError::LoadCurrentAccount(_) => "unable to load current account",
            GetHubError::LoadSecret(_) => "unable to load secret",
            GetHubError::Auth(_) => "unable to authenticate",
            GetHubError::Hub(_) => "unable to create Google Drive hub",
        };

        f.write_str(s)
    }
}
