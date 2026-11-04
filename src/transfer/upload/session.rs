//! Upload sessions kept across runs (#87, #36): an interrupted resumable upload continues in the
//! next run where Drive stopped, instead of starting again from byte 0.
//!
//! A record per upload is kept under the account directory, in
//! `uploads/<sha256 of the source path and the target>.json` (mode 0600, the directory 0700):
//! the session URL, the bytes Drive confirmed, the source (canonical path, size, modification
//! time), the target (parents, name, media type, the file id and what kind of upload it is), the
//! chunk size and when the session started. The session URL authorizes uploads to the session,
//! so the record is kept from other users and never printed.
//!
//! The record also names the Google account the session was started as: the email address
//! Drive reports (`about.get`), or a service account's `client_email` and subject. The account
//! directory alone does not say that: Application Default Credentials, for one, can be another
//! account on the next run, and a session of one account must not be continued as another.
//!
//! A run that uploads the same source to the same target finds the record and continues the
//! session if the source did not change (same path, size and modification time), the run is the
//! same account, and the session is younger than [`SESSION_LIFETIME`], Google's limit;
//! otherwise the upload starts over in a new session. The record is deleted when the upload completes or the
//! session turns out to be gone. Records that nothing touched for longer than the session
//! lifetime are deleted when an upload starts.
//!
//! The source of a relay (`transfer/relay.rs`) is a Drive file of another account, not a local
//! file: its identity is the account it is read as, its file id, its size and its
//! `md5Checksum` (its `version` when it has none) ([`RemoteSource`]), so that a record is never
//! continued with the content of another file, another revision or another account. A local
//! source serializes as before, so the records of earlier versions still load and keep their
//! names.

use std::{
    fmt::{self, Debug, Formatter, Write},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{
    app_config::{account_type::AccountType, AppConfig},
    auth::service::parse_key,
    // Warnings go above the progress bars.
    transfer::progress::eprintln,
};

/// The directory of the records, under the account directory.
pub(crate) const UPLOADS_DIR: &str = "uploads";

/// The directory of the records of the current account, if there is one.
#[must_use]
pub(crate) fn account_session_dir() -> Option<PathBuf> {
    let config = AppConfig::load_current_account().ok()?;
    Some(config.account_base_path().join(UPLOADS_DIR))
}

/// Who the current account is, when that is known without asking Drive: a service account's
/// `client_email` and subject. `None` for a user and for Application Default Credentials, which
/// the uploader asks Drive about.
#[must_use]
pub(crate) fn account_session_identity() -> Option<String> {
    let config = AppConfig::load_current_account().ok()?;
    let AccountType::ServiceAccount { subject } = config.account_type().ok()? else {
        return None;
    };
    let content = std::fs::read(config.service_account_key_path()).ok()?;
    let key = parse_key(&content).ok()?;
    Some(service_account_identity(
        &key.client_email,
        subject.as_deref(),
    ))
}

/// The identity of the service account `client_email` acting for `subject`, if any.
#[must_use]
pub(crate) fn service_account_identity(client_email: &str, subject: Option<&str>) -> String {
    match subject {
        Some(subject) => format!("service account {client_email} as {subject}"),
        None => format!("service account {client_email}"),
    }
}

/// Google expires an upload session a week after it started.
pub(crate) const SESSION_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The format of the records; a record of another version is not used.
const RECORD_VERSION: u32 = 2;

/// A modification time, since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mtime {
    pub(crate) secs: u64,
    pub(crate) nanos: u32,
}

/// The file an upload reads. An upload continues only for the same one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceIdentity {
    /// The canonical path of a local file; for a Drive file, a name made of its account and id
    /// ([`SourceIdentity::remote`]), which only names the record.
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    /// `None` where the file system has none, and for a Drive file. A local file without one is
    /// never continued.
    pub(crate) mtime: Option<Mtime>,
    /// The Drive file a relay reads; `None` for a local file (and in the records of earlier
    /// versions, which only had local files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote: Option<RemoteSource>,
}

/// The Drive file a relay reads, of another account than the one it uploads to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteSource {
    /// The Google account the file is read as (see [`SessionRecord::identity`]).
    pub(crate) account: String,
    pub(crate) file_id: String,
    /// The `md5Checksum` of the content, when Drive has one.
    pub(crate) md5: Option<String>,
    /// The `version`, only without an md5: it also changes with the metadata (a rename, a new
    /// permission), which the md5 does not, so with an md5 it would only cause needless starts
    /// from the beginning.
    pub(crate) version: Option<i64>,
}

