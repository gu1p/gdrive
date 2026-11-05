use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter, Write},
    ops::Not,
    path::Path,
};

use error_trace::ErrorTrace;

use crate::{
    app_config::{self, set_file_permissions, AppConfig},
    auth::lock::{AccountLock, LockError, LOCK_TIMEOUT},
    common::account_archive,
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) account_name: String,
}

pub(crate) fn export(config: &Config) -> Result<(), Error> {
    let Config { account_name } = config;
    let accounts = app_config::list_accounts().map_err(Error::ListAccounts)?;
    if accounts.contains(account_name).not() {
        return Err(Error::AccountNotFound(account_name.clone()));
    }

    let app_cfg = AppConfig::init_account(account_name).map_err(Error::InitAccount)?;
    let account_path = app_cfg.account_base_path();

    let archive_name = format!("gdrive_export-{}.tar", normalize_name(account_name));
    let archive_path = Path::new(&archive_name);
    // Under the lock of the account (see `auth::lock`): the archive holds the account's files as
    // a whole change of another gdrive process left them.
    AccountLock::new(&app_cfg.base_path, account_name)
        .map_err(|source| {
            Error::Lock(LockError::Io {
                path: account_path.to_path_buf(),
                source,
            })
        })?
        .hold(LOCK_TIMEOUT, |_| {
            account_archive::create(account_path, archive_path)
        })
        .map_err(Error::Lock)?
        .map_err(Error::CreateArchive)?;

    if let Err(err) = set_file_permissions(archive_path) {
        eprintln!(
            "Warning: Failed to set permissions on archive: {}",
            err.trace()
        );
    }

    println!("Exported account '{account_name}' to {archive_name}");

    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    ListAccounts(app_config::errors::ListAccounts),
    InitAccount(app_config::errors::InitAccount),
    AccountNotFound(String),
    CreateArchive(account_archive::errors::Create),
    Lock(LockError),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::ListAccounts(error) => Some(error),
            Error::InitAccount(error) => Some(error),
            Error::AccountNotFound(_) => None,
            Error::CreateArchive(error) => Some(error),
            // Shown as the lock error itself, which names the account and what to do.
            Error::Lock(error) => error.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ListAccounts(_) => f.write_str("unable to list accounts from config"),
            Error::InitAccount(_) => f.write_str("unable to initialize account in config"),
            Error::AccountNotFound(name) => write!(f, "account '{name}' not found"),
            Error::CreateArchive(_) => f.write_str("unable to create account archive"),
            Error::Lock(error) => error.fmt(f),
        }
    }
}

fn normalize_name(account_name: &str) -> Cow<'_, str> {
    match account_name
        .char_indices()
        .find_map(|(index, c)| c.is_alphanumeric().not().then_some(index))
    {
        None => Cow::Borrowed(account_name),
        Some(index) => {
            let mut normalized = String::with_capacity(account_name.len());
            let (init, rest) = account_name.split_at(index);
            write!(normalized, "{init}_").unwrap();
            normalized.extend(
                rest.chars()
                    .skip(1)
                    .map(|c| if c.is_alphanumeric() { c } else { '_' }),
            );
            Cow::Owned(normalized)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_name() {
        assert_eq!(super::normalize_name("hello123"), "hello123");
        assert_eq!(
            super::normalize_name("smile 😀! It's fine"),
            "smile____It_s_fine"
        );
    }
}
