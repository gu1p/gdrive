pub(crate) mod account_type;
pub(crate) mod errors;

use std::{
    fs, io,
    ops::Not,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};

use crate::{
    auth::{
        lock::{AccountLock, Held, LOCK_TIMEOUT},
        store::{self, TokenFile},
    },
    common::json_error::RedactedJsonError,
};

const SYSTEM_CONFIG_DIR_NAME: &str = ".config";
const BASE_PATH_DIR_NAME: &str = "gdrive3";
const ACCOUNT_CONFIG_NAME: &str = "account.json";
const SECRET_CONFIG_NAME: &str = "secret.json";
const TOKENS_CONFIG_NAME: &str = "tokens.json";

#[derive(Debug, Clone)]
pub(crate) struct AppConfig {
    pub(crate) base_path: PathBuf,
    pub(crate) account: Account,
    account_config_path: OnceLock<PathBuf>,
    account_base_path: OnceLock<PathBuf>,
    secret_path: OnceLock<PathBuf>,
    tokens_path: OnceLock<PathBuf>,
}

/// Stores the OAuth user account `config` with `secret` and `tokens`, in a change that holds
/// the account's lock (`held`): creates the account directory if needed, then replaces the
/// secret and the tokens, each atomically.
pub(crate) fn add_account(
    config: &AppConfig,
    secret: &Secret,
    tokens: &TokenFile,
    held: &Held<'_>,
) -> Result<(), errors::AddAccount> {
    debug_assert_eq!(held.account().tokens_path(), config.tokens_path());
    config
        .create_account_dir()
        .map_err(|error| errors::AddAccount::InitAccount(error.into()))?;
    config
        .save_secret(secret)
        .map_err(errors::AddAccount::SaveSecret)?;
    tokens
        .write_locked(held)
        .map_err(errors::AddAccount::SaveTokens)
}

pub(crate) fn switch_account(config: &AppConfig) -> Result<(), errors::SaveAccountConfig> {
    config.save_account_config()
}

pub(crate) fn list_accounts() -> Result<Vec<String>, errors::ListAccounts> {
    let base_path =
        AppConfig::default_base_path().map_err(errors::ListAccounts::DefaultBasePath)?;
    list_accounts_in(base_path)
}

fn list_accounts_in(base_path: PathBuf) -> Result<Vec<String>, errors::ListAccounts> {
    if let Err(source) = fs::create_dir_all(&base_path) {
        return Err(errors::ListAccounts::CreateBaseDir {
            path: base_path,
            source,
        });
    }
    let entries = match fs::read_dir(&base_path) {
        Ok(entries) => entries,
        Err(source) => {
            return Err(errors::ListAccounts::ListFiles {
                path: base_path,
                source,
            })
        }
    };

    let mut accounts: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| account_type::is_account_dir(&entry.path()))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();

    accounts.sort();

    Ok(accounts)
}

impl AppConfig {
    #[must_use]
    fn new(base_path: PathBuf, account: Account) -> Self {
        AppConfig {
            base_path,
            account,
            account_config_path: OnceLock::new(),
            account_base_path: OnceLock::new(),
            secret_path: OnceLock::new(),
            tokens_path: OnceLock::new(),
        }
    }

    #[must_use]
    pub(crate) fn has_current_account() -> bool {
        AppConfig::default_base_path().is_ok_and(|base_path| {
            let account_config_path = base_path.join(ACCOUNT_CONFIG_NAME);
            account_config_path.exists()
        })
    }

    pub(crate) fn load_current_account() -> Result<AppConfig, errors::LoadCurrentAccount> {
        let base_path =
            AppConfig::default_base_path().map_err(errors::LoadCurrentAccount::DefaultBasePath)?;
        let account_config = AppConfig::load_account_config()
            .map_err(errors::LoadCurrentAccount::LoadAccountConfig)?;
        let account = Account::new(&account_config.current);
        Ok(AppConfig::new(base_path, account))
    }