impl SourceIdentity {
    /// The identity of the Drive file `file_id` of `size` bytes, read as `account`. Without an
    /// md5 nor a version, nothing tells whether it changed, and its session is not continued.
    #[must_use]
    pub(crate) fn remote(
        account: &str,
        file_id: &str,
        size: u64,
        md5: Option<&str>,
        version: Option<i64>,
    ) -> Self {
        SourceIdentity {
            path: PathBuf::from(format!("drive:{account}/{file_id}")),
            size,
            mtime: None,
            remote: Some(RemoteSource {
                account: account.to_owned(),
                file_id: file_id.to_owned(),
                md5: md5.map(str::to_ascii_lowercase),
                version: if md5.is_some() { None } else { version },
            }),
        }
    }

    /// Why a session of this source can never be continued: nothing tells whether it changed.
    fn unverifiable(&self) -> Option<&'static str> {
        match &self.remote {
            Some(remote) => (remote.md5.is_none() && remote.version.is_none())
                .then_some("the Drive file has no md5 or version to tell whether it changed"),
            None => self
                .mtime
                .is_none()
                .then_some("the file has no modification time to tell whether it changed"),
        }
    }

    /// The identity of the file at `path` with `metadata`.
    pub(crate) async fn of(path: &Path, metadata: &std::fs::Metadata) -> Self {
        let path = tokio::fs::canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_path_buf());
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|since| Mtime {
                secs: since.as_secs(),
                nanos: since.subsec_nanos(),
            });
        SourceIdentity {
            path,
            size: metadata.len(),
            mtime,
            remote: None,
        }
    }
}

/// The Drive side of an upload: what it writes, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordTarget {
    pub(crate) parents: Vec<String>,
    pub(crate) name: String,
    /// The media type of the content.
    pub(crate) mime: String,
    /// The file an update replaces.
    pub(crate) existing_file_id: Option<String>,
    /// The Google type of a conversion.
    pub(crate) convert_to: Option<String>,
    /// The id of the file the session creates (pre-generated) or updates. Not part of the
    /// record's name: a later run of the same upload has no id yet and takes this one.
    pub(crate) file_id: Option<String>,
    /// The marker a conversion is created with (`appProperties`); like the id, a later run
    /// takes it over.
    #[serde(default)]
    pub(crate) marker: Option<String>,
    /// The `modifiedTime` the new file is given (RFC 3339), a relay's copy of the source's.
    /// Left out when there is none, so that the records of other uploads keep their names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) modified_time: Option<String>,
    /// The `createdTime` the new file is given, like `modified_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_time: Option<String>,
}

impl RecordTarget {
    /// The upload without what a later run takes over from the record.
    fn key(&self) -> RecordTarget {
        RecordTarget {
            file_id: None,
            marker: None,
            ..self.clone()
        }
    }

    /// Whether `self` and `other` are the same upload, whatever their file ids and markers.
    fn same_upload(&self, other: &RecordTarget) -> bool {
        self.key() == other.key()
    }
}

/// The record of an upload session.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    pub(crate) version: u32,
    pub(crate) session_url: String,
    /// The bytes Drive confirmed holding when the record was written.
    pub(crate) offset: u64,
    pub(crate) source: SourceIdentity,
    pub(crate) target: RecordTarget,
    pub(crate) chunk_size: u64,
    /// When the session started, in seconds since the Unix epoch.
    pub(crate) created: u64,
    /// The Google account the session was started as.
    pub(crate) identity: String,
}

impl Debug for SessionRecord {
    /// Without the session URL, which authorizes uploads.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionRecord")
            .field("version", &self.version)
            .field("session_url", &"<redacted>")
            .field("offset", &self.offset)
            .field("source", &self.source)
            .field("target", &self.target)
            .field("chunk_size", &self.chunk_size)
            .field("created", &self.created)
            .field("identity", &self.identity)
            .finish()
    }
}

impl SessionRecord {
    /// A record of the session at `session_url`, started now as `identity`.
    #[must_use]
    pub(crate) fn new(
        session_url: String,
        source: SourceIdentity,
        target: RecordTarget,
        chunk_size: u64,
        identity: String,
    ) -> Self {
        SessionRecord {
            version: RECORD_VERSION,
            session_url,
            offset: 0,
            source,
            target,
            chunk_size,
            created: unix_now(),
            identity,
        }
    }

