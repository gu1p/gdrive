//! `files download --recursive` against the fake Drive, which answers listings and media
//! downloads concurrently.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::{self, Future},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use super::{download_directory, PartialFiles};
use crate::{
    common::drive_file::MIME_TYPE_DRIVE_FOLDER,
    files::download::{errors, Config, Destination, ExistingFileAction},
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile},
        progress::{JobId, Mode, NoProgress, ProgressEvent, ProgressSink},
        scheduler::{
            capture::{output, Captured},
            Options,
        },
    },
};

fn config(destination: &Path, jobs: usize, continue_on_error: bool) -> Config {
    Config {
        file_id: String::from("root"),
        existing_file_action: ExistingFileAction::Abort,
        follow_shortcuts: false,
        download_directories: true,
        destination: Destination::Path(destination.to_path_buf()),
        acknowledge_abuse: false,
        transfer: Options::new(NonZeroUsize::new(jobs).unwrap(), continue_on_error),
        progress: Mode::Never,
    }
}

fn root_folder() -> google_drive3::api::File {
    google_drive3::api::File {
        id: Some(String::from("root")),
        name: Some(String::from("Root")),
        mime_type: Some(MIME_TYPE_DRIVE_FOLDER.to_string()),
        ..google_drive3::api::File::default()
    }
}

struct Downloaded {
    result: Result<(), errors::Download>,
    out: Captured,
    err: Captured,
}

impl Downloaded {
    fn error(&self) -> String {
        match &self.result {
            Ok(()) => panic!("the download should fail"),
            Err(error) => crate::error_message(error),
        }
    }
}

async fn download(
    server: &FakeDrive,
    config: &Config,
    cancel: impl Future<Output = ()> + Send,
) -> Downloaded {
    download_reporting(server, config, Arc::new(NoProgress), cancel).await
}

/// [`download`], reporting to `progress`.
async fn download_reporting(
    server: &FakeDrive,
    config: &Config,
    progress: Arc<dyn ProgressSink>,
    cancel: impl Future<Output = ()> + Send,
) -> Downloaded {
    let output = output();
    let hub = server.hub();
    let run = download_directory(
        &hub,
        root_folder(),
        config,
        &output.printer,
        progress,
        cancel,
    );
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the download should end");
    Downloaded {
        result,
        out: output.out,
        err: output.err,
    }
}

/// `path` (with `/`) below `dir`.
fn local(dir: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(dir.to_path_buf(), |path, name| path.join(name))
}

/// `path` with the platform's separator, as gdrive prints it.
fn shown(path: &str) -> String {
    local(Path::new(""), path).display().to_string()
}

/// The names in `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Root with files and folders on three levels; every file's content is its id.
fn three_levels() -> Vec<FakeFile> {
    let mut files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::folder("a1", "a1", "a"),
    ];
    for folder in ["root", "a", "b", "a1"] {
        for i in 0..4 {
            let id = format!("{folder}-f{i}");
            files.push(FakeFile::blob(&id, &format!("{id}.txt"), folder, &id));
        }
    }
    files
}

/// The local path and content of every blob of `files` below the destination `dir`.
fn expected_files(files: &[FakeFile]) -> Vec<(String, String)> {
    let path_of = |id: &str| -> String {
        let mut names = Vec::new();
        let mut current = id;
        while current != "root" {
            let file = files.iter().find(|file| file.id == current).unwrap();
            names.push(file.name.clone());
            current = &file.parents[0];
        }
        names.push(String::from("Root"));
        names.reverse();
        names.join("/")
    };
    files
        .iter()
        .filter_map(|file| Some((path_of(&file.id), file.content.clone()?)))
        .collect()
}

#[tokio::test]
async fn three_levels_download_with_at_most_jobs_transfers_at_once() {
    let files = three_levels();
    let behavior = Behavior {
        delay: Duration::from_millis(10),
        media_delay: Duration::from_millis(40),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files.clone(), behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 3, false),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    for (path, content) in expected_files(&files) {
        assert_eq!(
            std::fs::read_to_string(local(destination.path(), &path)).unwrap(),
            content,
            "{path}"
        );
    }
    assert_eq!(server.media().len(), 16);
    assert_eq!(server.media_max_in_flight(), 3);

    let out = downloaded.out.lines();
    assert_eq!(out[0], "Creating directory Root");
    for path in ["Root/a", "Root/b", "Root/a/a1"] {
        assert!(
            out.contains(&format!("Creating directory {}", shown(path))),
            "{out:?}"
        );
    }
    for (path, _) in expected_files(&files) {
        assert!(
            out.contains(&format!("Downloading file '{}'", shown(&path))),
            "{path}"
        );
    }
    // 16 files of 7 or 8 bytes ("root-f0" ... "a1-f3").
    let size = expected_files(&files)
        .iter()
        .map(|(_, content)| content.len())
        .sum::<usize>();
    assert!(out.contains(&format!(
        "Found 16 files in 4 directories with a total size of {size} B"
    )));
    assert_eq!(
        out.last().unwrap(),
        &format!("Downloaded 16 files in 4 directories with a total size of {size} B")
    );
    assert_eq!(out.len(), 4 + 16 + 2);
    assert_eq!(
        downloaded.err.lines(),
        [format!(
            "Summary: 16 transferred ({size} B), 0 unchanged, 0 skipped, 0 failed, 0 cancelled"
        )]
    );
}

