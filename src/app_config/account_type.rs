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
use crate::auth::{
    lock::{AccountLock, LOCKS_DIR_NAME, LOCK_TIMEOUT},
    store::{self, TokenFile},
};

const SERVICE_ACCOUNT_KEY_NAME: &str = "service_account.json";

/// The directory under the config root where [`AppConfig::install`] prepares an account before it
/// moves it into place. It has no `account.json` of its own, so it is never listed as an account,
/// and no account may have its name.
pub(crate) const STAGING_DIR_NAME: &str = ".staging";

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
///
/// A directory that gdrive uses itself is never an account, whatever is in it: the lock of an
/// account named `tokens.json` is the file `.locks/tokens.json`.
pub(super) fn is_account_dir(path: &Path) -> bool {
    let reserved = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_reserved_name);
    !reserved
        && (path.join(TOKENS_CONFIG_NAME).exists() || path.join(ACCOUNT_CONFIG_NAME).is_file())
}

/// The names under the config root that gdrive uses itself: the file that names the current
/// account, the staging directory and the directory of the account locks. No account may have
/// one of them, in any case (Windows and macOS file systems ignore it).
const RESERVED_NAMES: [&str; 3] = [ACCOUNT_CONFIG_NAME, STAGING_DIR_NAME, LOCKS_DIR_NAME];

/// What [`is_valid_account_name`] requires, for messages.
pub(crate) const ACCOUNT_NAME_RULES: &str = "an account name must not be empty, '.' or '..', \
     contain '/' or '\\', or be 'account.json', '.staging' or '.locks', which gdrive uses itself";

/// Whether `name` can be the name of an account: it becomes a directory under the config root,
/// so it must be one plain path component, and none of the [`RESERVED_NAMES`].
pub(crate) fn is_valid_account_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) && !name.contains(['/', '\\'])
        && !is_reserved_name(name)
}

/// Whether `name` is one of the [`RESERVED_NAMES`], in any case.
fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

impl AppConfig {
    /// The account `account_name` in the config root `base_path`, or `None` if `account_name`
    /// cannot be an account name.
    pub(crate) fn named_account_in(base_path: PathBuf, account_name: &str) -> Option<AppConfig> {
        is_valid_account_name(account_name)
            .then(|| AppConfig::new(base_path, Account::new(account_name)))
    }

