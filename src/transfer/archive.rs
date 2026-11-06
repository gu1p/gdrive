//! An archive of Drive files, made while it is uploaded (`files migrate --archive`): one plain
//! tar, no compression, of a folder of the source account (or of one file), streamed from the
//! source through memory into a resumable upload of the destination account.
//!
//! - [`Layout`]: the archive as a fixed list of segments, decided before the upload starts.
//!   Segments of bytes held in memory (headers, padding, the end-of-archive blocks, exported
//!   Google Docs) alternate with blob segments, the content of a file of the source, which is
//!   never held: [`ArchiveSource`] reads it through the relay's [`DriveSource`] (one reply open
//!   at a time) as the uploader asks for it. The uploader needs the total size up front and may
//!   read any offset again (a chunk sent again, a new session from byte 0, a continued run), so
//!   any offset can be read again, and a file read again goes through `DriveSource`'s proofs
//!   that it is still the content that was listed.
//! - Format: POSIX.1-2001 (pax interchange). Every entry has a ustar header; a pax extended
//!   header (`x`, records `path` and `size`) comes before an entry whose path is not ASCII or
//!   longer than the 100 bytes of the ustar name field, or whose size does not fit its 11 octal
//!   digits (8 GiB and more; that field then holds the size in GNU's base-256 too). pax is the
//!   standard form of long UTF-8 names and big sizes: GNU tar, bsdtar, Python's `tarfile` and
//!   the `tar` crate read it, and a pax `path` is UTF-8 by definition, where a GNU long name is
//!   bytes of whatever charset the reader assumes. No global headers and no GNU extensions but
//!   that base-256 field.
//! - Header fields are fixed: mode 0644 for files and 0755 for directories, uid and gid 0, empty
//!   user and group names, the modification time the caller gives (Drive's `modifiedTime` in
//!   whole seconds). Nothing comes from the clock or the host: the same entries give the same
//!   bytes ([`Layout::digest`] covers every byte held in memory and the id, size and md5 of
//!   every blob).
//!
//! What every archive that completes has been checked for, in a continued run as in one run:
//! - Every blob with an md5 is hashed from its first byte, and at its last byte the md5 is
//!   compared with the listed `md5Checksum`. A read that starts past the bytes hashed so far
//!   first reads the blob from there up to its offset, so no blob is checked from its middle.
//!   The read that would return the last byte fails instead when the md5s differ, and so does
//!   every later read of that blob, so the archive is never completed with it: the error names
//!   the entry and tells whether the file changed in the source (its metadata shows another
//!   content, as the relay asks) or the bytes read are not the listed ones. A changed size, a
//!   deleted file and a failed read fail the same way. A blob without an md5 is archived
//!   unchecked, as the relay copies one.
//! - The uploader compares the md5 of the bytes it read with Drive's md5 of the archive.
//! - A run that continues the session of an earlier run (the uploader's `record_in`, with
//!   [`session_identity`]) does so only when the layout digest is the same: the same bytes in
//!   memory (so the exports of the first run, see [`ExportsCache`]) and the same id, size and
//!   md5 of every blob. It sends only what the session does not hold yet, but first reads the
//!   part the session holds again from the source, to hash it (the uploader's own catch-up:
//!   the upload is not made with `Upload::trust_continued_content`): that part is downloaded a
//!   second time and uploaded once, and both checks above cover the whole archive. Bytes that
//!   an earlier run sent and that differ from the ones read now make Drive's md5 differ from
//!   the md5 of the bytes read; so do bytes sent again within a run (a chunk sent again, a new
//!   session after one expired). The session of an archive with a blob without md5 is never
//!   recorded, so it is never continued.
//!
//! Exports are not the same bytes twice, so the exports of an archive whose session is recorded
//! are kept in [`ExportsCache`] next to the session records, until the archive completes or the
//! cache is pruned with the stale records.

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter, Write as _},
    io,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::{
    common::parse_md5_digest,
    hub::Hub,
    // Warnings go above the progress bars.
    transfer::{
        progress::eprintln,
        relay::{account_of, DriveSource, RelayError, Requested},
        upload::{
            session::{self, SourceIdentity},
            Source, UploadError,
        },
    },
};