#[tokio::test]
async fn downloads_start_while_the_folder_is_still_being_listed() {
    let behavior = Behavior {
        delay: Duration::from_millis(150),
        ..Behavior::default()
    };
    let server = FakeDrive::start(three_levels(), behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 4, false),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    let (first_media, last_list) = server.first_media_and_last_list();
    assert!(
        first_media.unwrap() < last_list.unwrap(),
        "the root's files are downloaded while a1 is listed"
    );
    // The root, then a and b in one query, then a1.
    assert_eq!(server.request_count(), 3);
}

/// A folder that cannot be listed next to one that can.
fn with_unreadable_folder() -> (Vec<FakeFile>, Behavior) {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::blob("a-f", "a.txt", "a", "aaa"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::blob("b-f", "b.txt", "b", "bbb"),
    ];
    let behavior = Behavior {
        unreadable: HashSet::from([String::from("b")]),
        ..Behavior::default()
    };
    (files, behavior)
}

#[tokio::test]
async fn a_folder_that_cannot_be_listed_fails_and_the_others_finish_with_continue_on_error() {
    let (files, behavior) = with_unreadable_folder();
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 2, true),
        future::pending(),
    )
    .await;

    assert_eq!(
        downloaded.error(),
        "Error: the recursive download is incomplete: 1 folder failed"
    );
    assert_eq!(
        std::fs::read_to_string(local(destination.path(), "Root/a/a.txt")).unwrap(),
        "aaa"
    );
    let err = downloaded.err.lines();
    assert!(
        err[0].starts_with("Error: unable to list folder 'b' with id 'b'"),
        "{err:?}"
    );
    assert_eq!(
        err.last().unwrap(),
        "Summary: 1 transferred (3 B), 0 unchanged, 0 skipped, 0 failed, 0 cancelled; \
        1 folder failed"
    );
    assert!(
        !downloaded.out.text().contains("Downloaded "),
        "no success line for an incomplete download"
    );
}

