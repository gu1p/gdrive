//! The layout of an archive, read back with the `tar` crate, and [`ArchiveSource`] reading the
//! blobs of a fake Drive.

use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;

use super::{
    decode, encode, field, header, hex, pax_record, ArchiveError, ArchiveSource, Blob,
    CachedExport, Content, Entry, EntryError, EntryFailure, ExportsCache, Layout, Manifest, Part,
    END_OF_ARCHIVE,
};
use crate::transfer::{
    fake_drive::{Behavior, FakeDrive, FakeFile},
    relay::RelayError,
    upload::{Source, UploadError},
};

const MTIME: u64 = 1_700_000_000;

fn folder(path: &str) -> Entry {
    Entry {
        path: path.to_owned(),
        mtime: MTIME,
        content: Content::Folder,
    }
}

fn bytes(path: &str, content: &[u8]) -> Entry {
    Entry {
        path: path.to_owned(),
        mtime: MTIME + 1,
        content: Content::Bytes(Bytes::copy_from_slice(content)),
    }
}

fn blob_of(path: &str, file: &FakeFile) -> Entry {
    Entry {
        path: path.to_owned(),
        mtime: MTIME + 2,
        content: Content::Blob(Blob {
            id: file.id.clone(),
            size: file.size.unwrap(),
            md5: file.md5.clone(),
            resource_key: None,
            modified_time: None,
        }),
    }
}

/// The bytes of `layout`, with the content of each blob from `blobs` by id.
fn materialize(layout: &Layout, blobs: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    let mut archive = Vec::new();
    for segment in &layout.segments {
        assert_eq!(segment.start, archive.len() as u64);
        match &segment.part {
            Part::Memory(bytes) => archive.extend_from_slice(bytes),
            Part::Blob(index) => {
                let blob = &layout.blobs[*index].blob;
                archive.extend_from_slice(&blobs[&blob.id]);
            }
        }
    }
    assert_eq!(archive.len() as u64, layout.size());
    archive
}

/// An entry as the `tar` crate reads it.
#[derive(Debug, PartialEq, Eq)]
struct Unpacked {
    path: String,
    directory: bool,
    size: u64,
    mtime: u64,
    mode: u32,
    content: Vec<u8>,
}

fn unpack(archive: &[u8]) -> Vec<Unpacked> {
    let mut reader = tar::Archive::new(archive);
    reader
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            assert_eq!(header.uid().unwrap(), 0);
            assert_eq!(header.gid().unwrap(), 0);
            assert_eq!(header.username_bytes(), Some(&b""[..]));
            assert_eq!(header.groupname_bytes(), Some(&b""[..]));
            assert!(header.as_ustar().is_some(), "a ustar header");
            let path = String::from_utf8(entry.path_bytes().into_owned()).unwrap();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            Unpacked {
                path,
                directory: header.entry_type().is_dir(),
                size: entry.size(),
                mtime: header.mtime().unwrap(),
                mode: header.mode().unwrap(),
                content,
            }
        })
        .collect()
}

fn unpacked(path: &str, directory: bool, mtime: u64, content: &[u8]) -> Unpacked {
    Unpacked {
        path: path.to_owned(),
        directory,
        size: content.len() as u64,
        mtime,
        mode: if directory { 0o755 } else { 0o644 },
        content: content.to_vec(),
    }
}

