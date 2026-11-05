//! The lock of an account's files, shared by every gdrive process that uses the config root.
//!
//! # What it protects
//!
//! The files of an account directory that hold its authorization (`tokens.json`, `secret.json`)
//! and the directory itself, which `account remove` deletes and `account import` and
//! [`AppConfig::install`](crate::app_config::AppConfig::install) put in place. A change of them
//! is one step for the other processes: what a writer read and checked under the lock is still
//! what is on disk when it writes, and no writer sees the half of another one's change.
//!
//! There is one lock per account: an exclusive advisory lock (`flock` on Unix, `LockFileEx` on
//! Windows, through fd-lock) on the empty file `<config root>/.locks/<account name>`. It lives
//! outside the account directory, which removing the account deletes, so that removing an
//! account and adding it again keep one lock: a process that still holds it for the removed
//! directory and the process that adds the account again exclude each other. Lock files are never
//! deleted. Their names are account names, which [`is_valid_account_name`] keeps to one plain path
//! component that is neither `.locks` nor any other name gdrive uses in the config root: the rule
//! that makes a name safe as the name of the account directory makes it safe here.
//!
//! # Who takes it, and for how long
//!
//! Every operation that changes an account's files, and the export, which reads all of them:
//!
//! - the token store of user and service accounts (`store::TokenStore`): read `tokens.json`,
//!   check that it is still the file of the authorization this process refreshes with, write the
//!   new file and rename it into place;
//! - `account add` and `account reauth`: read the previous tokens, check the audit-log grant, write
//!   the secret (`add`) and the tokens;
//! - `account remove`: delete the directory;
//! - `account import`: check that the account does not exist, unpack it;
//! - adding a service account or Application Default Credentials (`AppConfig::install`): check the
//!   account that is there, swap in the directory that was prepared beside it;
//! - `account export`: read the directory into the archive.
//!
//! The lock is held only for these local file operations, never across a network request or the
//! browser login, which happen before. `account list`, `account switch` and `account current`
//! never take it.
//!
//! # Waiting
//!
//! An operation waits at most [`LOCK_TIMEOUT`] for another one. Then the token store keeps the
//! refreshed token in memory and the command goes on; every other operation fails with
//! [`LockError::Timeout`] ("... nothing was changed, try again in a moment") and changes nothing.
//!
//! # Why it cannot be split
//!
//! The lock is held only inside [`AccountLock::hold`], by a synchronous closure: there is no await
//! point while it is held, so no future can be dropped halfway. Async callers run the whole of it,
//! the wait for the lock included, on a blocking thread ([`AccountLock::hold_blocking`]). tokio
//! never cancels a blocking task that has started, and dropping its handle (a timeout, Ctrl-C)
//! only detaches it: the closure runs to its end, and the lock is released after its last file
//! operation returned. If the process dies instead, the operating system releases the lock, and
//! since every file is replaced by a rename, it is either the old or the new one. A process whose
//! account directory was removed finds no tokens under the lock, writes nothing, and so never
//! recreates the directory.

use std::{
    error::Error,
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::app_config::account_type::is_valid_account_name;

/// The directory of the lock files, in the config root.
pub(crate) const LOCKS_DIR_NAME: &str = ".locks";

/// The tokens file in an account directory (`AppConfig::tokens_path`).
const TOKENS_FILE_NAME: &str = "tokens.json";

/// How long an operation waits for another gdrive process to finish its change of the account.
/// A change holds the lock for a few local file operations only.
pub(crate) const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a waiting operation tries the lock again.
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// The lock of the account `name` in the config root `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountLock {
    base: PathBuf,
    name: String,
}