/// The media type of an archive.
pub(crate) const MEDIA_TYPE: &str = "application/x-tar";

/// The unit of a tar archive.
const BLOCK: u64 = 512;

/// The largest size the 11 octal digits of a ustar size field hold, 8 GiB - 1.
const MAX_OCTAL_SIZE: u64 = 0o77_777_777_777;

/// What ends an archive: two blocks of zeros.
const END_OF_ARCHIVE: [u8; 1024] = [0; 1024];

/// The bytes of the ustar name field.
const USTAR_NAME_LEN: usize = 100;

/// The name of the header that carries the pax records of the next entry, as Python's `tarfile`
/// names it: a fixed name, so that the archive's bytes do not depend on anything else.
const PAX_HEADER_NAME: &[u8] = b"././@PaxHeader";

/// One file or folder of an archive, in the order of the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The path in the archive: names joined with `/`, without a `/` at the end.
    pub(crate) path: String,
    /// The modification time, in seconds since the Unix epoch.
    pub(crate) mtime: u64,
    pub(crate) content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Content {
    Folder,
    /// Bytes held in memory: an export.
    Bytes(Bytes),
    /// A file of the source account, read when the upload asks for it.
    Blob(Blob),
}

/// A file of the source account in an archive: what reading it needs, as the listing gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Blob {
    pub(crate) id: String,
    pub(crate) size: u64,
    pub(crate) md5: Option<String>,
    pub(crate) resource_key: Option<String>,
    /// Compared before a file without md5 is read again.
    pub(crate) modified_time: Option<DateTime<Utc>>,
}

/// The bytes of an archive: segments one after the other, from offset 0 to [`Layout::size`].
#[derive(Debug)]
pub(crate) struct Layout {
    /// By offset; none is empty.
    segments: Vec<Segment>,
    blobs: Vec<LaidBlob>,
    size: u64,
}

#[derive(Debug)]
struct Segment {
    start: u64,
    part: Part,
}

#[derive(Debug)]
enum Part {
    Memory(Bytes),
    /// The blob of this index of [`Layout::blobs`].
    Blob(usize),
}

#[derive(Debug)]
struct LaidBlob {
    blob: Blob,
    /// The path of its entry, for errors.
    path: String,
}

impl Layout {
    /// The archive of `entries`, in their order, ended by two blocks of zeros.
    #[must_use]
    pub(crate) fn new(entries: Vec<Entry>) -> Self {
        let mut builder = Builder::default();
        for entry in entries {
            let is_folder = matches!(entry.content, Content::Folder);
            let size = match &entry.content {
                Content::Folder => 0,
                Content::Bytes(bytes) => bytes.len() as u64,
                Content::Blob(blob) => blob.size,
            };
            builder.memory(&header(&entry.path, is_folder, size, entry.mtime));
            match entry.content {
                Content::Folder => {}
                Content::Bytes(bytes) => builder.bytes(bytes),
                Content::Blob(blob) => builder.blob(blob, entry.path),
            }
            builder.memory(&vec![0; padding(size)]);
        }
        builder.memory(&END_OF_ARCHIVE);
        builder.finish()
    }

    /// The size of the archive.
    #[must_use]
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Whether every blob has an md5, which a continued session needs.
    #[must_use]
    pub(crate) fn checks_every_blob(&self) -> bool {
        self.blobs.iter().all(|laid| laid.blob.md5.is_some())
    }

    /// The hex sha256 of the layout: every byte held in memory, and the id, size and md5 of every
    /// blob, in the order of the archive.
    #[must_use]
    pub(crate) fn digest(&self) -> String {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"gdrive archive layout 1\n");
        for segment in &self.segments {
            match &segment.part {
                Part::Memory(bytes) => {
                    context.update(b"m");
                    field(&mut context, bytes);
                }
                Part::Blob(index) => {
                    let blob = &self.blobs[*index].blob;
                    context.update(b"b");
                    field(&mut context, blob.id.as_bytes());
                    context.update(&blob.size.to_be_bytes());
                    field(
                        &mut context,
                        blob.md5.as_deref().unwrap_or_default().as_bytes(),
                    );
                }
            }
        }
        hex(context.finish().as_ref())
    }

    /// The index of the segment that holds `offset` (below the size), and where it ends.
    fn segment_at(&self, offset: u64) -> (usize, u64) {
        let index = self
            .segments
            .partition_point(|segment| segment.start <= offset)
            .saturating_sub(1);
        let end = self
            .segments
            .get(index + 1)
            .map_or(self.size, |next| next.start);
        (index, end)
    }
}

