use std::{
    error,
    fmt::{Display, Formatter},
    path::PathBuf,
};

use crate::{
    app_config::{self, account_type, AppConfig},
    auth::lock::{AccountLock, LockError, LOCK_TIMEOUT},
    common::account_archive,
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) archive_path: PathBuf,
}

pub(crate) fn import(config: &Config) -> Result<(), Error> {
    let account_name =
        account_archive::get_account_name(&config.archive_path).map_err(Error::ReadAccountName)?;
    // The directory is unpacked into the config root: it must not take the place of gdrive's own
    // files there, such as account.json.
    if !account_type::is_valid_account_name(&account_name) {
        return Err(Error::InvalidName(account_name));
    }

    let accounts = app_config::list_accounts().map_err(Error::ListAccounts)?;
    if accounts.contains(&account_name) {
        return Err(Error::AccountExists(account_name.clone()));
    }

    let config_base_path = AppConfig::default_base_path().map_err(Error::DefaultBasePath)?;
    let account_path = config_base_path.join(&account_name);
    let lock = AccountLock::new(&config_base_path, &account_name).map_err(|source| {
        Error::Lock(LockError::Io {
            path: account_path.clone(),
            source,
        })
    })?;

    // Unpacked into a private directory under the staging directory first, and moved into
    // place with one rename: a failure or a crash halfway never leaves half an account under its
    // name. The staging directory is removed when it goes out of scope.
    let staging_root = config_base_path.join(account_type::STAGING_DIR_NAME);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&staging_root).map_err(Error::Install)?;
    let staging = tempfile::Builder::new()
        .prefix("import-")
        .tempdir_in(&staging_root)
        .map_err(Error::Install)?;
    account_archive::unpack(&config.archive_path, staging.path()).map_err(Error::Unpack)?;
    let unpacked = staging.path().join(&account_name);

    // Under the lock of the account (see `auth::lock`), which a process of an account of this
    // name that was removed may still hold, and where the check is made again: another process
    // may have added the account in the meantime.
    lock.hold(LOCK_TIMEOUT, |_| {
        if AppConfig::named_account_in(config_base_path.clone(), &account_name)
            .is_some_and(|account| account.exists())
        {
            return Err(Error::AccountExists(account_name.clone()));
        }
        std::fs::rename(&unpacked, &account_path).map_err(Error::Install)
    })
    .map_err(Error::Lock)??;

    println!("Imported account '{account_name}'");

    if !AppConfig::has_current_account() {
        let app_cfg = AppConfig::load_account(&account_name).map_err(Error::LoadAccount)?;
        println!("Switched to account '{account_name}'");
        app_config::switch_account(&app_cfg).map_err(Error::SwitchAccount)?;
    }

    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    ReadAccountName(account_archive::errors::GetAccountName),
    ListAccounts(app_config::errors::ListAccounts),
    AccountExists(String),
    InvalidName(String),
    DefaultBasePath(app_config::errors::DefaultBasePath),
    Unpack(account_archive::errors::Unpack),
    LoadAccount(app_config::errors::LoadAccount),
    SwitchAccount(app_config::errors::SaveAccountConfig),
    Lock(LockError),
    /// The account could not be staged or moved into place.
    Install(std::io::Error),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::AccountExists(_) | Error::InvalidName(_) => None,
            Error::ReadAccountName(error) => Some(error),
            Error::Unpack(error) => Some(error),
            Error::ListAccounts(error) => Some(error),
            Error::LoadAccount(error) => Some(error),
            Error::DefaultBasePath(error) => Some(error),
            Error::SwitchAccount(error) => Some(error),
            // Shown as the lock error itself, which names the account and what to do.
            Error::Lock(error) => error.source(),
            Error::Install(error) => Some(error),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ReadAccountName(_) => f.write_str("unable to read the account name"),
            Error::ListAccounts(_) => f.write_str("unable to list accounts"),
            Error::AccountExists(name) => write!(f, "Account '{name}' already exists"),
            Error::InvalidName(name) => write!(
                f,
                "the archive holds the account '{name}', which cannot be imported: {}",
                account_type::ACCOUNT_NAME_RULES
            ),
            Error::DefaultBasePath(_) => f.write_str("unable to get the default base path"),
            Error::Unpack(_) => f.write_str("unable to unpack account archive"),
            Error::LoadAccount(_) => f.write_str("unable to load account"),
            Error::SwitchAccount(_) => f.write_str("unable to switch account"),
            Error::Lock(error) => error.fmt(f),
            Error::Install(_) => f.write_str("unable to put the imported account in place"),
        }
    }
}