#[tokio::test]
async fn a_folder_that_cannot_be_listed_stops_the_download_by_default() {
    let (files, behavior) = with_unreadable_folder();
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 2, false),
        future::pending(),
    )
    .await;

    let error = downloaded.error();
    assert!(
        error.starts_with("Error: the recursive download is incomplete: 1 folder failed"),
        "{error}"
    );
    assert!(
        error.ends_with("; use --continue-on-error to finish the other transfers after a failure"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_file_stops_the_download_and_removes_the_temporary_files() {
    let files = vec![
        FakeFile::blob("stalled", "stalled.bin", "root", "0123456789"),
        FakeFile::blob("bad", "bad.bin", "root", "never"),
        FakeFile::blob("later", "later.bin", "root", "later"),
    ];
    let behavior = Behavior {
        media_stalled: HashSet::from([String::from("stalled")]),
        media_missing: HashMap::from([(String::from("bad"), Duration::from_millis(300))]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 2, false),
        future::pending(),
    )
    .await;

    let error = downloaded.error();
    assert!(
        error
            .starts_with("Error: the recursive download is incomplete: 1 file failed, 2 cancelled"),
        "{error}"
    );
    assert!(error.contains("--continue-on-error"), "{error}");
    let err = downloaded.err.lines();
    assert!(
        err[0].starts_with(&format!(
            "Error: unable to download '{}': unable to download file from drive",
            shown("Root/bad.bin")
        )),
        "{err:?}"
    );
    assert_eq!(
        err.last().unwrap(),
        "Summary: 0 transferred (0 B), 0 unchanged, 0 skipped, 1 failed, 2 cancelled"
    );
    // The stalled download had written half of its content to a temporary file.
    let stalled = server.media().iter().filter(|id| *id == "stalled").count();
    assert_eq!(stalled, 1);
    assert_eq!(
        entries(&destination.path().join("Root")),
        Vec::<String>::new(),
        "no temporary file and no file is left"
    );
}

/// Waits until `path` exists.
async fn appears(path: &Path) {
    while !path.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn ctrl_c_removes_only_the_temporary_files_of_the_cancelled_downloads() {
    let files = vec![
        FakeFile::blob("s1", "s1.bin", "root", "0123456789"),
        FakeFile::blob("s2", "s2.bin", "root", "abcdefghij"),
        FakeFile::blob("done", "done.bin", "root", "done"),
    ];
    let behavior = Behavior {
        media_stalled: HashSet::from([String::from("s1"), String::from("s2")]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("Root");
    std::fs::create_dir(&root).unwrap();
    // Temporary names that are not this run's: s1's first name is taken, so its download takes
    // the second; s2's first name is free, but its third is taken (a gap).
    std::fs::write(root.join(".gdrive-s1.part"), "stale").unwrap();
    std::fs::write(root.join(".gdrive-s2-2.part"), "other process").unwrap();

    // Once both downloads write their temporary files, another gdrive takes the next names,
    // and then Ctrl-C comes.
    let cancel = {
        let root = root.clone();
        async move {
            appears(&root.join(".gdrive-s1-1.part")).await;
            appears(&root.join(".gdrive-s2.part")).await;
            std::fs::write(root.join(".gdrive-s1-2.part"), "concurrent").unwrap();
            std::fs::write(root.join(".gdrive-s2-1.part"), "concurrent").unwrap();
        }
    };
    let downloaded = download(&server, &config(destination.path(), 3, false), cancel).await;

    assert_eq!(
        downloaded.error(),
        "Error: the recursive download is incomplete: interrupted, 2 cancelled"
    );
    assert_eq!(
        downloaded.err.lines()[0],
        "Interrupted: cancelling the transfers in progress"
    );
    assert_eq!(
        entries(&root),
        [
            ".gdrive-s1-2.part",
            ".gdrive-s1.part",
            ".gdrive-s2-1.part",
            ".gdrive-s2-2.part",
            "done.bin"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".gdrive-s1.part")).unwrap(),
        "stale"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".gdrive-s2-2.part")).unwrap(),
        "other process"
    );
}

#[tokio::test]
async fn the_cleanup_removes_the_files_it_created_and_no_other_file_of_these_names() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("x.bin");
    let partial = PartialFiles::new(target.clone(), "X");
    std::fs::write(
        dir.path().join(".gdrive-X-2.part"),
        "left by another process",
    )
    .unwrap();

    // The first free name, as `save_to_file` takes it; then the content starts over.
    let (first, file) = super::super::create_temp_file(&target, "X").await.unwrap();
    drop(partial.track(file));
    let (second, file) = super::super::create_temp_file(&target, "X").await.unwrap();
    drop(partial.track(file));
    assert_eq!(
        (first.file_name().unwrap(), second.file_name().unwrap()),
        (".gdrive-X.part".as_ref(), ".gdrive-X-1.part".as_ref())
    );
    // The first was replaced by another file of the same name since.
    std::fs::remove_file(&first).unwrap();
    std::fs::write(&first, "someone else's").unwrap();

    partial.remove().await;

    assert_eq!(entries(dir.path()), [".gdrive-X-2.part", ".gdrive-X.part"]);
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "someone else's");
}

