//! Reading and writing `tokens.json`.
//!
//! The layout is the one of yup-oauth2's disk storage, which every gdrive 3.x release wrote: a
//! JSON array of `{"scopes": [..], "token": {"access_token", "refresh_token", "expires_at",
//! "id_token"}}` entries. gdrive reads and writes it itself, through [`TokenStore`], because
//! yup-oauth2's disk storage would store a refresh response without `refresh_token` as `null`
//! (the refresh-only flow does not carry the old refresh token over) and rewrites the file in
//! place. Here a missing refresh token never replaces a stored one, and the file is replaced
//! atomically.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::{Path, PathBuf},
};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncWriteExt, sync::Mutex};
use yup_oauth2::storage::{TokenInfo, TokenStorage, TokenStorageError};

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
    /// other credentials do not count.
    pub(crate) fn get_for(&self, credential: Option<&str>, scopes: &[&str]) -> Option<&TokenInfo> {
        self.entries
            .iter()
            .filter(|entry| entry.credential.as_deref() == credential)
            .filter(|entry| covers(&entry.scopes, scopes))
            .max_by_key(|entry| (!entry.token.is_expired(), entry.token.expires_at))
            .map(|entry| &entry.token)
    }

    /// The refresh token to use for this account: the one of the most recently issued entry
    /// (latest expiry) that has a refresh token.
    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.credential.is_none())
            .filter(|entry| entry.token.refresh_token.is_some())
            .max_by_key(|entry| entry.token.expires_at)
            .and_then(|entry| entry.token.refresh_token.as_deref())
    }

    /// [`TokenFile::set_for`] for an OAuth user account.
    #[cfg(test)]
    pub(crate) fn set(&mut self, scopes: &[&str], token: TokenInfo) {
        self.set_for(None, scopes, token);
    }

    /// Stores `token` of `credential` for exactly `scopes`, replacing the entry for the same
    /// credential and scope set.
    ///
    /// For an OAuth user account (no credential), a token without a refresh token keeps the
    /// refresh token that is already stored, first the one of the replaced entry, otherwise
    /// [`TokenFile::refresh_token`]: token responses to a refresh request normally carry no
    /// refresh token, and the stored one stays valid.
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
        let index = self.entries.iter().position(|entry| {
            entry.credential.as_deref() == credential && same_scopes(&entry.scopes, scopes)
        });

        if token.refresh_token.is_none() && credential.is_none() {
            token.refresh_token = index
                .and_then(|index| self.entries[index].token.refresh_token.clone())
                .or_else(|| self.refresh_token().map(str::to_owned));
        }

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

        // Neither the content nor the serde_json error, which can quote it: it holds the tokens.
        Self::from_json(&content)
            .map(Some)
            .map_err(|error| LoadError::Parse {
                path: path.to_path_buf(),
                error: RedactedJsonError::from(&error),
            })
    }

    /// Replaces `path` atomically: writes a temporary file next to it (mode 0600 on Unix) and
    /// renames it over `path`.
    pub(crate) async fn save(&self, path: &Path) -> io::Result<()> {
        let json = self.to_json().map_err(io::Error::other)?;
        replace_file(path, json.as_bytes(), write_and_sync).await
    }
}

/// Replaces `path` atomically with `content`, like [`TokenFile::save`]: the file is created with
/// mode 0600 on Unix from the start, never made private after the fact. For other files that hold
/// credentials, such as a service account key.
pub(crate) async fn write_private_file(path: &Path, content: &[u8]) -> io::Result<()> {
    replace_file(path, content, write_and_sync).await
}

