//! `files sync` against the fake Drive and temporary directories.

mod deletion;
mod safety;

use std::{
    collections::BTreeMap,
    future::{self, Future},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{errors, sync_with_hub, Config, DeleteMode, Direction};
use crate::{
    common::drive_file::MIME_TYPE_DRIVE_DOCUMENT,
    files::export::ExportFormats,
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile},
        progress::{Mode, ProgressDisplay, ProgressEvent, ProgressSink},
        scheduler::{
            capture::{output, Captured},
            JobKind, Options,
        },
        upload::UploadOptions,
    },
};

/// The Drive folder every test syncs, id `root`.
fn root() -> FakeFile {
    FakeFile {
        parents: Vec::new(),
        ..FakeFile::folder("root", "Root", "")
    }
}

/// `root` and `files`.
fn drive(files: Vec<FakeFile>) -> Vec<FakeFile> {
    let mut all = vec![root()];
    all.extend(files);
    all
}

fn config(direction: Direction, local_dir: &Path) -> Config {
    Config {
        direction,
        local_dir: local_dir.to_path_buf(),
        folder_id: String::from("root"),
        dry_run: false,
        delete: None,
        export: None,
        acknowledge_abuse: false,
        transfer: Options::new(NonZeroUsize::new(2).unwrap(), false),
        upload: UploadOptions::default(),
        progress: Mode::Never,
    }
}

fn dry(config: Config) -> Config {
    Config {
        dry_run: true,
        ..config
    }
}

fn deleting(config: Config) -> Config {
    Config {
        delete: Some(DeleteMode::Trash),
        ..config
    }
}

struct Synced {
    result: Result<(), errors::Sync>,
    out: Captured,
    err: Captured,
}

impl Synced {
    /// The lines on stdout, sorted: the real run prints them in the order the jobs run.
    fn lines(&self) -> Vec<String> {
        let mut lines = self.out.lines();
        lines.sort();
        lines
    }

    fn error(&self) -> String {
        match &self.result {
            Ok(()) => panic!("the sync should fail; stderr: {}", self.err.text()),
            Err(error) => crate::error_message(error),
        }
    }

    fn ok(&self) {
        if let Err(error) = &self.result {
            panic!(
                "the sync should succeed: {}\nstdout: {}\nstderr: {}",
                crate::error_message(error),
                self.out.text(),
                self.err.text()
            );
        }
    }

    fn summary(&self) -> String {
        self.err
            .lines()
            .into_iter()
            .rfind(|line| line.contains("Summary:"))
            .unwrap_or_default()
    }
}

async fn sync(server: &FakeDrive, config: &Config) -> Synced {
    sync_until(server, config, future::pending()).await
}

async fn sync_until(
    server: &FakeDrive,
    config: &Config,
    cancel: impl Future<Output = ()> + Send + 'static,
) -> Synced {
    let display = ProgressDisplay::recursive(Mode::Never, config.transfer.jobs);
    sync_showing(server, config, display, cancel).await
}

async fn sync_showing(
    server: &FakeDrive,
    config: &Config,
    display: ProgressDisplay,
    cancel: impl Future<Output = ()> + Send + 'static,
) -> Synced {
    let output = output();
    let hub = server.hub();
    let run = sync_with_hub(&hub, config, &output.printer, display, cancel);
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the sync should end");
    Synced {
        result,
        out: output.out,
        err: output.err,
    }
}

fn lines(lines: &[&str]) -> Vec<String> {
    let mut lines = lines
        .iter()
        .map(|line| (*line).to_owned())
        .collect::<Vec<_>>();
    lines.sort();
    lines
}

/// `path` (with `/`) below `dir`.
fn local(dir: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(dir.to_path_buf(), |path, name| path.join(name))
}

/// Writes `files` (path with `/`, content) below `dir`, with their directories.
fn write(dir: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let path = local(dir, path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(local(dir, path)).unwrap()
}

fn time(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().into()
}

fn set_time(path: &Path, time: DateTime<Utc>) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::from(time))
        .unwrap();
}

fn modified(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path).unwrap().modified().unwrap().into()
}

/// Everything below `dir`: each path (with `/`) and its content, `None` for a directory, and
/// its modification time.
fn snapshot(dir: &Path) -> BTreeMap<String, (Option<Vec<u8>>, SystemTime)> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let content = if metadata.is_dir() {
                pending.push(path.clone());
                None
            } else if metadata.is_file() {
                Some(std::fs::read(&path).unwrap())
            } else {
                Some(Vec::new())
            };
            entries.insert(rel, (content, metadata.modified().unwrap()));
        }
    }
    entries
}

