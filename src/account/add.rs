use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
    io::{self, Write},
};

use yup_oauth2::storage::TokenInfo;

use crate::{
    app_config::{self, account_type::AccountType},
    auth::{self, login, store::TokenFile},
    hub,
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) mode: login::Mode,
    pub(crate) port: u16,
}

pub(crate) async fn add(config: &Config) -> Result<(), Box<Error>> {
    let options = login::Options::new(config.mode, config.port, None);
    options.check().map_err(Error::Login)?;

    println!("To add an account you need a Google Client ID and Client Secret.");
    println!(
        "Instructions for how to create credentials can be found here: \
        https://github.com/glotlabs/gdrive/blob/main/docs/create_google_api_credentials.md"
    );
    if config.mode == login::Mode::Browser {
        println!(
            "If the browser you will use runs on another machine (for example when gdrive runs \
            on a remote server), use 'gdrive account add --manual' instead."
        );
    }
    println!();

    let secret = secret_prompt().map_err(Error::Prompt)?;

    let token = login::login(&secret, &options)
        .await
        .map_err(Error::Login)?;

    let hub = token_hub(&token).map_err(Error::HubCreation)?;
    let email = authorized_email(&hub)
        .await
        .map_err(|error| Error::About(*error))?
        .map_or(Cow::Borrowed("unknown"), Into::into);

    let existing = app_config::AppConfig::load_account(&email).map_err(Error::LoadAccount)?;
    if !matches!(existing.account_type(), Ok(AccountType::User)) {
        return Err(Error::NotAUserAccount(email.into_owned()).into());
    }

    // When the account is added again with the same client, Google may leave out the refresh
    // token; keep the stored one then.
    let previous = existing.tokens_of_client(&secret.client_id).await;
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
        "Keep them safe! If someone gets access to them, they will also be able to access your \
        Google Drive."
    );

    app_config::switch_account(&app_cfg).map_err(Error::SwitchAccount)?;
    println!();
    println!("Logged in as {}", app_cfg.account.name);

    Ok(())
}

/// A Drive hub that uses the access token of a login that just happened.
pub(super) fn token_hub(token: &TokenInfo) -> io::Result<hub::Hub> {
    hub::Hub::new(token.access_token.clone().unwrap_or_default())
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
    Login(login::LoginError),
    LoadAccount(app_config::errors::LoadAccount),
    AddAccount(app_config::errors::AddAccount),
    SwitchAccount(app_config::errors::SaveAccountConfig),
    About(google_drive3::Error),
    NotAUserAccount(String),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::HubCreation(error) | Error::Prompt(error) => Some(error),
            // Shown as the login error itself, which says what to do.
            Error::Login(error) => error.source(),
            Error::LoadAccount(error) => Some(error),
            Error::AddAccount(error) => Some(error),
            Error::SwitchAccount(error) => Some(error),
            Error::About(error) => Some(error),
            Error::NotAUserAccount(_) => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Error::Login(error) => return error.fmt(f),
            Error::HubCreation(_) => "unable to create a Google Drive hub",
            Error::Prompt(_) => "failed to get input from user",
            Error::LoadAccount(_) => "unable to load the account config",
            Error::AddAccount(_) => "unable to add account in the config",
            Error::SwitchAccount(_) => "unable to switch account in the config",
            Error::About(_) => "failed to get user info",
            Error::NotAUserAccount(name) => {
                return write!(
                    f,
                    "account '{name}' exists already and does not log in with an OAuth client; \
                     nothing was changed\nUse `gdrive account remove {name}` to remove it first."
                )
            }
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
