use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
    io::{self, Write},
    path::Path,
};

use yup_oauth2::storage::TokenInfo;

use crate::{
    app_config,
    auth::{
        self,
        store::{self, TokenFile},
    },
    hub,
};

pub(crate) async fn add() -> Result<(), Box<Error>> {
    println!("To add an account you need a Google Client ID and Client Secret.");
    println!(
        "Instructions for how to create credentials can be found here:\
        https://github.com/glotlabs/gdrive/blob/main/docs/create_google_api_credentials.md"
    );
    println!(
        "Note that if you are using gdrive on a remote server you should read this first:\
        https://github.com/glotlabs/gdrive#using-gdrive-on-a-remote-server"
    );
    println!();

    let secret = secret_prompt().map_err(Error::Prompt)?;

    let tmp_dir = tempfile::tempdir().map_err(Error::Tempdir)?;
    let tokens_path = tmp_dir.path().join("tokens.json");

    let auth = hub::Auth::new(&secret, &tokens_path)
        .await
        .map_err(Error::Auth)?;

    // Get access tokens
    auth.token(&auth::DRIVE_SCOPES)
        .await
        .map_err(Error::AccessToken)?;

    let hub = hub::Hub::new(auth.0).map_err(Error::HubCreation)?;
    let email = authorized_email(&hub)
        .await
        .map_err(|error| Error::About(*error))?
        .map_or(Cow::Borrowed("unknown"), Into::into);

    let token = received_token(&tokens_path).await?;
    // When the account is added again with the same client, Google may leave out the refresh
    // token; keep the stored one then.
    let previous = app_config::AppConfig::load_account(&email)
        .map_err(Error::LoadAccount)?
        .tokens_of_client(&secret.client_id)
        .await;
    let tokens = TokenFile::after_login(previous.as_ref(), &auth::DRIVE_SCOPES, token);
    auth::warn_if_no_refresh_token(&tokens, &email);

    let app_cfg = app_config::add_account(&email, &secret, &tokens)
        .await
        .map_err(Error::AddAccount)?;

    println!();
    println!(
        "Saved account credentials in {}",
        app_cfg.base_path.display()
    );
    println!(
        "Keep them safe! If someone gets access to them, they will also be able to access your\
        Google Drive."
    );

    app_config::switch_account(&app_cfg).map_err(Error::SwitchAccount)?;
    println!();
    println!("Logged in as {}", app_cfg.account.name);

    Ok(())
}

/// The token the login flow saved in `tokens_path`.
pub(super) async fn received_token(tokens_path: &Path) -> Result<TokenInfo, Box<Error>> {
    store::TokenFile::load(tokens_path)
        .await
        .map_err(Error::ReadNewTokens)?
        .and_then(|tokens| tokens.get(&auth::DRIVE_SCOPES).cloned())
        .ok_or_else(|| Box::new(Error::NoTokenReceived))
}

/// The email address of the Google account the hub is authorized for.
pub(super) async fn authorized_email(
    hub: &hub::Hub,
) -> Result<Option<String>, Box<google_drive3::Error>> {
    let (_, about) = hub
        .about()
        .get()
        .param("fields", "user")
        .doit()
        .await
        .map_err(Box::new)?;
    Ok(about.user.and_then(|user| user.email_address))
}

#[derive(Debug)]
pub(crate) enum Error {
    HubCreation(io::Error),
    Prompt(io::Error),
    Tempdir(io::Error),
    Auth(io::Error),
    ReadNewTokens(store::LoadError),
    NoTokenReceived,
    LoadAccount(app_config::errors::LoadAccount),
    AddAccount(app_config::errors::AddAccount),
    SwitchAccount(app_config::errors::SaveAccountConfig),
    AccessToken(yup_oauth2::Error),
    About(google_drive3::Error),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::HubCreation(error)
            | Error::Prompt(error)
            | Error::Tempdir(error)
            | Error::Auth(error) => Some(error),
            Error::ReadNewTokens(error) => Some(error),
            Error::NoTokenReceived => None,
            Error::LoadAccount(error) => Some(error),
            Error::AddAccount(error) => Some(error),
            Error::SwitchAccount(error) => Some(error),
            Error::AccessToken(error) => Some(error),
            Error::About(error) => Some(error),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Error::HubCreation(_) => "unable to create a Google Drive hub",
            Error::Prompt(_) => "failed to get input from user",
            Error::Tempdir(_) => "failed to create temporary directory",
            Error::Auth(_) => "failed to authenticate",
            Error::ReadNewTokens(_) => "unable to read the new tokens",
            Error::NoTokenReceived => "the authorization returned no token",
            Error::LoadAccount(_) => "unable to load the account config",
            Error::AddAccount(_) => "unable to add account in the config",
            Error::SwitchAccount(_) => "unable to switch account in the config",
            Error::AccessToken(_) => "failed to get access token",
            Error::About(_) => "failed to get user info",
        };

        f.write_str(s)
    }
}

fn secret_prompt() -> Result<app_config::Secret, io::Error> {
    let client_id = prompt_input("Client ID")?;
    let client_secret = prompt_input("Client secret")?;

    Ok(app_config::Secret {
        client_id,
        client_secret,
    })
}

fn prompt_input(msg: &str) -> Result<String, io::Error> {
    print!("{msg}: ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(input.trim().to_string())
}
