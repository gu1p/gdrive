//! The type of an account, and the files of the accounts that do not log in with an OAuth client.
//!
//! An account is a directory under the config root. The accounts of gdrive 3.x log in with an
//! OAuth client: `secret.json` holds the client and `tokens.json` the authorization. That is
//! [`AccountType::User`], and such an account has no file of its own that names its type. The
//! other types are recorded in the account's own `account.json` (not the `account.json` of the
//! config root, which names the current account):
//!
//! - `{"type": "service_account"}`, with `"subject": "user@example.com"` for domain-wide
//!   delegation: the key file is stored as `service_account.json`, and `tokens.json` caches the
//!   access tokens issued for it.
//! - `{"type": "application_default_credentials"}`: nothing else is stored; the credentials are
//!   looked up again on every run.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{Account, AppConfig, ACCOUNT_CONFIG_NAME, TOKENS_CONFIG_NAME};
use crate::auth::store::{self, TokenFile};

const SERVICE_ACCOUNT_KEY_NAME: &str = "service_account.json";

/// How an account gets its access tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AccountType {
    /// An OAuth client and a user's authorization (`secret.json`, `tokens.json`).
    User,
    /// A service account key (`service_account.json`).
    ServiceAccount {
        /// The Google Workspace user the service account acts for (domain-wide delegation).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
    },
    /// Application Default Credentials, looked up on every run; nothing else is stored.
    ApplicationDefaultCredentials,
}

/// Whether `path` is the directory of an account: gdrive 3.x accounts have `tokens.json`, the
/// others have `account.json` (a service account has no `tokens.json` until it got a token, an
/// ADC account never has one).
pub(super) fn is_account_dir(path: &Path) -> bool {
    path.join(TOKENS_CONFIG_NAME).exists() || path.join(ACCOUNT_CONFIG_NAME).is_file()
}

/// Whether `name` can be the name of an account: it becomes a directory under the config root,
/// so it must be one plain path component.
pub(crate) fn is_valid_account_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) && !name.contains(['/', '\\'])
}

impl AppConfig {
    /// The account `account_name` in the config root `base_path`, or `None` if `account_name`
    /// cannot be an account name.
    pub(crate) fn named_account_in(base_path: PathBuf, account_name: &str) -> Option<AppConfig> {
        is_valid_account_name(account_name)
            .then(|| AppConfig::new(base_path, Account::new(account_name)))
    }

    /// The type of this account, from its `account.json`; [`AccountType::User`] without one.
    pub(crate) fn account_type(&self) -> Result<AccountType, LoadAccountType> {
        let path = self.account_type_path();
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(AccountType::User)
            }
            Err(source) => return Err(LoadAccountType::Read { path, source }),
        };
        // The file holds no secret, so the serde_json message, which names an unknown type, is
        // kept.
        serde_json::from_slice(&content).map_err(|source| LoadAccountType::Parse { path, source })
    }

    /// The key file of a service account.
    pub(crate) fn service_account_key_path(&self) -> PathBuf {
        self.account_base_path().join(SERVICE_ACCOUNT_KEY_NAME)
    }

    fn account_type_path(&self) -> PathBuf {
        self.account_base_path().join(ACCOUNT_CONFIG_NAME)
    }

    /// Fails if this account exists and logs in with an OAuth client: a service account must not
    /// be mixed into its directory. Accounts of the other types may be replaced.
    pub(crate) fn check_replaceable(&self) -> Result<(), InstallAccount> {
        let account_type = self.account_type().map_err(InstallAccount::LoadType)?;
        let has_user_files = self.secret_path().exists() || self.tokens_path().exists();
        if account_type == AccountType::User && has_user_files {
            return Err(InstallAccount::UserAccountExists(self.account.name.clone()));
        }
        Ok(())
    }

    /// Stores this account as `account_type`, which must not be [`AccountType::User`], with
    /// the content of its key file, if it has one, and its first tokens, if there are any.
    ///
    /// An existing account of another type than [`AccountType::User`] is replaced: its cached
    /// tokens are removed, because they may have been issued for another key or subject. Every
    /// file is written atomically and with mode 0600 on Unix; `account.json` comes after the key,
    /// so an interrupted run never leaves an account without its key.
    pub(crate) async fn install(
        &self,
        account_type: &AccountType,
        key: Option<&[u8]>,
        tokens: Option<&TokenFile>,
    ) -> Result<(), InstallAccount> {
        use InstallAccount as E;

        debug_assert_ne!(account_type, &AccountType::User);
        self.check_replaceable()?;

        let dir = self.account_base_path();
        create_private_dir(dir)
            .await
            .map_err(|source| E::CreateDir {
                path: dir.to_path_buf(),
                source,
            })?;

        let tokens_path = self.tokens_path();
        remove_if_exists(tokens_path).await?;

        let key_path = self.service_account_key_path();
        match key {
            Some(key) => store::write_private_file(&key_path, key)
                .await
                .map_err(|source| E::Write {
                    path: key_path.clone(),
                    source,
                })?,
            None => remove_if_exists(&key_path).await?,
        }

        let type_path = self.account_type_path();
        let content = serde_json::to_vec_pretty(account_type).map_err(E::Serialize)?;
        store::write_private_file(&type_path, &content)
            .await
            .map_err(|source| E::Write {
                path: type_path.clone(),
                source,
            })?;

        if let Some(tokens) = tokens {
            tokens.save(tokens_path).await.map_err(|source| E::Write {
                path: tokens_path.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }
}

async fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).await
}

