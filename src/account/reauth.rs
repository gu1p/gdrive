use std::{
    error,
    fmt::{Display, Formatter},
    io,
};

use crate::{
    account::add::authorized_email,
    app_config::{self, AppConfig},
    auth::{self, store},
    hub,
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The account to authorize again; the current account when `None`.
    pub(crate) account_name: Option<String>,
}

/// Runs the authorization again for an existing account, with its stored client credentials,
/// and replaces its tokens. Nothing is changed unless the new authorization is for the same
/// Google account.
pub(crate) async fn reauth(config: &Config) -> Result<(), Box<Error>> {
    let account_name = match &config.account_name {
        Some(name) => name.clone(),
        None => {
            AppConfig::load_current_account()
                .map_err(Error::LoadCurrentAccount)?
                .account
                .name
        }
    };

    let accounts = app_config::list_accounts().map_err(Error::ListAccounts)?;
    if !accounts.contains(&account_name) {
        return Err(Error::AccountNotFound(account_name).into());
    }
    let app_cfg = AppConfig::load_account(&account_name).map_err(Error::LoadAccount)?;
    let secret = app_cfg.load_secret().map_err(Error::LoadSecret)?;

    println!("Authorizing account '{account_name}' again with its stored client credentials.");

    let tmp_dir = tempfile::tempdir().map_err(Error::Tempdir)?;
    let tmp_tokens = tmp_dir.path().join("tokens.json");
    let auth = hub::Auth::new(&secret, &tmp_tokens)
        .await
        .map_err(Error::Auth)?;
    auth.token(&auth::DRIVE_SCOPES)
        .await
        .map_err(Error::AccessToken)?;

    let hub = hub::Hub::new(auth.0).map_err(Error::HubCreation)?;
    let email = authorized_email(&hub).await.map_err(Error::About)?;
    if let Some(email) = email {
        if email != account_name {
            return Err(Error::OtherGoogleAccount {
                account: account_name,
                authorized: email,
            }
            .into());
        }
    }

    let tokens = store::TokenFile::load(&tmp_tokens)
        .await
        .map_err(Error::ReadNewTokens)?
        .ok_or(Error::NoTokenReceived)?;
    tokens
        .save(app_cfg.tokens_path())
        .await
        .map_err(Error::SaveTokens)?;

    println!();
    println!("Authorized account '{account_name}' again");

    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    LoadCurrentAccount(app_config::errors::LoadCurrentAccount),
    ListAccounts(app_config::errors::ListAccounts),
    AccountNotFound(String),
    LoadAccount(app_config::errors::LoadAccount),
    LoadSecret(app_config::errors::LoadSecret),
    Tempdir(io::Error),
    Auth(io::Error),
    AccessToken(yup_oauth2::Error),
    HubCreation(io::Error),
    About(Box<google_drive3::Error>),
    OtherGoogleAccount { account: String, authorized: String },
    ReadNewTokens(store::LoadError),
    NoTokenReceived,
    SaveTokens(io::Error),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::LoadCurrentAccount(source) => Some(source),
            Error::ListAccounts(source) => Some(source),
            Error::LoadAccount(source) => Some(source),
            Error::LoadSecret(source) => Some(source),
            Error::Tempdir(source)
            | Error::Auth(source)
            | Error::HubCreation(source)
            | Error::SaveTokens(source) => Some(source),
            Error::AccessToken(source) => Some(source),
            Error::About(source) => Some(source),
            Error::ReadNewTokens(source) => Some(source),
            Error::AccountNotFound(_)
            | Error::OtherGoogleAccount { .. }
            | Error::NoTokenReceived => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::LoadCurrentAccount(_) => f.write_str(
                "unable to load the current account; name the account: 'gdrive account reauth NAME'",
            ),
            Error::ListAccounts(_) => f.write_str("unable to list accounts"),
            Error::AccountNotFound(name) => write!(
                f,
                "account '{name}' not found; run 'gdrive account list' to see the accounts, or \
                 'gdrive account add' to add one"
            ),
            Error::LoadAccount(_) => f.write_str("unable to load the account"),
            Error::LoadSecret(_) => f.write_str("unable to load the client credentials of the account"),
            Error::Tempdir(_) => f.write_str("failed to create temporary directory"),
            Error::Auth(_) => f.write_str("failed to authenticate"),
            Error::AccessToken(_) => f.write_str("failed to get access token"),
            Error::HubCreation(_) => f.write_str("unable to create a Google Drive hub"),
            Error::About(_) => f.write_str("failed to get user info"),
            Error::OtherGoogleAccount {
                account,
                authorized,
            } => write!(
                f,
                "the authorization is for '{authorized}', not for account '{account}'; nothing \
                 was changed. Sign in as '{account}', or add '{authorized}' with 'gdrive account add'"
            ),
            Error::ReadNewTokens(_) => f.write_str("unable to read the new tokens"),
            Error::NoTokenReceived => f.write_str("the authorization returned no token"),
            Error::SaveTokens(_) => f.write_str("unable to save the new tokens"),
        }
    }
}