/// Collects the segments of a [`Layout`]: bytes in memory that follow each other in one segment,
/// an export (already in memory) and a blob in segments of their own.
#[derive(Default)]
struct Builder {
    segments: Vec<Segment>,
    blobs: Vec<LaidBlob>,
    pending: Vec<u8>,
    size: u64,
}

impl Builder {
    fn memory(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let bytes = Bytes::from(std::mem::take(&mut self.pending));
            self.push(Part::Memory(bytes));
        }
    }

    fn bytes(&mut self, bytes: Bytes) {
        self.flush();
        if !bytes.is_empty() {
            self.push(Part::Memory(bytes));
        }
    }

    fn blob(&mut self, blob: Blob, path: String) {
        self.flush();
        if blob.size > 0 {
            self.blobs.push(LaidBlob { blob, path });
            self.push(Part::Blob(self.blobs.len() - 1));
        }
    }

    fn push(&mut self, part: Part) {
        let len = match &part {
            Part::Memory(bytes) => bytes.len() as u64,
            Part::Blob(index) => self.blobs[*index].blob.size,
        };
        self.segments.push(Segment {
            start: self.size,
            part,
        });
        self.size += len;
    }

    fn finish(mut self) -> Layout {
        self.flush();
        Layout {
            segments: self.segments,
            blobs: self.blobs,
            size: self.size,
        }
    }
}

/// The zeros after `size` bytes of content, up to the next block.
fn padding(size: u64) -> usize {
    // Below one block.
    usize::try_from((BLOCK - size % BLOCK) % BLOCK).unwrap_or_default()
}

/// The header of an entry: a ustar header, with a pax extended header before it when the path
/// or the size does not fit.
fn header(path: &str, is_folder: bool, size: u64, mtime: u64) -> Vec<u8> {
    let mut name = path.to_owned();
    if is_folder {
        name.push('/');
    }
    let mut records = Vec::new();
    let fits = name.is_ascii() && name.len() <= USTAR_NAME_LEN;
    if !fits {
        pax_record(&mut records, "path", name.as_bytes());
    }
    if size > MAX_OCTAL_SIZE {
        pax_record(&mut records, "size", size.to_string().as_bytes());
    }

    let mut header = Vec::new();
    if !records.is_empty() {
        let pax_size = records.len() as u64;
        header.extend_from_slice(&ustar(
            PAX_HEADER_NAME,
            tar::EntryType::XHeader,
            0o644,
            pax_size,
            mtime,
        ));
        header.extend_from_slice(&records);
        header.resize(header.len() + padding(pax_size), 0);
    }
    let (entry_type, mode) = if is_folder {
        (tar::EntryType::Directory, 0o755)
    } else {
        (tar::EntryType::Regular, 0o644)
    };
    let name = if fits {
        name.into_bytes()
    } else {
        fallback_name(&name)
    };
    header.extend_from_slice(&ustar(&name, entry_type, mode, size, mtime));
    header
}

/// The ustar name of an entry whose path is in a pax record, for readers that do not read pax:
/// the path with `_` for every character that is not ASCII, cut to the field.
fn fallback_name(name: &str) -> Vec<u8> {
    let mut fallback = name
        .chars()
        .map(|c| u8::try_from(c).ok().filter(u8::is_ascii).unwrap_or(b'_'))
        .collect::<Vec<_>>();
    fallback.truncate(USTAR_NAME_LEN);
    fallback
}