impl AccountLock {
    /// The lock of the account `name` in the config root `base`. Fails if `name` cannot be an
    /// account name ([`is_valid_account_name`]).
    pub(crate) fn new(base: &Path, name: &str) -> io::Result<Self> {
        if !is_valid_account_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{name}' cannot be the name of an account"),
            ));
        }
        Ok(AccountLock {
            base: base.to_path_buf(),
            name: name.to_owned(),
        })
    }

    /// The lock of the account whose tokens file is `tokens_path`,
    /// `<config root>/<account name>/tokens.json`.
    pub(crate) fn of_tokens_file(tokens_path: &Path) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "'{}' is not the tokens file of an account",
                    tokens_path.display()
                ),
            )
        };
        if tokens_path.file_name() != Some(OsStr::new(TOKENS_FILE_NAME)) {
            return Err(invalid());
        }
        let dir = tokens_path.parent().ok_or_else(invalid)?;
        let name = dir
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(invalid)?;
        let base = dir.parent().ok_or_else(invalid)?;
        AccountLock::new(base, name)
    }

    /// The account directory.
    pub(crate) fn account_dir(&self) -> PathBuf {
        self.base.join(&self.name)
    }

    /// The tokens file of the account.
    pub(crate) fn tokens_path(&self) -> PathBuf {
        self.account_dir().join(TOKENS_FILE_NAME)
    }

    /// The lock file.
    fn path(&self) -> PathBuf {
        self.base.join(LOCKS_DIR_NAME).join(&self.name)
    }

    /// Runs `change` while this process holds the lock, waiting at most `timeout` for another
    /// one. Blocks the thread: async code calls [`AccountLock::hold_blocking`].
    ///
    /// The config root must exist: the lock directory is created in it, the root never is, so
    /// that a process whose config root was removed does not create it again.
    pub(crate) fn hold<T>(
        &self,
        timeout: Duration,
        change: impl FnOnce(&Held<'_>) -> T,
    ) -> Result<T, LockError> {
        let file = self.open().map_err(|source| LockError::Io {
            path: self.path(),
            source,
        })?;
        let mut lock = fd_lock::RwLock::new(file);
        let deadline = Instant::now() + timeout;
        let mut waiting = false;
        loop {
            match lock.try_write() {
                // The guard lives until `change` returned, and is dropped on unwinding too.
                Ok(_guard) => return Ok(change(&Held { lock: self })),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) => {
                    return Err(LockError::Io {
                        path: self.path(),
                        source,
                    })
                }
            }
            if !waiting {
                waiting = true;
                #[cfg(test)]
                test_hooks::waiting(&self.tokens_path());
            }
            if Instant::now() >= deadline {
                return Err(LockError::Timeout {
                    account: self.name.clone(),
                });
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }

    /// [`AccountLock::hold`] on a blocking thread. The returned future may be dropped at any
    /// point without splitting the change: the blocking task, once started, runs to its end, and
    /// releases the lock after its last file operation (see the module documentation).
    pub(crate) async fn hold_blocking<T, F>(
        self,
        timeout: Duration,
        change: F,
    ) -> Result<T, LockError>
    where
        T: Send + 'static,
        F: FnOnce(&Held<'_>) -> T + Send + 'static,
    {
        let path = self.path();
        match tokio::task::spawn_blocking(move || self.hold(timeout, change)).await {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(LockError::Io {
                path,
                source: io::Error::other(error),
            }),
        }
    }

    /// Opens the lock file, creating it (and the lock directory) if needed: the directory with
    /// mode 0700, the file with mode 0600 on Unix.
    fn open(&self) -> io::Result<fs::File> {
        let dir = self.base.join(LOCKS_DIR_NAME);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(dir.join(&self.name))
    }
}

/// Proof that this process holds the lock of an account, handed to the change of
/// [`AccountLock::hold`]. The account's tokens file is written only with it
/// (`store::TokenFile::write_locked`).
pub(crate) struct Held<'a> {
    lock: &'a AccountLock,
}

impl Held<'_> {
    /// The account whose lock is held.
    pub(crate) fn account(&self) -> &AccountLock {
        self.lock
    }
}

/// The lock of an account could not be taken.
#[derive(Debug)]
pub(crate) enum LockError {
    /// Another gdrive process held it for longer than the operation waits.
    Timeout {
        account: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl Display for LockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Timeout { account } => write!(
                f,
                "another gdrive process is updating the files of account '{account}'; nothing \
                 was changed, try again in a moment"
            ),
            LockError::Io { path, .. } => write!(f, "unable to lock '{}'", path.display()),
        }
    }
}

impl Error for LockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            LockError::Timeout { .. } => None,
            LockError::Io { source, .. } => Some(source),
        }
    }
}

impl From<LockError> for io::Error {
    fn from(error: LockError) -> Self {
        let kind = match &error {
            LockError::Timeout { .. } => io::ErrorKind::TimedOut,
            LockError::Io { source, .. } => source.kind(),
        };
        io::Error::new(kind, error)
    }
}

