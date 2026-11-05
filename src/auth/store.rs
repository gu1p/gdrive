//! Reading and writing `tokens.json`.
//!
//! The layout is the one of yup-oauth2's disk storage, which every gdrive 3.x release wrote: a
//! JSON array of `{"scopes": [..], "token": {"access_token", "refresh_token", "expires_at",
//! "id_token"}}` entries. gdrive reads and writes it itself, through [`TokenStore`], because
//! yup-oauth2's disk storage would store a refresh response without `refresh_token` as `null`
//! (the refresh-only flow does not carry the old refresh token over) and rewrites the file in
//! place. Here a missing refresh token never replaces a stored one, and the file is replaced
//! atomically.
//!
//! The scopes of the entries also record what the account was granted: the login stores its
//! token for the scopes Google granted, and [`TokenFile::audit_log_granted`] reads from them
//! whether the account may read the audit log.
//!
//! The entries of an OAuth user account belong to one authorization, the one whose refresh
//! token they carry ([`TokenFile::refresh_token`]). A login replaces the whole file. A process
//! that loaded the tokens before that login refreshes with the refresh token of the earlier
//! authorization, which may lack the audit log or be revoked; [`TokenStore`] keeps what it
//! refreshes in memory then and leaves the file to the new authorization.
//!
//! Every write of the file is one change under the lock of the account (see [`super::lock`]): a
//! refresh reads, checks and writes under one lock, a login waits for a refresh that is under
//! way, and a refresh that starts during a login sees the new file.

use std::{
    error::Error,
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use yup_oauth2::storage::{TokenInfo, TokenStorage, TokenStorageError};

#[cfg(test)]
use super::lock::test_hooks;
use super::{
    lock::{AccountLock, Held, LockError, LOCK_TIMEOUT},
    AUDIT_LOG_SCOPE,
};
use crate::common::json_error::RedactedJsonError;

/// One `tokens.json` entry: a token and the scopes it was issued for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TokenEntry {
    pub(crate) scopes: Vec<String>,
    pub(crate) token: TokenInfo,
    /// The credential a service account token was issued for (see
    /// `auth::service::credential_id`), so that a token of a replaced key or subject is never
    /// taken for the new one. Absent for the tokens of OAuth user accounts, which keeps their
    /// layout that of gdrive 3.x.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) credential: Option<String>,
}

/// The content of `tokens.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TokenFile {
    entries: Vec<TokenEntry>,
}

