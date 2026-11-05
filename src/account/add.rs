use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
    io::{self, Write},
};

use yup_oauth2::storage::TokenInfo;

use crate::{
    app_config::{self, account_type::AccountType},
    auth::{
        self,
        lock::{AccountLock, LOCK_TIMEOUT},
        login,
        store::TokenFile,
    },
    common::retry::retry_read,
    hub,
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) mode: login::Mode,
    pub(crate) port: u16,
    /// `--audit-log`: also ask for read access to the audit log of the organization.
    pub(crate) audit_log: bool,
}

pub(crate) async fn add(config: &Config) -> Result<(), Box<Error>> {
    let asked = if config.audit_log {
        login::AuditLog::Required
    } else {
        login::AuditLog::No
    };
    let options = login::Options::new(config.mode, config.port, None).with_audit_log(asked);
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

    let authorization = login::login(&secret, &options)
        .await
        .map_err(Error::Login)?;

    let hub = token_hub(&authorization.token).map_err(Error::HubCreation)?;
    let email = authorized_email(&hub)
        .await
        .map_err(|error| Error::About(*error))?
        .map_or(Cow::Borrowed("unknown"), Into::into);

    let app_cfg = app_config::AppConfig::load_account(&email).map_err(Error::LoadAccount)?;
    if !matches!(app_cfg.account_type(), Ok(AccountType::User)) {
        return Err(Error::NotAUserAccount(email.into_owned()).into());
    }

    let granted_by_google = authorization.audit_log;
    let Added {
        had_audit_log,
        tokens: login::LoginTokens { tokens, audit_log },
    } = save_added(app_cfg.clone(), secret, authorization, asked).await?;
    auth::warn_if_no_refresh_token(&tokens, &email);

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
    if let Some(loss) = auth::audit_log_loss(asked, granted_by_google, audit_log) {
        if had_audit_log {
            eprintln!(
                "{}",
                auth::audit_log_lost_warning(&app_cfg.account.name, loss)
            );
        }
    }

    Ok(())
}

/// What [`save_added`] stored.
struct Added {
    /// Whether the replaced tokens of the account recorded the audit-log grant.
    had_audit_log: bool,
    tokens: login::LoginTokens,
}

/// Stores the account `app_cfg` after a login, in one change under the lock of the account
/// (see `auth::lock`): reads the tokens it replaces, builds the new ones
/// ([`login::tokens_after_login`]), and writes the secret and the tokens. A refresh that is under
/// way finishes first, and one that starts now sees the new tokens; nothing is written when the
/// login cannot be saved.
async fn save_added(
    app_cfg: app_config::AppConfig,
    secret: app_config::Secret,
    authorization: login::Authorization,
    asked: login::AuditLog,
) -> Result<Added, Box<Error>> {
    use app_config::errors::{AddAccount, InitAccount};

    // The lock lives in the config root, which the first `account add` creates.
    tokio::fs::create_dir_all(&app_cfg.base_path)
        .await
        .map_err(|error| {
            Error::AddAccount(AddAccount::InitAccount(InitAccount::CreateAccountDir(
                error,
            )))
        })?;
    let lock = AccountLock::new(&app_cfg.base_path, &app_cfg.account.name)
        .map_err(|error| Error::AddAccount(AddAccount::SaveTokens(error)))?;
    let saved = lock
        .hold_blocking(LOCK_TIMEOUT, move |held| {
            // Adding an account again replaces its authorization, with whichever client.
            let had_audit_log = TokenFile::read(app_cfg.tokens_path())
                .ok()
                .flatten()
                .is_some_and(|tokens| tokens.audit_log_granted());
            // When the account is added again with the same client, Google may leave out the
            // refresh token; keep the stored one then.
            let previous = app_cfg.tokens_of_client(&secret.client_id);
            let tokens = login::tokens_after_login(previous.as_ref(), authorization, asked)
                .map_err(|error| Box::new(Error::Login(error)))?;
            app_config::add_account(&app_cfg, &secret, &tokens.tokens, held)
                .map_err(|error| Box::new(Error::AddAccount(error)))?;
            Ok(Added {
                had_audit_log,
                tokens,
            })
        })
        .await;
    match saved {
        Ok(added) => added,
        Err(error) => Err(Error::AddAccount(AddAccount::SaveTokens(error.into())).into()),
    }
}