#[test]
fn a_tree_unpacks_with_its_names_kinds_sizes_times_and_content() {
    let long_name = "n".repeat(200);
    let long_dir = format!("photos/{long_name}");
    let longer = format!("{long_dir}/{long_name}.txt");
    assert!(longer.len() > 255);
    let entries = vec![
        folder("photos"),
        bytes("photos/a.txt", b"aaaa"),
        folder("photos/empty"),
        bytes("photos/r\u{e9}sum\u{e9} \u{65e5}\u{672c}.txt", b"unicode"),
        folder(&long_dir),
        bytes(&longer, &[7; 1000]),
        bytes("photos/exactly-512", &[1; 512]),
        bytes("photos/empty.txt", b""),
    ];
    let layout = Layout::new(entries);
    let archive = materialize(&layout, &HashMap::new());

    assert_eq!(archive.len() % 512, 0);
    assert!(archive.ends_with(&END_OF_ARCHIVE));
    assert_eq!(
        unpack(&archive),
        [
            unpacked("photos/", true, MTIME, b""),
            unpacked("photos/a.txt", false, MTIME + 1, b"aaaa"),
            unpacked("photos/empty/", true, MTIME, b""),
            unpacked(
                "photos/r\u{e9}sum\u{e9} \u{65e5}\u{672c}.txt",
                false,
                MTIME + 1,
                b"unicode"
            ),
            unpacked(&format!("{long_dir}/"), true, MTIME, b""),
            unpacked(&longer, false, MTIME + 1, &[7; 1000]),
            unpacked("photos/exactly-512", false, MTIME + 1, &[1; 512]),
            unpacked("photos/empty.txt", false, MTIME + 1, b""),
        ]
    );
}

#[test]
fn only_paths_that_do_not_fit_ustar_get_a_pax_header() {
    let plain = header("photos/a.txt", false, 4, MTIME);
    assert_eq!(plain.len(), 512, "a ustar header alone");
    assert_eq!(&plain[..12], b"photos/a.txt");
    assert_eq!(plain[156], b'0');
    assert_eq!(&plain[257..265], b"ustar\x0000");

    let hundred = "a".repeat(100);
    assert_eq!(header(&hundred, false, 0, MTIME).len(), 512);
    let longer = "a".repeat(101);
    let with_pax = header(&longer, false, 0, MTIME);
    assert_eq!(
        with_pax.len(),
        3 * 512,
        "pax header, its records, ustar header"
    );
    assert_eq!(with_pax[156], b'x');
    let records = &with_pax[512..1024];
    assert!(records.starts_with(format!("111 path={longer}\n").as_bytes()));
    // The ustar name is the start of the path.
    assert_eq!(&with_pax[1024..1124], &longer.as_bytes()[..100]);

    let unicode = header("d\u{e9}j\u{e0}", true, 0, MTIME);
    assert_eq!(unicode[156], b'x');
    assert!(unicode[512..].starts_with("16 path=d\u{e9}j\u{e0}/\n".as_bytes()));
    assert_eq!(
        &unicode[1024..1031],
        b"d_j_/\0\0",
        "ASCII in the ustar name"
    );
    assert_eq!(unicode[1024 + 156], b'5');
}

#[test]
fn pax_records_count_their_own_digits() {
    for value_len in 0..2000 {
        let mut records = Vec::new();
        pax_record(&mut records, "path", "x".repeat(value_len).as_bytes());
        let text = String::from_utf8(records).unwrap();
        let (len, _) = text.split_once(' ').unwrap();
        assert_eq!(len.parse::<usize>().unwrap(), text.len(), "{text:?}");
    }
}

/// The bytes of a layout with every blob made of zeros, read and seeked like a file, so that
/// the `tar` crate can list an archive of a huge blob without reading it.
struct Zeros<'a> {
    layout: &'a Layout,
    position: u64,
}

impl Read for Zeros<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.layout.size() || buf.is_empty() {
            return Ok(0);
        }
        let (index, end) = self.layout.segment_at(self.position);
        let segment = &self.layout.segments[index];
        let len = usize::try_from(end - self.position)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        match &segment.part {
            Part::Memory(bytes) => {
                let at = usize::try_from(self.position - segment.start).unwrap();
                buf[..len].copy_from_slice(&bytes[at..at + len]);
            }
            Part::Blob(_) => buf[..len].fill(0),
        }
        self.position += len as u64;
        Ok(len)
    }
}

impl Seek for Zeros<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = match position {
            SeekFrom::Start(at) => at,
            SeekFrom::Current(by) => self.position.checked_add_signed(by).unwrap(),
            SeekFrom::End(by) => self.layout.size().checked_add_signed(by).unwrap(),
        };
        Ok(self.position)
    }
}