/// Hooks that let tests stop an operation at the steps that matter, to make overlaps certain.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, LazyLock, Mutex},
    };

    use tokio::sync::Notify;

    static WAITING: LazyLock<Mutex<HashMap<PathBuf, Arc<Notify>>>> = LazyLock::new(Mutex::default);

    /// Notified when an operation on the account of the tokens file `tokens_path` finds the lock
    /// held and waits.
    pub(crate) fn when_waiting(tokens_path: &Path) -> Arc<Notify> {
        Arc::clone(
            WAITING
                .lock()
                .unwrap()
                .entry(tokens_path.to_path_buf())
                .or_default(),
        )
    }

    pub(super) fn waiting(tokens_path: &Path) {
        if let Some(notify) = WAITING.lock().unwrap().get(tokens_path) {
            notify.notify_one();
        }
    }

    /// Stops an operation at a point under the lock: it notifies `reached` and waits for
    /// `resume`. Usable from the blocking thread that holds the lock.
    #[derive(Clone, Default)]
    pub(crate) struct Pause {
        pub(crate) reached: Arc<Notify>,
        pub(crate) resume: Arc<Notify>,
    }

    impl Pause {
        pub(crate) fn stop(&self) {
            self.reached.notify_one();
            futures::executor::block_on(self.resume.notified());
        }
    }

    /// Bounds a step that only finishes if the other operation behaves as it must.
    pub(crate) async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(10), future)
            .await
            .unwrap_or_else(|_| panic!("{what} did not happen"))
    }

    /// Another gdrive process that holds the lock `lock` on a blocking thread until
    /// [`Elsewhere::finish`], and then runs its change under it.
    pub(crate) struct Elsewhere {
        release: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Elsewhere {
        /// Returns once the lock is held.
        pub(crate) async fn hold(
            lock: super::AccountLock,
            change: impl FnOnce(&super::Held<'_>) + Send + 'static,
        ) -> Elsewhere {
            let (held, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
            let task = tokio::task::spawn_blocking({
                let (held, release) = (Arc::clone(&held), Arc::clone(&release));
                move || {
                    lock.hold(super::LOCK_TIMEOUT, |locked| {
                        held.notify_one();
                        futures::executor::block_on(release.notified());
                        change(locked);
                    })
                    .unwrap();
                }
            });
            within("the lock of the other process", held.notified()).await;
            Elsewhere { release, task }
        }

        /// Lets the other process make its change and release the lock.
        pub(crate) async fn finish(self) {
            self.release.notify_one();
            within("the change of the other process", self.task)
                .await
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::AppConfig;

    #[test]
    fn the_lock_of_an_account_is_outside_its_directory_and_named_after_it() {
        let base = Path::new("/config/gdrive3");
        let lock = AccountLock::new(base, "me@example.com").unwrap();
        assert_eq!(lock.path(), base.join(".locks").join("me@example.com"));
        assert_eq!(lock.account_dir(), base.join("me@example.com"));

        // The same lock as the one derived from the account's tokens file.
        let app_cfg = AppConfig::named_account_in(base.to_path_buf(), "me@example.com").unwrap();
        assert_eq!(lock.tokens_path(), app_cfg.tokens_path());
        assert_eq!(
            AccountLock::of_tokens_file(app_cfg.tokens_path()).unwrap(),
            lock
        );

        // Names that could escape the lock directory or take its place are no account names.
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            ".locks",
            ".LOCKS",
            ".staging",
            "account.json",
        ] {
            assert!(AccountLock::new(base, name).is_err(), "{name:?}");
        }
        for path in [
            "/config/gdrive3/me@example.com/secret.json",
            "tokens.json",
            "/tokens.json",
            "/config/gdrive3/.locks/tokens.json",
        ] {
            assert!(
                AccountLock::of_tokens_file(Path::new(path)).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn the_lock_excludes_other_holders_and_the_lock_file_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let lock = AccountLock::new(dir.path(), "me@example.com").unwrap();
        let other = lock.clone();

        let inner = lock
            .hold(LOCK_TIMEOUT, |held| {
                assert_eq!(held.account(), &other);
                // A second handle, of this process or another one, does not get it.
                other.hold(Duration::from_millis(50), |_| ()).unwrap_err()
            })
            .unwrap();
        assert!(matches!(inner, LockError::Timeout { .. }), "{inner:?}");
        assert_eq!(
            inner.to_string(),
            "another gdrive process is updating the files of account 'me@example.com'; \
             nothing was changed, try again in a moment"
        );
        // Released when the change returned.
        other.hold(LOCK_TIMEOUT, |_| ()).unwrap();

        let file = fs::metadata(lock.path()).unwrap();
        assert_eq!(file.len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
            let dir = fs::metadata(dir.path().join(LOCKS_DIR_NAME)).unwrap();
            assert_eq!(dir.permissions().mode() & 0o777, 0o700);
        }
        // Only the lock directory was created: no account directory.
        assert!(!lock.account_dir().exists());
    }

    #[test]
    fn the_lock_never_creates_the_config_root() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("removed");
        let lock = AccountLock::new(&base, "me@example.com").unwrap();
        let error = lock.hold(LOCK_TIMEOUT, |_| ()).unwrap_err();
        assert!(
            matches!(&error, LockError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound),
            "{error:?}"
        );
        assert!(!base.exists());
    }
}