    /// Why the record cannot continue an upload of `source` to `target`, if it cannot.
    #[must_use]
    pub(crate) fn mismatch(
        &self,
        source: &SourceIdentity,
        target: &RecordTarget,
    ) -> Option<&'static str> {
        let age = Duration::from_secs(unix_now().saturating_sub(self.created));
        if self.version != RECORD_VERSION {
            Some("it was written by another version of gdrive")
        } else if let Some(why) = self.source.unverifiable() {
            Some(why)
        } else if self.source != *source {
            Some("the file changed since")
        } else if !self.target.same_upload(target)
            || target
                .file_id
                .as_ref()
                .is_some_and(|id| self.target.file_id.as_ref() != Some(id))
        {
            Some("it was an upload to another Drive file")
        } else if age >= SESSION_LIFETIME {
            Some("its session has expired")
        } else {
            None
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The record file of one upload.
pub(crate) struct RecordFile {
    path: PathBuf,
    /// The session URL of the record this invocation wrote or continues. Only that record is
    /// deleted: another invocation may have written its own since.
    owned: Option<String>,
    /// A save failed: the user was told once, and the record is no longer kept.
    broken: bool,
}

impl RecordFile {
    /// The record of the upload of `source` to `target` in `dir`.
    #[must_use]
    pub(crate) fn new(dir: &Path, source: &SourceIdentity, target: &RecordTarget) -> Self {
        let key = target.key();
        let mut hashed = source.path.as_os_str().as_encoded_bytes().to_vec();
        hashed.push(0);
        hashed.extend_from_slice(serde_json::to_string(&key).unwrap_or_default().as_bytes());
        let digest = ring::digest::digest(&ring::digest::SHA256, &hashed);
        let name = digest
            .as_ref()
            .iter()
            .fold(String::new(), |mut name, byte| {
                let _ = write!(name, "{byte:02x}");
                name
            });
        RecordFile {
            path: dir.join(format!("{name}.json")),
            owned: None,
            broken: false,
        }
    }

    /// The record, if there is a readable one. An unreadable one is left alone: the next save
    /// replaces it, and pruning deletes it if nothing does.
    pub(crate) async fn load(&self) -> Option<SessionRecord> {
        let content = tokio::fs::read(&self.path).await.ok()?;
        serde_json::from_slice(&content).ok()
    }

    /// The loaded `record` is this invocation's now: it continues its session.
    pub(crate) fn adopt(&mut self, record: &SessionRecord) {
        self.owned = Some(record.session_url.clone());
    }

    /// Writes `record` (a whole new file, renamed over the old one), which makes it this
    /// invocation's. A failure is reported once on stderr; the upload goes on without a record.
    pub(crate) async fn save(&mut self, record: &SessionRecord) {
        if self.broken {
            return;
        }
        match self.write(record).await {
            Ok(()) => self.owned = Some(record.session_url.clone()),
            Err(error) => {
                self.broken = true;
                eprintln!(
                    "Warning: unable to save the upload session in '{}', an interrupted upload \
                     will start over: {error}",
                    self.path.display()
                );
                self.remove().await;
            }
        }
    }

    /// Writes `record` to a new temporary file of its own (created exclusively, so that it is
    /// neither another writer's nor a planted link) and renames it over the record.
    async fn write(&self, record: &SessionRecord) -> io::Result<()> {
        let content = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            let mut builder = tokio::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(dir).await?;
        }
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temp = self
            .path
            .with_file_name(format!("{name}.{:016x}.tmp", fastrand::u64(..)));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        let written = async {
            file.write_all(&content).await?;
            file.flush().await?;
            drop(file);
            tokio::fs::rename(&temp, &self.path).await
        }
        .await;
        if written.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        written
    }

    /// Deletes the record if it is still the one this invocation owns.
    pub(crate) async fn remove(&mut self) {
        let Some(owned) = self.owned.take() else {
            return;
        };
        if self
            .load()
            .await
            .is_some_and(|record| record.session_url == owned)
        {
            // A record that cannot be deleted is found stale by the next run.
            let _ = tokio::fs::remove_file(&self.path).await;
        }
    }
}

/// Deletes the records in `dir` that nothing wrote to for longer than the session lifetime:
/// their sessions have expired.
pub(crate) async fn prune(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_record = path
            .extension()
            .is_some_and(|extension| extension == "json" || extension == "tmp");
        let stale = entry
            .metadata()
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > SESSION_LIFETIME);
        if is_record && stale {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt::Write,
        time::{Duration, SystemTime},
    };

