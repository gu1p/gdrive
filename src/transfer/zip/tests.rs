//! The reader on zip files of real tools (`testdata/`, made by Info-ZIP's `zip` and by Python's
//! `zipfile` with its ZIP64 limits lowered) and on zip files written field by field.

use std::io::{self, Cursor};

use chrono::{TimeZone, Utc};

use super::{
    read_directory,
    testing::{noise, zip64_of, zip_of, Written},
    Entry, EntrySource, ZipError, CP850, STEP,
};
use crate::transfer::upload::{ReaderSource, Source};

const WINDOW: u64 = 1024 * 1024;

fn reader(zip: &[u8]) -> ReaderSource<Cursor<Vec<u8>>> {
    ReaderSource::new(Cursor::new(zip.to_vec()))
}

async fn try_entries(zip: &[u8]) -> Result<Vec<Entry>, ZipError> {
    read_directory(&mut reader(zip), zip.len() as u64).await
}

async fn entries(zip: &[u8]) -> Vec<Entry> {
    try_entries(zip).await.unwrap()
}

fn paths(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|entry| entry.path.join("/")).collect()
}

/// What the entry named `name` reads as, front to back.
async fn content_of(zip: &[u8], entries: &[Entry], name: &str) -> io::Result<Vec<u8>> {
    let entry = entries.iter().find(|entry| entry.name() == name).unwrap();
    read_to_end(&mut EntrySource::new(reader(zip), entry, WINDOW)).await
}

async fn read_to_end<S: Source>(source: &mut EntrySource<S>) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    let mut buf = vec![0; 100_000];
    loop {
        let read = source.read_at(content.len() as u64, &mut buf).await?;
        if read == 0 {
            return Ok(content);
        }
        content.extend(&buf[..read]);
    }
}

