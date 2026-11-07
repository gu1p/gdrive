//! `files migrate --archive` between two fake Drives: the tar that arrives in the destination is
//! unpacked with the `tar` crate.

use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    io::Read,
    num::NonZeroUsize,
    path::Path,
    sync::Arc,
    time::Duration,
};

use serde_json::json;

use super::{PROPERTY, PROPERTY_MD5};
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

mod parts;

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
        contents: false,
        replace_smaller: false,
        ask: None,
        max_part_size: None,
        same_account: false,
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

/// The mark of `archive` in the destination now: its manifest digest, once it was checked.
fn mark(dest: &FakeDrive, archive: &Created) -> Option<String> {
    dest.stored(&archive.id)?
        .app_properties
        .get(PROPERTY)
        .cloned()
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
    // Created without its mark, which it gets once it is checked.
    assert!(archive.app_properties.is_empty(), "{archive:?}");
    let manifest = mark(&dest, &archive).unwrap();
    assert_eq!(manifest.len(), 64, "{manifest}");
    let patches = dest.patches();
    assert_eq!(patches.len(), 1, "{patches:?}");
    assert_eq!(patches[0].0, archive.id);
    // The manifest, and the md5 of the bytes it was checked with.
    let md5 = format!("{:x}", md5::compute(&archive.content));
    let marked = json!({ PROPERTY: manifest, PROPERTY_MD5: md5 });
    assert_eq!(patches[0].1["appProperties"], marked);
    let stored = dest.stored(&archive.id).unwrap().app_properties;
    assert_eq!(json!(stored), marked);
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
        let manifest = mark(&dest, &archive).unwrap();
        archives.insert((archive.content, manifest));
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

/// Options of uploads in chunks of 256 KiB whose sessions are recorded in `uploads`.
fn continuing(uploads: &Path) -> UploadOptions {
    UploadOptions {
        chunk_size: CHUNK_ALIGNMENT,
        multipart_threshold: 0,
        session_dir: Some(uploads.to_path_buf()),
        ..UploadOptions::default()
    }
}

/// A first run of `big` that `dest` interrupts: its second chunk never gets a reply (its
/// behavior has `chunks_stalled_from` the second chunk), and the run is cancelled while it
/// waits. The network is back afterwards.
async fn interrupted(source: &FakeDrive, dest: &FakeDrive, options: &UploadOptions) {
    let first = migrate_with(
        source,
        dest,
        settings(&["big"]),
        options,
        dest.wait_for_stalled_chunks(1),
    )
    .await;
    assert!(
        first.summary().ends_with("; interrupted"),
        "{}",
        first.summary()
    );
    dest.stop_stalling();
}

/// The first bytes of the chunks the destination took, over all its sessions.
fn accepted(dest: &FakeDrive) -> Vec<u64> {
    dest.chunks()
        .iter()
        .filter(|chunk| matches!(chunk.status, 200 | 308))
        .filter_map(|chunk| chunk.first)
        .collect()
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
    assert!(record["target"].get("app_properties").is_none(), "{record}");
    let manifest = cache
        .strip_prefix("archive-")
        .and_then(|name| name.strip_suffix(".exports"))
        .unwrap()
        .to_owned();
    assert_eq!(manifest.len(), 64, "{manifest}");
    manifest
}

#[tokio::test]
async fn an_interrupted_archive_is_continued_by_the_next_run_with_the_first_runs_exports() {
    let dir = tempfile::tempdir().unwrap();
    let uploads = dir.path().join("uploads");
    let options = continuing(&uploads);
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

    interrupted(&source, &dest, &options).await;
    assert!(dest.created().is_empty());
    assert_eq!(source.exports().len(), 1);
    let manifest = left_by_the_first_run(&uploads);

    // The next run.
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
    assert_eq!(
        accepted(&dest),
        [0, CHUNK_ALIGNMENT, 2 * CHUNK_ALIGNMENT],
        "over both runs, Drive took each byte of the archive once"
    );
    let archive = the_archive(&dest);
    assert_eq!(
        mark(&dest, &archive),
        Some(manifest),
        "marked once it was checked"
    );
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
    // What the session held is read again from the source, from the first byte of the archive,
    // to check the whole archive; it is not uploaded again.
    assert_eq!(
        source.media_ranges()[media_before..],
        [
            (String::from("first"), None),
            (String::from("a"), None),
            (String::from("z"), None),
        ]
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

#[tokio::test]
async fn a_doc_whose_export_is_too_large_is_one_skipped_item() {
    let source = FakeDrive::start(
        vec![FakeFile::native("doc", "Notes", "root", DOCUMENT, "docx")],
        Behavior {
            export_too_large: HashSet::from([String::from("doc")]),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["doc"])).await;

    migrated.assert_ok();
    assert!(dest.created().is_empty());
    let too_large = RelayError::ExportTooLarge {
        limit: EXPORT_LIMIT,
    };
    assert_eq!(
        migrated.out.lines()[1..],
        [format!("skip Notes: {too_large}")]
    );
    assert_eq!(
        migrated.summary(),
        "Summary: 0 archives migrated (0 B), 0 already there, 1 skipped (unsupported), 0 failed, \
         0 cancelled"
    );
}

#[tokio::test]
async fn a_continued_archive_whose_session_expires_is_sent_again_whole_and_checked() {
    for corrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let options = continuing(&dir.path().join("uploads"));
        let content = std::iter::repeat_n('b', 900_000).collect::<String>();
        let source = FakeDrive::start(
            vec![
                FakeFile::folder("big", "big", "root"),
                FakeFile::blob("b", "b.bin", "big", &content),
            ],
            Behavior::default(),
        )
        .await;
        // The first run stops after one chunk; in the next, the session expires at its first
        // chunk, and the new session (the second one) keeps what it is sent, or other bytes.
        let dest = FakeDrive::start(
            Vec::new(),
            Behavior {
                chunks_stalled_from: Some(CHUNK_ALIGNMENT),
                expire_session_at: Some(CHUNK_ALIGNMENT),
                corrupt_session: corrupt.then_some(1),
                ..Behavior::default()
            },
        )
        .await;
        interrupted(&source, &dest, &options).await;

        let chunks_before = dest.chunks().len();
        let second = migrate_with(
            &source,
            &dest,
            settings(&["big"]),
            &options,
            std::future::pending(),
        )
        .await;

        let chunks = dest.chunks()[chunks_before..]
            .iter()
            .map(|chunk| (chunk.first, chunk.status))
            .collect::<Vec<_>>();
        assert_eq!(
            chunks[..2],
            [(None, 308), (Some(CHUNK_ALIGNMENT), 404)],
            "continued, then expired"
        );
        let resent = chunks[2..]
            .iter()
            .map(|(first, _)| *first)
            .collect::<Vec<_>>();
        assert_eq!(
            resent,
            [
                Some(0),
                Some(CHUNK_ALIGNMENT),
                Some(2 * CHUNK_ALIGNMENT),
                Some(3 * CHUNK_ALIGNMENT)
            ],
            "the new session gets everything from byte 0"
        );
        let archive = the_archive(&dest);
        if corrupt {
            assert!(second.result.is_err());
            let text = second.err.text();
            assert!(
                text.contains(&format!(
                    "Error: unable to migrate 'big.tar': the archive in the destination (id {}) \
                     is not what was sent",
                    archive.id
                )),
                "{text}"
            );
            assert_eq!(mark(&dest, &archive), None, "never marked");
        } else {
            second.assert_ok();
            let unpacked = unpack(&archive.content);
            assert_eq!(unpacked.len(), 2);
            assert_eq!(unpacked[1].3, content.as_bytes());
            assert!(mark(&dest, &archive).is_some(), "marked once checked");
        }
    }
}

#[tokio::test]
async fn a_wrong_prefix_sent_before_an_interruption_fails_the_next_run_and_is_never_marked() {
    let dir = tempfile::tempdir().unwrap();
    let options = continuing(&dir.path().join("uploads"));
    let good = std::iter::repeat_n('b', 900_000).collect::<String>();
    let listed = FakeFile::blob("b", "b.bin", "big", &good);
    // In the first run only, Drive sends other bytes for the start of b.bin (the listed size and
    // md5 stay): the first run uploads them and is interrupted before the end of b.bin.
    let wrong_start = format!("{}{}", "x".repeat(100_000), &good[100_000..]);
    let first_source = FakeDrive::start(
        vec![
            FakeFile::folder("big", "big", "root"),
            FakeFile {
                content: Some(wrong_start),
                ..listed.clone()
            },
        ],
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
    interrupted(&first_source, &dest, &options).await;

    // The next run reads the right bytes: they are not what Drive holds.
    let source = FakeDrive::start(
        vec![FakeFile::folder("big", "big", "root"), listed],
        Behavior::default(),
    )
    .await;
    let second = migrate_with(
        &source,
        &dest,
        settings(&["big"]),
        &options,
        std::future::pending(),
    )
    .await;

    assert!(second.result.is_err());
    let archive = the_archive(&dest);
    let text = second.err.text();
    assert!(
        text.contains(&format!(
            "Error: unable to migrate 'big.tar': the archive in the destination (id {}) is not \
             what was sent",
            archive.id
        )),
        "{text}"
    );
    assert!(text.contains("it is left there unmarked"), "{text}");
    assert_eq!(mark(&dest, &archive), None, "never marked");

    // A third run takes it for a file of that name with other content.
    let third = migrate_with(
        &source,
        &dest,
        settings(&["big"]),
        &options,
        std::future::pending(),
    )
    .await;
    assert!(third.result.is_err());
    assert!(
        third.err.text().contains(
            "Error: unable to migrate 'big.tar': it exists in the destination with different \
             content"
        ),
        "{}",
        third.err.text()
    );
    assert_eq!(dest.created().len(), 1);
    assert_eq!(first_source.writes(), Vec::<String>::new());
    assert_eq!(source.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn an_archive_that_cannot_be_marked_fails_and_is_reported_as_different_next_time() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    // The archive gets the first generated id.
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            patch_rejected: HashSet::from([String::from("gen-0")]),
            ..Behavior::default()
        },
    )
    .await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    assert!(migrated.result.is_err());
    let archive = the_archive(&dest);
    assert_eq!(archive.id, "gen-0");
    let text = migrated.err.text();
    assert!(
        text.contains(
            "Error: unable to migrate 'photos.tar': the archive 'photos.tar' is complete and \
             checked in the destination (id gen-0), but marking it could not be confirmed: a \
             later run reports it as already there if the mark was written, and otherwise as a \
             file of that name with other content; nothing was deleted"
        ),
        "{text}"
    );
    assert_eq!(unpack(&archive.content), archived_tree());
    assert_eq!(mark(&dest, &archive), None);
    assert_eq!(dest.deletes(), Vec::<String>::new());

    let again = migrate(&source, &dest, settings(&["photos"])).await;
    assert!(again.result.is_err());
    assert!(
        again.err.text().contains(
            "Error: unable to migrate 'photos.tar': it exists in the destination with different \
             content"
        ),
        "{}",
        again.err.text()
    );
    assert_eq!(dest.created().len(), 1);
}

#[tokio::test]
async fn an_archive_of_a_file_without_md5_or_modification_time_is_never_already_there() {
    let source = FakeDrive::start(
        vec![
            FakeFile::folder("photos", "photos", "root"),
            FakeFile {
                md5: None,
                ..FakeFile::blob("a", "a.txt", "photos", "aaaa")
            },
        ],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    assert!(mark(&dest, &the_archive(&dest)).is_some());

    // The same listing again: the file may have been edited, nothing tells.
    let again = migrate(&source, &dest, settings(&["photos"])).await;
    assert!(again.result.is_err());
    assert!(
        again.err.text().contains(
            "Error: unable to migrate 'photos.tar': it exists in the destination with different \
             content"
        ),
        "{}",
        again.err.text()
    );
    assert_eq!(dest.created().len(), 1);
}

#[tokio::test]
async fn a_marked_archive_with_other_bytes_or_without_its_md5_is_a_conflict() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    let archive = the_archive(&dest);
    let manifest = mark(&dest, &archive).unwrap();
    let md5 = format!("{:x}", md5::compute(&archive.content));

    // A destination photos.tar with `content` and the properties `marked`.
    let with = |content: &str, marked: &[(&str, &str)]| FakeFile {
        app_properties: marked
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        ..FakeFile::blob("old", "photos.tar", "root", content)
    };
    let same_bytes_md5 = format!("{:X}", md5::compute("same bytes"));
    for (existing, already_there) in [
        // Its content was replaced, its properties kept (a new version uploaded over it).
        (
            with(
                "other bytes",
                &[(PROPERTY, &manifest), (PROPERTY_MD5, &md5)],
            ),
            false,
        ),
        // The manifest without the md5 it was checked with.
        (with("other bytes", &[(PROPERTY, &manifest)]), false),
        // Both, and the listed md5 is the marked one (in other case).
        (
            with(
                "same bytes",
                &[(PROPERTY, &manifest), (PROPERTY_MD5, &same_bytes_md5)],
            ),
            true,
        ),
    ] {
        let dest = FakeDrive::start(vec![existing], Behavior::default()).await;
        let again = migrate(&source, &dest, settings(&["photos"])).await;
        if already_there {
            again.assert_ok();
            assert_eq!(again.out.lines()[1..], ["already-there photos.tar"]);
        } else {
            assert!(again.result.is_err());
            assert!(
                again.err.text().contains(
                    "Error: unable to migrate 'photos.tar': it exists in the destination with \
                     different content"
                ),
                "{}",
                again.err.text()
            );
        }
        assert!(dest.created().is_empty());
    }
}

/// `--to` the source account itself: one fake Drive is both sides.
fn in_one_account(settings: Settings) -> Settings {
    Settings {
        same_account: true,
        ..settings
    }
}

#[tokio::test]
async fn inside_one_account_a_folder_gets_its_tar_and_a_second_run_finds_it() {
    let drive = FakeDrive::start(source_tree(), Behavior::default()).await;

    let migrated = migrate(&drive, &drive, in_one_account(settings(&["photos"]))).await;

    migrated.assert_ok();
    let archive = the_archive(&drive);
    assert_eq!(archive.name, "photos.tar");
    // Next to the folder, at the top of My Drive.
    assert_eq!(archive.parents, ["root"]);
    assert_eq!(unpack(&archive.content), archived_tree());
    assert!(mark(&drive, &archive).is_some());
    // Nothing but the archive was written: its upload and its mark.
    assert_eq!(
        drive.writes(),
        [
            String::from("POST /upload/drive/v3/files"),
            format!("PATCH /drive/v3/files/{}", archive.id)
        ]
    );

    let again = migrate(&drive, &drive, in_one_account(settings(&["photos"]))).await;

    again.assert_ok();
    assert_eq!(again.out.lines()[1..], ["already-there photos.tar"]);
    assert_eq!(drive.created().len(), 1, "nothing new");
}

#[tokio::test]
async fn inside_one_account_an_archive_never_goes_into_what_it_archives() {
    let refused = |id: &str| {
        format!(
            "Error: the destination folder '{id}' is one of the folders to migrate, or inside \
             one; --into must name a folder outside them"
        )
    };

    // Into the folder itself, or a folder in it.
    let drive = FakeDrive::start(source_tree(), Behavior::default()).await;
    for into in ["photos", "jan"] {
        let migrated = migrate(
            &drive,
            &drive,
            in_one_account(Settings {
                into: Some(into.to_owned()),
                ..settings(&["photos"])
            }),
        )
        .await;
        let error = migrated.result.expect_err("an archive inside its source");
        assert_eq!(crate::error_message(&error), refused(into));
    }
    assert!(drive.created().is_empty());

    // All of My Drive, without `--into`: the top of My Drive is `root` there and has an id of
    // its own in the listing.
    let mut files = source_tree();
    for file in &mut files {
        if file.parents == ["root"] {
            file.parents = vec![String::from("drive0")];
        }
    }
    files.push(FakeFile::folder("drive0", "My Drive", "root").with_parents(&[]));
    let behavior = Behavior {
        my_drive: Some(String::from("drive0")),
        ..Behavior::default()
    };
    let drive = FakeDrive::start(files, behavior).await;
    for id in ["root", "drive0"] {
        let migrated = migrate(&drive, &drive, in_one_account(settings(&[id]))).await;
        let error = migrated.result.expect_err("an archive inside its source");
        assert_eq!(crate::error_message(&error), refused("drive0"));
    }
    assert!(drive.created().is_empty());
    assert_eq!(drive.writes(), Vec::<String>::new());
}

fn replacing(settings: Settings) -> Settings {
    Settings {
        replace_smaller: true,
        ..settings
    }
}

fn conflict_of_the_archive(migrated: &Migrated) -> bool {
    migrated.result.is_err()
        && migrated.err.text().contains(
            "Error: unable to migrate 'photos.tar': it exists in the destination with different \
             content",
        )
}

#[tokio::test]
async fn with_replace_smaller_the_archive_of_a_folder_that_grew_replaces_the_old_one() {
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    let first = the_archive(&dest);
    let first_mark = mark(&dest, &first).unwrap();
    let was = human_size(first.content.len() as u64);

    // A file more in the folder.
    let mut grown = source_tree();
    grown.push(FakeFile::blob("new", "new.txt", "photos", "new file").modified(FILE_TIME));
    let source = FakeDrive::start(grown, Behavior::default()).await;

    // Without the flag it is the conflict it was.
    let refused = migrate(&source, &dest, settings(&["photos"])).await;
    assert!(conflict_of_the_archive(&refused), "{}", refused.err.text());
    assert_eq!(dest.updates(), []);

    let writes = dest.writes().len();
    let dry = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            ..replacing(settings(&["photos"]))
        },
    )
    .await;
    dry.assert_ok();
    // Without the exports it is not larger than the old one, which has them.
    let lines = dry.out.lines();
    let line = lines.last().unwrap();
    assert!(line.starts_with("replace photos.tar ("), "{line}");
    assert!(
        line.ends_with(&format!(
            " without the exports of 2 Google Workspace files, was {was}) if the exports make \
             it larger"
        )),
        "{line}"
    );
    assert!(
        dry.summary().ends_with("; 1 smaller file to replace"),
        "{}",
        dry.summary()
    );
    assert_eq!(dest.writes().len(), writes, "a dry run writes nothing");

    let replaced = migrate(&source, &dest, replacing(settings(&["photos"]))).await;

    replaced.assert_ok();
    // The new archive is the new content of the same file.
    let updates = dest.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].0, first.id);
    assert_eq!(dest.created().len(), 1, "no second file");
    let mut expected = archived_tree();
    expected.push(file("photos/new.txt", "new file"));
    let mut unpacked = unpack(&updates[0].1);
    expected.sort();
    unpacked.sort();
    assert_eq!(unpacked, expected);
    let line = format!(
        "replace photos.tar ({}, was {was})",
        human_size(updates[0].1.len() as u64)
    );
    assert!(
        replaced.out.lines().contains(&line),
        "{}",
        replaced.out.text()
    );
    assert!(
        replaced.summary().ends_with("; 1 smaller file replaced"),
        "{}",
        replaced.summary()
    );
    // Marked anew, with the md5 of the new bytes.
    let stored = dest.stored(&first.id).unwrap().app_properties;
    assert_ne!(stored[PROPERTY], first_mark);
    assert_eq!(
        stored[PROPERTY_MD5],
        format!("{:x}", md5::compute(&updates[0].1))
    );

    let again = migrate(&source, &dest, replacing(settings(&["photos"]))).await;

    again.assert_ok();
    assert_eq!(again.out.lines()[1..], ["already-there photos.tar"]);
    assert_eq!(dest.updates().len(), 1);
}

