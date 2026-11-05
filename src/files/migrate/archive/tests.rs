//! `files migrate --archive` between two fake Drives: the tar that arrives in the destination is
//! unpacked with the `tar` crate.

use std::{
    collections::HashSet, future::Future, io::Read, num::NonZeroUsize, path::Path, sync::Arc,
    time::Duration,
};

use super::PROPERTY;
use crate::{
    common::{human_size::human_size, local_name},
    files::migrate::{run, Error, Settings},
    transfer::{
        archive::MEDIA_TYPE,
        fake_drive::{Behavior, Created, FakeDrive, FakeFile},
        progress::{NoProgress, ProgressDisplay},
        relay::{RelayError, Unsupported, EXPORT_LIMIT},
        scheduler::{
            capture::{output, Captured},
            Options,
        },
        upload::{UploadOptions, CHUNK_ALIGNMENT},
    },
};

const DOCUMENT: &str = "application/vnd.google-apps.document";
const DRAWING: &str = "application/vnd.google-apps.drawing";
const FORM: &str = "application/vnd.google-apps.form";

const FOLDER_TIME: &str = "2024-01-02T03:04:05.678Z";
const FOLDER_SECONDS: u64 = 1_704_164_645;
const FILE_TIME: &str = "2025-06-07T08:09:10Z";
const FILE_SECONDS: u64 = 1_749_283_750;

fn long_name() -> String {
    format!("{}.txt", "l".repeat(300))
}

/// `photos` with nested folders, an empty folder, two files of one name, names with `/`, `..`,
/// non-ASCII and over 255 bytes, a Doc, a Drawing, a shortcut and a Form.
fn source_tree() -> Vec<FakeFile> {
    let blob = |id: &str, name: &str, parent: &str, content: &str| {
        FakeFile::blob(id, name, parent, content).modified(FILE_TIME)
    };
    vec![
        FakeFile::folder("photos", "photos", "root").modified(FOLDER_TIME),
        blob("a", "a.txt", "photos", "aaaa"),
        blob("s1", "same.txt", "photos", "first"),
        blob("s2", "same.txt", "photos", "second"),
        blob("slash", "a/../b", "photos", "slash"),
        blob("uni", "r\u{e9}sum\u{e9} \u{2713}.txt", "photos", "unicode"),
        blob("long", &long_name(), "photos", "long"),
        FakeFile::native("doc", "notes", "photos", DOCUMENT, "exported document")
            .modified(FILE_TIME),
        FakeFile::native("drawing", "sketch", "photos", DRAWING, "png bytes").modified(FILE_TIME),
        FakeFile::shortcut("link", "link", "photos", "a", "text/plain"),
        FakeFile::native("form", "form", "photos", FORM, ""),
        FakeFile::folder("y2024", "2024", "photos").modified(FOLDER_TIME),
        blob("d", "d.txt", "y2024", "dddd"),
        FakeFile::folder("jan", "jan", "y2024").modified(FOLDER_TIME),
        blob("f", "f.txt", "jan", "ffffff"),
        FakeFile::folder("empty", "empty", "photos").modified(FOLDER_TIME),
    ]
}

/// An entry of the tar as the `tar` crate reads it: path, directory, mtime, content.
type Unpacked = (String, bool, u64, Vec<u8>);

fn unpack(archive: &[u8]) -> Vec<Unpacked> {
    let mut reader = tar::Archive::new(archive);
    reader
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = String::from_utf8(entry.path_bytes().into_owned()).unwrap();
            let directory = entry.header().entry_type().is_dir();
            let mtime = entry.header().mtime().unwrap();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            (path, directory, mtime, content)
        })
        .collect()
}

fn folder(path: &str) -> Unpacked {
    (format!("{path}/"), true, FOLDER_SECONDS, Vec::new())
}