/// A Drive hub that uses the access token of a login that just happened.
pub(super) fn token_hub(token: &TokenInfo) -> io::Result<hub::Hub> {
    hub::Hub::new(token.access_token.clone().unwrap_or_default())
}

/// The email address of the Google account the hub is authorized for.
pub(super) async fn authorized_email(
    hub: &hub::Hub,
) -> Result<Option<String>, Box<google_drive3::Error>> {
    let (_, about) = retry_read(hub.retrier(), "about.get", |mut delegate| async move {
        hub.about()
            .get()
            .param("fields", "user")
            .delegate(&mut delegate)
            .doit()
            .await
    })
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

#[cfg(test)]
mod tests {
    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::auth::lock::test_hooks::{when_waiting, within, Elsewhere};

    fn token(access: &str, refresh: Option<&str>) -> TokenInfo {
        TokenInfo {
            access_token: Some(access.to_owned()),
            refresh_token: refresh.map(str::to_owned),
            expires_at: Some(OffsetDateTime::now_utc() + Duration::hours(1)),
            id_token: None,
        }
    }

    #[tokio::test]
    async fn account_add_builds_the_new_tokens_from_the_file_it_finds_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let app_cfg =
            app_config::AppConfig::named_account_in(dir.path().to_path_buf(), "me@example.com")
                .unwrap();
        let secret = app_config::Secret {
            client_id: String::from("id"),
            client_secret: String::from("secret"),
        };
        let lock = AccountLock::new(dir.path(), "me@example.com").unwrap();
        // The account exists with a Drive-only authorization (r1).
        lock.clone()
            .hold_blocking(LOCK_TIMEOUT, {
                let (app_cfg, secret) = (app_cfg.clone(), secret.clone());
                move |held| {
                    let tokens =
                        TokenFile::after_login(None, &auth::DRIVE_SCOPES, token("old", Some("r1")));
                    app_config::add_account(&app_cfg, &secret, &tokens, held)
                }
            })
            .await
            .unwrap()
            .unwrap();

        // Another process holds the lock, and saves a new authorization (r2, with the audit log).
        let r2 = TokenFile::after_login(
            None,
            &auth::DRIVE_AND_AUDIT_LOG_SCOPES,
            token("other", Some("r2")),
        );
        let elsewhere = Elsewhere::hold(lock, {
            let r2 = r2.clone();
            move |held| r2.write_locked(held).unwrap()
        })
        .await;

        // The login, which Google answered without a refresh token, waits for the lock ...
        let waiting = when_waiting(app_cfg.tokens_path());
        let authorization = login::Authorization {
            token: token("new", None),
            audit_log: false,
        };
        let add = tokio::spawn(save_added(
            app_cfg.clone(),
            secret,
            authorization,
            login::AuditLog::No,
        ));
        within("the wait of the login", waiting.notified()).await;
        elsewhere.finish().await;

        // ... and builds the new tokens from the file it found then: it keeps r2, not r1.
        let added = within("the login", add).await.unwrap().unwrap();
        assert!(added.had_audit_log);
        assert!(!added.tokens.audit_log);
        let saved = TokenFile::load(app_cfg.tokens_path())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.refresh_token(), Some("r2"));
        assert_eq!(
            saved
                .get(&auth::DRIVE_SCOPES)
                .unwrap()
                .access_token
                .as_deref(),
            Some("new")
        );
        assert!(!saved.audit_log_granted());
        assert_eq!(app_cfg.load_secret().unwrap().client_id, "id");
    }

    #[tokio::test]
    async fn account_add_that_cannot_be_saved_writes_nothing() {
        // `--audit-log`, no refresh token from Google, none of the grant stored: nothing at all
        // is written, not even the secret or the account directory.
        let dir = tempfile::tempdir().unwrap();
        let app_cfg =
            app_config::AppConfig::named_account_in(dir.path().to_path_buf(), "me@example.com")
                .unwrap();
        let authorization = login::Authorization {
            token: token("new", None),
            audit_log: true,
        };
        let error = save_added(
            app_cfg.clone(),
            app_config::Secret {
                client_id: String::from("id"),
                client_secret: String::from("secret"),
            },
            authorization,
            login::AuditLog::Required,
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(
                &*error,
                Error::Login(login::LoginError::AuditLogWithoutRefreshToken)
            ),
            "{error:?}"
        );
        assert!(!app_cfg.account_base_path().exists());
    }
}