fn assert_damaged<T: std::fmt::Debug>(result: io::Result<T>) {
    let error = result.unwrap_err();
    let inner = error.get_ref().and_then(|inner| inner.downcast_ref());
    assert!(
        matches!(inner, Some(ZipError::Corrupt | ZipError::Malformed(_))),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_zip_of_info_zip_reads_with_its_folders_names_and_times() {
    let zip = include_bytes!("testdata/infozip.zip");
    let entries = entries(zip).await;
    assert_eq!(
        paths(&entries),
        [
            "tree",
            "tree/zero.dat",
            "tree/a\u{e7}\u{e3}o \u{e9}.txt",
            "tree/small.bin",
            "tree/empty",
            "tree/sub",
            "tree/sub/deep",
            "tree/sub/deep/text.txt",
        ]
    );
    let folders = entries.iter().filter(|entry| entry.is_folder);
    assert_eq!(folders.count(), 4);

    let text = b"hello hello hello hello hello hello hello hello\n";
    assert_eq!(content_of(zip, &entries, "text.txt").await.unwrap(), text);
    assert_eq!(content_of(zip, &entries, "small.bin").await.unwrap(), b"ab");
    assert_eq!(content_of(zip, &entries, "zero.dat").await.unwrap(), b"");
    let accented = content_of(zip, &entries, "a\u{e7}\u{e3}o \u{e9}.txt").await;
    assert_eq!(accented.unwrap(), b"accent\n");

    let text = entries.iter().find(|entry| entry.name() == "text.txt");
    assert_eq!(
        text.unwrap().modified,
        Utc.with_ymd_and_hms(2022, 3, 4, 5, 6, 8).single()
    );
}

#[tokio::test]
async fn a_zip64_of_python_reads() {
    let zip = include_bytes!("testdata/python_zip64.zip");
    let entries = entries(zip).await;
    assert_eq!(paths(&entries), ["dir/first.txt", "second.bin"]);
    assert_eq!(
        content_of(zip, &entries, "first.txt").await.unwrap(),
        b"first first first first first\n"
    );
    assert_eq!(
        content_of(zip, &entries, "second.bin").await.unwrap(),
        [0, 1, 2, 3]
    );
    // No time in UTC in it: the fields of the entry, as if in UTC.
    assert_eq!(
        entries[0].modified,
        Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 6).single()
    );
}

#[tokio::test]
async fn zip64_fields_replace_the_saturated_ones() {
    let zip = zip64_of(&[
        Written::deflated("a.txt", b"aaaaaaaaaaaaaaaa"),
        Written::stored("b.txt", b"bb"),
    ]);
    let entries = entries(&zip).await;
    assert_eq!(entries[0].size, 16);
    assert_eq!(entries[1].size, 2);
    assert_eq!(
        content_of(&zip, &entries, "a.txt").await.unwrap(),
        b"aaaaaaaaaaaaaaaa"
    );
    assert_eq!(content_of(&zip, &entries, "b.txt").await.unwrap(), b"bb");
}

#[tokio::test]
async fn a_missing_zip64_field_is_damage() {
    let mut zip = zip64_of(&[Written::stored("a.txt", b"a")]);
    // The field holds one value, where three are saturated.
    let at = zip
        .windows(4)
        .position(|bytes| bytes == [1, 0, 24, 0])
        .unwrap();
    zip[at + 2] = 8;
    assert!(matches!(
        try_entries(&zip).await,
        Err(ZipError::Malformed(_))
    ));
}

#[tokio::test]
async fn the_end_record_is_found_behind_a_comment_that_looks_like_one() {
    let mut zip = zip_of(&[Written::stored("a.txt", b"a")]);
    let comment = b"PK\x05\x06 not the end";
    let at = zip.len() - 2;
    zip[at..].copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
    zip.extend(comment);
    assert_eq!(paths(&entries(&zip).await), ["a.txt"]);
}

#[tokio::test]
async fn a_whole_end_record_in_the_comment_does_not_hide_the_entries() {
    let mut zip = zip_of(&[Written::stored("a.txt", b"a")]);
    // A record of an empty zip, with a comment of none, and 8 bytes after it.
    let mut comment = b"PK\x05\x06".to_vec();
    comment.extend([0; 18]);
    comment.extend(b"trailing");
    let at = zip.len() - 2;
    zip[at..].copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
    zip.extend(comment);
    assert_eq!(paths(&entries(&zip).await), ["a.txt"]);
}

#[tokio::test]
async fn bytes_after_the_end_record_do_not_hide_it() {
    let mut zip = zip_of(&[Written::stored("a.txt", b"a")]);
    zip.extend(b"something a tool added");
    assert_eq!(paths(&entries(&zip).await), ["a.txt"]);
}

#[tokio::test]
async fn sizes_that_cannot_be_are_damage() {
    let mut zip = zip64_of(&[Written::stored("a", b"a"), Written::stored("b", b"b")]);
    let fields = zip
        .windows(4)
        .enumerate()
        .filter(|(_, bytes)| *bytes == [1, 0, 24, 0])
        .map(|(at, _)| at + 4)
        .collect::<Vec<_>>();
    for at in fields {
        zip[at..at + 8].copy_from_slice(&(1u64 << 63).to_le_bytes());
    }
    assert!(matches!(
        try_entries(&zip).await,
        Err(ZipError::Malformed(_))
    ));
}

#[tokio::test]
async fn what_is_no_zip_says_so() {
    for bytes in [&b""[..], b"PK", &noise(100_000, 3)] {
        assert!(
            matches!(try_entries(bytes).await, Err(ZipError::NotZip)),
            "{} bytes",
            bytes.len()
        );
    }
    // Cut short, as an upload that did not finish.
    let zip = zip_of(&[Written::stored("a.txt", b"a")]);
    assert!(matches!(
        try_entries(&zip[..zip.len() - 1]).await,
        Err(ZipError::NotZip)
    ));
}

#[tokio::test]
async fn a_piece_of_a_split_zip_says_so() {
    let mut zip = zip_of(&[Written::stored("a.txt", b"a")]);
    let at = zip.len() - 22 + 4;
    zip[at] = 1;
    assert!(matches!(try_entries(&zip).await, Err(ZipError::Split)));
}

#[tokio::test]
async fn a_list_of_files_out_of_place_is_damage() {
    let mut zip = zip_of(&[Written::stored("a.txt", b"a")]);
    let at = zip.len() - 22 + 16;
    zip[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        try_entries(&zip).await,
        Err(ZipError::Malformed(_))
    ));
}

#[tokio::test]
async fn paths_never_climb_and_take_both_separators() {
    let zip = zip_of(&[
        Written::stored("../../up.txt", b"1"),
        Written::stored("/abs/./x.txt", b"2"),
        Written::stored("win\\dows\\y.txt", b"3"),
        Written::folder("win\\empty\\"),
        Written::folder("/"),
        Written::folder("../.."),
    ]);
    let entries = entries(&zip).await;
    assert_eq!(
        paths(&entries),
        ["up.txt", "abs/x.txt", "win/dows/y.txt", "win/empty"]
    );
    assert!(entries[3].is_folder);
}

