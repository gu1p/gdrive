use std::{
    error,
    fmt::{Display, Formatter},
    io,
};

use crate::{
    account::add::{authorized_email, token_hub},
    app_config::{self, AppConfig},
    auth::{self, login, store::TokenFile},
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The account to authorize again; the current account when `None`.
    pub(crate) account_name: Option<String>,
    pub(crate) mode: login::Mode,
    pub(crate) port: u16,
}

/// Runs the authorization again for an existing account, with its stored client credentials,
/// and replaces its tokens. Nothing is changed unless the new authorization is for the same
/// Google account.
pub(crate) async fn reauth(config: &Config) -> Result<(), Box<Error>> {
    login::Options::new(config.mode, config.port, None)
        .check()
        .map_err(Error::Login)?;

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

    // Account names are the email addresses of the Google accounts.
    let login_hint = account_name.contains('@').then(|| account_name.clone());
    let options = login::Options::new(config.mode, config.port, login_hint);
    let token = login::login(&secret, &options)
        .await
        .map_err(Error::Login)?;

    let hub = token_hub(&token).map_err(Error::HubCreation)?;
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

    // Google leaves out the refresh token when the grant still exists; keep the stored one then.
    let previous = app_cfg.tokens_of_client(&secret.client_id).await;
    let tokens = TokenFile::after_login(previous.as_ref(), &auth::DRIVE_SCOPES, token);
    auth::warn_if_no_refresh_token(&tokens, &account_name);
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
    Login(login::LoginError),
    HubCreation(io::Error),
    About(Box<google_drive3::Error>),
    OtherGoogleAccount { account: String, authorized: String },
    SaveTokens(io::Error),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::LoadCurrentAccount(source) => Some(source),
            Error::ListAccounts(source) => Some(source),
            Error::LoadAccount(source) => Some(source),
            Error::LoadSecret(source) => Some(source),
            // Shown as the login error itself, which says what to do.
            Error::Login(source) => source.source(),
            Error::HubCreation(source) | Error::SaveTokens(source) => Some(source),
            Error::About(source) => Some(source),
            Error::AccountNotFound(_) | Error::OtherGoogleAccount { .. } => None,
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
            Error::LoadSecret(_) => {
                f.write_str("unable to load the client credentials of the account")
            }
            Error::Login(source) => source.fmt(f),
            Error::HubCreation(_) => f.write_str("unable to create a Google Drive hub"),
            Error::About(_) => f.write_str("failed to get user info"),
            Error::OtherGoogleAccount {
                account,
                authorized,
            } => write!(
                f,
                "the authorization is for '{authorized}', not for account '{account}'; nothing \
                 was changed. Sign in as '{account}', or add '{authorized}' with 'gdrive account \
                 add'"
            ),
            Error::SaveTokens(_) => f.write_str("unable to save the new tokens"),
        }
    }
}