async fn remove_if_exists(path: &Path) -> Result<(), InstallAccount> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(InstallAccount::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug)]
pub(crate) enum LoadAccountType {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl Display for LoadAccountType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            LoadAccountType::Read { path, .. } => write!(f, "unable to read '{}'", path.display()),
            LoadAccountType::Parse { path, .. } => write!(
                f,
                "'{}' is not a valid account type file (was it written by a newer gdrive?)",
                path.display()
            ),
        }
    }
}

impl Error for LoadAccountType {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            LoadAccountType::Read { source, .. } => Some(source),
            LoadAccountType::Parse { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum InstallAccount {
    LoadType(LoadAccountType),
    UserAccountExists(String),
    CreateDir { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Remove { path: PathBuf, source: io::Error },
    Serialize(serde_json::Error),
}

impl Display for InstallAccount {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            InstallAccount::LoadType(_) => f.write_str("unable to load the existing account"),
            InstallAccount::UserAccountExists(name) => write!(
                f,
                "account '{name}' exists already and logs in with an OAuth client\n\
                 Use --name to choose another name, or remove it with `gdrive account remove \
                 {name}`."
            ),
            InstallAccount::CreateDir { path, .. } => {
                write!(f, "unable to create directory '{}'", path.display())
            }
            InstallAccount::Write { path, .. } => {
                write!(f, "unable to write '{}'", path.display())
            }
            InstallAccount::Remove { path, .. } => {
                write!(f, "unable to remove '{}'", path.display())
            }
            InstallAccount::Serialize(_) => f.write_str("unable to serialize the account type"),
        }
    }
}

impl Error for InstallAccount {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            InstallAccount::LoadType(source) => Some(source),
            InstallAccount::UserAccountExists(_) => None,
            InstallAccount::CreateDir { source, .. }
            | InstallAccount::Write { source, .. }
            | InstallAccount::Remove { source, .. } => Some(source),
            InstallAccount::Serialize(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::list_accounts_in;

    fn account(base: &Path, name: &str) -> AppConfig {
        AppConfig::named_account_in(base.to_path_buf(), name).unwrap()
    }

    #[test]
    fn account_type_files_round_trip() {
        let subject = AccountType::ServiceAccount {
            subject: Some(String::from("user@example.com")),
        };
        let json = serde_json::to_string(&subject).unwrap();
        assert_eq!(
            json,
            r#"{"type":"service_account","subject":"user@example.com"}"#
        );
        assert_eq!(serde_json::from_str::<AccountType>(&json).unwrap(), subject);

        let no_subject = AccountType::ServiceAccount { subject: None };
        let json = serde_json::to_string(&no_subject).unwrap();
        assert_eq!(json, r#"{"type":"service_account"}"#);
        assert_eq!(
            serde_json::from_str::<AccountType>(&json).unwrap(),
            no_subject
        );

        let adc = AccountType::ApplicationDefaultCredentials;
        let json = serde_json::to_string(&adc).unwrap();
        assert_eq!(json, r#"{"type":"application_default_credentials"}"#);
        assert_eq!(serde_json::from_str::<AccountType>(&json).unwrap(), adc);
    }

    #[test]
    fn accounts_without_account_json_are_user_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "me@example.com");
        assert_eq!(config.account_type().unwrap(), AccountType::User);
    }

    #[test]
    fn an_unknown_type_is_an_error_that_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        fs::create_dir_all(config.account_base_path()).unwrap();
        fs::write(config.account_type_path(), r#"{"type":"from_the_future"}"#).unwrap();

        let error = config.account_type().unwrap_err();
        assert!(matches!(error, LoadAccountType::Parse { .. }), "{error:?}");
        assert!(error.to_string().contains("account.json"), "{error}");
    }

    #[test]
    fn account_names_are_single_path_components() {
        for name in [
            "me@example.com",
            "robot",
            "a b",
            "sa@p.iam.gserviceaccount.com",
        ] {
            assert!(is_valid_account_name(name), "{name:?}");
        }
        for name in ["", ".", "..", "../me", "a/b", "a\\b", "/abs"] {
            assert!(!is_valid_account_name(name), "{name:?}");
            assert!(AppConfig::named_account_in(PathBuf::from("/base"), name).is_none());
        }
    }

    #[tokio::test]
    async fn install_writes_private_files_and_lists_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        let account_type = AccountType::ServiceAccount {
            subject: Some(String::from("user@example.com")),
        };
        let tokens = TokenFile::from_json(br#"[{"scopes":["https://www.googleapis.com/auth/drive"],"token":{"access_token":"a","refresh_token":null,"expires_at":null,"id_token":null}}]"#).unwrap();

        config
            .install(&account_type, Some(b"{\"key\": 1}"), Some(&tokens))
            .await
            .unwrap();

        assert_eq!(
            fs::read(config.service_account_key_path()).unwrap(),
            b"{\"key\": 1}"
        );
        assert_eq!(config.account_type().unwrap(), account_type);
        assert_eq!(
            TokenFile::load(config.tokens_path()).await.unwrap(),
            Some(tokens)
        );
        assert_eq!(
            list_accounts_in(dir.path().to_path_buf()).unwrap(),
            ["robot"]
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&config.service_account_key_path()), 0o600);
            assert_eq!(mode(&config.account_type_path()), 0o600);
            assert_eq!(mode(config.tokens_path()), 0o600);
            assert_eq!(mode(config.account_base_path()), 0o700);
        }
    }