/// Replaces `path` with `content`. `write` fills a new temporary file next to `path`, which is
/// renamed over `path` only when `write` succeeded. On any error the temporary file is removed
/// and `path` keeps its previous content.
async fn replace_file<W>(path: &Path, content: &[u8], write: W) -> io::Result<()>
where
    W: AsyncFnOnce(&mut File, &[u8]) -> io::Result<()>,
{
    let (temp_path, mut file) = create_temp_file(path).await?;
    let written = write(&mut file, content).await;
    // Closed before the rename, which Windows refuses for an open file.
    drop(file);
    let result = match written {
        Ok(()) => tokio::fs::rename(&temp_path, path).await,
        Err(error) => Err(error),
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    result
}

/// Writes all of `content` to `file` and makes it durable.
///
/// tokio's `File` hands each write to a background thread and reports its error only with the
/// next operation: `write_all` can return before the data is written, and `sync_all` does not
/// report a pending write error, but `flush` does. Without the flush a full disk (ENOSPC) could
/// go unnoticed and an empty file would replace a working tokens.json.
async fn write_and_sync(file: &mut File, content: &[u8]) -> io::Result<()> {
    file.write_all(content).await?;
    file.flush().await?;
    file.sync_all().await
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
async fn create_temp_file(path: &Path) -> io::Result<(PathBuf, File)> {
    let random = SystemRandom::new();
    let suffixes = std::iter::repeat_with(|| {
        let mut bytes = [0; 8];
        random
            .fill(&mut bytes)
            .map_err(|_| io::Error::other("unable to get random bytes from the system"))?;
        Ok(format!("{:016x}", u64::from_le_bytes(bytes)))
    });
    create_temp_file_named(path, suffixes).await
}

async fn create_temp_file_named(
    path: &Path,
    suffixes: impl Iterator<Item = io::Result<String>>,
) -> io::Result<(PathBuf, File)> {
    let mut clash = None;
    for suffix in suffixes.take(TEMP_FILE_ATTEMPTS) {
        let mut name = path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(format!(".{}.tmp", suffix?));
        let temp_path = path.with_file_name(name);

        match open_new_file(&temp_path).await {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => clash = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(clash.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
}

async fn open_new_file(path: &Path) -> io::Result<File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
}

/// `tokens.json` exposed to yup-oauth2 as its token storage.
pub(crate) struct TokenStore {
    path: PathBuf,
    file: Mutex<TokenFile>,
    /// The refresh token the authenticator refreshes with; stored with refreshed tokens that
    /// come without one.
    refresh_token: Option<String>,
    /// The credential of a service account whose tokens these are; see [`TokenEntry`].
    credential: Option<String>,
}

impl TokenStore {
    /// The tokens of an OAuth user account.
    pub(crate) fn new(path: PathBuf, file: TokenFile, refresh_token: Option<String>) -> Self {
        TokenStore {
            path,
            file: Mutex::new(file),
            refresh_token,
            credential: None,
        }
    }

    /// The tokens of the service account credential `credential`: only its entries are read,
    /// and its tokens are stored with it.
    pub(crate) fn for_credential(path: PathBuf, file: TokenFile, credential: String) -> Self {
        TokenStore {
            path,
            file: Mutex::new(file),
            refresh_token: None,
            credential: Some(credential),
        }
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
        if token.refresh_token.is_none() {
            token.refresh_token.clone_from(&self.refresh_token);
        }

        let mut file = self.file.lock().await;
        // Merge into what is on disk now, so that the update of another gdrive process that ran
        // in the meantime is kept.
        if let Ok(Some(on_disk)) = TokenFile::load(&self.path).await {
            *file = on_disk;
        }
        file.set_for(self.credential.as_deref(), scopes, token);
        file.save(&self.path).await.map_err(TokenStorageError::Io)
    }

    async fn get(&self, scopes: &[&str]) -> Option<TokenInfo> {
        self.file
            .lock()
            .await
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
    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::auth::DRIVE_SCOPES;

    const DRIVE: &str = "https://www.googleapis.com/auth/drive";
    const METADATA: &str = "https://www.googleapis.com/auth/drive.metadata.readonly";

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

    #[tokio::test]
    async fn save_replaces_the_file_atomically_and_load_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        assert_eq!(TokenFile::load(&path).await.unwrap(), None);

        tokio::fs::write(&path, "stale content").await.unwrap();
        let expected = file(&[(&DRIVE_SCOPES, token("a", Some("r"), Duration::hours(1)))]);
        expected.save(&path).await.unwrap();

        assert_eq!(TokenFile::load(&path).await.unwrap(), Some(expected));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tokens.json"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn failed_write_keeps_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let previous = file(&[(&DRIVE_SCOPES, token("a", Some("keep"), Duration::hours(1)))]);
        previous.save(&path).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        // The disk fills up halfway through the new content.
        let new_content = br#"[{"scopes":[],"token":{}}]"#;
        let error = replace_file(
            &path,
            new_content,
            async |file: &mut File, content: &[u8]| {
                file.write_all(&content[..content.len() / 2]).await?;
                file.flush().await?;
                Err(io::Error::from(io::ErrorKind::StorageFull))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);

        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tokens.json"]);
    }

    #[tokio::test]
    async fn temp_files_never_reuse_an_existing_name_or_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| Ok(name.to_string()))
                .collect::<Vec<_>>()
                .into_iter()
        };

        // Another process's temporary file.
        let other = dir.path().join("tokens.json.aaaa.tmp");
        tokio::fs::write(&other, "other process").await.unwrap();
        // A symlink planted where a temporary file would go.
        let victim = dir.path().join("victim");
        tokio::fs::write(&victim, "victim").await.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, dir.path().join("tokens.json.bbbb.tmp")).unwrap();

        let (temp_path, mut file) = create_temp_file_named(&path, names(&["aaaa", "bbbb", "cccc"]))
            .await
            .unwrap();
        file.write_all(b"mine").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let expected = if cfg!(unix) { "cccc" } else { "bbbb" };
        assert_eq!(
            temp_path,
            dir.path().join(format!("tokens.json.{expected}.tmp"))
        );
        assert_eq!(tokio::fs::read(&other).await.unwrap(), b"other process");
        assert_eq!(tokio::fs::read(&victim).await.unwrap(), b"victim");

        let error = create_temp_file_named(&path, names(&["aaaa"]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn concurrent_saves_do_not_mix_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
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
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tokens.json"]);
    }

    /// Why `write_and_sync` flushes: on a full device tokio's `write_all` succeeds and only the
    /// flush reports ENOSPC.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn write_errors_of_a_full_disk_are_reported() {
        async fn open_full() -> io::Result<File> {
            tokio::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .await
        }
        let Ok(mut file) = open_full().await else {
            return;
        };
        file.write_all(b"[]").await.unwrap();
        let error = file.flush().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);

        let mut file = open_full().await.unwrap();
        assert!(write_and_sync(&mut file, b"[]").await.is_err());
    }

    #[tokio::test]
    async fn load_reports_invalid_content_without_echoing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
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
        let path = dir.path().join("tokens.json");

        // yup-oauth2 writes the file ...
        let auth =
            yup_disk_authenticator(server.url("/token"), &path, CodeDelegate(Ok("code"))).await;
        auth.token(&DRIVE_SCOPES).await.unwrap();
        drop(auth);

        // ... and gdrive reads it.
        let written = TokenFile::load(&path).await.unwrap().unwrap();
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
        let path = dir.path().join("tokens.json");
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(1)))]);
        loaded.save(&path).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        let user = TokenStore::new(path.clone(), loaded.clone(), Some(String::from("r1")));
        for access_token in [None, Some(String::new())] {
            let empty = TokenInfo {
                access_token,
                ..token("", None, Duration::hours(1))
            };
            user.set(&DRIVE_SCOPES, empty).await.unwrap();
        }

        let service = TokenStore::for_credential(path.clone(), loaded, String::from("robot"));
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
        let path = dir.path().join("tokens.json");
        let loaded = file(&[(&DRIVE_SCOPES, token("old", Some("r1"), Duration::hours(-1)))]);
        loaded.save(&path).await.unwrap();

        let store = TokenStore::new(path.clone(), loaded, Some(String::from("r1")));
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