#[tokio::test]
async fn a_path_of_absurd_depth_is_damage() {
    let zip = zip_of(&[Written::stored(&"a/".repeat(10_000), b"")]);
    assert!(matches!(
        try_entries(&zip).await,
        Err(ZipError::Malformed(_))
    ));
}

#[tokio::test]
async fn names_are_utf8_when_they_can_be_and_code_page_850_otherwise() {
    let named = |name: &[u8], flags| Written {
        name: name.to_vec(),
        flags,
        ..Written::stored("x", b"")
    };
    let zip = zip_of(&[
        // Windows on a machine in Brazil: `ação.txt`.
        named(b"a\x87\xc6o.txt", 0),
        // macOS: UTF-8 without the mark.
        named("r\u{e9}sum\u{e9}.txt".as_bytes(), 0),
        // With the mark, and not valid: never another encoding.
        named(b"bad\xff.txt", 1 << 11),
    ]);
    assert_eq!(
        paths(&entries(&zip).await),
        [
            "a\u{e7}\u{e3}o.txt",
            "r\u{e9}sum\u{e9}.txt",
            "bad\u{fffd}.txt"
        ]
    );
    assert_eq!(CP850.chars().count(), 128);
}

#[tokio::test]
async fn a_unicode_path_field_names_the_entry_when_it_is_for_that_name() {
    let raw = b"caf\xe9.txt";
    let field = |crc32: u32| {
        let name = "caf\u{e9}.txt".as_bytes();
        let mut extra = vec![0x75, 0x70];
        extra.extend(u16::try_from(5 + name.len()).unwrap().to_le_bytes());
        extra.push(1);
        extra.extend(crc32.to_le_bytes());
        extra.extend(name);
        extra
    };
    let named = |crc32| Written {
        name: raw.to_vec(),
        extra: field(crc32),
        ..Written::stored("x", b"")
    };
    let zip = zip_of(&[named(crc32fast::hash(raw)), named(1)]);
    assert_eq!(
        paths(&entries(&zip).await),
        // The second field is for another name: a tool changed the name and not the field.
        ["caf\u{e9}.txt", "caf\u{da}.txt"]
    );
}

#[tokio::test]
async fn an_extended_time_wins_over_the_fields_of_the_entry() {
    let mut extra = vec![0x55, 0x54, 5, 0, 1];
    extra.extend(1_000_000_000_i32.to_le_bytes());
    let zip = zip_of(&[
        Written {
            extra,
            ..Written::stored("a", b"")
        },
        Written {
            dos: (0, 0),
            ..Written::stored("no-time", b"")
        },
        Written {
            extra: vec![0x55, 0x54, 5, 0, 1, 0xff, 0xff, 0xff, 0xff],
            ..Written::stored("before-1970", b"")
        },
    ]);
    let entries = entries(&zip).await;
    assert_eq!(
        entries[0].modified,
        Utc.with_ymd_and_hms(2001, 9, 9, 1, 46, 40).single()
    );
    assert_eq!(entries[1].modified, None);
    assert_eq!(
        entries[2].modified,
        Utc.with_ymd_and_hms(2021, 2, 3, 4, 5, 6).single()
    );
}

#[tokio::test]
async fn encrypted_entries_and_other_methods_are_unreadable() {
    let zip = zip_of(&[
        Written {
            flags: 1,
            ..Written::stored("secret", b"x")
        },
        Written {
            method: 12,
            ..Written::stored("bzip2", b"x")
        },
        Written::deflated("fine", b"x"),
    ]);
    let entries = entries(&zip).await;
    assert!(matches!(entries[0].unreadable(), Some(ZipError::Encrypted)));
    assert!(matches!(
        entries[1].unreadable(),
        Some(ZipError::Method(12))
    ));
    assert!(entries[2].unreadable().is_none());
}

#[tokio::test]
async fn nothing_in_it_with_a_crc_of_something_is_damage() {
    let zip = zip_of(&[
        Written {
            crc32: Some(1),
            ..Written::stored("a", b"")
        },
        Written::stored("b", b""),
    ]);
    let entries = entries(&zip).await;
    assert!(matches!(entries[0].unreadable(), Some(ZipError::Corrupt)));
    assert!(entries[1].unreadable().is_none());
}