    pub(crate) fn load_account(account_name: &str) -> Result<AppConfig, errors::LoadAccount> {
        let base_path = AppConfig::default_base_path().map_err(errors::LoadAccount)?;
        let account = Account::new(account_name);
        Ok(AppConfig::new(base_path, account))
    }

    /// The account `account_name` and its stored client credentials (secret.json). Unlike
    /// [`list_accounts`], this does not need tokens.json, so an account whose tokens are missing
    /// can still be authorized again.
    pub(crate) fn load_account_with_secret(
        account_name: &str,
    ) -> Result<(AppConfig, Secret), errors::LoadAccountWithSecret> {
        let base_path = AppConfig::default_base_path()
            .map_err(errors::LoadAccountWithSecret::DefaultBasePath)?;
        AppConfig::load_account_with_secret_in(base_path, account_name)
    }

    pub(crate) fn load_account_with_secret_in(
        base_path: PathBuf,
        account_name: &str,
    ) -> Result<(AppConfig, Secret), errors::LoadAccountWithSecret> {
        use errors::LoadAccountWithSecret as E;

        // The name becomes a directory under the config root: one plain path component only.
        if !account_type::is_valid_account_name(account_name) {
            return Err(E::InvalidName(account_name.to_owned()));
        }

        let config = AppConfig::new(base_path, Account::new(account_name));
        let secret = match config.load_secret() {
            Ok(secret) => secret,
            Err(errors::LoadSecret::Read { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                return Err(E::NotFound(account_name.to_owned()));
            }
            Err(error) => return Err(E::LoadSecret(error)),
        };
        if secret.client_id.trim().is_empty() || secret.client_secret.trim().is_empty() {
            return Err(E::IncompleteSecret(account_name.to_owned()));
        }

        Ok((config, secret))
    }

    pub(crate) fn init_account(account_name: &str) -> Result<AppConfig, errors::InitAccount> {
        let base_path = AppConfig::default_base_path()?;
        let account = Account::new(account_name);

        let config = AppConfig::new(base_path, account);
        config.create_account_dir()?;

        Ok(config)
    }

    pub(crate) fn remove_account(&self) -> Result<(), errors::RemoveAccount> {
        self.remove_account_dir(LOCK_TIMEOUT)?;

        let account_config =
            AppConfig::load_account_config().map_err(errors::RemoveAccount::LoadConfig)?;
        if self.account.name == account_config.current {
            let config_path = self.account_config_path();
            if let Err(source) = fs::remove_file(config_path) {
                return Err(errors::RemoveAccount::RemoveConfig {
                    path: config_path.to_path_buf(),
                    source,
                });
            }
        }

        Ok(())
    }

    /// Deletes the account directory, under the account's lock (waiting at most `timeout` for
    /// another gdrive process): a change that is under way finishes first, and nothing is removed
    /// when the lock cannot be had.
    pub(crate) fn remove_account_dir(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), errors::RemoveAccount> {
        let path = self.account_base_path();
        let error = |source| errors::RemoveAccount::RemoveDirectory {
            path: path.to_path_buf(),
            source,
        };
        AccountLock::new(&self.base_path, &self.account.name)
            .map_err(error)?
            .hold(timeout, |_| fs::remove_dir_all(path))
            .map_err(|lock_error| error(lock_error.into()))?
            .map_err(error)
    }

    /// Replaces `secret.json` atomically, with mode 0600 on Unix from the start.
    fn save_secret(&self, secret: &Secret) -> Result<(), errors::SaveSecret> {
        let content =
            serde_json::to_string_pretty(&secret).map_err(errors::SaveSecret::Serialize)?;
        let path = self.secret_path();
        store::write_private_file_blocking(path, content.as_bytes()).map_err(|source| {
            errors::SaveSecret::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    pub(crate) fn load_secret(&self) -> Result<Secret, errors::LoadSecret> {
        let path = self.secret_path();
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(source) => {
                return Err(errors::LoadSecret::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        match serde_json::from_str(&content) {
            Ok(secret) => Ok(secret),
            Err(error) => Err(errors::LoadSecret::Deserialize {
                path: path.to_path_buf(),
                error: RedactedJsonError::from(&error),
            }),
        }
    }

    /// The stored tokens of this account, if there are any and they were issued to the OAuth
    /// client `client_id`. A refresh token of another client is of no use. Blocks the thread: for
    /// a change under the account lock.
    pub(crate) fn tokens_of_client(&self, client_id: &str) -> Option<TokenFile> {
        let secret = self.load_secret().ok()?;
        if secret.client_id != client_id {
            return None;
        }
        TokenFile::read(self.tokens_path()).ok().flatten()
    }

    fn load_account_config() -> Result<AccountConfig, errors::LoadAccountConfig> {
        let base_path =
            AppConfig::default_base_path().map_err(errors::LoadAccountConfig::DefaultBasePath)?;
        let account_config_path = base_path.join(ACCOUNT_CONFIG_NAME);
        if account_config_path.exists().not() {
            return Err(errors::LoadAccountConfig::AccountConfigMissing);
        }
        let content = match fs::read_to_string(&account_config_path) {
            Ok(content) => content,
            Err(source) => {
                return Err(errors::LoadAccountConfig::ReadAccountConfig {
                    source,
                    path: account_config_path,
                })
            }
        };
        match serde_json::from_str(&content) {
            Ok(config) => Ok(config),
            Err(source) => Err(errors::LoadAccountConfig::Deserialize { content, source }),
        }
    }

    fn save_account_config(&self) -> Result<(), errors::SaveAccountConfig> {
        let account_config = AccountConfig {
            current: self.account.name.clone(),
        };

        let content = serde_json::to_string_pretty(&account_config)
            .map_err(errors::SaveAccountConfig::Serialize)?;
        let account_config_path = self.account_config_path();
        match fs::write(account_config_path, content) {
            Ok(()) => Ok(()),
            Err(source) => Err(errors::SaveAccountConfig::Write {
                path: account_config_path.to_path_buf(),
                source,
            }),
        }
    }

    #[must_use]
    fn account_config_path(&self) -> &Path {
        self.account_config_path
            .get_or_init(|| self.base_path.join(ACCOUNT_CONFIG_NAME))
    }

    #[must_use]
    pub(crate) fn account_base_path(&self) -> &Path {
        self.account_base_path
            .get_or_init(|| self.base_path.join(&self.account.name))
    }

    #[must_use]
    fn secret_path(&self) -> &Path {
        self.secret_path
            .get_or_init(|| self.account_base_path().join(SECRET_CONFIG_NAME))
    }

    #[must_use]
    pub(crate) fn tokens_path(&self) -> &Path {
        self.tokens_path
            .get_or_init(|| self.account_base_path().join(TOKENS_CONFIG_NAME))
    }

    pub(crate) fn default_base_path() -> Result<PathBuf, errors::DefaultBasePath> {
        let home_path = home::home_dir().ok_or(errors::DefaultBasePath)?;
        let base_path = home_path
            .join(SYSTEM_CONFIG_DIR_NAME)
            .join(BASE_PATH_DIR_NAME);
        Ok(base_path)
    }

    fn create_account_dir(&self) -> Result<(), errors::CreateAccountDir> {
        let path = self.account_base_path();
        fs::create_dir_all(path).map_err(errors::CreateAccountDir)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AccountConfig {
    pub(crate) current: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Account {
    pub(crate) name: String,
}

impl Account {
    #[must_use]
    fn new(name: &str) -> Account {
        Account {
            name: name.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Secret {
    pub(crate) client_id: String,
    // TODO: zeroize this string on Drop
    pub(crate) client_secret: String,
}

pub(crate) fn set_file_permissions(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKENS: &str = r#"[{"scopes":["https://www.googleapis.com/auth/drive"],"token":{"access_token":"a","refresh_token":"r","expires_at":null,"id_token":null}}]"#;

    #[test]
    fn tokens_of_client_requires_the_same_client_id() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig::new(dir.path().to_path_buf(), Account::new("me@example.com"));
        assert!(config.tokens_of_client("id").is_none());

        config.create_account_dir().unwrap();
        config
            .save_secret(&Secret {
                client_id: String::from("id"),
                client_secret: String::from("secret"),
            })
            .unwrap();
        fs::write(config.tokens_path(), TOKENS).unwrap();

        let tokens = config.tokens_of_client("id").unwrap();
        assert_eq!(tokens.refresh_token(), Some("r"));
        assert!(config.tokens_of_client("other-id").is_none());
    }

    #[test]
    fn account_with_secret_does_not_need_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let config = AppConfig::new(base.clone(), Account::new("me@example.com"));
        config.create_account_dir().unwrap();
        config
            .save_secret(&Secret {
                client_id: String::from("id"),
                client_secret: String::from("secret"),
            })
            .unwrap();
        // No tokens.json: list_accounts skips this account.
        assert!(!config.tokens_path().exists());

        let (loaded, secret) =
            AppConfig::load_account_with_secret_in(base.clone(), "me@example.com").unwrap();
        assert_eq!(loaded.tokens_path(), config.tokens_path());
        assert_eq!(secret.client_id, "id");
    }

    #[test]
    fn account_with_secret_rejects_unknown_accounts_bad_names_and_empty_credentials() {
        use errors::LoadAccountWithSecret as E;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        let error = AppConfig::load_account_with_secret_in(base.clone(), "nobody").unwrap_err();
        assert!(matches!(&error, E::NotFound(name) if name == "nobody"));
        assert!(error.to_string().contains("gdrive account list"));

        for name in ["", ".", "..", "../me", "a/b", "a\\b", "/abs"] {
            let error = AppConfig::load_account_with_secret_in(base.clone(), name).unwrap_err();
            assert!(matches!(error, E::InvalidName(_)), "{name:?}: {error:?}");
        }

        let config = AppConfig::new(base.clone(), Account::new("empty"));
        config.create_account_dir().unwrap();
        fs::write(
            config.secret_path(),
            r#"{"client_id":"","client_secret":"s"}"#,
        )
        .unwrap();
        let error = AppConfig::load_account_with_secret_in(base.clone(), "empty").unwrap_err();
        assert!(matches!(error, E::IncompleteSecret(_)), "{error:?}");
    }

    const SECRET: &str = "GOCSPX-not-a-real-secret";

    fn load_secret_from(content: &str) -> Result<Secret, errors::LoadSecret> {
        let base_path = tempfile::tempdir().unwrap();
        let config = AppConfig::new(base_path.path().to_path_buf(), Account::new("account"));
        fs::create_dir_all(config.account_base_path()).unwrap();
        fs::write(config.secret_path(), content).unwrap();
        config.load_secret()
    }

    #[test]
    fn malformed_secret_file_errors_do_not_print_the_secret() {
        let contents = [
            // serde_json quotes the string: invalid type: string "...", expected struct Secret
            format!(r#""{SECRET}""#),
            // ... and the number: invalid type: integer `...`, expected a string
            r#"{"client_id": "id", "client_secret": 4815162342}"#.to_owned(),
            format!(r#"{{"client_id": "id", "client_secret": "{SECRET}""#),
            format!(r#"{{"client_id": "id", "client_secret": "{SECRET}"}} trailing"#),
        ];

        for content in contents {
            let err = crate::common::hub_helper::GetHubError::LoadSecret(
                load_secret_from(&content).unwrap_err(),
            );
            let message = crate::error_message(&err);

            assert!(
                message.contains("is not a valid client credentials file ("),
                "{message}"
            );
            assert!(message.contains(" at line 1 column "), "{message}");
            assert!(!message.contains(SECRET), "{message}");
            assert!(!message.contains("4815162342"), "{message}");
        }
    }
}