/// The paths below `dir` (with `/`), sorted.
fn paths(dir: &Path) -> Vec<String> {
    snapshot(dir).into_keys().collect()
}

/// The local trash of the one run that used it: `<dir>/.gdrive-trash/<time>`.
fn run_trash(dir: &Path) -> PathBuf {
    let trash = dir.join(".gdrive-trash");
    let runs = std::fs::read_dir(&trash)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs.into_iter().next().unwrap()
}

// --- up ---

#[tokio::test]
async fn up_creates_folders_before_their_files() {
    let server = FakeDrive::start(drive(Vec::new()), Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("a.txt", "aaa"),
            ("sub/b.txt", "bb"),
            ("sub/deep/c.txt", "c"),
        ],
    );

    let synced = sync(&server, &config(Direction::Up, dir.path())).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        lines(&[
            "create a.txt",
            "create sub/",
            "create sub/b.txt",
            "create sub/deep/",
            "create sub/deep/c.txt",
        ])
    );
    let created = server.created();
    let find = |name: &str| {
        created
            .iter()
            .find(|file| file.name == name)
            .unwrap()
            .clone()
    };
    let (sub, deep) = (find("sub"), find("deep"));
    assert_eq!(sub.parents, ["root"]);
    assert_eq!(deep.parents, std::slice::from_ref(&sub.id));
    assert_eq!(find("a.txt").parents, ["root"]);
    assert_eq!(find("b.txt").parents, std::slice::from_ref(&sub.id));
    assert_eq!(find("c.txt").parents, std::slice::from_ref(&deep.id));
    assert_eq!(find("c.txt").content, b"c");
    // Every folder exists before anything is created in it.
    for file in &created {
        for parent in &file.parents {
            if let Some(folder) = created.iter().find(|folder| &folder.id == parent) {
                assert!(
                    folder.answered < file.arrived,
                    "{} before {}",
                    file.name,
                    folder.name
                );
            }
        }
    }
    // The ids came from one generateIds call, not one per file.
    assert!(created.iter().all(|file| file.id.starts_with("gen-")));
    assert_eq!(
        synced.summary(),
        "Summary: 5 created, 0 updated, 0 unchanged, 0 moved to the trash, 0 extra kept, 0 \
        skipped, 0 failed, 0 cancelled"
    );
}

#[tokio::test]
async fn up_updates_changed_files_in_place_and_leaves_unchanged_ones() {
    let files = drive(vec![
        FakeFile::blob("same", "same.txt", "root", "same"),
        // Same size, other content: the md5 decides.
        FakeFile::blob("edited", "edited.txt", "root", "old!"),
        FakeFile::blob("grown", "grown.txt", "root", "short"),
        FakeFile::folder("docs", "docs", "root"),
        FakeFile::blob("inner", "inner.txt", "docs", "inner"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("same.txt", "same"),
            ("edited.txt", "new!"),
            ("grown.txt", "much longer"),
            ("docs/inner.txt", "inner"),
            ("docs/new.txt", "new"),
        ],
    );

    let synced = sync(&server, &config(Direction::Up, dir.path())).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        lines(&[
            "create docs/new.txt",
            "unchanged docs/inner.txt",
            "unchanged same.txt",
            "update edited.txt",
            "update grown.txt",
        ])
    );
    let mut updates = server.updates();
    updates.sort();
    assert_eq!(
        updates,
        [
            (String::from("edited"), b"new!".to_vec()),
            (String::from("grown"), b"much longer".to_vec()),
        ],
        "a changed file gets a new revision under its id"
    );
    let created = server.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].name, "new.txt");
    assert_eq!(created[0].parents, ["docs"]);
    assert_eq!(server.patches(), []);
}