/// A ustar header block. The size goes in octal, or in base-256 when it does not fit (the
/// `tar` crate's rule); the user and group names and the device numbers stay empty.
fn ustar(name: &[u8], entry_type: tar::EntryType, mode: u32, size: u64, mtime: u64) -> [u8; 512] {
    let mut header = tar::Header::new_ustar();
    if let Some(ustar) = header.as_ustar_mut() {
        let len = name.len().min(ustar.name.len());
        ustar.name[..len].copy_from_slice(&name[..len]);
    }
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(size);
    header.set_mtime(mtime);
    header.set_entry_type(entry_type);
    header.set_cksum();
    *header.as_bytes()
}

/// Appends the pax record `<length> <key>=<value>\n`, whose length counts its own digits.
fn pax_record(records: &mut Vec<u8>, key: &str, value: &[u8]) {
    let rest = key.len() + value.len() + 3;
    let mut len = rest + digits(rest);
    while len != rest + digits(len) {
        len = rest + digits(len);
    }
    records.extend_from_slice(format!("{len} {key}=").as_bytes());
    records.extend_from_slice(value);
    records.push(b'\n');
}

fn digits(n: usize) -> usize {
    n.to_string().len()
}

/// The digest of what an archive is made from, computable from the listing alone (without
/// exporting anything): per entry, in the order of the archive, its path and kind; for a blob
/// its id, size and md5 (and its `modifiedTime` when it has no md5); for an exported Workspace
/// file its id, `modifiedTime` and export format. A destination file whose `appProperties`
/// carry it is that archive.
pub(crate) struct Manifest {
    context: ring::digest::Context,
}

impl Manifest {
    #[must_use]
    pub(crate) fn new() -> Self {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"gdrive archive manifest 1\n");
        Manifest { context }
    }

    pub(crate) fn folder(&mut self, path: &str) {
        self.context.update(b"d");
        field(&mut self.context, path.as_bytes());
    }

    pub(crate) fn blob(&mut self, path: &str, blob: &Blob) {
        self.context.update(b"f");
        field(&mut self.context, path.as_bytes());
        field(&mut self.context, blob.id.as_bytes());
        self.context.update(&blob.size.to_be_bytes());
        let md5 = blob.md5.as_deref().unwrap_or_default().to_ascii_lowercase();
        field(&mut self.context, md5.as_bytes());
        if blob.md5.is_none() {
            // Without md5, the modification time tells another content apart, as DriveSource
            // compares it before reading again.
            field(
                &mut self.context,
                rfc3339(blob.modified_time.as_ref()).as_bytes(),
            );
        }
    }

    /// A Workspace file `id`, modified at `modified_time`, exported as `format` (an extension).
    pub(crate) fn export(
        &mut self,
        path: &str,
        id: &str,
        modified_time: Option<&DateTime<Utc>>,
        format: &str,
    ) {
        self.context.update(b"e");
        field(&mut self.context, path.as_bytes());
        field(&mut self.context, id.as_bytes());
        field(&mut self.context, rfc3339(modified_time).as_bytes());
        field(&mut self.context, format.as_bytes());
    }

    /// The digest, in hex.
    #[must_use]
    pub(crate) fn finish(self) -> String {
        hex(self.context.finish().as_ref())
    }
}

