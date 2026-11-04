pub(crate) mod errors;

use std::{
    fs, io,
    ops::Not,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};

use crate::auth::store::TokenFile;

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

pub(crate) async fn add_account(
    account_name: &str,
    secret: &Secret,
    tokens: &TokenFile,
) -> Result<AppConfig, errors::AddAccount> {
    let config = AppConfig::init_account(account_name).map_err(errors::AddAccount::InitAccount)?;
    config
        .save_secret(secret)
        .map_err(errors::AddAccount::SaveSecret)?;
    tokens
        .save(config.tokens_path())
        .await
        .map_err(errors::AddAccount::SaveTokens)?;
    Ok(config)
}

pub(crate) fn switch_account(config: &AppConfig) -> Result<(), errors::SaveAccountConfig> {
    config.save_account_config()
}

pub(crate) fn list_accounts() -> Result<Vec<String>, errors::ListAccounts> {
    let base_path =
        AppConfig::default_base_path().map_err(errors::ListAccounts::DefaultBasePath)?;
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
        .filter(|entry| entry.path().join(TOKENS_CONFIG_NAME).exists())
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

    pub(crate) fn init_account(account_name: &str) -> Result<AppConfig, errors::InitAccount> {
        let base_path = AppConfig::default_base_path()?;
        let account = Account::new(account_name);

        let config = AppConfig::new(base_path, account);
        config.create_account_dir()?;

        Ok(config)
    }

    pub(crate) fn remove_account(&self) -> Result<(), errors::RemoveAccount> {
        let path = self.account_base_path();
        if let Err(source) = fs::remove_dir_all(path) {
            return Err(errors::RemoveAccount::RemoveDirectory {
                path: path.to_path_buf(),
                source,
            });
        }

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

    fn save_secret(&self, secret: &Secret) -> Result<(), errors::SaveSecret> {
        let content =
            serde_json::to_string_pretty(&secret).map_err(errors::SaveSecret::Serialize)?;
        let path = self.secret_path();
        if let Err(source) = fs::write(path, content) {
            return Err(errors::SaveSecret::Write {
                path: path.to_path_buf(),
                source,
            });
        }

        if let Err(err) = set_file_permissions(path) {
            eprintln!("Warning: Failed to set file permissions on secrets file: {err}");
        }

        Ok(())
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
            Err(source) => Err(errors::LoadSecret::Deserialize { content, source }),
        }
    }

    /// The stored tokens of this account, if there are any and they were issued to the OAuth
    /// client `client_id`. A refresh token of another client is of no use.
    pub(crate) async fn tokens_of_client(&self, client_id: &str) -> Option<TokenFile> {
        let secret = self.load_secret().ok()?;
        if secret.client_id != client_id {
            return None;
        }
        TokenFile::load(self.tokens_path()).await.ok().flatten()
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

    #[tokio::test]
    async fn tokens_of_client_requires_the_same_client_id() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig::new(dir.path().to_path_buf(), Account::new("me@example.com"));
        assert!(config.tokens_of_client("id").await.is_none());

        config.create_account_dir().unwrap();
        config
            .save_secret(&Secret {
                client_id: String::from("id"),
                client_secret: String::from("secret"),
            })
            .unwrap();
        fs::write(config.tokens_path(), TOKENS).unwrap();

        let tokens = config.tokens_of_client("id").await.unwrap();
        assert_eq!(tokens.refresh_token(), Some("r"));
        assert!(config.tokens_of_client("other-id").await.is_none());
    }
}