#[test]
fn a_9_gib_entry_has_its_size_in_pax_and_base_256() {
    const SIZE: u64 = 9 << 30;
    let huge = Blob {
        id: String::from("huge"),
        size: SIZE,
        md5: None,
        resource_key: None,
        modified_time: None,
    };
    let layout = Layout::new(vec![
        folder("big"),
        Entry {
            path: String::from("big/disk.img"),
            mtime: MTIME,
            content: Content::Blob(huge),
        },
        bytes("big/after.txt", b"after"),
    ]);

    let header = header("big/disk.img", false, SIZE, MTIME);
    assert_eq!(header[156], b'x');
    assert!(header[512..].starts_with(b"19 size=9663676416\n"));
    let size_field = &header[1024 + 124..1024 + 136];
    assert_eq!(size_field[0], 0x80, "base-256 in the ustar header too");
    assert_eq!(&size_field[4..], &SIZE.to_be_bytes());

    let mut archive = tar::Archive::new(Zeros {
        layout: &layout,
        position: 0,
    });
    let mut listed = Vec::new();
    for entry in archive.entries_with_seek().unwrap() {
        let mut entry = entry.unwrap();
        let path = String::from_utf8(entry.path_bytes().into_owned()).unwrap();
        let mut content = Vec::new();
        if entry.size() < 100 {
            entry.read_to_end(&mut content).unwrap();
        }
        listed.push((path, entry.size(), content));
    }
    assert_eq!(
        listed,
        [
            (String::from("big/"), 0, Vec::new()),
            (String::from("big/disk.img"), SIZE, Vec::new()),
            (String::from("big/after.txt"), 5, b"after".to_vec()),
        ]
    );
    assert_eq!(layout.size(), 512 + 3 * 512 + SIZE + 1024 + 1024);
}

fn sample() -> Vec<Entry> {
    vec![
        folder("root"),
        bytes("root/doc.docx", b"export"),
        Entry {
            path: String::from("root/blob.bin"),
            mtime: MTIME,
            content: Content::Blob(Blob {
                id: String::from("blob"),
                size: 700,
                md5: Some(String::from("0123456789abcdef0123456789abcdef")),
                resource_key: None,
                modified_time: None,
            }),
        },
    ]
}

/// The blob of [`sample`].
fn blob(entries: &[Entry]) -> Blob {
    match &entries[2].content {
        Content::Blob(blob) => blob.clone(),
        _ => unreachable!(),
    }
}

#[test]
fn the_same_entries_give_the_same_layout_and_digest() {
    let one = Layout::new(sample());
    let two = Layout::new(sample());
    assert_eq!(one.size(), two.size());
    assert_eq!(one.digest(), two.digest());
    assert_eq!(one.digest().len(), 64);

    let changed = |change: fn(&mut Vec<Entry>)| {
        let mut entries = sample();
        change(&mut entries);
        Layout::new(entries).digest()
    };
    let base = one.digest();
    for other in [
        changed(|entries| entries[1] = bytes("root/doc.docx", b"exporT")),
        changed(|entries| entries[0].mtime += 1),
        changed(|entries| {
            entries[2].content = Content::Blob(Blob {
                md5: Some(String::from("ffffffffffffffffffffffffffffffff")),
                ..blob(entries)
            });
        }),
        changed(|entries| {
            entries[2].content = Content::Blob(Blob {
                id: String::from("other"),
                ..blob(entries)
            });
        }),
        changed(|entries| {
            entries[2].content = Content::Blob(Blob {
                size: 701,
                ..blob(entries)
            });
        }),
    ] {
        assert_ne!(other, base);
    }
    assert!(one.checks_every_blob());
    let without_md5 = Layout::new(vec![Entry {
        content: Content::Blob(Blob {
            md5: None,
            id: String::from("x"),
            size: 1,
            resource_key: None,
            modified_time: None,
        }),
        ..folder("x")
    }]);
    assert!(!without_md5.checks_every_blob());
}

