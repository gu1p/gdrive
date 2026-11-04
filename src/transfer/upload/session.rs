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

/// The local file an upload reads. An upload continues only for the same one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceIdentity {
    /// The canonical path.
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    /// `None` where the file system has none: such a source is never continued.
    pub(crate) mtime: Option<Mtime>,
}

impl SourceIdentity {
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
        } else if self.source.mtime.is_none() {
            Some("the file has no modification time to tell whether it changed")
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
            broken: false,
        }
    }

    /// The record, if there is a readable one. An unreadable one is deleted.
    pub(crate) async fn load(&self) -> Option<SessionRecord> {
        let content = tokio::fs::read(&self.path).await.ok()?;
        let record = serde_json::from_slice(&content).ok();
        if record.is_none() {
            self.remove().await;
        }
        record
    }

    /// Writes `record` (a whole new file, renamed over the old one). A failure is reported
    /// once on stderr; the upload goes on without a record.
    pub(crate) async fn save(&mut self, record: &SessionRecord) {
        if self.broken {
            return;
        }
        if let Err(error) = self.write(record).await {
            self.broken = true;
            eprintln!(
                "Warning: unable to save the upload session in '{}', an interrupted upload \
                 will start over: {error}",
                self.path.display()
            );
            self.remove().await;
        }
    }

    async fn write(&self, record: &SessionRecord) -> io::Result<()> {
        let content = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            let mut builder = tokio::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(dir).await?;
        }
        let temp = self.path.with_extension("json.tmp");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        file.write_all(&content).await?;
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&temp, &self.path).await
    }

    /// Deletes the record, if there is one.
    pub(crate) async fn remove(&self) {
        // A record that cannot be deleted is replaced or found stale by the next run.
        let _ = tokio::fs::remove_file(&self.path).await;
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
    use std::time::{Duration, SystemTime};

    use super::{prune, Mtime, RecordFile, RecordTarget, SessionRecord, SourceIdentity};

    fn source() -> SourceIdentity {
        SourceIdentity {
            path: "/data/a.bin".into(),
            size: 10,
            mtime: Some(Mtime { secs: 1, nanos: 2 }),
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
        }
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
        assert!(!file.path.exists(), "an unreadable record is deleted");
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