fn file(path: &str, content: &str) -> Unpacked {
    (
        path.to_owned(),
        false,
        FILE_SECONDS,
        content.as_bytes().to_vec(),
    )
}

/// `~` and 8 hex digits of the md5 of `id`: the suffix of a name that another file keeps.
fn tag(id: &str) -> String {
    format!("~{}", &format!("{:x}", md5::compute(id))[..8])
}

/// What the archive of [`source_tree`] holds, in its order.
fn archived_tree() -> Vec<Unpacked> {
    let long = local_name::portable_name(&long_name(), "long").unwrap();
    assert_eq!(long.len(), 255);
    assert_eq!(&long[242..], format!("{}.txt", tag("long")));
    vec![
        folder("photos"),
        folder("photos/2024"),
        file("photos/2024/d.txt", "dddd"),
        folder("photos/2024/jan"),
        file("photos/2024/jan/f.txt", "ffffff"),
        file("photos/a.txt", "aaaa"),
        file("photos/a_.._b", "slash"),
        folder("photos/empty"),
        file(&format!("photos/{long}"), "long"),
        file("photos/notes.docx", "exported document"),
        file("photos/r\u{e9}sum\u{e9} \u{2713}.txt", "unicode"),
        file("photos/same.txt", "first"),
        file(&format!("photos/same{}.txt", tag("s2")), "second"),
        file("photos/sketch.png", "png bytes"),
    ]
}

fn settings(file_ids: &[&str]) -> Settings {
    Settings {
        file_ids: file_ids.iter().map(|&id| id.to_owned()).collect(),
        into: None,
        dry_run: false,
        trash_source: false,
        archive: true,
        transfer: Options::new(NonZeroUsize::new(4).unwrap(), false),
    }
}

struct Migrated {
    result: Result<(), Error>,
    out: Captured,
    err: Captured,
}

impl Migrated {
    fn assert_ok(&self) {
        assert!(
            self.result.is_ok(),
            "{:?}\nstdout:\n{}\nstderr:\n{}",
            self.result,
            self.out.text(),
            self.err.text()
        );
    }

    fn summary(&self) -> String {
        self.err.lines().last().cloned().unwrap_or_default()
    }

    fn sorted_out(&self) -> Vec<String> {
        let mut lines = self.out.lines();
        lines.sort();
        lines
    }
}

async fn migrate_with(
    source: &FakeDrive,
    dest: &FakeDrive,
    settings: Settings,
    options: &UploadOptions,
    cancel: impl Future<Output = ()> + Send,
) -> Migrated {
    let output = output();
    let (source, dest) = (source.hub(), dest.hub());
    let run = run(
        &source,
        &dest,
        &settings,
        options,
        &output.printer,
        ProgressDisplay::reporting_to(Arc::new(NoProgress)),
        cancel,
    );
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the migration should end");
    Migrated {
        result,
        out: output.out,
        err: output.err,
    }
}

async fn migrate(source: &FakeDrive, dest: &FakeDrive, settings: Settings) -> Migrated {
    migrate_with(
        source,
        dest,
        settings,
        &UploadOptions::default(),
        std::future::pending(),
    )
    .await
}

/// The one file the run created in the destination.
fn the_archive(dest: &FakeDrive) -> Created {
    let created = dest.created();
    assert_eq!(created.len(), 1, "exactly one file: {created:?}");
    created.into_iter().next().unwrap()
}