/// `photos` with files that have content only: the size of its archive is known from the listing.
fn plain_tree() -> Vec<FakeFile> {
    source_tree()
        .into_iter()
        .filter(|file| !["doc", "drawing", "link", "form"].contains(&file.id.as_str()))
        .collect()
}

#[tokio::test]
async fn with_replace_smaller_only_a_larger_archive_replaces_and_a_dry_run_tells() {
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let source = FakeDrive::start(plain_tree(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    let first = the_archive(&dest);
    let size = first.content.len() as u64;

    // A file less in the folder: a smaller archive, which replaces nothing.
    let shrunk = plain_tree()
        .into_iter()
        .filter(|file| file.id != "uni")
        .collect();
    let source = FakeDrive::start(shrunk, Behavior::default()).await;
    for dry_run in [true, false] {
        let migrated = migrate(
            &source,
            &dest,
            Settings {
                dry_run,
                ..replacing(settings(&["photos"]))
            },
        )
        .await;

        assert!(
            conflict_of_the_archive(&migrated),
            "{}",
            migrated.err.text()
        );
    }
    assert_eq!(dest.updates(), []);
    assert_eq!(
        dest.stored(&first.id).unwrap().content,
        Some(String::from_utf8_lossy(&first.content).into_owned())
    );

    // A file more: one header and one block of content more, which a dry run knows.
    let mut grown = plain_tree();
    grown.push(FakeFile::blob("new", "new.txt", "photos", "new file").modified(FILE_TIME));
    let source = FakeDrive::start(grown, Behavior::default()).await;
    let dry = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            ..replacing(settings(&["photos"]))
        },
    )
    .await;

    dry.assert_ok();
    assert_eq!(
        dry.out.lines()[1..],
        [format!(
            "replace photos.tar ({}, was {})",
            human_size(size + 1024),
            human_size(size)
        )]
    );
    assert_eq!(dest.updates(), []);
}