impl TokenFile {
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[TokenEntry] {
        &self.entries
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn from_json(json: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(json)
    }

    pub(crate) fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// The best token of an OAuth user account for `scopes`: [`TokenFile::get_for`] without a
    /// credential.
    pub(crate) fn get(&self, scopes: &[&str]) -> Option<&TokenInfo> {
        self.get_for(None, scopes)
    }

    /// The best token of `credential` for `scopes`: among its entries whose scopes include all
    /// of `scopes`, an unexpired one if there is any, and the one that expires last. Entries of
    /// other credentials do not count, nor, for an OAuth user account, entries of an earlier
    /// authorization ([`TokenFile::user_entries`]).
    pub(crate) fn get_for(&self, credential: Option<&str>, scopes: &[&str]) -> Option<&TokenInfo> {
        let refresh_token = self.refresh_token();
        self.entries
            .iter()
            .filter(|entry| match credential {
                None => entry.credential.is_none() && of_authorization(entry, refresh_token),
                Some(_) => entry.credential.as_deref() == credential,
            })
            .filter(|entry| covers(&entry.scopes, scopes))
            .max_by_key(|entry| (!entry.token.is_expired(), entry.token.expires_at))
            .map(|entry| &entry.token)
    }

    /// The refresh token of the account's authorization: the one of the most recently issued
    /// entry (latest expiry) that has a refresh token, among the entries that record the
    /// audit-log grant if there are any.
    ///
    /// A file this version writes holds one authorization: a login replaces the whole file, and
    /// [`TokenStore`] never writes into the file of an authorization other than the one it
    /// refreshes with. The preference keeps the choice right in a file where a process of an
    /// earlier, Drive-only authorization wrote its tokens next to those of a login with the
    /// audit log, as earlier builds let it: an entry that records the grant is written only
    /// under an authorization that was granted it, and a later login without the grant removes
    /// every such entry.
    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.credential.is_none())
            .filter(|entry| entry.token.refresh_token.is_some())
            .max_by_key(|entry| (records_audit_log(entry), entry.token.expires_at))
            .and_then(|entry| entry.token.refresh_token.as_deref())
    }

    /// The entries of the OAuth user account's authorization: those with its refresh token
    /// ([`TokenFile::refresh_token`]), and those without any (gdrive 3.x stored refreshed
    /// tokens without one). An entry with another refresh token is of an earlier authorization
    /// and is never used.
    fn user_entries(&self) -> impl Iterator<Item = &TokenEntry> {
        let refresh_token = self.refresh_token();
        self.entries.iter().filter(move |entry| {
            entry.credential.is_none() && of_authorization(entry, refresh_token)
        })
    }

    /// Whether the OAuth user account was granted read access to the audit log
    /// ([`AUDIT_LOG_SCOPE`]): one of the entries of its authorization
    /// ([`TokenFile::user_entries`]) is stored for a scope set that includes it. The refresh
    /// token of that authorization ([`TokenFile::refresh_token`]) is then one of these entries'.
    ///
    /// Only a login that was granted the audit log writes such an entry
    /// ([`TokenFile::after_login`]), and only the next login removes it: a refresh for other
    /// scopes adds or replaces the entry of those scopes and leaves this one alone, and
    /// [`TokenStore`] stores a token asked for the audit log only while this holds.
    pub(crate) fn audit_log_granted(&self) -> bool {
        self.user_entries().any(records_audit_log)
    }

    /// Whether the account's refresh token ([`TokenFile::refresh_token`]) is that of an entry
    /// that records the audit-log grant: an authorization that was granted the audit log issued
    /// it, so a refresh with it brings a token for the audit log.
    pub(crate) fn refresh_token_has_audit_log(&self) -> bool {
        let Some(refresh_token) = self.refresh_token() else {
            return false;
        };
        self.user_entries().any(|entry| {
            records_audit_log(entry) && entry.token.refresh_token.as_deref() == Some(refresh_token)
        })
    }

    /// [`TokenFile::audit_log_granted`] of the tokens file at `path`; `false` when it is missing
    /// or cannot be read.
    pub(crate) async fn audit_log_granted_at(path: &Path) -> bool {
        TokenFile::load(path)
            .await
            .ok()
            .flatten()
            .is_some_and(|tokens| tokens.audit_log_granted())
    }

    /// [`TokenFile::set_for`] for an OAuth user account.
    #[cfg(test)]
    pub(crate) fn set(&mut self, scopes: &[&str], token: TokenInfo) {
        self.set_for(None, scopes, token);
    }

    /// Stores `token` of `credential` for exactly `scopes`, replacing the entry for the same
    /// credential and scope set.
    ///
    /// For an OAuth user account (no credential), `token` is one of the account's authorization
    /// ([`TokenFile::refresh_token`]); [`TokenStore`] makes sure of that. Without a refresh token
    /// it gets the authorization's: token responses to a refresh request normally carry none,
    /// and the stored one stays valid. A refresh token it brings becomes the authorization's,
    /// on every entry. Entries of an earlier authorization are dropped.
    ///
    /// Entries of other credentials are left alone while they are valid (a process that still
    /// has an earlier key of the account may use them) and dropped once they expired.
    pub(crate) fn set_for(
        &mut self,
        credential: Option<&str>,
        scopes: &[&str],
        mut token: TokenInfo,
    ) {
        self.entries
            .retain(|entry| entry.credential.as_deref() == credential || !entry.token.is_expired());

        if credential.is_none() {
            let current = self.refresh_token().map(str::to_owned);
            self.entries.retain(|entry| {
                entry.credential.is_some() || of_authorization(entry, current.as_deref())
            });
            match &token.refresh_token {
                None => token.refresh_token = current,
                Some(new) if current.as_deref() != Some(new.as_str()) => {
                    for entry in &mut self.entries {
                        if entry.credential.is_none() && entry.token.refresh_token.is_some() {
                            entry.token.refresh_token = Some(new.clone());
                        }
                    }
                }
                Some(_) => {}
            }
        }

        let index = self.entries.iter().position(|entry| {
            entry.credential.as_deref() == credential && same_scopes(&entry.scopes, scopes)
        });
        match index {
            Some(index) => self.entries[index].token = token,
            None => self.entries.push(TokenEntry {
                scopes: scopes.iter().map(|&scope| scope.to_owned()).collect(),
                token,
                credential: credential.map(str::to_owned),
            }),
        }
    }

    /// The tokens file after a new authorization of the account: only the new token, stored
    /// for `scopes`, because the new grant supersedes the old entries.
    ///
    /// Google omits the refresh token when the user had already granted offline access to the
    /// client (#150, #51). The new token then keeps the refresh token of `previous`, which
    /// stays valid, instead of being stored with `refresh_token: null`. `previous` must be the
    /// tokens of the same account and OAuth client.
    pub(crate) fn after_login(
        previous: Option<&TokenFile>,
        scopes: &[&str],
        mut token: TokenInfo,
    ) -> TokenFile {
        if token.refresh_token.is_none() {
            token.refresh_token = previous
                .and_then(TokenFile::refresh_token)
                .map(str::to_owned);
        }
        TokenFile {
            entries: vec![TokenEntry {
                scopes: scopes.iter().map(|&scope| scope.to_owned()).collect(),
                token,
                credential: None,
            }],
        }
    }

    /// Reads `path`. A missing file is `Ok(None)`.
    pub(crate) async fn load(path: &Path) -> Result<Option<Self>, LoadError> {
        let content = match tokio::fs::read(path).await {
            Ok(content) => content,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LoadError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };

        Self::parse(path, &content).map(Some)
    }

    /// [`TokenFile::load`], blocking the thread: for a change under the account lock.
    pub(crate) fn read(path: &Path) -> Result<Option<Self>, LoadError> {
        match std::fs::read(path) {
            Ok(content) => Self::parse(path, &content).map(Some),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(LoadError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Neither the content nor the `serde_json` error, which can quote it: it holds the tokens.
    fn parse(path: &Path, content: &[u8]) -> Result<Self, LoadError> {
        Self::from_json(content).map_err(|error| LoadError::Parse {
            path: path.to_path_buf(),
            error: RedactedJsonError::from(&error),
        })
    }

    /// Replaces the tokens file of the account whose lock is held, atomically: writes a
    /// temporary file next to it (mode 0600 on Unix) and renames it over the file. Temporary
    /// files that a killed writer left behind are removed first; under the lock no other one is
    /// being written.
    pub(crate) fn write_locked(&self, held: &Held<'_>) -> io::Result<()> {
        let path = held.account().tokens_path();
        let json = self.to_json().map_err(io::Error::other)?;
        remove_stale_temp_files(&path);
        replace_file(&path, json.as_bytes(), write_and_sync)
    }

    /// Replaces `path`, the tokens file of an account, under the lock of the account. For tests:
    /// gdrive writes tokens only inside a change that holds the lock ([`TokenFile::write_locked`]).
    #[cfg(test)]
    pub(crate) async fn save(&self, path: &Path) -> io::Result<()> {
        let file = self.clone();
        AccountLock::of_tokens_file(path)?
            .hold_blocking(LOCK_TIMEOUT, move |held| file.write_locked(held))
            .await?
    }
}

/// Replaces `path` atomically with `content`, like [`TokenFile::write_locked`]: the file is
/// created with mode 0600 on Unix from the start, never made private after the fact. For other
/// files that hold credentials, such as a service account key.
pub(crate) async fn write_private_file(path: &Path, content: &[u8]) -> io::Result<()> {
    let (path, content) = (path.to_path_buf(), content.to_vec());
    tokio::task::spawn_blocking(move || replace_file(&path, &content, write_and_sync))
        .await
        .map_err(io::Error::other)?
}

/// [`write_private_file`], blocking the thread: for a change under the account lock.
pub(crate) fn write_private_file_blocking(path: &Path, content: &[u8]) -> io::Result<()> {
    replace_file(path, content, write_and_sync)
}

/// Replaces `path` with `content`. `write` fills a new temporary file next to `path`, which is
/// renamed over `path` only when `write` succeeded. On any error the temporary file is removed
/// and `path` keeps its previous content.
fn replace_file<W>(path: &Path, content: &[u8], write: W) -> io::Result<()>
where
    W: FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
{
    let (temp_path, mut file) = create_temp_file(path)?;
    let written = write(&mut file, content);
    // Closed before the rename, which Windows refuses for an open file.
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Writes all of `content` to `file` and makes it durable. `write_all` reports the error of each
/// write (a full disk, ENOSPC), so an empty file never replaces a working tokens.json.
fn write_and_sync(file: &mut fs::File, content: &[u8]) -> io::Result<()> {
    file.write_all(content)?;
    file.sync_all()
}

/// Removes the temporary files of `path` ([`create_temp_file`]) that a writer left behind when
/// it was killed between writing and renaming one. Only under the account lock, which every
/// writer of `path` holds.
fn remove_stale_temp_files(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(OsStr::to_str)) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let is_temp = file_name
            .to_str()
            .and_then(|file_name| file_name.strip_prefix(name)?.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .is_some_and(|suffix| {
                suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            });
        if is_temp {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Whether `entry` is stored for a scope set that includes [`AUDIT_LOG_SCOPE`].
fn records_audit_log(entry: &TokenEntry) -> bool {
    entry.scopes.iter().any(|scope| scope == AUDIT_LOG_SCOPE)
}

/// Whether `entry` of an OAuth user account belongs to the authorization whose refresh token is
/// `refresh_token`: it has that refresh token, or none at all.
fn of_authorization(entry: &TokenEntry, refresh_token: Option<&str>) -> bool {
    entry
        .token
        .refresh_token
        .as_deref()
        .is_none_or(|own| Some(own) == refresh_token)
}

fn covers(stored: &[String], requested: &[&str]) -> bool {
    requested
        .iter()
        .all(|requested| stored.iter().any(|stored| stored == requested))
}

fn same_scopes(stored: &[String], requested: &[&str]) -> bool {
    covers(stored, requested)
        && stored
            .iter()
            .all(|stored| requested.iter().any(|requested| stored == requested))
}

/// How many random names `create_temp_file` tries before it gives up.
const TEMP_FILE_ATTEMPTS: usize = 8;

/// Creates a new temporary file next to `path`, named `<file name>.<random>.tmp`.
///
/// The file is created exclusively (`create_new`): an existing file is never opened and a
/// symlink is never followed. With a random name, two gdrive processes that share the config
/// directory never write into the same file, even when they have the same PID (PID 1 in two
/// containers). A name that exists already is skipped.
fn create_temp_file(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let random = SystemRandom::new();
    let suffixes = std::iter::repeat_with(|| {
        let mut bytes = [0; 8];
        random
            .fill(&mut bytes)
            .map_err(|_| io::Error::other("unable to get random bytes from the system"))?;
        Ok(format!("{:016x}", u64::from_le_bytes(bytes)))
    });
    create_temp_file_named(path, suffixes)
}

fn create_temp_file_named(
    path: &Path,
    suffixes: impl Iterator<Item = io::Result<String>>,
) -> io::Result<(PathBuf, fs::File)> {
    let mut clash = None;
    for suffix in suffixes.take(TEMP_FILE_ATTEMPTS) {
        let mut name = path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(format!(".{}.tmp", suffix?));
        let temp_path = path.with_file_name(name);

        match open_new_file(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => clash = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(clash.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
}

fn open_new_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// `tokens.json` exposed to yup-oauth2 as its token storage.
///
/// A refreshed token is merged into the file on disk, so that the updates of other gdrive
/// processes are kept, but only while that file is of the authorization this store refreshes
/// with: its [`TokenFile::refresh_token`] is the refresh token of this store. Once the account
/// was authorized again (`account add`, `account reauth`), the file belongs to the new
/// authorization, and this store keeps its tokens in memory for the rest of the process. Its
/// refresh token may lack scopes the new authorization has, such as the audit log, or be
/// revoked, so neither it nor a token refreshed with it may end up in the new file. Nor does
/// the store write anything once the account was removed.
///
/// The read, the check and the write of an update are one change under the lock of the account
/// ([`AccountLock`]), run on a blocking thread so that it ends as one even if yup-oauth2's
/// future is dropped. When another process holds the lock for longer than [`LOCK_TIMEOUT`], the
/// refreshed token is kept in memory and the command goes on.
pub(crate) struct TokenStore {
    lock: AccountLock,
    state: Mutex<StoreState>,
    /// The credential of a service account whose tokens these are; see [`TokenEntry`].
    credential: Option<String>,
    lock_timeout: Duration,
    #[cfg(test)]
    pause: Option<test_hooks::Pause>,
}

struct StoreState {
    /// The tokens this process works with: the file as it read it last, with its own updates.
    file: TokenFile,
    /// The refresh token of the authorization whose file this store may write; stored with
    /// refreshed tokens that come without one. It starts as the one the authenticator
    /// refreshes with, and follows a refresh token that a refresh answer brought once that is
    /// in the file.
    refresh_token: Option<String>,
}

impl TokenStore {
    /// The tokens of an OAuth user account, in the tokens file of `lock`'s account.
    pub(crate) fn new(lock: AccountLock, file: TokenFile, refresh_token: Option<String>) -> Self {
        TokenStore {
            lock,
            state: Mutex::new(StoreState {
                file,
                refresh_token,
            }),
            credential: None,
            lock_timeout: LOCK_TIMEOUT,
            #[cfg(test)]
            pause: None,
        }
    }

    /// The tokens of the service account credential `credential`: only its entries are read,
    /// and its tokens are stored with it.
    pub(crate) fn for_credential(lock: AccountLock, file: TokenFile, credential: String) -> Self {
        TokenStore {
            credential: Some(credential),
            ..TokenStore::new(lock, file, None)
        }
    }

    #[cfg(test)]
    fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    /// Stops every update after it read and checked the file under the lock, before it writes.
    #[cfg(test)]
    fn with_pause(mut self, pause: test_hooks::Pause) -> Self {
        self.pause = Some(pause);
        self
    }
}

/// Stores `token` for `scopes` in `file`. Not a token asked for the audit log of an account that
/// was not granted it: yup-oauth2 refreshes and stores a token for any scope set it is asked
/// for, and such an entry would claim a grant Google never gave (a refresh brings only the scopes
/// of the grant). Returns whether it was stored.
fn store_in(
    credential: Option<&str>,
    file: &mut TokenFile,
    scopes: &[&str],
    token: TokenInfo,
) -> bool {
    if credential.is_none() && scopes.contains(&AUDIT_LOG_SCOPE) && !file.audit_log_granted() {
        return false;
    }
    file.set_for(credential, scopes, token);
    true
}

/// A refreshed token to store: one change of the account's files under its lock, which
/// [`TokenStore::set`] runs on a blocking thread.
struct Refresh {
    lock: AccountLock,
    credential: Option<String>,
    /// The refresh token of the authorization whose file the store may write.
    identity: Option<String>,
    /// The tokens of the process.
    memory: TokenFile,
    scopes: Vec<String>,
    token: TokenInfo,
    timeout: Duration,
    #[cfg(test)]
    pause: Option<test_hooks::Pause>,
}

impl Refresh {
    /// The tokens of the process after the change, and whether the file was written.
    fn run(self) -> (TokenFile, io::Result<bool>) {
        let mut memory = self.memory;
        let credential = self.credential.as_deref();
        let identity = self.identity.as_deref();
        let scopes: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
        let token = self.token;
        let fallback = token.clone();
        #[cfg(test)]
        let pause = self.pause;

        let changed = self.lock.hold(self.timeout, |held| {
            // The account was removed: write nothing, and do not create its directory again.
            if !held.account().account_dir().is_dir() {
                store_in(credential, &mut memory, &scopes, token);
                return Ok(false);
            }
            let superseded = match TokenFile::read(&held.account().tokens_path()) {
                // Merge into what is on disk now, so that the update of another gdrive process
                // that ran in the meantime is kept; but not into the file of a newer
                // authorization.
                Ok(Some(on_disk))
                    if credential.is_none() && on_disk.refresh_token() != identity =>
                {
                    true
                }
                Ok(Some(on_disk)) => {
                    memory = on_disk;
                    false
                }
                // No tokens yet (a service account gets its tokens.json with its first token),
                // or unreadable ones: the tokens of this process are written.
                Ok(None) | Err(_) => false,
            };
            if !store_in(credential, &mut memory, &scopes, token) {
                return Ok(false);
            }
            #[cfg(test)]
            if let Some(pause) = &pause {
                pause.stop();
            }
            if superseded {
                return Ok(false);
            }
            memory.write_locked(held).map(|()| true)
        });

        let result = match changed {
            Ok(result) => result,
            // Another process is changing the account's files, or the config root was removed:
            // keep the token for this process only.
            Err(LockError::Timeout { .. }) => {
                store_in(credential, &mut memory, &scopes, fallback);
                Ok(false)
            }
            Err(LockError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                store_in(credential, &mut memory, &scopes, fallback);
                Ok(false)
            }
            Err(error) => Err(error.into()),
        };
        (memory, result)
    }
}

#[async_trait::async_trait]
impl TokenStorage for TokenStore {
    async fn set(&self, scopes: &[&str], mut token: TokenInfo) -> Result<(), TokenStorageError> {
        // yup-oauth2 also hands over the "token" of an answer without one (`{}` with a 503): it
        // must not replace a working token. Nor is a service account token without an expiry
        // cached: it would never be renewed.
        let no_access_token = token.access_token.as_deref().is_none_or(str::is_empty);
        if no_access_token || (self.credential.is_some() && token.expires_at.is_none()) {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        // A refresh token in the answer replaces the authorization's (Google does not do that
        // for this flow today). This store then writes the file of the new one, once it is in
        // the file. yup-oauth2's authenticator keeps refreshing with the refresh token it was
        // built with for the rest of the process; should Google stop accepting that one, the
        // process fails with invalid_grant at its next refresh, and the next one loads the new
        // refresh token from the file.
        let brought = token.refresh_token.clone();
        if token.refresh_token.is_none() {
            token.refresh_token.clone_from(&state.refresh_token);
        }
        let refresh = Refresh {
            lock: self.lock.clone(),
            credential: self.credential.clone(),
            identity: state.refresh_token.clone(),
            memory: state.file.clone(),
            scopes: scopes.iter().map(|&scope| scope.to_owned()).collect(),
            token,
            timeout: self.lock_timeout,
            #[cfg(test)]
            pause: self.pause.clone(),
        };

        // On a blocking thread: should this future be dropped (the refresh timeout of
        // `StoredCredentials`, Ctrl-C), the change still runs to its end under the lock.
        let (memory, result) = match tokio::task::spawn_blocking(move || refresh.run()).await {
            Ok(outcome) => outcome,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => return Err(TokenStorageError::Io(io::Error::other(error))),
        };
        state.file = memory;
        match result {
            Ok(true) => {
                if brought.is_some() {
                    state.refresh_token = brought;
                }
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(error) => Err(TokenStorageError::Io(error)),
        }
    }

    async fn get(&self, scopes: &[&str]) -> Option<TokenInfo> {
        self.state
            .lock()
            .await
            .file
            .get_for(self.credential.as_deref(), scopes)
            .cloned()
    }
}

#[derive(Debug)]
pub(crate) enum LoadError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        error: RedactedJsonError,
    },
}

impl Display for LoadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Read { path, .. } => write!(f, "unable to read '{}'", path.display()),
            LoadError::Parse { path, error } => {
                write!(
                    f,
                    "'{}' is not a valid tokens file ({error})",
                    path.display()
                )
            }
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            LoadError::Read { source, .. } => Some(source),
            LoadError::Parse { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::{
        app_config::AppConfig,
        auth::{
            lock::test_hooks::{when_waiting, within, Elsewhere, Pause},
            DRIVE_SCOPES,
        },
    };

    const DRIVE: &str = "https://www.googleapis.com/auth/drive";
    const METADATA: &str = "https://www.googleapis.com/auth/drive.metadata.readonly";
    const AUDIT: &str = "https://www.googleapis.com/auth/admin.reports.audit.readonly";
    /// The scopes of a login with `--audit-log`.
    const DRIVE_AND_AUDIT: [&str; 3] = [DRIVE, METADATA, AUDIT];

    fn token(access: &str, refresh: Option<&str>, expires_in: Duration) -> TokenInfo {
        TokenInfo {
            access_token: Some(access.to_owned()),
            refresh_token: refresh.map(str::to_owned),
            expires_at: Some(OffsetDateTime::now_utc() + expires_in),
            id_token: None,
        }
    }

    fn file(entries: &[(&[&str], TokenInfo)]) -> TokenFile {
        TokenFile {
            entries: entries
                .iter()
                .map(|(scopes, token)| TokenEntry {
                    scopes: scopes.iter().map(|&s| s.to_owned()).collect(),
                    token: token.clone(),
                    credential: None,
                })
                .collect(),
        }
    }

    /// A tokens.json as gdrive 3.9 (yup-oauth2 disk storage) wrote it, with redacted values.
    const GDRIVE_39_TOKENS: &str = r#"[{"scopes":["https://www.googleapis.com/auth/drive","https://www.googleapis.com/auth/drive.metadata.readonly"],"token":{"access_token":"ya29.redacted","refresh_token":"1//redacted","expires_at":[2023,120,10,22,33,123456789,0,0,0],"id_token":null}}]"#;

    #[test]
    fn reads_the_gdrive_39_layout_and_writes_it_back_unchanged() {
        let file = TokenFile::from_json(GDRIVE_39_TOKENS.as_bytes()).unwrap();
        assert_eq!(file.entries().len(), 1);
        assert_eq!(file.entries()[0].scopes, DRIVE_SCOPES);
        assert_eq!(file.refresh_token(), Some("1//redacted"));
        assert!(file.get(&[METADATA]).unwrap().is_expired());

        assert_eq!(file.to_json().unwrap(), GDRIVE_39_TOKENS);
    }

    #[test]
    fn get_matches_supersets_and_prefers_unexpired_tokens() {
        let file = file(&[
            (&[METADATA], token("old", Some("r"), Duration::hours(-2))),
            (&DRIVE_SCOPES, token("new", Some("r"), Duration::hours(1))),
        ]);

        assert_eq!(
            file.get(&[METADATA]).unwrap().access_token.as_deref(),
            Some("new")
        );
        assert_eq!(
            file.get(&[DRIVE]).unwrap().access_token.as_deref(),
            Some("new")
        );
        assert!(file
            .get(&["https://www.googleapis.com/auth/gmail"])
            .is_none());
    }

    #[test]
    fn refresh_token_comes_from_the_latest_entry_that_has_one() {
        let file = file(&[
            (
                &DRIVE_SCOPES,
                token("a", Some("older"), Duration::hours(-5)),
            ),
            (&[METADATA], token("b", Some("newer"), Duration::hours(-1))),
            (&[DRIVE], token("c", None, Duration::hours(1))),
        ]);
        assert_eq!(file.refresh_token(), Some("newer"));

        let without = self::file(&[(&DRIVE_SCOPES, token("a", None, Duration::hours(1)))]);
        assert_eq!(without.refresh_token(), None);
    }

    #[test]
    fn tokens_of_other_credentials_are_never_read() {
        let mut file = TokenFile::default();
        file.set_for(
            Some("alice"),
            &[DRIVE],
            token("a", None, Duration::hours(1)),
        );
        file.set_for(Some("bob"), &[DRIVE], token("b", None, Duration::hours(1)));

        let access = |file: &TokenFile, credential| {
            file.get_for(credential, &[DRIVE])
                .and_then(|token| token.access_token.clone())
        };
        assert_eq!(access(&file, Some("alice")).as_deref(), Some("a"));
        assert_eq!(access(&file, Some("bob")).as_deref(), Some("b"));
        assert_eq!(access(&file, Some("carol")), None);
        // Nor by a user account, which has no credential.
        assert_eq!(access(&file, None), None);
        assert_eq!(file.refresh_token(), None);

        // Replacing alice's token leaves bob's alone.
        file.set_for(
            Some("alice"),
            &[DRIVE],
            token("a2", None, Duration::hours(1)),
        );
        assert_eq!(access(&file, Some("alice")).as_deref(), Some("a2"));
        assert_eq!(access(&file, Some("bob")).as_deref(), Some("b"));
        assert_eq!(file.entries().len(), 2);
    }

    #[test]
    fn expired_tokens_of_other_credentials_are_dropped_on_write() {
        let mut file = TokenFile::default();
        file.set_for(
            Some("old-key"),
            &[DRIVE],
            token("old", None, Duration::hours(-1)),
        );
        file.set_for(
            Some("other"),
            &[DRIVE],
            token("valid", None, Duration::hours(1)),
        );

        file.set_for(
            Some("new-key"),
            &[DRIVE],
            token("new", None, Duration::hours(1)),
        );

        let credentials: Vec<_> = file
            .entries()
            .iter()
            .map(|entry| entry.credential.as_deref())
            .collect();
        assert_eq!(credentials, [Some("other"), Some("new-key")]);
        let json = file.to_json().unwrap();
        assert!(json.contains(r#""credential":"new-key""#), "{json}");
    }

    #[test]
    fn set_keeps_the_stored_refresh_token_when_the_new_token_has_none() {
        let mut file = file(&[(
            &DRIVE_SCOPES,
            token("old", Some("keep"), Duration::hours(-1)),
        )]);

        // Same scope set, given in another order: the entry is replaced, not duplicated.
        file.set(&[METADATA, DRIVE], token("fresh", None, Duration::hours(1)));
        assert_eq!(file.entries().len(), 1);
        let stored = &file.entries()[0].token;
        assert_eq!(stored.access_token.as_deref(), Some("fresh"));
        assert_eq!(stored.refresh_token.as_deref(), Some("keep"));

        // A new scope set gets its own entry, still with the account's refresh token.
        file.set(&[METADATA], token("meta", None, Duration::hours(1)));
        assert_eq!(file.entries().len(), 2);
        assert_eq!(
            file.entries()[1].token.refresh_token.as_deref(),
            Some("keep")
        );

        // A token that brings its own refresh token wins.
        file.set(
            &DRIVE_SCOPES,
            token("rotated", Some("new"), Duration::hours(1)),
        );
        assert_eq!(
            file.entries()[0].token.refresh_token.as_deref(),
            Some("new")
        );
    }

    #[test]
    fn after_login_keeps_the_previous_refresh_token_when_google_omits_it() {
        let previous = TokenFile::from_json(GDRIVE_39_TOKENS.as_bytes()).unwrap();
        let reauthorized = TokenInfo {
            access_token: Some(String::from("ya29.new")),
            refresh_token: None,
            expires_at: Some(OffsetDateTime::new_utc(
                time::Date::from_ordinal_date(2026, 308).unwrap(),
                time::Time::from_hms(12, 0, 0).unwrap(),
            )),
            id_token: None,
        };

        let merged = TokenFile::after_login(Some(&previous), &DRIVE_SCOPES, reauthorized);
        assert_eq!(
            merged.to_json().unwrap(),
            r#"[{"scopes":["https://www.googleapis.com/auth/drive","https://www.googleapis.com/auth/drive.metadata.readonly"],"token":{"access_token":"ya29.new","refresh_token":"1//redacted","expires_at":[2026,308,12,0,0,0,0,0,0],"id_token":null}}]"#
        );
    }

    #[test]
    fn after_login_drops_old_entries_and_prefers_a_new_refresh_token() {
        let previous = file(&[
            (
                &DRIVE_SCOPES,
                token("old", Some("old-refresh"), Duration::hours(-1)),
            ),
            (
                &[METADATA],
                token("old-meta", Some("old-refresh"), Duration::minutes(30)),
            ),
        ]);

        let merged = TokenFile::after_login(
            Some(&previous),
            &DRIVE_SCOPES,
            token("new", Some("new-refresh"), Duration::hours(1)),
        );
        assert_eq!(merged.entries().len(), 1);
        assert_eq!(merged.entries()[0].scopes, DRIVE_SCOPES);
        assert_eq!(merged.refresh_token(), Some("new-refresh"));
        assert_eq!(
            merged.get(&[METADATA]).unwrap().access_token.as_deref(),
            Some("new")
        );
    }

    #[test]
    fn after_login_without_previous_tokens_stores_what_google_returned() {
        let merged =
            TokenFile::after_login(None, &DRIVE_SCOPES, token("new", None, Duration::hours(1)));
        assert_eq!(merged.entries().len(), 1);
        assert_eq!(merged.refresh_token(), None);
    }

    /// The tokens file of the account `me@example.com` in the config root `dir`, whose
    /// directory is created.
    fn tokens_path(dir: &Path) -> PathBuf {
        let account = dir.join("me@example.com");
        std::fs::create_dir_all(&account).unwrap();
        account.join("tokens.json")
    }

    fn lock(path: &Path) -> AccountLock {
        AccountLock::of_tokens_file(path).unwrap()
    }

    /// The names in the account directory of the tokens file `path`, in order.
    fn account_entries(path: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn access<'a>(file: &'a TokenFile, scopes: &[&str]) -> Option<&'a str> {
        file.get(scopes)?.access_token.as_deref()
    }

    #[test]
    fn the_audit_log_grant_is_recorded_by_the_login_and_kept_by_refreshes() {
        assert_eq!(AUDIT, crate::auth::AUDIT_LOG_SCOPE);
        let mut file = TokenFile::after_login(
            None,
            &DRIVE_AND_AUDIT,
            token("login", Some("r"), Duration::hours(1)),
        );
        assert_eq!(file.entries()[0].scopes, DRIVE_AND_AUDIT);
        assert!(file.audit_log_granted());
        // What the existing commands ask for (the Drive scopes, or one of them) is served by the
        // entry of the login, and so is the audit log.
        for scopes in [&DRIVE_SCOPES[..], &[DRIVE], &[METADATA], &[AUDIT]] {
            assert_eq!(access(&file, scopes), Some("login"), "{scopes:?}");
        }

        // The access token expires. The refresh for the Drive scopes, whose answer has no
        // refresh token, gets an entry of its own with the account's refresh token ...
        file.entries[0].token.expires_at = Some(OffsetDateTime::now_utc() - Duration::hours(1));
        file.set(&DRIVE_SCOPES, token("drive", None, Duration::hours(1)));
        assert_eq!(access(&file, &DRIVE_SCOPES), Some("drive"));
        assert_eq!(
            file.get(&DRIVE_SCOPES).unwrap().refresh_token.as_deref(),
            Some("r")
        );
        // ... and the entry of the grant stays, with its refresh token: the audit log is still
        // asked of it.
        assert!(file.audit_log_granted());
        let grant = file.get(&[AUDIT]).unwrap();
        assert_eq!(grant.access_token.as_deref(), Some("login"));
        assert!(grant.is_expired());
        assert_eq!(grant.refresh_token.as_deref(), Some("r"));

        // Refreshed for the audit log, then again for Drive: the grant is never lost.
        file.set(&[AUDIT], token("audit", None, Duration::hours(1)));
        file.set(&DRIVE_SCOPES, token("drive-2", None, Duration::hours(1)));
        assert!(file.audit_log_granted());
        assert_eq!(access(&file, &[AUDIT]), Some("audit"));
        assert_eq!(access(&file, &DRIVE_SCOPES), Some("drive-2"));
        assert_eq!(file.refresh_token(), Some("r"));
        assert_eq!(file.entries().len(), 3);
        assert!(file
            .entries()
            .iter()
            .all(|entry| entry.token.refresh_token.as_deref() == Some("r")));
    }

    #[tokio::test]
    async fn only_a_login_that_was_granted_the_audit_log_records_it() {
        let drive_only = TokenFile::after_login(
            None,
            &DRIVE_SCOPES,
            token("a", Some("r"), Duration::hours(1)),
        );
        assert!(!drive_only.audit_log_granted());
        assert!(drive_only.get(&[AUDIT]).is_none());

        // The next login replaces the grant: one without the audit log drops it.
        let previous = TokenFile::after_login(
            None,
            &DRIVE_AND_AUDIT,
            token("old", Some("r"), Duration::hours(1)),
        );
        let again = TokenFile::after_login(
            Some(&previous),
            &DRIVE_SCOPES,
            token("new", None, Duration::hours(1)),
        );
        assert!(!again.audit_log_granted());
        assert_eq!(again.refresh_token(), Some("r"));

        // Nor does a missing or invalid file.
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        assert!(!TokenFile::audit_log_granted_at(&path).await);
        tokio::fs::write(&path, "not json").await.unwrap();
        assert!(!TokenFile::audit_log_granted_at(&path).await);
        previous.save(&path).await.unwrap();
        assert!(TokenFile::audit_log_granted_at(&path).await);

        // The tokens of a service account never count.
        let mut service = TokenFile::default();
        service.set_for(
            Some("robot"),
            &DRIVE_AND_AUDIT,
            token("s", None, Duration::hours(1)),
        );
        assert!(!service.audit_log_granted());
    }

    #[tokio::test]
    async fn store_never_records_an_audit_log_grant_that_was_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        // What yup-oauth2 stores after a refresh it ran for a scope set that no entry covers.
        let store = TokenStore::new(lock(&path), loaded, Some(String::from("r1")));
        for scopes in [&[AUDIT][..], &DRIVE_AND_AUDIT] {
            store
                .set(scopes, token("refreshed", None, Duration::hours(1)))
                .await
                .unwrap();
        }
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert!(store.get(&[AUDIT]).await.is_none());

        // Drive tokens are stored as before.
        store
            .set(&DRIVE_SCOPES, token("fresh", None, Duration::hours(1)))
            .await
            .unwrap();
        let on_disk = TokenFile::load(&path).await.unwrap().unwrap();
        assert_eq!(access(&on_disk, &DRIVE_SCOPES), Some("fresh"));
        assert!(!on_disk.audit_log_granted());
        assert_eq!(on_disk.entries().len(), 1);
    }

    #[tokio::test]
    async fn store_caches_audit_log_tokens_while_the_account_has_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let loaded = TokenFile::after_login(
            None,
            &DRIVE_AND_AUDIT,
            token("login", Some("r1"), Duration::hours(-1)),
        );
        loaded.save(&path).await.unwrap();

        let store = TokenStore::new(lock(&path), loaded, Some(String::from("r1")));
        store
            .set(&[AUDIT], token("audit", None, Duration::hours(1)))
            .await
            .unwrap();
        let on_disk = TokenFile::load(&path).await.unwrap().unwrap();
        assert_eq!(access(&on_disk, &[AUDIT]), Some("audit"));
        assert_eq!(
            on_disk.get(&[AUDIT]).unwrap().refresh_token.as_deref(),
            Some("r1")
        );
        assert!(on_disk.audit_log_granted());

        // Another gdrive process authorized the account again without the audit log: the
        // token this process refreshed for it is not stored (nor would any other be).
        let reauthorized = TokenFile::after_login(
            None,
            &DRIVE_SCOPES,
            token("reauth", Some("r2"), Duration::hours(1)),
        );
        reauthorized.save(&path).await.unwrap();
        store
            .set(&[AUDIT], token("audit-2", None, Duration::hours(1)))
            .await
            .unwrap();
        assert_eq!(TokenFile::load(&path).await.unwrap().unwrap(), reauthorized);
    }

    fn refresh_tokens(file: &TokenFile) -> Vec<Option<&str>> {
        file.entries()
            .iter()
            .map(|entry| entry.token.refresh_token.as_deref())
            .collect()
    }

    #[test]
    fn the_refresh_token_is_that_of_the_authorization_that_records_the_grant() {
        // A file as an older process could leave it: the login with the audit log (r2), and
        // after it the Drive tokens that a process of the earlier, Drive-only authorization
        // (r1) refreshed, which expire later.
        let mut file = file(&[
            (
                &DRIVE_AND_AUDIT,
                token("login", Some("r2"), Duration::hours(-1)),
            ),
            (
                &DRIVE_SCOPES,
                token("old-1", Some("r1"), Duration::hours(1)),
            ),
            (&[DRIVE], token("old-2", Some("r1"), Duration::hours(2))),
        ]);
        assert_eq!(file.refresh_token(), Some("r2"));
        assert!(file.audit_log_granted());
        // The tokens of the earlier authorization are never used, valid as they are.
        for scopes in [&DRIVE_SCOPES[..], &[DRIVE], &[AUDIT]] {
            assert_eq!(access(&file, scopes), Some("login"), "{scopes:?}");
        }

        // The next write drops them.
        file.set(&DRIVE_SCOPES, token("fresh", None, Duration::hours(1)));
        assert_eq!(file.entries().len(), 2);
        assert_eq!(refresh_tokens(&file), [Some("r2"), Some("r2")]);
        assert_eq!(access(&file, &DRIVE_SCOPES), Some("fresh"));
        assert!(file.audit_log_granted());
    }

    #[test]
    fn entries_of_an_earlier_authorization_are_never_used() {
        let mut file = file(&[
            (&DRIVE_SCOPES, token("new", Some("r2"), Duration::hours(1))),
            (&[METADATA], token("old", Some("r1"), Duration::minutes(30))),
            // gdrive 3.x stored refreshed tokens without a refresh token: those count.
            (&[DRIVE], token("legacy", None, Duration::hours(2))),
        ]);
        assert_eq!(file.refresh_token(), Some("r2"));
        assert_eq!(access(&file, &[METADATA]), Some("new"));
        assert_eq!(access(&file, &[DRIVE]), Some("legacy"));

        file.set(&[METADATA], token("meta", None, Duration::hours(1)));
        let scopes: Vec<_> = file.entries().iter().map(|e| e.scopes.len()).collect();
        assert_eq!(scopes, [2, 1, 1]);
        assert_eq!(refresh_tokens(&file), [Some("r2"), None, Some("r2")]);
        assert_eq!(access(&file, &[METADATA]), Some("meta"));
    }

    #[test]
    fn a_refresh_token_in_a_refresh_answer_becomes_the_authorizations() {
        let mut file = TokenFile::after_login(
            None,
            &DRIVE_AND_AUDIT,
            token("login", Some("r1"), Duration::hours(1)),
        );
        file.set(&DRIVE_SCOPES, token("drive", None, Duration::hours(1)));
        file.set(&[AUDIT], token("rotated", Some("r2"), Duration::hours(1)));

        assert_eq!(file.refresh_token(), Some("r2"));
        assert_eq!(refresh_tokens(&file), [Some("r2"); 3]);
        assert!(file.audit_log_granted());
        assert_eq!(access(&file, &DRIVE_SCOPES), Some("drive"));
    }

    #[tokio::test]
    async fn store_never_writes_into_the_file_of_a_newer_authorization() {
        // (scopes of the authorization the process loaded, of the login that replaced it,
        // asked of the store)
        let cases: [(&[&str], &[&str], &[&str]); 5] = [
            (&DRIVE_SCOPES, &DRIVE_AND_AUDIT, &DRIVE_SCOPES),
            (&DRIVE_SCOPES, &DRIVE_SCOPES, &[DRIVE]),
            (&DRIVE_AND_AUDIT, &DRIVE_SCOPES, &DRIVE_SCOPES),
            (&DRIVE_AND_AUDIT, &DRIVE_SCOPES, &[AUDIT]),
            (&DRIVE_AND_AUDIT, &DRIVE_AND_AUDIT, &DRIVE_AND_AUDIT),
        ];
        for (loaded_scopes, login_scopes, asked) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = tokens_path(dir.path());
            let loaded = TokenFile::after_login(
                None,
                loaded_scopes,
                token("old", Some("r1"), Duration::hours(-1)),
            );
            loaded.save(&path).await.unwrap();
            let store = TokenStore::new(lock(&path), loaded, Some(String::from("r1")));

            // The account is authorized again: with a new refresh token, or with none at all.
            for new_refresh_token in [Some("r2"), None] {
                let login = TokenFile::after_login(
                    None,
                    login_scopes,
                    token("login", new_refresh_token, Duration::hours(1)),
                );
                login.save(&path).await.unwrap();
                #[cfg(unix)]
                let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap());

                store
                    .set(asked, token("refreshed", None, Duration::hours(1)))
                    .await
                    .unwrap();

                let case = format!("{loaded_scopes:?} -> {login_scopes:?}, {asked:?}");
                assert_eq!(TokenFile::load(&path).await.unwrap(), Some(login), "{case}");
                #[cfg(unix)]
                assert_eq!(
                    std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap()),
                    inode,
                    "{case}: the file was written"
                );
                // The process goes on with what it refreshed.
                let kept = store.get(asked).await.unwrap();
                assert_eq!(kept.access_token.as_deref(), Some("refreshed"), "{case}");
                assert_eq!(kept.refresh_token.as_deref(), Some("r1"), "{case}");
            }
        }
    }

    #[tokio::test]
    async fn store_writes_into_the_file_of_a_login_that_kept_its_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();
        let store = TokenStore::new(lock(&path), loaded.clone(), Some(String::from("r1")));

        // `account reauth --audit-log`, and Google left out the refresh token: the login kept
        // r1, so this is still the authorization the process refreshes with.
        let login = TokenFile::after_login(
            Some(&loaded),
            &DRIVE_AND_AUDIT,
            token("login", None, Duration::hours(-1)),
        );
        login.save(&path).await.unwrap();
        store
            .set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)))
            .await
            .unwrap();

        let on_disk = TokenFile::load(&path).await.unwrap().unwrap();
        assert!(on_disk.audit_log_granted());
        assert_eq!(access(&on_disk, &DRIVE_SCOPES), Some("refreshed"));
        assert_eq!(refresh_tokens(&on_disk), [Some("r1"), Some("r1")]);
    }

    /// (scopes of the authorization a process loaded, of the login that replaces it)
    const REAUTHORIZATIONS: [(&[&str], &[&str]); 2] = [
        (&DRIVE_SCOPES, &DRIVE_AND_AUDIT),
        (&DRIVE_AND_AUDIT, &DRIVE_SCOPES),
    ];

    #[tokio::test]
    async fn a_login_waits_for_a_refresh_that_already_passed_its_check() {
        for (loaded_scopes, login_scopes) in REAUTHORIZATIONS {
            let dir = tempfile::tempdir().unwrap();
            let path = tokens_path(dir.path());
            let loaded = TokenFile::after_login(
                None,
                loaded_scopes,
                token("old", Some("r1"), Duration::hours(-1)),
            );
            loaded.save(&path).await.unwrap();
            let pause = Pause::default();
            let store = Arc::new(
                TokenStore::new(lock(&path), loaded, Some(String::from("r1")))
                    .with_pause(pause.clone()),
            );

            // The older process has read the file and checked it, and stops before it writes.
            let refresh = tokio::spawn({
                let store = Arc::clone(&store);
                async move {
                    store
                        .set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)))
                        .await
                }
            });
            within("the check of the refresh", pause.reached.notified()).await;

            // Now the login saves: it has to wait ...
            let waiting = when_waiting(&path);
            let login = TokenFile::after_login(
                None,
                login_scopes,
                token("login", Some("r2"), Duration::hours(1)),
            );
            let save = tokio::spawn({
                let (login, path) = (login.clone(), path.clone());
                async move { login.save(&path).await }
            });
            within("the wait of the login", waiting.notified()).await;

            // ... until the refresh wrote, into the file of its own authorization.
            pause.resume.notify_one();
            within("the refresh", refresh).await.unwrap().unwrap();
            within("the save of the login", save)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                TokenFile::load(&path).await.unwrap(),
                Some(login),
                "{loaded_scopes:?} -> {login_scopes:?}"
            );

            // The next refresh of the older process finds the new file and writes nothing.
            let before = tokio::fs::read(&path).await.unwrap();
            pause.resume.notify_one();
            store
                .set(&DRIVE_SCOPES, token("later", None, Duration::hours(1)))
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        }
    }

    #[tokio::test]
    async fn a_refresh_waits_for_a_login_that_holds_the_lock_and_then_leaves_its_file_alone() {
        for (loaded_scopes, login_scopes) in REAUTHORIZATIONS {
            let dir = tempfile::tempdir().unwrap();
            let path = tokens_path(dir.path());
            let loaded = TokenFile::after_login(
                None,
                loaded_scopes,
                token("old", Some("r1"), Duration::hours(-1)),
            );
            loaded.save(&path).await.unwrap();
            let store = TokenStore::new(lock(&path), loaded.clone(), Some(String::from("r1")));
            let login = TokenFile::after_login(
                Some(&loaded),
                login_scopes,
                token("login", Some("r2"), Duration::hours(1)),
            );

            // The login holds the lock: it is about to write.
            let elsewhere = Elsewhere::hold(lock(&path), {
                let login = login.clone();
                move |held| login.write_locked(held).unwrap()
            })
            .await;

            // The older process refreshes now: it waits for the login ...
            let waiting = when_waiting(&path);
            let refresh = store.set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)));
            let login_finishes = async {
                within("the wait of the refresh", waiting.notified()).await;
                elsewhere.finish().await;
            };
            let (result, ()) = tokio::join!(refresh, login_finishes);
            result.unwrap();

            // ... and then finds the file of the new authorization: it keeps its token to itself.
            let case = format!("{loaded_scopes:?} -> {login_scopes:?}");
            assert_eq!(TokenFile::load(&path).await.unwrap(), Some(login), "{case}");
            let kept = store.get(&DRIVE_SCOPES).await.unwrap();
            assert_eq!(kept.access_token.as_deref(), Some("refreshed"), "{case}");
        }
    }

    #[tokio::test]
    async fn a_refresh_whose_caller_is_dropped_still_finishes_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();
        let pause = Pause::default();
        let store = Arc::new(
            TokenStore::new(lock(&path), loaded, Some(String::from("r1")))
                .with_pause(pause.clone()),
        );

        // The refresh read and checked the file under the lock; then its caller goes away, as on
        // the refresh timeout of `StoredCredentials` or Ctrl-C.
        let caller = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                store
                    .set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)))
                    .await
            }
        });
        within("the check of the refresh", pause.reached.notified()).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());

        // The change goes on under the lock: an operation that starts now waits for it ...
        let waiting = when_waiting(&path);
        let reader = tokio::spawn(lock(&path).hold_blocking(LOCK_TIMEOUT, |held| {
            TokenFile::read(&held.account().tokens_path())
                .unwrap()
                .unwrap()
        }));
        within("the wait of the other operation", waiting.notified()).await;
        pause.resume.notify_one();

        // ... and then sees the whole of it: the refreshed token, in the file of the account.
        let seen = within("the other operation", reader)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(access(&seen, &DRIVE_SCOPES), Some("refreshed"));
        assert_eq!(seen.refresh_token(), Some("r1"));
        assert_eq!(account_entries(&path), ["tokens.json"]);
    }

    #[tokio::test]
    async fn removing_and_adding_an_account_again_keeps_one_lock() {
        let dir = tempfile::tempdir().unwrap();
        let app_cfg =
            AppConfig::named_account_in(dir.path().to_path_buf(), "me@example.com").unwrap();
        let path = app_cfg.tokens_path().to_path_buf();
        std::fs::create_dir_all(app_cfg.account_base_path()).unwrap();
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();
        let pause = Pause::default();
        let store = Arc::new(
            TokenStore::new(lock(&path), loaded, Some(String::from("r1")))
                .with_pause(pause.clone()),
        );

        // A refresh has read and checked the file, and stops before it writes.
        let refresh = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                store
                    .set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)))
                    .await
            }
        });
        within("the check of the refresh", pause.reached.notified()).await;

        // `account remove` waits for it, and then removes the directory.
        let waiting = when_waiting(&path);
        let remove = tokio::task::spawn_blocking({
            let app_cfg = app_cfg.clone();
            move || app_cfg.remove_account_dir(LOCK_TIMEOUT)
        });
        within("the wait of the removal", waiting.notified()).await;
        pause.resume.notify_one();
        within("the refresh", refresh).await.unwrap().unwrap();
        within("the removal", remove).await.unwrap().unwrap();
        assert!(!app_cfg.account_base_path().exists());

        // `account add` again, with a new authorization, under the same lock.
        let login = TokenFile::after_login(
            None,
            &DRIVE_AND_AUDIT,
            token("login", Some("r2"), Duration::hours(1)),
        );
        lock(&path)
            .hold_blocking(LOCK_TIMEOUT, {
                let (app_cfg, login) = (app_cfg.clone(), login.clone());
                move |held| {
                    let secret = crate::app_config::Secret {
                        client_id: String::from("id"),
                        client_secret: String::from("secret"),
                    };
                    crate::app_config::add_account(&app_cfg, &secret, &login, held)
                }
            })
            .await
            .unwrap()
            .unwrap();

        // The older process refreshes again: it finds the new file and writes nothing.
        pause.resume.notify_one();
        store
            .set(&DRIVE_SCOPES, token("later", None, Duration::hours(1)))
            .await
            .unwrap();
        assert_eq!(TokenFile::load(&path).await.unwrap(), Some(login));
        assert_eq!(account_entries(&path), ["secret.json", "tokens.json"]);
    }

    #[tokio::test]
    async fn a_refresh_of_a_removed_account_writes_nothing_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("config");
        let path = tokens_path(&base);
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();
        let store = TokenStore::new(lock(&path), loaded, Some(String::from("r1")));

        // The account directory is removed, then the whole config root.
        for removed in [path.parent().unwrap(), &base] {
            std::fs::remove_dir_all(removed).unwrap();
            store
                .set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)))
                .await
                .unwrap();
            assert!(!removed.exists(), "{}", removed.display());
            // The process goes on with what it refreshed.
            let kept = store.get(&DRIVE_SCOPES).await.unwrap();
            assert_eq!(kept.access_token.as_deref(), Some("refreshed"));
        }
    }

    #[tokio::test]
    async fn an_operation_that_cannot_get_the_lock_in_time_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let app_cfg =
            AppConfig::named_account_in(dir.path().to_path_buf(), "me@example.com").unwrap();
        let path = app_cfg.tokens_path().to_path_buf();
        std::fs::create_dir_all(app_cfg.account_base_path()).unwrap();
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();
        let store = TokenStore::new(lock(&path), loaded, Some(String::from("r1")))
            .with_lock_timeout(std::time::Duration::from_millis(50));
        let short = std::time::Duration::from_millis(50);

        // Another process holds the lock all along.
        let elsewhere = Elsewhere::hold(lock(&path), |_| ()).await;

        // A refresh keeps its token in memory and goes on.
        store
            .set(&DRIVE_SCOPES, token("refreshed", None, Duration::hours(1)))
            .await
            .unwrap();
        let kept = store.get(&DRIVE_SCOPES).await.unwrap();
        assert_eq!(kept.access_token.as_deref(), Some("refreshed"));

        // A login fails, as a login reports it.
        let login = file(&[(
            &DRIVE_SCOPES,
            token("login", Some("r2"), Duration::hours(1)),
        )]);
        let error = lock(&path)
            .hold_blocking(short, move |held| login.write_locked(held))
            .await
            .unwrap_err();
        assert!(matches!(error, LockError::Timeout { .. }), "{error:?}");
        let error = io::Error::from(error);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let expected = "another gdrive process is updating the files of account \
                        'me@example.com'; nothing was changed, try again in a moment";
        assert_eq!(crate::error_message(&error), format!("Error: {expected}"));

        // `account remove` fails the same way.
        let error = app_cfg.remove_account_dir(short).unwrap_err();
        let message = crate::error_message(&error);
        assert!(message.ends_with(expected), "{message}");
        assert!(app_cfg.account_base_path().exists());
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);

        // Once the lock is free, the refresh writes again.
        elsewhere.finish().await;
        store
            .set(&DRIVE_SCOPES, token("next", None, Duration::hours(1)))
            .await
            .unwrap();
        let on_disk = TokenFile::load(&path).await.unwrap().unwrap();
        assert_eq!(access(&on_disk, &DRIVE_SCOPES), Some("next"));
    }

    #[tokio::test]
    async fn save_replaces_the_file_atomically_and_load_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        assert_eq!(TokenFile::load(&path).await.unwrap(), None);

        tokio::fs::write(&path, "stale content").await.unwrap();
        let expected = file(&[(&DRIVE_SCOPES, token("a", Some("r"), Duration::hours(1)))]);
        expected.save(&path).await.unwrap();

        assert_eq!(
            TokenFile::load(&path).await.unwrap(),
            Some(expected.clone())
        );
        assert_eq!(TokenFile::read(&path).unwrap(), Some(expected));
        assert_eq!(account_entries(&path), ["tokens.json"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn a_write_removes_the_temporary_files_a_killed_writer_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let account = path.parent().unwrap();
        for name in [
            "tokens.json.0123456789abcdef.tmp",
            "tokens.json.other.tmp",
            "secret.json.0123456789abcdef.tmp",
        ] {
            std::fs::write(account.join(name), "half").unwrap();
        }

        file(&[(&DRIVE_SCOPES, token("a", Some("r"), Duration::hours(1)))])
            .save(&path)
            .await
            .unwrap();
        assert_eq!(
            account_entries(&path),
            [
                "secret.json.0123456789abcdef.tmp",
                "tokens.json",
                "tokens.json.other.tmp"
            ]
        );
    }

    #[tokio::test]
    async fn failed_write_keeps_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let previous = file(&[(&DRIVE_SCOPES, token("a", Some("keep"), Duration::hours(1)))]);
        previous.save(&path).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        // The disk fills up halfway through the new content.
        let new_content = br#"[{"scopes":[],"token":{}}]"#;
        let error = replace_file(&path, new_content, |file: &mut fs::File, content: &[u8]| {
            file.write_all(&content[..content.len() / 2])?;
            Err(io::Error::from(io::ErrorKind::StorageFull))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);

        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert_eq!(account_entries(&path), ["tokens.json"]);
    }

    #[test]
    fn temp_files_never_reuse_an_existing_name_or_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let account = path.parent().unwrap();
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| Ok(name.to_string()))
                .collect::<Vec<_>>()
                .into_iter()
        };

        // Another process's temporary file.
        let other = account.join("tokens.json.aaaa.tmp");
        std::fs::write(&other, "other process").unwrap();
        // A symlink planted where a temporary file would go.
        let victim = account.join("victim");
        std::fs::write(&victim, "victim").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, account.join("tokens.json.bbbb.tmp")).unwrap();

        let (temp_path, mut file) =
            create_temp_file_named(&path, names(&["aaaa", "bbbb", "cccc"])).unwrap();
        file.write_all(b"mine").unwrap();
        drop(file);

        let expected = if cfg!(unix) { "cccc" } else { "bbbb" };
        assert_eq!(
            temp_path,
            account.join(format!("tokens.json.{expected}.tmp"))
        );
        assert_eq!(std::fs::read(&other).unwrap(), b"other process");
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim");

        let error = create_temp_file_named(&path, names(&["aaaa"])).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn concurrent_saves_do_not_mix_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let files: Vec<_> = (0..16)
            .map(|i| {
                let access = format!("access-{i}-{}", "x".repeat(4096));
                file(&[(&DRIVE_SCOPES, token(&access, Some("r"), Duration::hours(1)))])
            })
            .collect();

        let results = futures::future::join_all(files.iter().map(|file| file.save(&path))).await;
        for result in results {
            result.unwrap();
        }

        let saved = TokenFile::load(&path).await.unwrap().unwrap();
        assert!(files.contains(&saved));
        assert_eq!(account_entries(&path), ["tokens.json"]);
    }

    /// A full device is reported, so an empty file never replaces a working tokens.json.
    #[cfg(target_os = "linux")]
    #[test]
    fn write_errors_of_a_full_disk_are_reported() {
        let Ok(mut file) = fs::OpenOptions::new().write(true).open("/dev/full") else {
            return;
        };
        let error = write_and_sync(&mut file, b"[]").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[tokio::test]
    async fn load_reports_invalid_content_without_echoing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        // The second and third make serde_json quote the token in its own message.
        for content in [
            r#"{"refresh_token": "secret"}"#,
            r#"[{"scopes": [], "token": "ya29.secret"}]"#,
            r#"[{"scopes": "1//secret", "token": {}}]"#,
        ] {
            tokio::fs::write(&path, content).await.unwrap();

            let error = TokenFile::load(&path).await.unwrap_err();
            assert!(matches!(error, LoadError::Parse { .. }));
            // The whole chain, as main prints it
            let message = crate::error_message(&error);
            assert!(
                message.contains("is not a valid tokens file ("),
                "{message}"
            );
            assert!(!message.contains("secret"), "{message}");
        }
    }

    /// A flow delegate that answers the (non-listening) interactive installed flow with a code.
    struct CodeDelegate(Result<&'static str, &'static str>);

    impl yup_oauth2::authenticator_delegate::InstalledFlowDelegate for CodeDelegate {
        fn present_user_url<'a>(
            &'a self,
            _url: &'a str,
            _need_code: bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
        {
            let answer = self.0.map(str::to_owned).map_err(str::to_owned);
            Box::pin(async move { answer })
        }
    }

    async fn yup_disk_authenticator(
        token_uri: String,
        path: &Path,
        delegate: CodeDelegate,
    ) -> yup_oauth2::authenticator::Authenticator<hyper_util::client::legacy::connect::HttpConnector>
    {
        let secret = yup_oauth2::ApplicationSecret {
            client_id: String::from("id"),
            client_secret: String::from("secret"),
            token_uri,
            auth_uri: String::from("http://127.0.0.1:1/auth"),
            ..Default::default()
        };
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build_http::<String>();
        yup_oauth2::InstalledFlowAuthenticator::with_client(
            secret,
            yup_oauth2::InstalledFlowReturnMethod::Interactive,
            yup_oauth2::CustomHyperClientBuilder::from(client),
        )
        .persist_tokens_to_disk(path)
        .flow_delegate(Box::new(delegate))
        .build()
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn yup_disk_storage_and_token_file_read_each_others_files() {
        let server = crate::auth::test_support::MockServer::start(vec![(
            200,
            String::from(
                r#"{"access_token":"from-yup","refresh_token":"yup-refresh","expires_in":3599,"token_type":"Bearer"}"#,
            ),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());

        // yup-oauth2 writes the file ...
        let auth =
            yup_disk_authenticator(server.url("/token"), &path, CodeDelegate(Ok("code"))).await;
        auth.token(&DRIVE_SCOPES).await.unwrap();
        drop(auth);

        // ... and gdrive reads it. yup-oauth2 truncates the file and writes it without
        // flushing, so under load it can be missing, empty or half written for a moment: poll
        // (for at most 10 s) until it parses, instead of asserting at once.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let written = loop {
            match TokenFile::load(&path).await {
                Ok(Some(file)) => break file,
                Ok(None) | Err(LoadError::Parse { .. })
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                other => panic!("yup-oauth2 should have written a valid tokens.json: {other:?}"),
            }
        };
        assert_eq!(written.entries().len(), 1);
        assert_eq!(written.entries()[0].scopes, DRIVE_SCOPES);
        assert_eq!(written.refresh_token(), Some("yup-refresh"));

        // gdrive writes the file ...
        let mine = file(&[(
            &DRIVE_SCOPES,
            token("from-gdrive", Some("r"), Duration::hours(1)),
        )]);
        mine.save(&path).await.unwrap();

        // ... and yup-oauth2 uses its token without asking anybody.
        let auth =
            yup_disk_authenticator(server.url("/token"), &path, CodeDelegate(Err("asked"))).await;
        let access = auth.token(&[METADATA]).await.unwrap();
        assert_eq!(access.token(), Some("from-gdrive"));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn store_never_caches_a_token_without_access_token_or_a_service_token_without_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(1)))]);
        loaded.save(&path).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        let user = TokenStore::new(lock(&path), loaded.clone(), Some(String::from("r1")));
        for access_token in [None, Some(String::new())] {
            let empty = TokenInfo {
                access_token,
                ..token("", None, Duration::hours(1))
            };
            user.set(&DRIVE_SCOPES, empty).await.unwrap();
        }

        let service = TokenStore::for_credential(lock(&path), loaded, String::from("robot"));
        let no_expiry = TokenInfo {
            expires_at: None,
            ..token("forever", None, Duration::hours(1))
        };
        service.set(&DRIVE_SCOPES, no_expiry).await.unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert_eq!(
            user.get(&DRIVE_SCOPES)
                .await
                .unwrap()
                .access_token
                .as_deref(),
            Some("old")
        );
    }

    #[tokio::test]
    async fn store_merges_into_the_current_file_and_keeps_the_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = tokens_path(dir.path());
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();

        let store = TokenStore::new(lock(&path), loaded, Some(String::from("r1")));
        store
            .set(&DRIVE_SCOPES, token("fresh", None, Duration::hours(1)))
            .await
            .unwrap();

        let on_disk = TokenFile::load(&path).await.unwrap().unwrap();
        assert_eq!(on_disk.entries().len(), 1);
        assert_eq!(
            on_disk.entries()[0].token.access_token.as_deref(),
            Some("fresh")
        );
        assert_eq!(
            on_disk.entries()[0].token.refresh_token.as_deref(),
            Some("r1")
        );
        assert_eq!(
            store
                .get(&[METADATA])
                .await
                .unwrap()
                .access_token
                .as_deref(),
            Some("fresh")
        );
    }
}