/// Text content of `len` bytes, a run of the alphabet from `seed` on.
fn content(len: usize, seed: u8) -> String {
    (0..len)
        .map(|i| char::from(b'a' + u8::try_from((i + usize::from(seed)) % 26).unwrap()))
        .collect()
}

/// `root/` with an export and three blobs, on a fake source; the archive and its reference
/// bytes.
async fn source_archive(files: Vec<FakeFile>, behavior: Behavior) -> (FakeDrive, Layout, Vec<u8>) {
    let mut entries = vec![folder("root"), bytes("root/doc.docx", b"exported document")];
    let mut contents = HashMap::new();
    for file in &files {
        entries.push(blob_of(&format!("root/{}", file.name), file));
        contents.insert(
            file.id.clone(),
            file.content.clone().unwrap_or_default().into_bytes(),
        );
    }
    let layout = Layout::new(entries);
    let reference = materialize(&layout, &contents);
    let drive = FakeDrive::start(files, behavior).await;
    (drive, layout, reference)
}

fn three_blobs() -> Vec<FakeFile> {
    vec![
        FakeFile::blob("one", "one.txt", "root", &content(3000, 1)),
        FakeFile::blob("two", "two.txt", "root", &content(511, 2)),
        FakeFile::blob("three", "three.txt", "root", &content(5000, 3)),
    ]
}

async fn read_all(source: &mut ArchiveSource<'_>, piece: usize, size: u64) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut buf = vec![0; piece];
    loop {
        let read = source
            .read_at(archive.len() as u64, &mut buf)
            .await
            .unwrap();
        if read == 0 {
            break;
        }
        archive.extend_from_slice(&buf[..read]);
    }
    assert_eq!(archive.len() as u64, size);
    archive
}