#[tokio::test]
async fn documents_and_shortcuts_are_counted_as_skipped() {
    let files = vec![
        FakeFile::blob("x", "x.txt", "root", "x"),
        FakeFile::document("d1", "notes", "root"),
        FakeFile::folder("a", "a", "root"),
        FakeFile::document("d2", "plan", "a"),
        FakeFile::shortcut("s", "link", "a", "x", "text/plain"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 2, false),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    assert_eq!(server.media(), ["x"], "documents have no media to download");
    assert_eq!(
        downloaded.err.lines(),
        [
            "2 Google Docs files skipped; use `gdrive files export` to download them",
            "1 shortcut skipped; a recursive download does not follow shortcuts",
            "Summary: 1 transferred (1 B), 0 unchanged, 3 skipped, 0 failed, 0 cancelled",
        ]
    );
    assert_eq!(
        downloaded.out.lines().last().unwrap(),
        "Downloaded 1 files in 2 directories with a total size of 1 B"
    );
}

#[tokio::test]
async fn an_incomplete_search_fails_the_download_unless_continue_on_error() {
    let files = vec![FakeFile::blob("x", "x.txt", "root", "x")];
    let behavior = Behavior {
        incomplete: HashSet::from([String::from("root")]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;

    let destination = tempfile::tempdir().unwrap();
    let strict = download(
        &server,
        &config(destination.path(), 2, false),
        future::pending(),
    )
    .await;
    let error = strict.error();
    assert!(
        error.starts_with("Error: Drive did not search everything while listing the folders"),
        "{error}"
    );
    assert!(error.contains("--continue-on-error"), "{error}");
    assert!(strict.err.lines()[0].starts_with("Warning: Drive did not search everything"));
    assert!(local(destination.path(), "Root/x.txt").exists());

    let destination = tempfile::tempdir().unwrap();
    let lenient = download(
        &server,
        &config(destination.path(), 2, true),
        future::pending(),
    )
    .await;
    lenient.result.unwrap();
    assert!(lenient.out.text().contains("Downloaded 1 files"));
}

#[tokio::test]
async fn identical_local_files_are_unchanged_with_overwrite() {
    let files = vec![
        FakeFile::blob("same", "same.txt", "root", "same"),
        FakeFile::blob("new", "new.txt", "root", "new content"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("Root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("same.txt"), "same").unwrap();
    std::fs::write(root.join("new.txt"), "old content").unwrap();
    let config = Config {
        existing_file_action: ExistingFileAction::Overwrite,
        ..config(destination.path(), 2, false)
    };

    let downloaded = download(&server, &config, future::pending()).await;

    downloaded.result.unwrap();
    assert_eq!(server.media(), ["new"]);
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "new content"
    );
    assert_eq!(
        downloaded.err.lines(),
        ["Summary: 1 transferred (11 B), 1 unchanged, 0 skipped, 0 failed, 0 cancelled"]
    );
}

#[tokio::test]
async fn an_existing_local_file_fails_without_overwrite() {
    let files = vec![FakeFile::blob("x", "x.txt", "root", "x")];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("Root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x.txt"), "mine").unwrap();

    let downloaded = download(
        &server,
        &config(destination.path(), 2, false),
        future::pending(),
    )
    .await;

    assert!(downloaded
        .error()
        .starts_with("Error: the recursive download is incomplete"));
    let err = downloaded.err.lines();
    assert!(
        err[0].starts_with(&format!(
            "Error: unable to download '{}': file '",
            shown("Root/x.txt")
        )) && err[0].ends_with("' already exists, use --overwrite to overwrite it"),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(root.join("x.txt")).unwrap(), "mine");
    assert_eq!(server.media(), Vec::<String>::new());
}

/// The bytes reported for each file of a recursive download, by label, and whether the file was
/// in place already when they were.
struct Written {
    destination: PathBuf,
    labels: Mutex<HashMap<JobId, String>>,
    reports: Mutex<BTreeMap<String, Vec<(u64, bool)>>>,
}

impl ProgressSink for Written {
    fn event(&self, event: &ProgressEvent<'_>) {
        match event {
            ProgressEvent::Queued { job, label, .. } => {
                self.labels
                    .lock()
                    .unwrap()
                    .insert(*job, (*label).to_owned());
            }
            ProgressEvent::Bytes { job, bytes } => {
                let label = self.labels.lock().unwrap()[job].clone();
                let renamed = self.destination.join(&label).exists();
                let mut reports = self.reports.lock().unwrap();
                reports.entry(label).or_default().push((*bytes, renamed));
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn downloaded_bytes_are_reported_as_they_are_written() {
    let big = "0123456789".repeat(30_000);
    let files = vec![
        FakeFile::blob("big", "big.bin", "root", &big),
        FakeFile::blob("small", "small.txt", "root", "small"),
        FakeFile::blob("same", "same.txt", "root", "same"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("Root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("same.txt"), "same").unwrap();
    let config = Config {
        existing_file_action: ExistingFileAction::Overwrite,
        ..config(destination.path(), 2, false)
    };
    let written = Arc::new(Written {
        destination: destination.path().to_path_buf(),
        labels: Mutex::default(),
        reports: Mutex::default(),
    });

    let downloaded = download_reporting(
        &server,
        &config,
        Arc::clone(&written) as Arc<dyn ProgressSink>,
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    let reports = written.reports.lock().unwrap();
    let totals = reports
        .iter()
        .map(|(label, reports)| {
            let bytes = reports.iter().map(|(bytes, _)| bytes).sum::<u64>();
            (label.clone(), bytes)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        totals,
        [
            (shown("Root/big.bin"), big.len() as u64),
            (shown("Root/small.txt"), 5),
        ],
        "an unchanged file transfers nothing"
    );
    for (label, reports) in reports.iter() {
        assert!(
            reports.iter().all(|(_, renamed)| !renamed),
            "{label}: reported while it was written: {reports:?}"
        );
    }
}
