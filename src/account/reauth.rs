use std::{
    error,
    fmt::{Display, Formatter},
    io,
};

use yup_oauth2::storage::TokenInfo;

use crate::{
    account::add::{authorized_email, token_hub},
    app_config::{self, account_type::AccountType, AppConfig},
    auth::{self, login, store::TokenFile},
    hub::Hub,
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

    if let Some(account_type) = non_user_account_type(&account_name) {
        return Err(Error::NoLogin {
            account: account_name,
            account_type,
        }
        .into());
    }

    // Only secret.json is needed: an account whose tokens.json is missing (which list_accounts
    // skips) is exactly one that needs a new authorization.
    let (app_cfg, secret) =
        AppConfig::load_account_with_secret(&account_name).map_err(Error::LoadAccount)?;

    println!("Authorizing account '{account_name}' again with its stored client credentials.");

    // Account names are the email addresses of the Google accounts.
    let login_hint = account_name.contains('@').then(|| account_name.clone());
    let options = login::Options::new(config.mode, config.port, login_hint);
    let token = login::login(&secret, &options)
        .await
        .map_err(Error::Login)?;

    let hub = token_hub(&token).map_err(Error::HubCreation)?;
    save_reauthorized(&app_cfg, &secret, &hub, token).await?;

    println!();
    println!("Authorized account '{account_name}' again");

    Ok(())
}

/// The type of the account `account_name`, if it exists and does not log in with an OAuth client.
fn non_user_account_type(account_name: &str) -> Option<AccountType> {
    let base_path = AppConfig::default_base_path().ok()?;
    let account_type = AppConfig::named_account_in(base_path, account_name)?
        .account_type()
        .ok()?;
    (account_type != AccountType::User).then_some(account_type)
}