/// `time` in RFC 3339 with milliseconds; empty without one.
fn rfc3339(time: Option<&DateTime<Utc>>) -> String {
    time.map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Feeds `bytes` to `context` after its length, so that fields cannot run into each other.
fn field(context: &mut ring::digest::Context, bytes: &[u8]) {
    context.update(&(bytes.len() as u64).to_be_bytes());
    context.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The md5 of a blob, read in order from its first byte, as far as it got.
struct Hashing {
    blob: usize,
    context: md5::Context,
    hashed: u64,
}

/// The bytes read at a time to hash the start of a blob that a read does not begin at.
const CATCH_UP: usize = 256 * 1024;

/// What is known of the md5 of a blob that has one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    /// Not compared yet.
    Pending,
    /// Its bytes have the listed md5.
    Verified,
    /// Its bytes have the md5 `actual`, not the listed one. `changed`: whether the metadata
    /// shows another content, `None` until that is known.
    Failed {
        actual: String,
        changed: Option<bool>,
    },
}

/// An archive as the uploader reads it: the bytes of its [`Layout`] at any offset, with the
/// blobs read from the source account through [`DriveSource`].
pub(crate) struct ArchiveSource<'a> {
    hub: &'a Hub,
    layout: &'a Layout,
    /// The blob being read and its reader: one reply of the source is open at a time.
    open: Option<(usize, DriveSource<'a>)>,
    /// What the reader of each blob found out before it was dropped, for the next one.
    requested: Vec<Option<Requested>>,
    hashing: Option<Hashing>,
    /// Per blob, its md5 check.
    checks: Vec<Check>,
}

impl<'a> ArchiveSource<'a> {
    /// The archive of `layout`, whose blobs are files that `hub` reads.
    #[must_use]
    pub(crate) fn new(hub: &'a Hub, layout: &'a Layout) -> Self {
        ArchiveSource {
            hub,
            layout,
            open: None,
            requested: vec![None; layout.blobs.len()],
            hashing: None,
            checks: vec![Check::Pending; layout.blobs.len()],
        }
    }

    async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, EntryFailure> {
        if buf.is_empty() || offset >= self.layout.size {
            return Ok(0);
        }
        let (index, end) = self.layout.segment_at(offset);
        let segment = &self.layout.segments[index];
        let len = usize::try_from(end - offset).map_or(buf.len(), |left| left.min(buf.len()));
        match &segment.part {
            Part::Memory(bytes) => {
                // Within the segment, which is in memory.
                let at = usize::try_from(offset - segment.start).unwrap_or_default();
                buf[..len].copy_from_slice(&bytes[at..at + len]);
                Ok(len)
            }
            Part::Blob(blob) => {
                self.read_blob(*blob, offset - segment.start, &mut buf[..len])
                    .await
            }
        }
    }

    /// Reads the blob `index` at `offset` of its content. A blob with an md5 is hashed from its
    /// first byte: a read past the bytes hashed so far (the first read of a continued session)
    /// reads what lies between from the source first. Once its md5 failed, every read of the
    /// blob fails.
    async fn read_blob(
        &mut self,
        index: usize,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, EntryFailure> {
        let layout = self.layout;
        let laid = &layout.blobs[index];
        let failure = |error| EntryFailure {
            path: laid.path.clone(),
            error,
        };
        let Some(expected) = laid.blob.md5.as_deref() else {
            // Unchecked, as the relay copies a file without md5.
            return self.read_source(index, offset, buf).await.map_err(failure);
        };
        match &self.checks[index] {
            Check::Pending => {}
            Check::Verified => return self.read_source(index, offset, buf).await.map_err(failure),
            Check::Failed { .. } => return Err(failure(self.mismatch(index, expected).await)),
        }

        if self
            .hashing
            .as_ref()
            .is_none_or(|hashing| hashing.blob != index)
        {
            self.hashing = Some(Hashing {
                blob: index,
                context: md5::Context::new(),
                hashed: 0,
            });
        }
        let mut scratch = Vec::new();
        while let Some(hashed) = self
            .hashing
            .as_ref()
            .map(|hashing| hashing.hashed)
            .filter(|&hashed| hashed < offset)
        {
            if scratch.is_empty() {
                scratch = vec![0; CATCH_UP];
            }
            let len = usize::try_from(offset - hashed)
                .map_or(scratch.len(), |left| left.min(scratch.len()));
            let read = self
                .read_source(index, hashed, &mut scratch[..len])
                .await
                .map_err(failure)?;
            // Only once the read returned: a read dropped on the way loses nothing.
            if let Some(hashing) = &mut self.hashing {
                hashing.context.consume(&scratch[..read]);
                hashing.hashed += read as u64;
            }
        }
        let read = self
            .read_source(index, offset, buf)
            .await
            .map_err(failure)?;

        let Some(hashing) = self
            .hashing
            .as_mut()
            .filter(|hashing| hashing.blob == index)
        else {
            // This call set the state for the blob, and nothing else ran since: never taken.
            return Err(failure(EntryError::Changed));
        };
        let end = offset + read as u64;
        if offset <= hashing.hashed && hashing.hashed < end {
            let skip = usize::try_from(hashing.hashed - offset).unwrap_or_default();
            hashing.context.consume(&buf[skip..read]);
            hashing.hashed = end;
        }
        if hashing.hashed < laid.blob.size {
            return Ok(read);
        }
        // The whole blob is hashed. Its outcome is kept before anything is awaited, so that a
        // read dropped while a mismatch is told apart does not lose it.
        let Some(done) = self.hashing.take() else {
            return Err(failure(EntryError::Changed));
        };
        let actual = done.context.compute();
        if parse_md5_digest(expected) == Some(actual) {
            self.checks[index] = Check::Verified;
            return Ok(read);
        }
        self.checks[index] = Check::Failed {
            actual: format!("{actual:x}"),
            changed: None,
        };
        Err(failure(self.mismatch(index, expected).await))
    }

    /// Reads the blob `index` at `offset` from the source; never 0 bytes.
    async fn read_source(
        &mut self,
        index: usize,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, EntryError> {
        let reader = self.reader(index).map_err(read_error)?;
        let read = reader
            .read_at(offset, buf)
            .await
            .map_err(|error| read_error(RelayError::of_upload(UploadError::Read(error))))?;
        if read == 0 {
            // The reader ends only past the size, which the offset is not.
            return Err(EntryError::Changed);
        }
        Ok(read)
    }

    /// Why the blob `index`, whose md5 failed, is not what was listed: it changed in the source
    /// (its metadata shows another content, as the relay asks), or the bytes read are not the
    /// listed ones. Asked once; a question dropped on the way is asked again.
    async fn mismatch(&mut self, index: usize, expected: &str) -> EntryError {
        let (actual, known) = match &self.checks[index] {
            Check::Failed { actual, changed } => (actual.clone(), *changed),
            Check::Pending | Check::Verified => return EntryError::Changed,
        };
        let changed = if let Some(changed) = known {
            changed
        } else {
            let changed = match self.reader(index) {
                Ok(reader) => reader.changed_since_listing().await,
                Err(_) => false,
            };
            if let Check::Failed { changed: slot, .. } = &mut self.checks[index] {
                *slot = Some(changed);
            }
            changed
        };
        if changed {
            EntryError::Changed
        } else {
            EntryError::Corrupt {
                expected: expected.to_ascii_lowercase(),
                actual,
            }
        }
    }

    /// The reader of the blob `index`: the open one, or a new one that takes over what the
    /// last reader of that blob found out.
    fn reader(&mut self, index: usize) -> Result<&mut DriveSource<'a>, RelayError> {
        if self.open.as_ref().is_none_or(|(open, _)| *open != index) {
            if let Some((closed, reader)) = self.open.take() {
                self.requested[closed] = reader.requested();
            }
            let blob = &self.layout.blobs[index].blob;
            let file = google_drive3::api::File {
                id: Some(blob.id.clone()),
                size: i64::try_from(blob.size).ok(),
                md5_checksum: blob.md5.clone(),
                modified_time: blob.modified_time,
                ..google_drive3::api::File::default()
            };
            let label = format!("download of '{}'", self.layout.blobs[index].path);
            let mut reader = DriveSource::new(
                self.hub,
                &file,
                (&blob.id, blob.resource_key.as_deref()),
                blob.size,
                label,
            )
            .map_err(RelayError::Source)?;
            if let Some(requested) = self.requested[index].take() {
                reader = reader.after(requested);
            }
            self.open = Some((index, reader));
        }
        match &mut self.open {
            Some((_, reader)) => Ok(reader),
            None => Err(RelayError::SourceChanged),
        }
    }
}

impl Source for ArchiveSource<'_> {
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.read(offset, buf).await.map_err(io::Error::other)
    }

    fn size(&mut self) -> impl std::future::Future<Output = io::Result<u64>> {
        std::future::ready(Ok(self.layout.size))
    }
}