/// Answers every question of `--ask` with `answer`, and counts them.
#[derive(Debug)]
struct Answering {
    answer: bool,
    questions: std::sync::Mutex<Vec<String>>,
}

impl crate::files::migrate::ask::Ask for Answering {
    fn ask(&self, question: String) -> futures::future::BoxFuture<'_, bool> {
        self.questions.lock().unwrap().push(question);
        Box::pin(async move { self.answer })
    }
}

#[tokio::test]
async fn ask_puts_a_changed_folders_archive_to_the_user() {
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let source = FakeDrive::start(plain_tree(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    let first = the_archive(&dest);
    let was = human_size(first.content.len() as u64);

    // A file less in the folder, one header and one block of content: the new archive is the
    // smaller one.
    let shrunk = plain_tree()
        .into_iter()
        .filter(|file| file.id != "a")
        .collect();
    let source = FakeDrive::start(shrunk, Behavior::default()).await;
    let asking = |answer| {
        let user = Arc::new(Answering {
            answer,
            questions: std::sync::Mutex::new(Vec::new()),
        });
        let settings = Settings {
            ask: Some(user.clone()),
            ..settings(&["photos"])
        };
        (user, settings)
    };

    let (user, no) = asking(false);
    let kept = migrate(&source, &dest, no).await;

    kept.assert_ok();
    assert_eq!(kept.out.lines()[1..], ["keep-destination photos.tar"]);
    assert_eq!(dest.updates(), []);
    let questions = user.questions.lock().unwrap().clone();
    assert_eq!(questions.len(), 1);
    let now = human_size(first.content.len() as u64 - 1024);
    assert_eq!(
        questions[0],
        format!(
            "\nphotos.tar is in the destination with other content:\n  new archive: {now}\n  \
             \x20            https://drive.google.com/drive/folders/photos\n  destination: {was}\n  \
             \x20            https://drive.google.com/file/d/{}/view\n\
             Replace the destination's file? [y/N] ",
            first.id
        )
    );
    assert!(
        kept.summary().ends_with(
            "0 already there, 0 skipped (unsupported), 0 failed, 0 cancelled; 0 files replaced, \
             1 conflict left alone"
        ),
        "{}",
        kept.summary()
    );

    let (_, yes) = asking(true);
    let replaced = migrate(&source, &dest, yes).await;

    replaced.assert_ok();
    let updates = dest.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].0, first.id);
    assert_eq!(updates[0].1.len(), first.content.len() - 1024);
    assert_eq!(
        replaced.out.lines()[1..],
        [format!("replace photos.tar ({now}, was {was})")]
    );
    // It is the archive of the folder as it is now.
    let again = migrate(&source, &dest, settings(&["photos"])).await;
    again.assert_ok();
    assert_eq!(again.out.lines()[1..], ["already-there photos.tar"]);
}

#[tokio::test]
async fn a_dry_run_of_an_archive_of_exports_only_promises_no_replace() {
    // A Google Docs file by its id: without its export there is nothing to archive.
    let source = FakeDrive::start(
        vec![
            FakeFile::native("doc", "notes", "root", DOCUMENT, "exported document")
                .modified(FILE_TIME),
        ],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(
        vec![FakeFile::blob("old", "notes.tar", "root", "small")],
        Behavior::default(),
    )
    .await;

    let dry = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            ..replacing(settings(&["doc"]))
        },
    )
    .await;

    dry.assert_ok();
    let lines = dry.out.lines();
    let line = lines.last().unwrap();
    assert!(
        line.starts_with("replace notes.tar (") && line.ends_with(" if the exports make it larger"),
        "{line}"
    );
}