    #[tokio::test]
    async fn a_service_account_without_tokens_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        config
            .install(
                &AccountType::ServiceAccount { subject: None },
                Some(b"{}"),
                None,
            )
            .await
            .unwrap();

        assert!(!config.tokens_path().exists());
        assert_eq!(
            list_accounts_in(dir.path().to_path_buf()).unwrap(),
            ["robot"]
        );
    }

    #[tokio::test]
    async fn install_replaces_a_service_account_and_drops_its_cached_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        let first = AccountType::ServiceAccount {
            subject: Some(String::from("a@example.com")),
        };
        config.install(&first, Some(b"old"), None).await.unwrap();
        fs::write(config.tokens_path(), "[]").unwrap();

        let second = AccountType::ServiceAccount { subject: None };
        config.install(&second, Some(b"new"), None).await.unwrap();

        assert_eq!(fs::read(config.service_account_key_path()).unwrap(), b"new");
        assert_eq!(config.account_type().unwrap(), second);
        assert!(!config.tokens_path().exists());
    }

    #[tokio::test]
    async fn an_adc_account_is_only_its_account_json() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        config
            .install(
                &AccountType::ServiceAccount { subject: None },
                Some(b"{}"),
                None,
            )
            .await
            .unwrap();
        fs::write(config.tokens_path(), "[]").unwrap();

        // Replacing a service account by ADC drops its key and tokens.
        config
            .install(&AccountType::ApplicationDefaultCredentials, None, None)
            .await
            .unwrap();
        assert_eq!(
            config.account_type().unwrap(),
            AccountType::ApplicationDefaultCredentials
        );
        assert!(!config.service_account_key_path().exists());
        assert!(!config.tokens_path().exists());
        assert_eq!(
            list_accounts_in(dir.path().to_path_buf()).unwrap(),
            ["robot"]
        );
    }

    #[tokio::test]
    async fn install_refuses_to_mix_into_a_user_account() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "me@example.com");
        fs::create_dir_all(config.account_base_path()).unwrap();
        fs::write(
            config.secret_path(),
            r#"{"client_id":"i","client_secret":"s"}"#,
        )
        .unwrap();

        let error = config
            .install(
                &AccountType::ServiceAccount { subject: None },
                Some(b"{}"),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, InstallAccount::UserAccountExists(name) if name == "me@example.com"),
            "{error:?}"
        );
        assert!(error.to_string().contains("--name"), "{error}");
        assert!(!config.service_account_key_path().exists());
        assert!(!config.account_type_path().exists());
    }
}