/// A failed read of a blob, as the relay tells it: a source that changed apart.
fn read_error(error: RelayError) -> EntryError {
    match error {
        RelayError::SourceChanged => EntryError::Changed,
        error => EntryError::Read(Box::new(error)),
    }
}

/// Why an entry of an archive could not be archived as it was listed.
#[derive(Debug)]
pub(crate) enum EntryError {
    /// The file changed in the source (its size, md5 or content), or was deleted, since it was
    /// listed.
    Changed,
    /// The bytes read do not have the md5 Drive lists, and its metadata is unchanged.
    Corrupt { expected: String, actual: String },
    /// Reading it failed for good.
    Read(Box<RelayError>),
}

/// An entry that could not be archived.
#[derive(Debug)]
pub(crate) struct EntryFailure {
    /// Its path in the archive.
    pub(crate) path: String,
    pub(crate) error: EntryError,
}

impl Display for EntryFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        match &self.error {
            EntryError::Changed => write!(
                f,
                "'{path}' changed in the source while the archive was made; run the command again"
            ),
            EntryError::Corrupt { expected, actual } => write!(
                f,
                "the bytes read of '{path}' are not the ones the source lists (md5 {actual}, not \
                 {expected}); run the command again"
            ),
            EntryError::Read(_) => write!(
                f,
                "unable to read '{path}' from the source; run the command again"
            ),
        }
    }
}