    /// Whether this account exists: its directory is one that `gdrive account list` lists.
    #[must_use]
    pub(crate) fn exists(&self) -> bool {
        is_account_dir(self.account_base_path())
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
    /// An existing account of another type than [`AccountType::User`] is replaced as a whole,
    /// cached tokens included, because they were issued for another key or subject.
    ///
    /// The new account is written into a new directory under the staging directory first (files
    /// with mode 0600, the directory 0700 on Unix), and moved into place only when all of it is
    /// written; an earlier account is moved aside just before and removed after. When anything
    /// fails, the earlier account stays as it was. Only a crash between the two renames leaves no
    /// account under the name; the earlier one is then in the staging directory.
    pub(crate) async fn install(
        &self,
        account_type: &AccountType,
        key: Option<&[u8]>,
        tokens: Option<&TokenFile>,
    ) -> Result<(), InstallAccount> {
        self.install_failing_at(account_type, key, tokens, None)
            .await
    }

    /// [`AppConfig::install`], with an injected failure at `fail_at` for tests; `None` outside
    /// tests.
    async fn install_failing_at(
        &self,
        account_type: &AccountType,
        key: Option<&[u8]>,
        tokens: Option<&TokenFile>,
        fail_at: Option<InstallStep>,
    ) -> Result<(), InstallAccount> {
        use InstallAccount as E;

        debug_assert_ne!(account_type, &AccountType::User);
        self.check_replaceable()?;

        let staging_root = self.base_path.join(STAGING_DIR_NAME);
        create_private_dir(&staging_root, true)
            .await
            .map_err(|source| E::CreateDir {
                path: staging_root.clone(),
                source,
            })?;
        let name = random_name().map_err(|source| E::CreateDir {
            path: staging_root.clone(),
            source,
        })?;
        let staging = staging_root.join(format!("{name}.new"));
        create_private_dir(&staging, false)
            .await
            .map_err(|source| E::CreateDir {
                path: staging.clone(),
                source,
            })?;

        let result = async {
            if let Some(key) = key {
                write_staged(
                    &staging,
                    SERVICE_ACCOUNT_KEY_NAME,
                    key,
                    fail_at,
                    InstallStep::WriteKey,
                )
                .await?;
            }
            let content = serde_json::to_vec_pretty(account_type).map_err(E::Serialize)?;
            write_staged(
                &staging,
                ACCOUNT_CONFIG_NAME,
                &content,
                fail_at,
                InstallStep::WriteType,
            )
            .await?;
            if let Some(tokens) = tokens {
                let content = tokens.to_json().map_err(E::Serialize)?;
                write_staged(
                    &staging,
                    TOKENS_CONFIG_NAME,
                    content.as_bytes(),
                    fail_at,
                    InstallStep::WriteTokens,
                )
                .await?;
            }
            let aside = staging_root.join(format!("{name}.old"));
            self.move_into_place(staging.clone(), aside, fail_at).await
        }
        .await;

        if result.is_err() {
            let _ = tokio::fs::remove_dir_all(&staging).await;
        }
        result
    }

    /// Replaces the account directory with `staging`, moving an existing one to `aside` first
    /// and removing it after. The check that the account may be replaced and the two renames are
    /// one change under the lock of the account (see `auth::lock`): a refresh of the replaced
    /// account that is under way finishes first, and nothing is moved when the lock cannot be
    /// had. The replaced account, now in the staging directory, is removed after.
    async fn move_into_place(
        &self,
        staging: PathBuf,
        aside: PathBuf,
        fail_at: Option<InstallStep>,
    ) -> Result<(), InstallAccount> {
        let dir = self.account_base_path().to_path_buf();
        let move_error = {
            let dir = dir.clone();
            move |source| InstallAccount::Move {
                path: dir.clone(),
                source,
            }
        };
        let lock = AccountLock::new(&self.base_path, &self.account.name).map_err(&move_error)?;
        let (account, moved_aside) = (self.clone(), aside.clone());
        let had_account = lock
            .hold_blocking(LOCK_TIMEOUT, move |_| {
                account.check_replaceable()?;
                account.swap_in(&staging, &moved_aside, fail_at)
            })
            .await
            .map_err(|error| move_error(error.into()))??;

        if had_account {
            if let Err(error) = tokio::fs::remove_dir_all(&aside).await {
                eprintln!(
                    "Warning: unable to remove the replaced account in '{}': {error}",
                    aside.display()
                );
            }
        }
        Ok(())
    }

    /// Moves an existing account directory to `aside` and `staging` in its place; on failure,
    /// the earlier account is put back. Returns whether there was one. Blocks the thread: under
    /// the account lock.
    fn swap_in(
        &self,
        staging: &Path,
        aside: &Path,
        fail_at: Option<InstallStep>,
    ) -> Result<bool, InstallAccount> {
        let dir = self.account_base_path();
        let move_error = |source| InstallAccount::Move {
            path: dir.to_path_buf(),
            source,
        };

        let had_account = match inject(fail_at, InstallStep::MoveOldAside).and(std::fs::exists(dir))
        {
            Ok(false) => false,
            Ok(true) => {
                fs::rename(dir, aside).map_err(move_error)?;
                true
            }
            Err(source) => return Err(move_error(source)),
        };

        let moved_in = match inject(fail_at, InstallStep::MoveNewIn) {
            Ok(()) => fs::rename(staging, dir),
            Err(error) => Err(error),
        };
        if let Err(source) = moved_in {
            if had_account {
                // Put the earlier account back.
                let _ = fs::rename(aside, dir);
            }
            return Err(move_error(source));
        }
        Ok(had_account)
    }
}

/// The steps of [`AppConfig::install`] that can fail, for failure-injection tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallStep {
    WriteKey,
    WriteType,
    WriteTokens,
    MoveOldAside,
    MoveNewIn,
}