    use super::{prune, Mtime, RecordFile, RecordTarget, SessionRecord, SourceIdentity};

    fn source() -> SourceIdentity {
        SourceIdentity {
            path: "/data/a.bin".into(),
            size: 10,
            mtime: Some(Mtime { secs: 1, nanos: 2 }),
            remote: None,
        }
    }

    fn identity() -> String {
        String::from("me@example.com")
    }

    #[test]
    fn a_service_account_is_its_email_and_subject() {
        assert_eq!(
            super::service_account_identity("sa@p.iam.gserviceaccount.com", None),
            "service account sa@p.iam.gserviceaccount.com"
        );
        assert_eq!(
            super::service_account_identity("sa@p.iam.gserviceaccount.com", Some("me@example.com")),
            "service account sa@p.iam.gserviceaccount.com as me@example.com"
        );
    }

    fn target() -> RecordTarget {
        RecordTarget {
            parents: vec![String::from("folder")],
            name: String::from("a.bin"),
            mime: String::from("application/octet-stream"),
            existing_file_id: None,
            convert_to: None,
            file_id: Some(String::from("id-1")),
            marker: None,
            modified_time: None,
            created_time: None,
        }
    }

    /// A record as gdrive wrote it before relays: no `remote` source, no file times.
    const EARLIER_RECORD: &str = r#"{
        "version": 2,
        "session_url": "https://upload.example/?upload_id=1",
        "offset": 512,
        "source": {"path": "/data/a.bin", "size": 10, "mtime": {"secs": 1, "nanos": 2}},
        "target": {
            "parents": ["folder"],
            "name": "a.bin",
            "mime": "application/octet-stream",
            "existing_file_id": null,
            "convert_to": null,
            "file_id": "id-1",
            "marker": null
        },
        "chunk_size": 256,
        "created": 4102444800,
        "identity": "me@example.com"
    }"#;

    #[test]
    fn records_of_local_uploads_written_before_relays_still_load_and_keep_their_names() {
        let mut record: SessionRecord = serde_json::from_str(EARLIER_RECORD).unwrap();
        assert_eq!(record.source, source());
        assert_eq!(record.target, target());
        record.created = super::unix_now();
        assert_eq!(record.mismatch(&source(), &target()), None);

        // The name is the digest of the path and the target as the earlier versions
        // serialized them, which is what they look for.
        let earlier_key = r#"{"parents":["folder"],"name":"a.bin","mime":"application/octet-stream","existing_file_id":null,"convert_to":null,"file_id":null,"marker":null}"#;
        let mut hashed = b"/data/a.bin\0".to_vec();
        hashed.extend_from_slice(earlier_key.as_bytes());
        let digest = ring::digest::digest(&ring::digest::SHA256, &hashed);
        let expected = digest
            .as_ref()
            .iter()
            .fold(String::new(), |mut name, byte| {
                let _ = write!(name, "{byte:02x}");
                name
            });
        let file = RecordFile::new(std::path::Path::new("/uploads"), &source(), &target());
        assert_eq!(
            file.path,
            std::path::Path::new("/uploads").join(format!("{expected}.json"))
        );

        // And a local record written now is what the earlier versions read.
        let written = serde_json::to_value(&record).unwrap();
        assert!(written["source"].get("remote").is_none(), "{written}");
        assert!(
            written["target"].get("modified_time").is_none(),
            "{written}"
        );
    }

    fn remote(account: &str, id: &str, md5: Option<&str>, version: Option<i64>) -> SourceIdentity {
        SourceIdentity::remote(account, id, 10, md5, version)
    }

    #[test]
    fn a_relay_continues_only_the_same_revision_of_the_same_file_of_the_same_account() {
        let md5 = "0123456789abcdef0123456789ABCDEF";
        let source = remote("a@example.com", "file-1", Some(md5), Some(7));
        assert_eq!(
            source.remote.as_ref().unwrap().md5.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            source.remote.as_ref().unwrap().version,
            None,
            "the md5 identifies the content; the version also counts metadata changes"
        );
        let record = SessionRecord::new(
            String::from("url"),
            source.clone(),
            target(),
            256,
            identity(),
        );
        assert_eq!(record.mismatch(&source, &target()), None);
        assert_eq!(
            record.mismatch(
                &remote("a@example.com", "file-1", Some(md5), Some(8)),
                &target()
            ),
            None,
            "a rename does not change the content"
        );
        for other in [
            remote("b@example.com", "file-1", Some(md5), Some(7)),
            remote("a@example.com", "file-2", Some(md5), Some(7)),
            remote(
                "a@example.com",
                "file-1",
                Some("ffffffffffffffffffffffffffffffff"),
                Some(7),
            ),
            SourceIdentity::remote("a@example.com", "file-1", 11, Some(md5), Some(7)),
        ] {
            assert_eq!(
                record.mismatch(&other, &target()),
                Some("the file changed since"),
                "{other:?}"
            );
        }

        // Without an md5, the version tells; without either, nothing does.
        let versioned = remote("a@example.com", "file-1", None, Some(7));
        let record = SessionRecord::new(
            String::from("url"),
            versioned.clone(),
            target(),
            256,
            identity(),
        );
        assert_eq!(record.mismatch(&versioned, &target()), None);
        assert!(record
            .mismatch(&remote("a@example.com", "file-1", None, Some(8)), &target())
            .is_some());
        let unknown = remote("a@example.com", "file-1", None, None);
        let record = SessionRecord::new(
            String::from("url"),
            unknown.clone(),
            target(),
            256,
            identity(),
        );
        assert_eq!(
            record.mismatch(&unknown, &target()),
            Some("the Drive file has no md5 or version to tell whether it changed")
        );

        // Each account's file has a record of its own, apart from local uploads.
        let dir = std::path::Path::new("/uploads");
        let name = |source: &SourceIdentity| RecordFile::new(dir, source, &target()).path;
        assert_ne!(
            name(&source),
            name(&remote("b@example.com", "file-1", Some(md5), None))
        );
        assert_ne!(
            name(&source),
            name(&remote("a@example.com", "file-2", Some(md5), None))
        );
        assert_ne!(name(&source), name(&super::tests::source()));
    }

    #[tokio::test]
    async fn a_relay_record_is_saved_and_loaded_with_its_source_and_times() {
        let dir = tempfile::tempdir().unwrap();
        let source = remote("a@example.com", "file-1", None, Some(3));
        let target = RecordTarget {
            modified_time: Some(String::from("2026-01-02T03:04:05.000Z")),
            created_time: Some(String::from("2025-01-02T03:04:05.000Z")),
            ..target()
        };
        let mut file = RecordFile::new(dir.path(), &source, &target);
        let record = SessionRecord::new(String::from("url"), source, target, 256, identity());
        file.save(&record).await;
        assert_eq!(file.load().await, Some(record));
    }

    #[test]
    fn the_record_name_depends_on_source_and_target_but_not_the_new_id() {
        let dir = std::path::Path::new("/uploads");
        let name = |source: &SourceIdentity, target: &RecordTarget| {
            RecordFile::new(dir, source, target).path.clone()
        };
        let base = name(&source(), &target());
        assert_eq!(base.parent(), Some(dir));
        assert_eq!(base.extension().unwrap(), "json");
        assert_eq!(base.file_stem().unwrap().len(), 64);

        let no_id = RecordTarget {
            file_id: None,
            ..target()
        };
        assert_eq!(name(&source(), &no_id), base);
        let renamed = RecordTarget {
            name: String::from("b.bin"),
            ..target()
        };
        assert_ne!(name(&source(), &renamed), base);
        let moved = SourceIdentity {
            path: "/data/b.bin".into(),
            ..source()
        };
        assert_ne!(name(&moved, &target()), base);
    }

    #[test]
    fn a_record_continues_only_the_same_upload_of_the_same_file() {
        let record = SessionRecord::new(String::from("url"), source(), target(), 256, identity());
        assert_eq!(record.mismatch(&source(), &target()), None);
        let later_run = RecordTarget {
            file_id: None,
            ..target()
        };
        assert_eq!(record.mismatch(&source(), &later_run), None);

        let touched = SourceIdentity {
            mtime: Some(Mtime { secs: 1, nanos: 3 }),
            ..source()
        };
        assert!(record.mismatch(&touched, &target()).is_some());
        let grown = SourceIdentity {
            size: 11,
            ..source()
        };
        assert!(record.mismatch(&grown, &target()).is_some());
        let other_id = RecordTarget {
            file_id: Some(String::from("id-2")),
            ..target()
        };
        assert!(record.mismatch(&source(), &other_id).is_some());
        let other_folder = RecordTarget {
            parents: Vec::new(),
            ..target()
        };
        assert!(record.mismatch(&source(), &other_folder).is_some());

        let old = SessionRecord {
            created: record.created - 8 * 24 * 60 * 60,
            ..record.clone()
        };
        assert_eq!(
            old.mismatch(&source(), &target()),
            Some("its session has expired")
        );

        let no_mtime = SourceIdentity {
            mtime: None,
            ..source()
        };
        let record = SessionRecord::new(
            String::from("url"),
            no_mtime.clone(),
            target(),
            256,
            identity(),
        );
        assert!(record.mismatch(&no_mtime, &target()).is_some());
    }

    #[test]
    fn the_debug_output_leaves_out_the_session_url() {
        let record = SessionRecord::new(
            String::from("https://upload.example/?upload_id=secret"),
            source(),
            target(),
            256,
            identity(),
        );
        let debug = format!("{record:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[tokio::test]
    async fn records_are_saved_privately_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let mut file = RecordFile::new(&uploads, &source(), &target());
        assert!(file.load().await.is_none());

        let mut record =
            SessionRecord::new(String::from("url"), source(), target(), 256, identity());
        file.save(&record).await;
        record.offset = 512;
        file.save(&record).await;
        assert_eq!(file.load().await, Some(record));
        let names: Vec<_> = std::fs::read_dir(&uploads)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(names, [file.path.clone()], "no temporary file is left");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &std::path::Path| {
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777
            };
            assert_eq!(mode(&file.path), 0o600);
            assert_eq!(mode(&uploads), 0o700);
        }

        std::fs::write(&file.path, b"not json").unwrap();
        assert!(file.load().await.is_none());
        assert!(
            file.path.exists(),
            "an unreadable record is left to be replaced"
        );
    }

    #[tokio::test]
    async fn a_record_is_deleted_only_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let record =
            |url: &str| SessionRecord::new(url.into(), source(), target(), 256, identity());
        let mut first = RecordFile::new(dir.path(), &source(), &target());
        let mut second = RecordFile::new(dir.path(), &source(), &target());

        first.save(&record("first")).await;
        // Loading does not make it the second invocation's.
        assert!(second.load().await.is_some());
        second.remove().await;
        assert!(first.path.exists());

        // The second invocation replaces it: the first one's record is gone already.
        second.save(&record("second")).await;
        first.remove().await;
        assert_eq!(first.load().await.unwrap().session_url, "second");

        // Continuing a session makes its record the invocation's.
        let mut third = RecordFile::new(dir.path(), &source(), &target());
        let loaded = third.load().await.unwrap();
        third.adopt(&loaded);
        third.remove().await;
        assert!(!third.path.exists());
    }

    #[tokio::test]
    async fn saves_write_temporary_files_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let file = RecordFile::new(dir.path(), &source(), &target());
        // What a fixed temporary name would have been, taken by someone else.
        let planted = file.path.with_extension("json.tmp");
        std::fs::write(&planted, b"not gdrive's").unwrap();

        let saves = (0..8).map(|n| {
            let mut file = RecordFile::new(dir.path(), &source(), &target());
            async move {
                let record =
                    SessionRecord::new(format!("url-{n}"), source(), target(), 256, identity());
                file.save(&record).await;
                file.broken
            }
        });
        let broken = futures::future::join_all(saves).await;

        assert!(broken.iter().all(|broken| !broken), "every save succeeds");
        assert!(file.load().await.is_some());
        assert_eq!(std::fs::read(&planted).unwrap(), b"not gdrive's");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(names.len(), 2, "the record and the planted file: {names:?}");
    }

    #[tokio::test]
    async fn stale_records_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.json");
        let fresh = dir.path().join("fresh.json");
        let other = dir.path().join("notes.txt");
        for path in [&old, &fresh, &other] {
            std::fs::write(path, b"{}").unwrap();
        }
        let long_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        for path in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }

        prune(dir.path()).await;

        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists(), "only records are deleted");
    }
}