#[tokio::test]
async fn a_folder_arrives_as_one_tar_that_unpacks_to_the_tree() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    migrated.assert_ok();
    let archive = the_archive(&dest);
    assert_eq!(archive.name, "photos.tar");
    assert_eq!(archive.mime_type, MEDIA_TYPE);
    assert_eq!(archive.parents, ["root"]);
    let manifest = &archive.app_properties[PROPERTY];
    assert_eq!(manifest.len(), 64, "{manifest}");
    assert_eq!(archive.app_properties.len(), 1);
    assert_eq!(unpack(&archive.content), archived_tree());

    // The source is only read; the Doc and the Drawing were exported in the default formats.
    assert_eq!(source.writes(), Vec::<String>::new());
    let mut exports = source.exports();
    exports.sort();
    assert_eq!(
        exports,
        [
            (
                String::from("doc"),
                String::from(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                )
            ),
            (String::from("drawing"), String::from("image/png")),
        ]
    );

    let found = &migrated.out.lines()[0];
    assert!(found.starts_with("Found 12 files in 4 folders"), "{found}");
    let skips = [
        format!(
            "skip photos/form: {}",
            Unsupported::NativeType(FORM.to_owned())
        ),
        format!("skip photos/link: {}", Unsupported::Shortcut),
    ];
    let create = format!(
        "create photos.tar ({})",
        human_size(archive.content.len() as u64)
    );
    let mut expected = vec![found.clone(), skips[0].clone(), skips[1].clone(), create];
    expected.sort();
    assert_eq!(migrated.sorted_out(), expected);
    let size = human_size(archive.content.len() as u64);
    assert_eq!(
        migrated.summary(),
        format!(
            "Summary: 1 archive migrated ({size}, 2 Google Workspace files exported), 0 already \
             there, 2 skipped (unsupported), 0 failed, 0 cancelled"
        )
    );
    assert!(migrated
        .err
        .text()
        .contains("Skipped 2 unsupported items, which cannot be migrated:"));
}

#[tokio::test]
async fn the_bytes_do_not_depend_on_the_order_drive_lists_things_in() {
    let mut archives = HashSet::new();
    for reverse in [false, true] {
        let mut files = source_tree();
        if reverse {
            files.reverse();
        }
        let source = FakeDrive::start(files, Behavior::default()).await;
        let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
        migrate(&source, &dest, settings(&["photos"]))
            .await
            .assert_ok();
        let archive = the_archive(&dest);
        archives.insert((archive.content, archive.app_properties[PROPERTY].clone()));
    }
    assert_eq!(archives.len(), 1);
}

#[tokio::test]
async fn a_dry_run_writes_nothing_anywhere_and_exports_nothing() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert_eq!(source.writes(), Vec::<String>::new());
    assert_eq!(dest.writes(), Vec::<String>::new());
    assert!(source.exports().is_empty() && source.media().is_empty());
    assert!(dest.created().is_empty());
    let create = migrated
        .out
        .lines()
        .into_iter()
        .find(|line| line.starts_with("create "))
        .unwrap();
    assert!(
        create.starts_with("create photos.tar (")
            && create.ends_with(" without the exports of 2 Google Workspace files)"),
        "{create}"
    );
    let summary = migrated.summary();
    assert!(
        summary.starts_with("Dry run, nothing was changed. Summary: 1 archive to migrate ("),
        "{summary}"
    );
    assert!(
        summary.ends_with(
            " without the exports of 2 Google Workspace files), 0 already there, 2 skipped \
             (unsupported), 0 failed, 0 cancelled"
        ),
        "{summary}"
    );
}

#[tokio::test]
async fn a_second_run_finds_the_archive_already_there() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    let exports = source.exports().len();

    for dry_run in [false, true] {
        let again = migrate(
            &source,
            &dest,
            Settings {
                dry_run,
                ..settings(&["photos"])
            },
        )
        .await;

        again.assert_ok();
        assert_eq!(dest.created().len(), 1, "nothing new");
        assert_eq!(source.exports().len(), exports, "nothing exported again");
        assert_eq!(again.out.lines()[1..], ["already-there photos.tar"]);
        let dry = if dry_run {
            "Dry run, nothing was changed. Summary: 0 archives to migrate"
        } else {
            "Summary: 0 archives migrated"
        };
        assert_eq!(
            again.summary(),
            format!("{dry} (0 B), 1 already there, 0 skipped (unsupported), 0 failed, 0 cancelled")
        );
    }
}