/// The injected failure of a test at `step`.
fn inject(fail_at: Option<InstallStep>, step: InstallStep) -> io::Result<()> {
    if fail_at == Some(step) {
        return Err(io::Error::other(format!("injected failure at {step:?}")));
    }
    Ok(())
}

async fn write_staged(
    staging: &Path,
    name: &str,
    content: &[u8],
    fail_at: Option<InstallStep>,
    step: InstallStep,
) -> Result<(), InstallAccount> {
    let path = staging.join(name);
    let written = match inject(fail_at, step) {
        Ok(()) => store::write_private_file(&path, content).await,
        Err(error) => Err(error),
    };
    written.map_err(|source| InstallAccount::Write { path, source })
}

/// A random name for a staging directory.
fn random_name() -> io::Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};

    let mut bytes = [0; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("unable to get random bytes from the system"))?;
    Ok(format!("{:016x}", u64::from_le_bytes(bytes)))
}

async fn create_private_dir(path: &Path, recursive: bool) -> io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).await
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
    Move { path: PathBuf, source: io::Error },
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
            InstallAccount::Move { path, .. } => {
                write!(f, "unable to move the new account to '{}'", path.display())
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
            | InstallAccount::Move { source, .. } => Some(source),
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
        for name in [
            "",
            ".",
            "..",
            "../me",
            "a/b",
            "a\\b",
            "/abs",
            "account.json",
            "Account.JSON",
            ".staging",
            ".locks",
            ".Locks",
        ] {
            assert!(!is_valid_account_name(name), "{name:?}");
            assert!(AppConfig::named_account_in(PathBuf::from("/base"), name).is_none());
        }
    }

    /// The lock of an account named `tokens.json` is `.locks/tokens.json`, and a staged account
    /// has an `account.json`: neither makes gdrive's own directory an account.
    #[test]
    fn the_directories_gdrive_uses_itself_are_never_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        for (directory, file) in [
            (LOCKS_DIR_NAME, TOKENS_CONFIG_NAME),
            (LOCKS_DIR_NAME, ACCOUNT_CONFIG_NAME),
            (STAGING_DIR_NAME, TOKENS_CONFIG_NAME),
            (STAGING_DIR_NAME, ACCOUNT_CONFIG_NAME),
            ("me@example.com", TOKENS_CONFIG_NAME),
        ] {
            std::fs::create_dir_all(base.join(directory)).unwrap();
            std::fs::write(base.join(directory).join(file), "[]").unwrap();
        }

        assert!(!is_account_dir(&base.join(LOCKS_DIR_NAME)));
        assert!(!is_account_dir(&base.join(STAGING_DIR_NAME)));
        assert!(is_account_dir(&base.join("me@example.com")));
        assert_eq!(
            list_accounts_in(base.to_path_buf()).unwrap(),
            ["me@example.com"]
        );
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
    async fn install_swaps_the_directory_in_under_the_account_lock() {
        use crate::auth::lock::{
            test_hooks::{when_waiting, within, Elsewhere},
            AccountLock,
        };

        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        config
            .install(&AccountType::ApplicationDefaultCredentials, None, None)
            .await
            .unwrap();

        // Another process, a refresh of the service account for example, holds the lock: the
        // new account waits for it before it replaces the directory.
        let lock = AccountLock::new(dir.path(), "robot").unwrap();
        let elsewhere = Elsewhere::hold(lock, |_| ()).await;
        let waiting = when_waiting(config.tokens_path());
        let install = tokio::spawn({
            let config = config.clone();
            async move {
                config
                    .install(
                        &AccountType::ServiceAccount { subject: None },
                        Some(b"new-key"),
                        Some(&new_tokens()),
                    )
                    .await
            }
        });
        within("the wait of the install", waiting.notified()).await;
        assert_eq!(
            config.account_type().unwrap(),
            AccountType::ApplicationDefaultCredentials
        );
        elsewhere.finish().await;
        within("the install", install).await.unwrap().unwrap();
        assert_eq!(
            config.account_type().unwrap(),
            AccountType::ServiceAccount { subject: None }
        );
        assert!(config.tokens_path().is_file());
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

    /// The files of `dir` and their content; empty when it does not exist.
    fn snapshot(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let Ok(entries) = fs::read_dir(dir) else {
            return std::collections::BTreeMap::new();
        };
        entries
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn staging_leftovers(base: &Path) -> Vec<String> {
        snapshot(&base.join(STAGING_DIR_NAME)).into_keys().collect()
    }

    const ALL_STEPS: [InstallStep; 5] = [
        InstallStep::WriteKey,
        InstallStep::WriteType,
        InstallStep::WriteTokens,
        InstallStep::MoveOldAside,
        InstallStep::MoveNewIn,
    ];

    fn new_tokens() -> TokenFile {
        TokenFile::from_json(br#"[{"scopes":["https://www.googleapis.com/auth/drive"],"token":{"access_token":"new","refresh_token":null,"expires_at":null,"id_token":null}}]"#).unwrap()
    }

    #[tokio::test]
    async fn a_failed_replacement_leaves_the_earlier_account_as_it_was() {
        let bob = AccountType::ServiceAccount {
            subject: Some(String::from("bob@example.com")),
        };
        for step in ALL_STEPS {
            let dir = tempfile::tempdir().unwrap();
            let config = account(dir.path(), "robot");
            let alice = AccountType::ServiceAccount {
                subject: Some(String::from("alice@example.com")),
            };
            config
                .install(&alice, Some(b"alice-key"), None)
                .await
                .unwrap();
            fs::write(config.tokens_path(), "[]").unwrap();
            let before = snapshot(config.account_base_path());

            let error = config
                .install_failing_at(&bob, Some(b"bob-key"), Some(&new_tokens()), Some(step))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("unable to"), "{step:?}: {error}");

            assert_eq!(snapshot(config.account_base_path()), before, "{step:?}");
            assert_eq!(config.account_type().unwrap(), alice, "{step:?}");
            assert_eq!(
                staging_leftovers(dir.path()),
                Vec::<String>::new(),
                "{step:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_first_install_leaves_no_account() {
        for step in ALL_STEPS {
            let dir = tempfile::tempdir().unwrap();
            let config = account(dir.path(), "robot");

            config
                .install_failing_at(
                    &AccountType::ServiceAccount { subject: None },
                    Some(b"key"),
                    Some(&new_tokens()),
                    Some(step),
                )
                .await
                .unwrap_err();

            assert!(!config.account_base_path().exists(), "{step:?}");
            assert_eq!(
                list_accounts_in(dir.path().to_path_buf()).unwrap(),
                Vec::<String>::new()
            );
            assert_eq!(
                staging_leftovers(dir.path()),
                Vec::<String>::new(),
                "{step:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_replacement_swaps_in_the_complete_new_account() {
        let dir = tempfile::tempdir().unwrap();
        let config = account(dir.path(), "robot");
        config
            .install(
                &AccountType::ServiceAccount { subject: None },
                Some(b"old"),
                None,
            )
            .await
            .unwrap();
        fs::write(config.account_base_path().join("stale.tmp"), "x").unwrap();

        let new_type = AccountType::ServiceAccount {
            subject: Some(String::from("bob@example.com")),
        };
        config
            .install(&new_type, Some(b"new"), Some(&new_tokens()))
            .await
            .unwrap();

        let files = snapshot(config.account_base_path());
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["account.json", "service_account.json", "tokens.json"]
        );
        assert_eq!(files["service_account.json"], b"new");
        assert_eq!(config.account_type().unwrap(), new_type);
        assert_eq!(staging_leftovers(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn the_staging_directory_is_no_account() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join(STAGING_DIR_NAME).join("0123.new");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("account.json"), r#"{"type":"service_account"}"#).unwrap();
        fs::write(staged.join("tokens.json"), "[]").unwrap();

        assert_eq!(
            list_accounts_in(dir.path().to_path_buf()).unwrap(),
            Vec::<String>::new()
        );
        assert!(!is_valid_account_name(".staging"));
        assert!(!is_valid_account_name(".STAGING"));
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