impl Error for EntryFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.error {
            EntryError::Read(error) => Some(error.as_ref()),
            EntryError::Changed | EntryError::Corrupt { .. } => None,
        }
    }
}

/// A failed upload of an archive.
#[derive(Debug)]
pub(crate) enum ArchiveError {
    /// An entry could not be archived as it was listed.
    Entry(EntryFailure),
    /// Drive's md5 of the archive it holds is not the md5 of the bytes read.
    Md5Mismatch {
        file_id: String,
        local: String,
        remote: String,
    },
    Upload(UploadError),
}

impl ArchiveError {
    /// The error of a failed upload of an archive: a failed read is the entry's.
    #[must_use]
    pub(crate) fn of_upload(error: UploadError) -> Self {
        match error {
            UploadError::Read(error) => {
                let is_entry = error
                    .get_ref()
                    .is_some_and(<dyn Error + Send + Sync>::is::<EntryFailure>);
                if !is_entry {
                    return ArchiveError::Upload(UploadError::Read(error));
                }
                let kind = error.kind();
                match error
                    .into_inner()
                    .map(<dyn Error + Send + Sync>::downcast::<EntryFailure>)
                {
                    Some(Ok(failure)) => ArchiveError::Entry(*failure),
                    Some(Err(inner)) => {
                        ArchiveError::Upload(UploadError::Read(io::Error::new(kind, inner)))
                    }
                    None => ArchiveError::Upload(UploadError::Read(io::Error::from(kind))),
                }
            }
            UploadError::Md5Mismatch {
                file_id,
                local,
                remote,
            } => ArchiveError::Md5Mismatch {
                file_id,
                local,
                remote,
            },
            error => ArchiveError::Upload(error),
        }
    }
}

impl Display for ArchiveError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::Entry(failure) => failure.fmt(f),
            ArchiveError::Md5Mismatch {
                file_id,
                local,
                remote,
            } => write!(
                f,
                "the archive in the destination (id {file_id}) is not what was sent: its md5 is \
                 {remote}, the bytes sent have {local}; delete it in the destination and run the \
                 command again"
            ),
            ArchiveError::Upload(_) => f.write_str("unable to upload the archive"),
        }
    }
}

impl Error for ArchiveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ArchiveError::Entry(failure) => failure.source(),
            ArchiveError::Upload(error) => Some(error),
            ArchiveError::Md5Mismatch { .. } => None,
        }
    }
}

/// The identity of the archive of the source file or folder `root_id` in a session record, if
/// the archive can be continued: every blob has an md5 and Drive names the account of `source`.
/// Warnings name the archive `label`.
pub(crate) async fn session_identity(
    source: &Hub,
    root_id: &str,
    layout: &Layout,
    label: &str,
) -> Option<SourceIdentity> {
    if !layout.checks_every_blob() {
        return None;
    }
    match account_of(source).await {
        Ok(Some(account)) => Some(SourceIdentity::archive(
            &account,
            root_id,
            layout.size(),
            &layout.digest(),
        )),
        Ok(None) => {
            eprintln!(
                "Warning: {label}: Drive does not name the account of the source, an interrupted \
                 archive will start over"
            );
            None
        }
        Err(_) => {
            eprintln!(
                "Warning: {label}: unable to look up the Google account of the source, an \
                 interrupted archive will start over"
            );
            None
        }
    }
}