#[tokio::test]
async fn a_wrong_crc_fails_the_read_that_would_complete_the_content() {
    for written in [Written::stored("a", b"abc"), Written::deflated("a", b"abc")] {
        let zip = zip_of(&[Written {
            crc32: Some(1),
            ..written
        }]);
        assert_damaged(content_of(&zip, &entries(&zip).await, "a").await);
    }
}

#[tokio::test]
async fn deflated_content_that_is_damaged_or_too_short_fails() {
    let content = noise(10_000, 5);
    let zip = zip_of(&[Written::deflated("a", &content)]);
    let entries = entries(&zip).await;

    // A declared size above what the stream makes.
    let mut long = entries[0].clone();
    long.size += 1;
    assert_damaged(read_to_end(&mut EntrySource::new(reader(&zip), &long, WINDOW)).await);

    // A compressed size below what the stream needs.
    let mut short = entries[0].clone();
    short.compressed = 100;
    assert_damaged(read_to_end(&mut EntrySource::new(reader(&zip), &short, WINDOW)).await);

    // A compressed size past the end of the zip.
    let mut past = entries[0].clone();
    past.compressed = zip.len() as u64;
    assert_damaged(read_to_end(&mut EntrySource::new(reader(&zip), &past, WINDOW)).await);

    // An offset where no header is.
    let mut elsewhere = entries[0].clone();
    elsewhere.offset += 1;
    assert_damaged(read_to_end(&mut EntrySource::new(reader(&zip), &elsewhere, WINDOW)).await);
}

/// A reader that counts the bytes read from it.
struct Counted {
    inner: ReaderSource<Cursor<Vec<u8>>>,
    bytes: u64,
}

impl Source for Counted {
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read_at(offset, buf).await?;
        self.bytes += read as u64;
        Ok(read)
    }

    fn size(&mut self) -> impl std::future::Future<Output = io::Result<u64>> {
        self.inner.size()
    }
}

/// Fills `buf` with what the entry holds at `offset`.
async fn read_exactly<S: Source>(source: &mut EntrySource<S>, offset: u64, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let read = source
            .read_at(offset + filled as u64, &mut buf[filled..])
            .await
            .unwrap();
        assert!(read > 0, "at {} of {}", offset + filled as u64, buf.len());
        filled += read;
    }
}

#[tokio::test]
async fn a_read_back_inside_the_window_downloads_nothing_and_one_further_back_starts_over() {
    for written in [
        Written::stored("a", &noise(1_000_000, 7)),
        Written::deflated("a", &noise(1_000_000, 7)),
    ] {
        let content = written.content.clone();
        let zip = zip_of(&[written]);
        let entries = entries(&zip).await;
        let counted = Counted {
            inner: reader(&zip),
            bytes: 0,
        };
        let mut source = EntrySource::new(counted, &entries[0], 200_000);

        let mut buf = vec![0; 600_000];
        read_exactly(&mut source, 0, &mut buf).await;
        assert_eq!(buf, content[..600_000]);
        let read = source.source.bytes;

        // A chunk sent again: the bytes are there, and nothing is read.
        let mut again = vec![0; 150_000];
        read_exactly(&mut source, 450_000, &mut again).await;
        assert_eq!(again, content[450_000..600_000]);
        assert_eq!(source.source.bytes, read, "nothing to read for it");

        // Further back than the window: read again from the start, with the same bytes.
        let mut old = vec![0; 1000];
        read_exactly(&mut source, 5000, &mut old).await;
        assert_eq!(old, content[5000..6000]);
        assert!(source.source.bytes > read, "read from the start again");

        // On to the end from there: the CRC-32 holds over all of it.
        let mut rest = vec![0; content.len() - 6000];
        read_exactly(&mut source, 6000, &mut rest).await;
        assert_eq!(rest, content[6000..]);
        let mut end = [0; 8];
        assert_eq!(
            source
                .read_at(content.len() as u64, &mut end)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn a_read_far_ahead_gets_there_and_a_tiny_window_misses_no_offset() {
    let content = noise(5 * STEP + 123, 9);
    let zip = zip_of(&[Written::deflated("a", &content)]);
    let entries = entries(&zip).await;
    let mut source = EntrySource::new(reader(&zip), &entries[0], 1);

    let mut buf = vec![0; 10];
    read_exactly(&mut source, 3 * STEP as u64 + 7, &mut buf).await;
    assert_eq!(buf, content[3 * STEP + 7..3 * STEP + 17]);
    read_exactly(&mut source, STEP as u64 + 1, &mut buf).await;
    assert_eq!(buf, content[STEP + 1..STEP + 11]);
}