/// Replaces the tokens of `app_cfg` with `token`, after checking with `hub` (authorized by
/// `token`) that the new authorization is for the same Google account. Nothing is written when
/// the check fails.
async fn save_reauthorized(
    app_cfg: &AppConfig,
    secret: &app_config::Secret,
    hub: &Hub,
    token: TokenInfo,
) -> Result<(), Box<Error>> {
    let account_name = &app_cfg.account.name;
    match authorized_email(hub).await.map_err(Error::About)? {
        Some(email) if email == *account_name => {}
        Some(email) => {
            return Err(Error::OtherGoogleAccount {
                account: account_name.clone(),
                authorized: email,
            }
            .into());
        }
        // Without the address the check cannot be made; do not replace the credentials blindly.
        None => {
            return Err(Error::UnknownGoogleAccount {
                account: account_name.clone(),
            }
            .into());
        }
    }

    // Google leaves out the refresh token when the grant still exists; keep the stored one then.
    let previous = app_cfg.tokens_of_client(&secret.client_id).await;
    let tokens = TokenFile::after_login(previous.as_ref(), &auth::DRIVE_SCOPES, token);
    auth::warn_if_no_refresh_token(&tokens, account_name);
    tokens
        .save(app_cfg.tokens_path())
        .await
        .map_err(Error::SaveTokens)?;
    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    LoadCurrentAccount(app_config::errors::LoadCurrentAccount),
    LoadAccount(app_config::errors::LoadAccountWithSecret),
    Login(login::LoginError),
    HubCreation(io::Error),
    About(Box<google_drive3::Error>),
    OtherGoogleAccount {
        account: String,
        authorized: String,
    },
    UnknownGoogleAccount {
        account: String,
    },
    SaveTokens(io::Error),
    NoLogin {
        account: String,
        account_type: AccountType,
    },
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::LoadCurrentAccount(source) => Some(source),
            // Shown as the load error itself, which names the account and what to do.
            Error::LoadAccount(source) => source.source(),
            // Shown as the login error itself, which says what to do.
            Error::Login(source) => source.source(),
            Error::HubCreation(source) | Error::SaveTokens(source) => Some(source),
            Error::About(source) => Some(source),
            Error::OtherGoogleAccount { .. }
            | Error::UnknownGoogleAccount { .. }
            | Error::NoLogin { .. } => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::LoadCurrentAccount(_) => f.write_str(
                "unable to load the current account; name the account: 'gdrive account reauth NAME'",
            ),
            Error::LoadAccount(source) => source.fmt(f),
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
            Error::UnknownGoogleAccount { account } => write!(
                f,
                "Google did not report the email address of the account you signed in with, so \
                 gdrive cannot check that it is '{account}'; nothing was changed. Run the command \
                 again, or add the account with 'gdrive account add'"
            ),
            Error::SaveTokens(_) => f.write_str("unable to save the new tokens"),
            Error::NoLogin {
                account,
                account_type: AccountType::ServiceAccount { .. },
            } => write!(
                f,
                "account '{account}' is a service account, which needs no authorization\n\
                 To use a new key, run `gdrive account add --service-account KEY_JSON --name \
                 {account}`."
            ),
            Error::NoLogin {
                account,
                account_type: AccountType::ApplicationDefaultCredentials,
            } => write!(
                f,
                "account '{account}' uses Application Default Credentials, which gdrive looks \
                 up on every run; there is nothing to authorize again"
            ),
            Error::NoLogin { account, .. } => {
                write!(f, "account '{account}' does not log in with an OAuth client")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::common::mock_server::{MockServer, Response};

    fn new_token() -> TokenInfo {
        TokenInfo {
            access_token: Some(String::from("new-access")),
            refresh_token: Some(String::from("new-refresh")),
            expires_at: Some(OffsetDateTime::now_utc() + Duration::hours(1)),
            id_token: None,
        }
    }

    /// An account directory with secret.json and, if given, tokens.json.
    fn account(base: &std::path::Path, tokens: Option<&str>) -> (AppConfig, app_config::Secret) {
        let dir = base.join("me@example.com");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("secret.json"),
            r#"{"client_id":"id","client_secret":"secret"}"#,
        )
        .unwrap();
        if let Some(tokens) = tokens {
            std::fs::write(dir.join("tokens.json"), tokens).unwrap();
        }
        AppConfig::load_account_with_secret_in(base.to_path_buf(), "me@example.com").unwrap()
    }

    #[tokio::test]
    async fn reauth_writes_the_tokens_of_an_account_whose_tokens_json_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (app_cfg, secret) = account(dir.path(), None);
        let server = MockServer::start(vec![Response::json(
            200,
            &json!({"user": {"emailAddress": "me@example.com"}}),
        )])
        .await;

        save_reauthorized(&app_cfg, &secret, &server.hub(), new_token())
            .await
            .unwrap();

        let saved = TokenFile::load(app_cfg.tokens_path())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.refresh_token(), Some("new-refresh"));
        assert_eq!(
            saved
                .get(&auth::DRIVE_SCOPES)
                .unwrap()
                .access_token
                .as_deref(),
            Some("new-access")
        );
        let about = &server.requests()[0];
        assert_eq!(about.path, "/drive/v3/about");
        assert_eq!(about.query_param("fields"), Some("user"));
    }

    const OLD_TOKENS: &str = r#"[{"scopes":["https://www.googleapis.com/auth/drive","https://www.googleapis.com/auth/drive.metadata.readonly"],"token":{"access_token":"old","refresh_token":"old-refresh","expires_at":null,"id_token":null}}]"#;

    async fn rejected(about: serde_json::Value) -> Box<Error> {
        let dir = tempfile::tempdir().unwrap();
        let (app_cfg, secret) = account(dir.path(), Some(OLD_TOKENS));
        let server = MockServer::start(vec![Response::json(200, &about)]).await;

        let error = save_reauthorized(&app_cfg, &secret, &server.hub(), new_token())
            .await
            .unwrap_err();

        let tokens = tokio::fs::read_to_string(app_cfg.tokens_path())
            .await
            .unwrap();
        assert_eq!(tokens, OLD_TOKENS, "the stored tokens were changed");
        error
    }

    #[tokio::test]
    async fn reauth_without_a_reported_email_changes_nothing() {
        for about in [json!({}), json!({"user": {"displayName": "Me"}})] {
            let error = rejected(about.clone()).await;
            assert!(
                matches!(&*error, Error::UnknownGoogleAccount { account } if account == "me@example.com"),
                "{about}: {error:?}"
            );
            assert!(error.to_string().contains("nothing was changed"));
        }
    }

    #[tokio::test]
    async fn reauth_as_another_google_account_changes_nothing() {
        let error = rejected(json!({"user": {"emailAddress": "other@example.com"}})).await;
        assert!(
            matches!(&*error, Error::OtherGoogleAccount { authorized, .. } if authorized == "other@example.com"),
            "{error:?}"
        );
    }
}