/// The export of a Workspace file in an archive, as the cache keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CachedExport {
    Content(Bytes),
    /// Larger than Drive exports: the file is not in the archive.
    TooLarge,
}

/// The exports of one archive, kept in the directory of the session records so that a run that
/// continues the archive's session makes the same bytes: `archive-<manifest digest>.exports`,
/// private like the records (0600, written to a temporary file and renamed), ending with the
/// sha256 of what comes before it. A file that does not end with its own digest is not used.
pub(crate) struct ExportsCache {
    path: PathBuf,
}

/// The first bytes of a cache file.
const CACHE_MAGIC: &[u8] = b"gdrive archive exports 1\n";

const CACHE_CONTENT: u8 = 0;
const CACHE_TOO_LARGE: u8 = 1;

impl ExportsCache {
    /// The cache of the archive with the manifest digest `manifest` (hex) in `dir`.
    #[must_use]
    pub(crate) fn new(dir: &Path, manifest: &str) -> Self {
        ExportsCache {
            path: dir.join(format!("archive-{manifest}.{}", session::EXPORTS_EXTENSION)),
        }
    }

    /// The exports by file id, if there is a cache file that is whole.
    pub(crate) async fn load(&self) -> Option<BTreeMap<String, CachedExport>> {
        let content = tokio::fs::read(&self.path).await.ok()?;
        decode(&content)
    }

    /// Writes `exports` by file id. A failure is a warning: the archive goes on, and a later
    /// run starts it over.
    pub(crate) async fn save(&self, exports: &BTreeMap<String, CachedExport>) {
        if let Err(error) = session::write_private(&self.path, &encode(exports)).await {
            eprintln!(
                "Warning: unable to keep the exports of the archive in '{}', an interrupted \
                 archive will start over: {error}",
                self.path.display()
            );
        }
    }

    /// Deletes the cache: its archive is complete.
    pub(crate) async fn remove(&self) {
        // One that cannot be deleted is pruned later.
        let _ = tokio::fs::remove_file(&self.path).await;
    }
}

fn encode(exports: &BTreeMap<String, CachedExport>) -> Vec<u8> {
    let mut content = CACHE_MAGIC.to_vec();
    content.extend_from_slice(&(exports.len() as u64).to_be_bytes());
    for (id, export) in exports {
        content.extend_from_slice(&(id.len() as u64).to_be_bytes());
        content.extend_from_slice(id.as_bytes());
        match export {
            CachedExport::Content(bytes) => {
                content.push(CACHE_CONTENT);
                content.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
                content.extend_from_slice(bytes);
            }
            CachedExport::TooLarge => content.push(CACHE_TOO_LARGE),
        }
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &content);
    content.extend_from_slice(digest.as_ref());
    content
}

fn decode(content: &[u8]) -> Option<BTreeMap<String, CachedExport>> {
    let digest_len = ring::digest::SHA256.output_len();
    let (body, digest) = content.split_at_checked(content.len().checked_sub(digest_len)?)?;
    if ring::digest::digest(&ring::digest::SHA256, body).as_ref() != digest {
        return None;
    }
    let mut reader = Reader(body.strip_prefix(CACHE_MAGIC)?);
    let mut exports = BTreeMap::new();
    for _ in 0..reader.number()? {
        let id = String::from_utf8(reader.bytes()?.to_vec()).ok()?;
        let export = match *reader.take(1)?.first()? {
            CACHE_CONTENT => CachedExport::Content(Bytes::copy_from_slice(reader.bytes()?)),
            CACHE_TOO_LARGE => CachedExport::TooLarge,
            _ => return None,
        };
        exports.insert(id, export);
    }
    reader.0.is_empty().then_some(exports)
}

/// Reads the fields of a cache file.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(taken)
    }

    fn number(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    /// Bytes after their length.
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.number()?).ok()?;
        self.take(len)
    }
}