#[tokio::test]
async fn reads_at_any_offset_and_of_any_length_give_the_archive() {
    let (drive, layout, reference) = source_archive(three_blobs(), Behavior::default()).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    assert_eq!(source.size().await.unwrap(), layout.size());

    for piece in [1 << 20, 7, 512] {
        assert_eq!(read_all(&mut source, piece, layout.size()).await, reference);
    }
    let mut random = fastrand::Rng::with_seed(7);
    for _ in 0..200 {
        let offset = random.u64(0..layout.size());
        let mut buf = vec![0; random.usize(1..4000)];
        let read = source.read_at(offset, &mut buf).await.unwrap();
        assert!(read > 0);
        let at = usize::try_from(offset).unwrap();
        assert_eq!(&buf[..read], &reference[at..at + read], "at {offset}");
    }
    assert_eq!(
        source.read_at(layout.size(), &mut [0; 10]).await.unwrap(),
        0
    );

    let names = unpack(&reference)
        .into_iter()
        .map(|entry| (entry.path, entry.content.len()))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            (String::from("root/"), 0),
            (String::from("root/doc.docx"), 17),
            (String::from("root/one.txt"), 3000),
            (String::from("root/two.txt"), 511),
            (String::from("root/three.txt"), 5000),
        ]
    );
    assert_eq!(drive.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn a_blob_read_again_after_the_next_one_is_resumed_at_the_offset() {
    let (drive, layout, reference) = source_archive(three_blobs(), Behavior::default()).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    assert_eq!(read_all(&mut source, 4096, layout.size()).await, reference);
    let media_before = drive.media_ranges().len();
    let gets_before = drive.gets().len();

    // A chunk that failed is sent again from the middle of the first blob.
    let first = layout
        .segments
        .iter()
        .find_map(|segment| match segment.part {
            Part::Blob(0) => Some(segment.start),
            _ => None,
        });
    let offset = first.unwrap() + 1000;
    let mut again = vec![0; 3000];
    let mut read = 0;
    while read < again.len() {
        read += source
            .read_at(offset + read as u64, &mut again[read..])
            .await
            .unwrap();
    }
    let at = usize::try_from(offset).unwrap();
    assert_eq!(again, &reference[at..at + 3000]);

    let media = drive.media_ranges()[media_before..].to_vec();
    assert_eq!(
        media,
        [
            (String::from("one"), Some(String::from("bytes=1000-"))),
            (String::from("two"), None),
        ]
    );
    // Read again only once the metadata showed the same content.
    assert_eq!(drive.gets()[gets_before..], ["one", "two"]);
}

async fn read_failure(
    files: Vec<FakeFile>,
    listed: Vec<FakeFile>,
    behavior: Behavior,
) -> EntryFailure {
    let mut entries = vec![folder("root")];
    for file in &listed {
        entries.push(blob_of(&format!("root/{}", file.name), file));
    }
    let layout = Layout::new(entries);
    let drive = FakeDrive::start(files, behavior).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    let mut offset = 0;
    let mut buf = vec![0; 1000];
    let error = loop {
        match source.read_at(offset, &mut buf).await {
            Ok(0) => panic!("the archive was read whole"),
            Ok(read) => offset += read as u64,
            Err(error) => break error,
        }
    };
    match ArchiveError::of_upload(UploadError::Read(error)) {
        ArchiveError::Entry(failure) => failure,
        error => panic!("{error:?}"),
    }
}

fn message(error: &(dyn std::error::Error + 'static)) -> String {
    crate::error_message(error)
}

#[tokio::test]
async fn bytes_that_are_not_the_listed_md5_fail_the_entry_as_corrupt() {
    let listed = FakeFile::blob("one", "one.txt", "root", "listed content");
    // Drive still lists that md5, but sends other bytes.
    let served = FakeFile {
        content: Some(String::from("served content")),
        ..listed.clone()
    };
    let failure = read_failure(vec![served], vec![listed.clone()], Behavior::default()).await;

    assert_eq!(failure.path, "root/one.txt");
    let EntryError::Corrupt { expected, actual } = &failure.error else {
        panic!("{failure:?}");
    };
    assert_eq!(Some(expected), listed.md5.as_ref());
    assert_eq!(*actual, format!("{:x}", md5::compute("served content")));
    let text = message(&failure);
    assert!(text.contains("'root/one.txt'"), "{text}");
    assert!(text.ends_with("run the command again"), "{text}");
}

#[tokio::test]
async fn a_file_that_changed_since_the_listing_fails_the_entry_as_changed() {
    let listed = FakeFile::blob("one", "one.txt", "root", "listed content");
    let now = FakeFile::blob("one", "one.txt", "root", "edited content");
    let failure = read_failure(vec![now], vec![listed], Behavior::default()).await;
    assert!(matches!(failure.error, EntryError::Changed), "{failure:?}");
    assert_eq!(
        message(&failure),
        "Error: 'root/one.txt' changed in the source while the archive was made; run the command \
         again"
    );

    // Another size: the reply is not what the listing describes.
    let listed = FakeFile::blob("one", "one.txt", "root", "short");
    let now = FakeFile::blob("one", "one.txt", "root", "much longer now");
    let failure = read_failure(vec![now], vec![listed], Behavior::default()).await;
    assert!(matches!(failure.error, EntryError::Changed), "{failure:?}");
}

#[tokio::test]
async fn a_deleted_file_fails_the_entry_with_the_read_error() {
    let file = FakeFile::blob("one", "one.txt", "root", "content");
    let behavior = Behavior {
        media_missing: HashMap::from([(String::from("one"), std::time::Duration::ZERO)]),
        ..Behavior::default()
    };
    let failure = read_failure(vec![file.clone()], vec![file], behavior).await;
    assert!(
        matches!(&failure.error, EntryError::Read(error) if matches!(**error, RelayError::Source(_))),
        "{failure:?}"
    );
    let text = message(&failure);
    assert!(
        text.starts_with(
            "Error: unable to read 'root/one.txt' from the source; run the command again: "
        ),
        "{text}"
    );
}

#[tokio::test]
async fn a_blob_without_md5_is_read_unchecked() {
    let file = FakeFile {
        md5: None,
        ..FakeFile::blob("one", "one.txt", "root", "content")
    };
    let (drive, layout, reference) = source_archive(vec![file], Behavior::default()).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    assert_eq!(read_all(&mut source, 100, layout.size()).await, reference);
    assert!(!layout.checks_every_blob());
}

#[tokio::test]
async fn the_exports_cache_keeps_its_exports_privately_and_refuses_a_damaged_file() {
    let dir = tempfile::tempdir().unwrap();
    let uploads = dir.path().join("uploads");
    let cache = ExportsCache::new(&uploads, "abc123");
    assert_eq!(
        cache.path,
        uploads.join("archive-abc123.exports"),
        "next to the session records"
    );
    assert_eq!(cache.load().await, None);

    let exports = BTreeMap::from([
        (
            String::from("doc"),
            CachedExport::Content(Bytes::from_static(b"docx bytes")),
        ),
        (String::from("big"), CachedExport::TooLarge),
        (String::from("empty"), CachedExport::Content(Bytes::new())),
    ]);
    cache.save(&exports).await;
    assert_eq!(cache.load().await, Some(exports.clone()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cache.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let mut content = std::fs::read(&cache.path).unwrap();
    let last = content.len() - 40;
    content[last] ^= 1;
    std::fs::write(&cache.path, &content).unwrap();
    assert_eq!(cache.load().await, None, "a damaged cache is not used");
    assert_eq!(decode(&encode(&exports)), Some(exports));
    assert_eq!(decode(b"short"), None);

    cache.remove().await;
    assert!(!cache.path.exists());
}

/// Where the content of the blob `index` of `layout` starts in the archive.
fn blob_start(layout: &Layout, index: usize) -> u64 {
    layout
        .segments
        .iter()
        .find_map(|segment| match segment.part {
            Part::Blob(blob) if blob == index => Some(segment.start),
            _ => None,
        })
        .unwrap()
}

/// The entry failure of an error of [`ArchiveSource::read_at`].
fn entry_failure(error: io::Error) -> EntryFailure {
    match ArchiveError::of_upload(UploadError::Read(error)) {
        ArchiveError::Entry(failure) => failure,
        error => panic!("{error:?}"),
    }
}

/// A blob of 3000 bytes whose listing (and metadata) gives the md5 of other bytes than Drive
/// sends: the listed file and the one the fake serves.
fn wrong_blob() -> (FakeFile, FakeFile) {
    let listed = FakeFile::blob("one", "one.txt", "root", &content(3000, 1));
    let served = FakeFile {
        content: Some(content(3000, 2)),
        ..listed.clone()
    };
    (listed, served)
}

#[tokio::test]
async fn a_failed_md5_is_kept_when_the_read_is_dropped_while_it_is_told_apart() {
    let (listed, served) = wrong_blob();
    let layout = Layout::new(vec![folder("root"), blob_of("root/one.txt", &listed)]);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let behavior = Behavior {
        metadata_gate: Some(Arc::clone(&gate)),
        ..Behavior::default()
    };
    let drive = FakeDrive::start(vec![served], behavior).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    let start = blob_start(&layout, 0);
    let mut buf = vec![0; 1000];
    for piece in 0..2 {
        let read = source
            .read_at(start + piece * 1000, &mut buf)
            .await
            .unwrap();
        assert_eq!(read, 1000);
    }

    // The last piece completes the md5, which is not the listed one; while the metadata is
    // asked whether the file changed, the uploader drops the read.
    let last = start + 2000;
    {
        let mut read = Box::pin(source.read_at(last, &mut buf));
        let asked = async {
            while drive.gets().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::select! {
            result = &mut read => panic!("the read should wait for the metadata: {result:?}"),
            () = asked => {}
        }
    }
    gate.add_permits(10);

    // The piece sent again fails, and so does any other read of the blob.
    for offset in [last, start + 500, start] {
        let failure = entry_failure(source.read_at(offset, &mut buf).await.unwrap_err());
        assert_eq!(failure.path, "root/one.txt");
        assert!(
            matches!(&failure.error, EntryError::Corrupt { expected, .. } if Some(expected) == listed.md5.as_ref()),
            "{failure:?}"
        );
    }
    assert_eq!(
        drive.gets(),
        ["one", "one"],
        "told apart once it is answered"
    );
}

#[tokio::test]
async fn a_read_inside_a_blob_hashes_it_from_its_first_byte() {
    // A good blob: the first read inside it reads its start first, in the same reply.
    let (drive, layout, reference) = source_archive(three_blobs(), Behavior::default()).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    let offset = blob_start(&layout, 0) + 1000;
    let mut archive = Vec::new();
    let mut buf = vec![0; 4096];
    loop {
        let read = source
            .read_at(offset + archive.len() as u64, &mut buf)
            .await
            .unwrap();
        if read == 0 {
            break;
        }
        archive.extend_from_slice(&buf[..read]);
    }
    let at = usize::try_from(offset).unwrap();
    assert_eq!(archive, &reference[at..]);
    assert_eq!(
        drive.media_ranges(),
        [
            (String::from("one"), None),
            (String::from("two"), None),
            (String::from("three"), None),
        ]
    );

    // A blob whose bytes are not the listed ones fails, though no read starts at its first byte.
    let (listed, served) = wrong_blob();
    let layout = Layout::new(vec![folder("root"), blob_of("root/one.txt", &listed)]);
    let drive = FakeDrive::start(vec![served], Behavior::default()).await;
    let hub = drive.hub();
    let mut source = ArchiveSource::new(&hub, &layout);
    let mut offset = blob_start(&layout, 0) + 1000;
    let error = loop {
        match source.read_at(offset, &mut buf).await {
            Ok(0) => panic!("the archive was read whole"),
            Ok(read) => offset += read as u64,
            Err(error) => break error,
        }
    };
    let failure = entry_failure(error);
    assert_eq!(failure.path, "root/one.txt");
    assert!(
        matches!(failure.error, EntryError::Corrupt { .. }),
        "{failure:?}"
    );
}

#[test]
fn a_blob_without_md5_is_told_apart_by_its_modification_time_in_the_manifest() {
    let digest = |md5: Option<&str>, modified: &str| {
        let mut manifest = Manifest::new();
        manifest.blob(
            "root/a.bin",
            &Blob {
                id: String::from("a"),
                size: 3,
                md5: md5.map(str::to_owned),
                resource_key: None,
                modified_time: Some(modified.parse().unwrap()),
            },
        );
        manifest.finish()
    };
    let (one, two) = ("2025-01-01T00:00:00Z", "2025-01-02T00:00:00Z");
    assert_ne!(digest(None, one), digest(None, two));
    assert_eq!(digest(None, one), digest(None, one));

    // With an md5, the time is no part of it: the input is what it was.
    let md5 = "0123456789ABCDEF0123456789abcdef";
    assert_eq!(digest(Some(md5), one), digest(Some(md5), two));
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(b"gdrive archive manifest 1\n");
    context.update(b"f");
    field(&mut context, b"root/a.bin");
    field(&mut context, b"a");
    context.update(&3_u64.to_be_bytes());
    field(&mut context, md5.to_ascii_lowercase().as_bytes());
    assert_eq!(digest(Some(md5), one), hex(context.finish().as_ref()));
}

#[test]
fn a_blob_without_md5_or_modification_time_makes_its_manifest_unlike_any_other() {
    let digest = |modified_time| {
        let mut manifest = Manifest::new();
        manifest.folder("root");
        manifest.blob(
            "root/a.bin",
            &Blob {
                id: String::from("a"),
                size: 3,
                md5: None,
                resource_key: None,
                modified_time,
            },
        );
        manifest.finish()
    };
    // The same listing twice: nothing would tell an edited file from the archived one.
    assert_ne!(digest(None), digest(None));
    let time = Some("2025-01-01T00:00:00Z".parse().unwrap());
    assert_eq!(digest(time), digest(time));
}