#[tokio::test]
async fn a_changed_folder_conflicts_with_its_archive_in_the_destination() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();

    let mut files = source_tree();
    files.push(FakeFile::blob("new", "new.txt", "photos", "new"));
    let changed = FakeDrive::start(files, Behavior::default()).await;
    let again = migrate(&changed, &dest, settings(&["photos"])).await;

    assert_eq!(
        crate::error_message(again.result.as_ref().unwrap_err()),
        "Error: the migration is incomplete: 1 file failed; use --continue-on-error to finish \
         the other transfers after a failure"
    );
    assert!(
        again.err.text().contains(
            "Error: unable to migrate 'photos.tar': it exists in the destination with different \
             content"
        ),
        "{}",
        again.err.text()
    );
    assert_eq!(dest.created().len(), 1);
    assert!(changed.exports().is_empty() && changed.media().is_empty());
}

#[tokio::test]
async fn a_single_file_is_an_archive_of_one_entry() {
    let source = FakeDrive::start(
        vec![
            FakeFile::blob("report", "report.pdf", "root", "%PDF").modified(FILE_TIME),
            FakeFile::native("doc", "Notes", "root", DOCUMENT, "docx").modified(FILE_TIME),
        ],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    migrate(&source, &dest, settings(&["report", "doc"]))
        .await
        .assert_ok();

    let mut created = dest.created();
    created.sort_by(|one, other| one.name.cmp(&other.name));
    let archives = created
        .iter()
        .map(|archive| (archive.name.as_str(), unpack(&archive.content)))
        .collect::<Vec<_>>();
    assert_eq!(
        archives,
        [
            ("Notes.tar", vec![file("Notes.docx", "docx")]),
            ("report.pdf.tar", vec![file("report.pdf", "%PDF")]),
        ]
    );
}

#[tokio::test]
async fn a_folder_listed_incompletely_or_not_at_all_fails_before_anything_is_uploaded() {
    for (behavior, message) in [
        (
            Behavior {
                incomplete: HashSet::from([String::from("y2024")]),
                ..Behavior::default()
            },
            "Error: unable to migrate 'photos.tar': Drive did not list 'photos/2024/' \
             completely, the archive would miss files; run the command again later",
        ),
        (
            Behavior {
                unreadable: HashSet::from([String::from("jan")]),
                ..Behavior::default()
            },
            "Error: unable to migrate 'photos.tar': the archive would miss what is in \
             'photos/2024/jan/': unable to list the folder in the source",
        ),
    ] {
        let source = FakeDrive::start(source_tree(), behavior).await;
        let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

        let migrated = migrate(
            &source,
            &dest,
            Settings {
                transfer: Options::new(NonZeroUsize::new(4).unwrap(), true),
                ..settings(&["photos"])
            },
        )
        .await;

        assert!(migrated.result.is_err());
        assert!(
            migrated.err.text().contains(message),
            "{}",
            migrated.err.text()
        );
        assert_eq!(dest.writes(), Vec::<String>::new());
        assert!(source.exports().is_empty() && source.media().is_empty());
    }
}

#[tokio::test]
async fn an_export_over_the_limit_is_left_out_and_counted_as_skipped() {
    let behavior = Behavior {
        export_too_large: HashSet::from([String::from("doc")]),
        ..Behavior::default()
    };
    let source = FakeDrive::start(source_tree(), behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    migrated.assert_ok();
    let mut expected = archived_tree();
    expected.retain(|(path, ..)| path != "photos/notes.docx");
    assert_eq!(unpack(&the_archive(&dest).content), expected);
    let too_large = RelayError::ExportTooLarge {
        limit: EXPORT_LIMIT,
    };
    let skip = format!("skip photos/notes: {too_large}");
    assert!(
        migrated.out.lines().contains(&skip),
        "{}",
        migrated.out.text()
    );
    assert!(
        migrated.summary().contains(
            ", 1 Google Workspace file exported), 0 already there, 3 skipped (unsupported)"
        ),
        "{}",
        migrated.summary()
    );
}

#[tokio::test]
async fn a_file_that_is_not_what_was_listed_fails_the_archive_and_nothing_is_created() {
    let mut files = source_tree();
    // Drive lists the md5 of other bytes than it sends.
    let a = files.iter_mut().find(|file| file.id == "a").unwrap();
    a.md5 = Some(format!("{:x}", md5::compute("AAAA")));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    assert!(migrated.result.is_err());
    let text = migrated.err.text();
    assert!(
        text.contains(
            "Error: unable to migrate 'photos.tar': the bytes read of 'photos/a.txt' are not the \
             ones the source lists"
        ),
        "{text}"
    );
    assert!(text.contains("; run the command again"), "{text}");
    assert!(dest.created().is_empty(), "no archive without that file");
}

/// A folder of a small blob, then two blobs of 300 000 bytes around a Doc whose exports differ
/// every time.
fn large_tree() -> Vec<FakeFile> {
    let content = |c: char| std::iter::repeat_n(c, 300_000).collect::<String>();
    vec![
        FakeFile::folder("big", "big", "root"),
        FakeFile::blob("first", "0.bin", "big", "first bytes"),
        FakeFile::blob("a", "a.bin", "big", &content('a')),
        FakeFile::native("doc", "notes", "big", DOCUMENT, "exported document").modified(FILE_TIME),
        FakeFile::blob("z", "z.bin", "big", &content('z')),
    ]
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Checks what an interrupted run left in `uploads`, the session record of the archive and its
/// exports; the manifest digest.
fn left_by_the_first_run(uploads: &Path) -> String {
    let names = files_in(uploads);
    assert_eq!(
        names.len(),
        2,
        "the session record and the exports: {names:?}"
    );
    let with = |extension: &str| {
        names
            .iter()
            .find(|name| {
                Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == extension)
            })
            .unwrap()
    };
    let (cache, record) = (with("exports"), with("json"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(uploads.join(cache))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(uploads.join(record)).unwrap()).unwrap();
    assert_eq!(record["offset"], CHUNK_ALIGNMENT);
    assert_eq!(record["source"]["remote"]["account"], "old@example.com");
    assert_eq!(record["source"]["remote"]["file_id"], "archive:big");
    assert_eq!(record["target"]["mime"], MEDIA_TYPE);
    let manifest = record["target"]["app_properties"][PROPERTY]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(*cache, format!("archive-{manifest}.exports"));
    manifest
}

#[tokio::test]
async fn an_interrupted_archive_is_continued_by_the_next_run_with_the_first_runs_exports() {
    let dir = tempfile::tempdir().unwrap();
    let uploads = dir.path().join("uploads");
    let options = UploadOptions {
        chunk_size: CHUNK_ALIGNMENT,
        multipart_threshold: 0,
        session_dir: Some(uploads.clone()),
        ..UploadOptions::default()
    };
    let source = FakeDrive::start(
        large_tree(),
        Behavior {
            exports_vary: true,
            account_email: Some(String::from("old@example.com")),
            ..Behavior::default()
        },
    )
    .await;
    // The second chunk never gets a reply: the run is interrupted while it waits.
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            chunks_stalled_from: Some(CHUNK_ALIGNMENT),
            account_email: Some(String::from("new@example.com")),
            ..Behavior::default()
        },
    )
    .await;

    let first = migrate_with(
        &source,
        &dest,
        settings(&["big"]),
        &options,
        dest.wait_for_stalled_chunks(1),
    )
    .await;

    assert!(
        first.summary().ends_with("; interrupted"),
        "{}",
        first.summary()
    );
    assert!(dest.created().is_empty());
    assert_eq!(source.exports().len(), 1);
    let manifest = left_by_the_first_run(&uploads);

    // The next run: the network is back.
    dest.stop_stalling();
    let media_before = source.media_ranges().len();
    assert_eq!(source.media()[0], "first", "read by the first run");
    let chunks_before = dest.chunks().len();
    let second = migrate_with(
        &source,
        &dest,
        settings(&["big"]),
        &options,
        std::future::pending(),
    )
    .await;

    second.assert_ok();
    // The session is asked where it stands and continued there.
    let chunks = dest.chunks()[chunks_before..]
        .iter()
        .map(|chunk| chunk.first)
        .collect::<Vec<_>>();
    assert_eq!(
        chunks,
        [None, Some(CHUNK_ALIGNMENT), Some(2 * CHUNK_ALIGNMENT)]
    );
    let archive = the_archive(&dest);
    assert_eq!(archive.app_properties[PROPERTY], manifest);
    let unpacked = unpack(&archive.content)
        .into_iter()
        .map(|(path, _, _, content)| (path, content))
        .collect::<Vec<_>>();
    assert_eq!(
        unpacked,
        [
            (String::from("big/"), Vec::new()),
            (String::from("big/0.bin"), b"first bytes".to_vec()),
            (String::from("big/a.bin"), vec![b'a'; 300_000]),
            (
                String::from("big/notes.docx"),
                b"exported document #0".to_vec()
            ),
            (String::from("big/z.bin"), vec![b'z'; 300_000]),
        ],
        "the export of the first run"
    );
    assert_eq!(source.exports().len(), 1, "not exported again");
    // The entries wholly in what the session held (the folder and 0.bin) are not read again;
    // a.bin, which the session's offset falls in, is read again from its first byte to be
    // hashed whole.
    assert_eq!(
        source.media_ranges()[media_before..],
        [(String::from("a"), None), (String::from("z"), None)]
    );
    assert_eq!(
        files_in(&uploads),
        Vec::<String>::new(),
        "record and cache deleted"
    );
    assert_eq!(source.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn a_wrong_file_that_an_interruption_fell_in_fails_the_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let options = UploadOptions {
        chunk_size: CHUNK_ALIGNMENT,
        multipart_threshold: 0,
        session_dir: Some(dir.path().join("uploads")),
        ..UploadOptions::default()
    };
    // Four chunks of the archive, nearly all of them b.bin, whose bytes are not the listed ones.
    let wrong = FakeFile {
        md5: Some(format!("{:x}", md5::compute("other bytes"))),
        ..FakeFile::blob(
            "b",
            "b.bin",
            "big",
            &std::iter::repeat_n('b', 900_000).collect::<String>(),
        )
    };
    let source = FakeDrive::start(
        vec![FakeFile::folder("big", "big", "root"), wrong],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            chunks_stalled_from: Some(CHUNK_ALIGNMENT),
            ..Behavior::default()
        },
    )
    .await;
    let first = migrate_with(
        &source,
        &dest,
        settings(&["big"]),
        &options,
        dest.wait_for_stalled_chunks(1),
    )
    .await;
    assert!(
        first.summary().ends_with("; interrupted"),
        "{}",
        first.summary()
    );

    // The next run continues inside b.bin, and still hashes it whole.
    dest.stop_stalling();
    let media_before = source.media_ranges().len();
    let second = migrate_with(
        &source,
        &dest,
        settings(&["big"]),
        &options,
        std::future::pending(),
    )
    .await;

    assert!(second.result.is_err());
    let text = second.err.text();
    assert!(
        text.contains(
            "Error: unable to migrate 'big.tar': the bytes read of 'big/b.bin' are not the ones \
             the source lists"
        ),
        "{text}"
    );
    assert!(dest.created().is_empty(), "the archive is not completed");
    assert_eq!(
        source.media_ranges()[media_before..],
        [(String::from("b"), None)],
        "read again from its first byte"
    );
}