#[tokio::test]
async fn up_keeps_extras_without_delete_and_trashes_them_with_it() {
    let files = drive(vec![
        FakeFile::blob("kept", "kept.txt", "root", "kept"),
        FakeFile::blob("old", "old.txt", "root", "old"),
        FakeFile::folder("gone", "gone", "root"),
        FakeFile::blob("gone-1", "1.txt", "gone", "1"),
        FakeFile::folder("gone-sub", "sub", "gone"),
        FakeFile::blob("gone-2", "2.txt", "gone-sub", "2"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("kept.txt", "kept")]);

    let server = FakeDrive::start(files.clone(), Behavior::default()).await;
    let kept = sync(&server, &config(Direction::Up, dir.path())).await;
    kept.ok();
    assert_eq!(
        kept.lines(),
        lines(&["extra gone/", "extra old.txt", "unchanged kept.txt"])
    );
    assert_eq!(server.patches(), []);

    let server = FakeDrive::start(files, Behavior::default()).await;
    let trashed = sync(&server, &deleting(config(Direction::Up, dir.path()))).await;
    trashed.ok();
    assert_eq!(
        trashed.lines(),
        lines(&[
            "extra->trash gone/",
            "extra->trash old.txt",
            "unchanged kept.txt"
        ])
    );
    let mut patches = server.patches();
    patches.sort_by(|a, b| a.0.cmp(&b.0));
    // The folder goes to the trash with everything in it: one request.
    assert_eq!(
        patches,
        [
            (String::from("gone"), json!({"trashed": true})),
            (String::from("old"), json!({"trashed": true})),
        ]
    );
    assert_eq!(server.deletes(), Vec::<String>::new(), "never files.delete");
    assert!(
        trashed.summary().contains("2 moved to the trash"),
        "{}",
        trashed.summary()
    );
}

// --- down ---

#[tokio::test]
async fn down_creates_folders_and_files_with_drive_times() {
    let files = drive(vec![
        FakeFile::blob("a", "a.txt", "root", "aaa").modified("2024-05-06T07:08:09.123Z"),
        FakeFile::folder("sub", "sub", "root"),
        FakeFile::blob("b", "b.txt", "sub", "bb").modified("2023-01-02T03:04:05.678Z"),
        FakeFile::folder("empty", "empty", "sub"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let parent = tempfile::tempdir().unwrap();
    // Created when missing.
    let dir = parent.path().join("dest");

    let synced = sync(&server, &config(Direction::Down, &dir)).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        lines(&[
            "create a.txt",
            "create sub/",
            "create sub/b.txt",
            "create sub/empty/"
        ])
    );
    assert_eq!(read(&dir, "a.txt"), "aaa");
    assert_eq!(read(&dir, "sub/b.txt"), "bb");
    assert!(local(&dir, "sub/empty").is_dir());
    // Full precision, so that the next run trusts it.
    assert_eq!(
        modified(&local(&dir, "a.txt")),
        time("2024-05-06T07:08:09.123Z")
    );
    assert_eq!(
        modified(&local(&dir, "sub/b.txt")),
        time("2023-01-02T03:04:05.678Z")
    );
    assert_eq!(paths(&dir), ["a.txt", "sub", "sub/b.txt", "sub/empty"]);
}

#[tokio::test]
async fn down_compares_by_time_then_md5() {
    let when = "2024-05-06T07:08:09.123Z";
    let files = drive(vec![
        // Same size and Drive's time: trusted without reading, whatever the content.
        FakeFile::blob("trusted", "trusted.txt", "root", "drive").modified(when),
        // Same size and content, another time: read, unchanged, and given Drive's time.
        FakeFile::blob("same", "same.txt", "root", "same").modified(when),
        // Same size, another content and time: changed.
        FakeFile::blob("edited", "edited.txt", "root", "new!").modified(when),
        // Another size: changed without reading.
        FakeFile::blob("grown", "grown.txt", "root", "longer").modified(when),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("trusted.txt", "local"),
            ("same.txt", "same"),
            ("edited.txt", "old!"),
            ("grown.txt", "short"),
        ],
    );
    set_time(&local(dir.path(), "trusted.txt"), time(when));

    let synced = sync(&server, &config(Direction::Down, dir.path())).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        lines(&[
            "unchanged same.txt",
            "unchanged trusted.txt",
            "update edited.txt",
            "update grown.txt",
        ])
    );
    assert_eq!(read(dir.path(), "trusted.txt"), "local");
    assert_eq!(read(dir.path(), "edited.txt"), "new!");
    assert_eq!(read(dir.path(), "grown.txt"), "longer");
    let mut media = server.media();
    media.sort();
    assert_eq!(media, ["edited", "grown"]);
    // The unchanged file read for its md5 got Drive's time.
    assert_eq!(modified(&local(dir.path(), "same.txt")), time(when));
    assert_eq!(modified(&local(dir.path(), "edited.txt")), time(when));
}

#[tokio::test]
async fn a_second_down_run_reads_nothing_and_changes_nothing() {
    let files = drive(vec![
        FakeFile::blob("a", "a.txt", "root", "aaa").modified("2024-05-06T07:08:09.123Z"),
        FakeFile::folder("sub", "sub", "root"),
        FakeFile::blob("b", "b.txt", "sub", "bb").modified("2024-05-06T07:08:09.456Z"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let server = FakeDrive::start(files.clone(), Behavior::default()).await;
    sync(&server, &config(Direction::Down, dir.path()))
        .await
        .ok();
    let before = snapshot(dir.path());

    let server = FakeDrive::start(files, Behavior::default()).await;
    let again = sync(&server, &config(Direction::Down, dir.path())).await;

    again.ok();
    assert_eq!(
        again.lines(),
        lines(&["unchanged a.txt", "unchanged sub/b.txt"])
    );
    assert_eq!(server.media(), Vec::<String>::new());
    assert_eq!(snapshot(dir.path()), before);
}

#[tokio::test]
async fn up_records_nothing_and_compares_by_md5_on_every_run() {
    // Drive's time is the time of the upload: it never decides anything for `up`.
    let files = drive(vec![
        FakeFile::blob("a", "a.txt", "root", "same").modified("2024-05-06T07:08:09.123Z"),
        FakeFile::blob("b", "b.txt", "root", "drive").modified("2024-05-06T07:08:09.123Z"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("a.txt", "same"), ("b.txt", "local")]);
    // Even the exact time does not make a file of the same size unchanged.
    set_time(
        &local(dir.path(), "b.txt"),
        time("2024-05-06T07:08:09.123Z"),
    );
    let before = snapshot(dir.path());

    let server = FakeDrive::start(files, Behavior::default()).await;
    let synced = sync(&server, &config(Direction::Up, dir.path())).await;

    synced.ok();
    assert_eq!(synced.lines(), lines(&["unchanged a.txt", "update b.txt"]));
    assert_eq!(server.updates(), [(String::from("b"), b"local".to_vec())]);
    // Nothing local is written: no time, no record.
    assert_eq!(snapshot(dir.path()), before);
}

#[tokio::test]
async fn down_exports_documents_with_export_and_skips_them_without() {
    let files = drive(vec![
        FakeFile::native(
            "doc",
            "Report",
            "root",
            MIME_TYPE_DRIVE_DOCUMENT,
            "exported",
        )
        .modified("2024-05-06T07:08:09.123Z"),
        FakeFile::blob("a", "a.txt", "root", "a"),
    ]);
    let dir = tempfile::tempdir().unwrap();

    let server = FakeDrive::start(files.clone(), Behavior::default()).await;
    let skipped = sync(&server, &config(Direction::Down, dir.path())).await;
    skipped.ok();
    assert_eq!(
        skipped.lines(),
        lines(&["create a.txt", "skip:document Report"])
    );
    assert!(
        skipped.summary().contains("1 skipped (1 document)"),
        "{}",
        skipped.summary()
    );

    let exporting = Config {
        export: Some(ExportFormats::DEFAULT),
        ..config(Direction::Down, dir.path())
    };
    let server = FakeDrive::start(files.clone(), Behavior::default()).await;
    let exported = sync(&server, &exporting).await;
    exported.ok();
    assert_eq!(
        exported.lines(),
        lines(&["create Report.docx", "unchanged a.txt"])
    );
    assert_eq!(read(dir.path(), "Report.docx"), "exported");
    assert_eq!(
        modified(&local(dir.path(), "Report.docx")),
        time("2024-05-06T07:08:09.123Z")
    );

    // Unchanged by its time on the next run.
    let server = FakeDrive::start(files, Behavior::default()).await;
    let again = sync(&server, &exporting).await;
    again.ok();
    assert_eq!(
        again.lines(),
        lines(&["unchanged Report.docx", "unchanged a.txt"])
    );
    assert_eq!(server.exports(), Vec::<(String, String)>::new());
}

#[tokio::test]
async fn down_delete_moves_extras_to_the_local_trash() {
    let files = drive(vec![FakeFile::blob("a", "a.txt", "root", "a")]);
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("a.txt", "a"),
            ("old.txt", "old"),
            ("gone/x.txt", "x"),
            ("gone/y/z.txt", "z"),
        ],
    );

    let server = FakeDrive::start(files, Behavior::default()).await;
    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        lines(&[
            "extra->trash gone/",
            "extra->trash old.txt",
            "unchanged a.txt"
        ])
    );
    let trash = run_trash(dir.path());
    assert_eq!(
        paths(&trash),
        ["gone", "gone/x.txt", "gone/y", "gone/y/z.txt", "old.txt"]
    );
    assert_eq!(read(&trash, "gone/y/z.txt"), "z");
    let top = paths(dir.path())
        .into_iter()
        .filter(|path| !path.starts_with(".gdrive-trash/"))
        .collect::<Vec<_>>();
    assert_eq!(top, [".gdrive-trash", "a.txt"]);
}

#[tokio::test]
async fn a_dry_run_prints_the_plan_and_what_it_would_do() {
    let files = drive(vec![
        FakeFile::blob("same", "same.txt", "root", "same"),
        FakeFile::blob("new", "new.txt", "root", "new"),
        FakeFile::folder("folder", "folder", "root"),
        FakeFile::blob("inner", "inner.txt", "folder", "inner"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("same.txt", "same"), ("old.txt", "old")]);

    let synced = sync(&server, &dry(deleting(config(Direction::Down, dir.path())))).await;

    synced.ok();
    // Folders before their contents, then the extras.
    assert_eq!(
        synced.out.lines(),
        [
            "create folder/",
            "create folder/inner.txt",
            "create new.txt",
            "unchanged same.txt",
            "extra->trash old.txt",
        ]
    );
    assert_eq!(
        synced.err.lines(),
        [
            "Dry run, nothing was changed. Summary: 3 created, 0 updated, 1 unchanged, 1 moved to \
            the trash, 0 extra kept, 0 skipped, 0 failed, 0 cancelled"
        ]
    );
    assert_eq!(paths(dir.path()), ["old.txt", "same.txt"]);
    assert_eq!(server.media(), Vec::<String>::new());
    assert_eq!(server.generated_ids(), 0);
}

#[tokio::test]
async fn ambiguous_names_are_skipped_and_never_deleted() {
    let files = drive(vec![
        // Two Drive files with one name.
        FakeFile::blob("dup1", "dup.txt", "root", "one"),
        FakeFile::blob("dup2", "dup.txt", "root", "two"),
        // Names that differ only in case.
        FakeFile::blob("upper", "Case.txt", "root", "upper"),
        FakeFile::blob("lower", "case.txt", "root", "lower"),
        // A Drive name and a local name that differ in case.
        FakeFile::blob("photo", "Photo.JPG", "root", "photo"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    // The name a download gives the second duplicate, and the local twin in another case.
    let suffixed = crate::common::local_name::unique_local_names(&[
        crate::common::local_name::NameEntry {
            id: "dup1",
            name: "dup.txt",
        },
        crate::common::local_name::NameEntry {
            id: "dup2",
            name: "dup.txt",
        },
    ])
    .unwrap()
    .names[1]
        .clone();
    write(
        dir.path(),
        &[
            (suffixed.as_str(), "two"),
            ("photo.jpg", "local photo"),
            ("other.txt", "x"),
        ],
    );

    for direction in [Direction::Up, Direction::Down] {
        let server = FakeDrive::start(files.clone(), Behavior::default()).await;
        let synced = sync(&server, &deleting(config(direction, dir.path()))).await;
        synced.ok();
        let out = synced.lines();
        for name in ["dup.txt", "Case.txt", "case.txt"] {
            assert!(
                out.contains(&format!("skip:ambiguous {name}")),
                "{direction:?} {out:?}"
            );
        }
        assert!(
            out.contains(&format!("skip:ambiguous {suffixed}")),
            "{direction:?} {out:?}"
        );
        let photo = match direction {
            Direction::Up => "skip:ambiguous photo.jpg",
            Direction::Down => "skip:ambiguous Photo.JPG",
        };
        assert!(out.contains(&String::from(photo)), "{direction:?} {out:?}");
        assert!(
            synced.err.text().contains("letter case"),
            "{}",
            synced.err.text()
        );
        assert_eq!(server.media(), Vec::<String>::new(), "{direction:?}");
        assert_eq!(server.updates(), [], "{direction:?}");
        // Only `other.txt` is an extra (down) or new (up).
        match direction {
            Direction::Up => {
                assert_eq!(server.patches(), []);
                let created = server.created();
                assert_eq!(created.len(), 1, "{created:?}");
                assert_eq!(created[0].name, "other.txt");
            }
            Direction::Down => assert!(out.contains(&String::from("extra->trash other.txt"))),
        }
    }
    assert_eq!(read(dir.path(), &suffixed), "two");
    assert_eq!(read(dir.path(), "photo.jpg"), "local photo");
}

#[tokio::test]
async fn a_failed_item_fails_the_sync_and_the_rest_goes_on_with_continue_on_error() {
    let files = drive(vec![
        FakeFile::blob("a", "a.txt", "root", "a"),
        FakeFile::blob("missing", "missing.txt", "root", "m"),
    ]);
    let behavior = Behavior {
        media_missing: [(String::from("missing"), Duration::ZERO)].into(),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        transfer: Options::new(NonZeroUsize::new(1).unwrap(), true),
        ..config(Direction::Down, dir.path())
    };

    let synced = sync(&server, &config).await;

    let error = synced.error();
    assert!(
        error.contains("the sync is incomplete: 1 item failed"),
        "{error}"
    );
    assert_eq!(read(dir.path(), "a.txt"), "a");
    assert!(synced
        .err
        .text()
        .contains("unable to download 'missing.txt'"));
    assert!(
        synced.summary().contains("1 created"),
        "{}",
        synced.summary()
    );
    assert!(
        synced.summary().contains("1 failed"),
        "{}",
        synced.summary()
    );
}

#[tokio::test]
async fn the_folder_must_be_a_folder_and_up_needs_the_local_directory() {
    let files = drive(vec![FakeFile::blob("file", "file.txt", "root", "x")]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let not_a_folder = Config {
        folder_id: String::from("file"),
        ..config(Direction::Down, dir.path())
    };
    let synced = sync(&server, &not_a_folder).await;
    assert!(
        synced.error().contains("is not a folder"),
        "{}",
        synced.error()
    );

    let missing = dir.path().join("missing");
    let synced = sync(&server, &config(Direction::Up, &missing)).await;
    assert!(
        synced.error().contains("does not exist"),
        "{}",
        synced.error()
    );
    assert!(!missing.exists());
}

#[tokio::test]
async fn names_match_as_a_download_maps_them() {
    // Decomposed (NFD, as macOS writes names) on Drive, composed (NFC) locally; and a name that
    // a download changes (`con.txt` becomes `con_.txt`) on both sides.
    let files = drive(vec![
        FakeFile::blob("cafe", "cafe\u{301}.txt", "root", "same"),
        FakeFile::blob("con", "con.txt", "root", "device"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[("caf\u{e9}.txt", "same"), ("con.txt", "device")],
    );

    for direction in [Direction::Up, Direction::Down] {
        let server = FakeDrive::start(files.clone(), Behavior::default()).await;
        let synced = sync(&server, &deleting(config(direction, dir.path()))).await;
        synced.ok();
        let out = synced.lines();
        assert_eq!(out.len(), 2, "{direction:?} {out:?}");
        assert!(
            out.iter().all(|line| line.starts_with("unchanged ")),
            "{direction:?} {out:?}"
        );
        assert_eq!(server.created().len(), 0);
        assert_eq!(server.patches(), []);
    }
    assert_eq!(paths(dir.path()), ["caf\u{e9}.txt", "con.txt"]);
}

/// Two local names that differ only in normalization: one file on macOS, two on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn local_names_that_differ_only_in_normalization_are_ambiguous() {
    let server = FakeDrive::start(drive(Vec::new()), Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("caf\u{e9}.txt", "composed"),
            ("cafe\u{301}.txt", "decomposed"),
        ],
    );

    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        lines(&[
            "skip:ambiguous caf\u{e9}.txt",
            "skip:ambiguous cafe\u{301}.txt"
        ])
    );
    assert_eq!(paths(dir.path()).len(), 2);
    assert!(
        synced.err.text().contains("Unicode normalization"),
        "{}",
        synced.err.text()
    );
}

#[tokio::test]
async fn a_second_up_run_finds_everything_unchanged() {
    let files = drive(vec![
        FakeFile::blob("changed", "changed.txt", "root", "old"),
        FakeFile::blob("old", "old.txt", "root", "old"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[("changed.txt", "new content"), ("dir/new.txt", "new")],
    );
    let config = deleting(config(Direction::Up, dir.path()));

    let first = sync(&server, &config).await;
    first.ok();
    assert_eq!(
        first.lines(),
        lines(&[
            "create dir/",
            "create dir/new.txt",
            "extra->trash old.txt",
            "update changed.txt",
        ])
    );

    // The same Drive, which now holds what the first run did.
    let second = sync(&server, &config).await;
    second.ok();
    assert_eq!(
        second.lines(),
        lines(&["unchanged changed.txt", "unchanged dir/new.txt"])
    );
    assert_eq!(server.created().len(), 2, "nothing created again");
    assert_eq!(server.updates().len(), 1, "nothing updated again");
}

#[tokio::test]
async fn a_large_changed_file_goes_up_through_a_resumable_update() {
    let files = drive(vec![FakeFile::blob("big", "big.bin", "root", "old")]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    // Two chunks of 256 KiB and a bit.
    let content = "x".repeat(2 * 256 * 1024 + 10);
    write(dir.path(), &[("big.bin", content.as_str())]);
    let config = Config {
        upload: UploadOptions {
            chunk_size: 256 * 1024,
            multipart_threshold: 0,
            ..UploadOptions::default()
        },
        ..config(Direction::Up, dir.path())
    };

    let synced = sync(&server, &config).await;

    synced.ok();
    assert_eq!(synced.lines(), lines(&["update big.bin"]));
    assert_eq!(
        server.updates(),
        [(String::from("big"), content.as_bytes().to_vec())]
    );
    let chunks = server.chunks();
    assert_eq!(chunks.len(), 3, "{chunks:?}");
    assert!(chunks
        .iter()
        .all(|chunk| chunk.status == 308 || chunk.status == 200));
    assert_eq!(server.created().len(), 0);

    let again = sync(&server, &config).await;
    again.ok();
    assert_eq!(again.lines(), lines(&["unchanged big.bin"]));
}

/// The totals a run planned, the bytes it confirmed, the file jobs that ended.
#[derive(Debug, Default, PartialEq, Eq)]
struct Shown {
    planned: Option<(u64, u64)>,
    confirmed: u64,
    finished: u64,
}

#[derive(Default)]
struct Recorder(std::sync::Mutex<Shown>);

impl ProgressSink for Recorder {
    fn event(&self, event: &ProgressEvent<'_>) {
        let mut shown = self.0.lock().unwrap();
        match *event {
            ProgressEvent::Planned { files, bytes } => shown.planned = Some((files, bytes)),
            ProgressEvent::Bytes { bytes, .. } => shown.confirmed += bytes,
            ProgressEvent::Finished {
                kind: JobKind::Media,
                ..
            } => shown.finished += 1,
            _ => {}
        }
    }
}

#[tokio::test]
async fn the_transfers_of_a_sync_show_their_progress() {
    for direction in [Direction::Up, Direction::Down] {
        let files = drive(vec![
            FakeFile::blob("same", "same.txt", "root", "same").modified("2024-05-06T07:08:09.123Z"),
            FakeFile::blob("changed", "changed.txt", "root", "drive!"),
            FakeFile::blob("drive-only", "drive.txt", "root", "drive only"),
        ]);
        let server = FakeDrive::start(files, Behavior::default()).await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                ("same.txt", "same"),
                ("changed.txt", "local"),
                ("new/local.txt", "local only"),
            ],
        );
        let recorder = std::sync::Arc::new(Recorder::default());
        let display = ProgressDisplay::reporting_to(
            std::sync::Arc::clone(&recorder) as std::sync::Arc<dyn ProgressSink>
        );

        let synced = sync_showing(
            &server,
            &config(direction, dir.path()),
            display,
            future::pending(),
        )
        .await;

        synced.ok();
        let shown = recorder.0.lock().unwrap();
        // `same.txt` is unchanged by md5: `down` gives it Drive's time, a job too.
        let expected = match direction {
            // changed.txt (5 bytes) and new/local.txt (10), the folder's file a follow-up.
            Direction::Up => Shown {
                planned: Some((2, 15)),
                confirmed: 15,
                finished: 2,
            },
            // changed.txt (6), drive.txt (10), and the time of same.txt.
            Direction::Down => Shown {
                planned: Some((3, 16)),
                confirmed: 16,
                finished: 3,
            },
        };
        assert_eq!(*shown, expected, "{direction:?}");
    }
}
