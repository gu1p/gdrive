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
    common::drive_file::{
        MIME_TYPE_DRIVE_DOCUMENT, MIME_TYPE_DRIVE_DRAWING, MIME_TYPE_DRIVE_FOLDER,
        MIME_TYPE_DRIVE_PRESENTATION, MIME_TYPE_DRIVE_SPREADSHEET,
    },
    files::{
        download::{errors, Config, Destination, ExistingFileAction},
        export::ExportFormats,
    },
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile},
        progress::{JobId, Mode, NoProgress, ProgressDisplay, ProgressEvent, ProgressSink},
        scheduler::{
            capture::{output, Captured},
            Options, Printer,
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
        export: None,
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
        ProgressDisplay::reporting_to(progress),
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

/// `path` with the platform's separator, as gdrive prints local paths on stdout. Warnings and
/// errors show paths below the destination with `/`.
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
        // The first three downloads wait for each other: all three are in flight at once.
        media_rendezvous: 3,
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
        // The listing of a1 waits for a download: it ends only if downloads start before the
        // walk is done (else after a timeout, too late for the check below).
        list_after_media: HashSet::from([String::from("a1")]),
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
    // The path below the destination, with `/` on every platform.
    assert!(
        err[0].starts_with(
            "Error: unable to download 'Root/bad.bin': unable to download file from drive"
        ),
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

/// Waits until the file at `path` has content. A download writes to its temporary file only
/// after the cleanup took note of it; a file that merely exists may not be noted yet.
async fn written(path: &Path) {
    while !std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn ctrl_c_removes_only_the_temporary_files_of_the_cancelled_downloads() {
    // Two at a time, in this order: s2 starts only once the job of done.bin has ended, so Ctrl-C
    // (once s2 writes) never finds done.bin in flight, however slow the machine is.
    let files = vec![
        FakeFile::blob("done", "done.bin", "root", "done"),
        FakeFile::blob("s1", "s1.bin", "root", "0123456789"),
        FakeFile::blob("s2", "s2.bin", "root", "abcdefghij"),
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

    // Once done.bin is complete and both other downloads write their temporary files, another
    // gdrive takes the next names, and then Ctrl-C comes. done.bin is waited for, and its job
    // given a moment to return after the rename: under load it may finish after the others
    // started writing.
    let cancel = {
        let root = root.clone();
        async move {
            written(&root.join(".gdrive-s1-1.part")).await;
            written(&root.join(".gdrive-s2.part")).await;
            std::fs::write(root.join(".gdrive-s1-2.part"), "concurrent").unwrap();
            std::fs::write(root.join(".gdrive-s2-1.part"), "concurrent").unwrap();
        }
    };
    let downloaded = download(&server, &config(destination.path(), 2, false), cancel).await;

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
async fn a_temporary_file_replaced_right_away_under_its_name_is_left_alone() {
    // ext4 gives the inode of a removed file to the next file created, and Windows may give a
    // file created under a name that was just removed the old one's identity: compared without an
    // open handle, the new file passed for the job's own.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("x.bin");
    for round in 0..200 {
        let partial = PartialFiles::new(target.clone(), "X");
        let (temp, file) = super::super::create_temp_file(&target, "X").await.unwrap();
        drop(partial.track(file));
        assert_eq!(temp.file_name().unwrap(), ".gdrive-X.part");
        std::fs::remove_file(&temp).unwrap();
        std::fs::write(&temp, "someone else's").unwrap();

        partial.remove().await;

        assert_eq!(
            std::fs::read_to_string(&temp).unwrap(),
            "someone else's",
            "round {round}"
        );
        std::fs::remove_file(&temp).unwrap();
    }
}

#[tokio::test]
async fn the_kept_handle_lets_the_download_rename_its_temporary_file_into_place() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("x.bin");
    let partial = PartialFiles::new(target.clone(), "X");
    let (temp, file) = super::super::create_temp_file(&target, "X").await.unwrap();
    let mut file = partial.track(file);
    file.write_all(b"content").await.unwrap();
    file.flush().await.unwrap();
    drop(file);

    // As `save_to_file` does, while the cleanup still holds the file open.
    tokio::fs::rename(&temp, &target).await.unwrap();
    partial.remove().await;

    assert_eq!(entries(dir.path()), ["x.bin"]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "content");
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
    // The path below the destination with `/`, then the local file as the platform shows it.
    assert!(
        err[0].starts_with("Error: unable to download 'Root/x.txt': file '")
            && err[0].ends_with("' already exists, use --overwrite to overwrite it"),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(root.join("x.txt")).unwrap(), "mine");
    assert_eq!(server.media(), Vec::<String>::new());
}

/// `config` exporting per `spec`.
fn exporting(destination: &Path, spec: &str, continue_on_error: bool) -> Config {
    Config {
        export: Some(ExportFormats::parse(spec).unwrap()),
        ..config(destination, 2, continue_on_error)
    }
}

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

/// A blob, a document of each exportable type (one in a subfolder) and a form, which has no
/// export format.
fn mixed_tree() -> Vec<FakeFile> {
    vec![
        FakeFile::blob("x", "x.txt", "root", "x"),
        FakeFile::native("doc", "report", "root", MIME_TYPE_DRIVE_DOCUMENT, "doc!")
            .modified("2024-05-06T07:08:09.123Z"),
        FakeFile::native(
            "sheet",
            "budget",
            "root",
            MIME_TYPE_DRIVE_SPREADSHEET,
            "sheet!",
        ),
        FakeFile::native(
            "slides",
            "deck",
            "root",
            MIME_TYPE_DRIVE_PRESENTATION,
            "slides!",
        ),
        FakeFile::native(
            "drawing",
            "diagram",
            "root",
            MIME_TYPE_DRIVE_DRAWING,
            "drawing!",
        ),
        FakeFile::native(
            "form",
            "survey",
            "root",
            "application/vnd.google-apps.form",
            "",
        ),
        FakeFile::folder("a", "a", "root"),
        FakeFile::native("notes", "notes", "a", MIME_TYPE_DRIVE_DOCUMENT, "notes!"),
    ]
}

/// The exports of `server`, sorted, as (file id, MIME type).
fn exports(server: &FakeDrive) -> Vec<(String, String)> {
    let mut exports = server.exports();
    exports.sort();
    exports
}

fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|&(a, b)| (a.to_owned(), b.to_owned()))
        .collect()
}

#[tokio::test]
async fn the_default_profile_exports_every_exportable_type_and_counts_the_form_as_skipped() {
    let server = FakeDrive::start(mixed_tree(), Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &exporting(destination.path(), "default", false),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    let root = destination.path().join("Root");
    assert_eq!(
        entries(&root),
        [
            "a",
            "budget.xlsx",
            "deck.pptx",
            "diagram.png",
            "report.docx",
            "x.txt"
        ]
    );
    assert_eq!(entries(&root.join("a")), ["notes.docx"]);
    for (path, content) in [
        ("Root/report.docx", "doc!"),
        ("Root/budget.xlsx", "sheet!"),
        ("Root/deck.pptx", "slides!"),
        ("Root/diagram.png", "drawing!"),
        ("Root/a/notes.docx", "notes!"),
        ("Root/x.txt", "x"),
    ] {
        assert_eq!(
            std::fs::read_to_string(local(destination.path(), path)).unwrap(),
            content,
            "{path}"
        );
    }
    assert_eq!(
        exports(&server),
        pairs(&[
            ("doc", DOCX),
            ("drawing", "image/png"),
            ("notes", DOCX),
            ("sheet", XLSX),
            ("slides", PPTX),
        ])
    );
    assert_eq!(server.media(), ["x"]);

    // The export has the document's modification time.
    let modified = std::fs::metadata(root.join("report.docx"))
        .unwrap()
        .modified()
        .unwrap();
    let modified = chrono::DateTime::<chrono::Utc>::from(modified);
    assert_eq!(modified.timestamp_millis(), 1_714_979_289_123);

    let out = downloaded.out.lines();
    assert!(
        out.contains(&format!(
            "Exporting document '{}'",
            shown("Root/report.docx")
        )),
        "{out:?}"
    );
    assert!(out.contains(&format!(
        "Exporting drawing '{}'",
        shown("Root/diagram.png")
    )));
    assert!(out.contains(&String::from("Found 5 Google Docs files to export")));
    assert_eq!(
        out[out.len() - 2..],
        [
            String::from("Downloaded 1 files in 2 directories with a total size of 1 B"),
            String::from("Exported 5 Google Docs files"),
        ]
    );
    // 1 + 4 + 6 + 7 + 8 + 6 bytes.
    assert_eq!(
        downloaded.err.lines(),
        [
            "Warning: skipping 'Root/survey' (application/vnd.google-apps.form): gdrive has no \
            export format for this type",
            "1 Google Workspace file skipped; gdrive has no export format for their types \
            (Forms, Sites, My Maps, Apps Script, ...)",
            "Summary: 6 transferred (32 B), 0 unchanged, 1 skipped, 0 failed, 0 cancelled",
        ]
    );
}

#[tokio::test]
async fn overrides_choose_the_format_of_each_type() {
    let server = FakeDrive::start(mixed_tree(), Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &exporting(
            destination.path(),
            "docs=pdf,sheets=csv,slides=pdf,drawings=svg",
            false,
        ),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    let root = destination.path().join("Root");
    assert_eq!(
        entries(&root),
        [
            "a",
            "budget.csv",
            "deck.pdf",
            "diagram.svg",
            "report.pdf",
            "x.txt"
        ]
    );
    assert_eq!(entries(&root.join("a")), ["notes.pdf"]);
    assert_eq!(
        exports(&server),
        pairs(&[
            ("doc", "application/pdf"),
            ("drawing", "image/svg+xml"),
            ("notes", "application/pdf"),
            ("sheet", "text/csv"),
            ("slides", "application/pdf"),
        ])
    );
}

#[tokio::test]
async fn the_md_profile_exports_documents_as_markdown_and_skip_leaves_a_type_out() {
    let server = FakeDrive::start(mixed_tree(), Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &exporting(destination.path(), "md,sheets=skip,drawings=skip", false),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    let root = destination.path().join("Root");
    assert_eq!(entries(&root), ["a", "deck.pptx", "report.md", "x.txt"]);
    assert_eq!(entries(&root.join("a")), ["notes.md"]);
    assert_eq!(
        exports(&server),
        pairs(&[
            ("doc", "text/markdown"),
            ("notes", "text/markdown"),
            ("slides", PPTX),
        ])
    );
    let err = downloaded.err.lines();
    assert!(
        err.contains(&String::from(
            "2 Google Docs files skipped; use `gdrive files export` to download them"
        )),
        "{err:?}"
    );
    assert_eq!(
        err.last().unwrap(),
        "Summary: 4 transferred (18 B), 0 unchanged, 3 skipped, 0 failed, 0 cancelled"
    );
}

#[tokio::test]
async fn an_export_and_a_file_of_the_same_name_get_distinct_local_names() {
    let files = vec![
        FakeFile::blob("b-report", "report.docx", "root", "the uploaded file"),
        FakeFile::native(
            "d-report",
            "report",
            "root",
            MIME_TYPE_DRIVE_DOCUMENT,
            "the document",
        ),
        // Already has the extension: it is not added twice.
        FakeFile::native(
            "d-notes",
            "notes.md",
            "root",
            MIME_TYPE_DRIVE_DOCUMENT,
            "# notes",
        ),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &exporting(destination.path(), "md,docs=docx", false),
        future::pending(),
    )
    .await;

    downloaded.result.unwrap();
    let root = destination.path().join("Root");
    let names = entries(&root);
    assert_eq!(names.len(), 3, "{names:?}");
    assert_eq!(names[0], "notes.md.docx");
    assert_eq!(names[1], "report.docx");
    assert!(
        names[2].starts_with("report~")
            && Path::new(&names[2]).extension() == Some("docx".as_ref()),
        "{names:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("report.docx")).unwrap(),
        "the uploaded file",
        "the lower id keeps the name"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(&names[2])).unwrap(),
        "the document"
    );
    let err = downloaded.err.lines();
    assert!(
        err[0].starts_with("Warning: In 'Root': 'report.docx' (id d-report) is saved as 'report~"),
        "{err:?}"
    );
}

/// A tree with an export Drive refuses as too large next to two that work.
fn with_too_large_export() -> (Vec<FakeFile>, Behavior) {
    let files = vec![
        FakeFile::native("big", "big", "root", MIME_TYPE_DRIVE_DOCUMENT, "huge"),
        FakeFile::native("small", "small", "root", MIME_TYPE_DRIVE_DOCUMENT, "small"),
        FakeFile::blob("x", "x.txt", "root", "x"),
    ];
    let behavior = Behavior {
        export_too_large: HashSet::from([String::from("big")]),
        ..Behavior::default()
    };
    (files, behavior)
}

#[tokio::test]
async fn an_export_over_the_size_limit_fails_and_the_others_finish_with_continue_on_error() {
    let (files, behavior) = with_too_large_export();
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &exporting(destination.path(), "default", true),
        future::pending(),
    )
    .await;

    assert_eq!(
        downloaded.error(),
        "Error: the recursive download is incomplete: 1 file failed"
    );
    let err = downloaded.err.lines();
    assert_eq!(
        err[0],
        format!(
            "Error: unable to export '{}': the exported file would be larger than the 10 MB \
            that the Drive API exports; try another format, or download the file from \
            drive.google.com: Drive answered 403 Forbidden: This file is too large to be \
            exported.",
            shown("Root/big.docx")
        )
    );
    assert_eq!(
        err.last().unwrap(),
        "Summary: 2 transferred (6 B), 0 unchanged, 0 skipped, 1 failed, 0 cancelled"
    );
    assert_eq!(
        entries(&destination.path().join("Root")),
        ["small.docx", "x.txt"],
        "no temporary file is left"
    );
    let big = server
        .exports()
        .iter()
        .filter(|(id, _)| id == "big")
        .count();
    assert_eq!(big, 1, "a too large export is not repeated");
}

#[tokio::test]
async fn an_export_over_the_size_limit_stops_the_download_by_default() {
    let (files, behavior) = with_too_large_export();
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();

    let downloaded = download(
        &server,
        &exporting(destination.path(), "default", false),
        future::pending(),
    )
    .await;

    let error = downloaded.error();
    assert!(
        error.starts_with("Error: the recursive download is incomplete: 1 file failed"),
        "{error}"
    );
    assert!(error.contains("--continue-on-error"), "{error}");
    assert!(downloaded.err.lines()[0].contains("10 MB"));
}

#[tokio::test]
async fn an_existing_export_fails_without_overwrite_and_is_replaced_with_it() {
    let files = vec![FakeFile::native(
        "doc",
        "report",
        "root",
        MIME_TYPE_DRIVE_DOCUMENT,
        "new",
    )];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("Root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("report.docx"), "mine").unwrap();

    let strict = download(
        &server,
        &exporting(destination.path(), "default", false),
        future::pending(),
    )
    .await;
    let err = strict.err.lines();
    assert!(
        err[0].starts_with(&format!(
            "Error: unable to export '{}': file '",
            shown("Root/report.docx")
        )) && err[0].ends_with("' already exists, use --overwrite to overwrite it"),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("report.docx")).unwrap(),
        "mine"
    );
    assert_eq!(server.exports(), Vec::<(String, String)>::new());

    let overwrite = Config {
        existing_file_action: ExistingFileAction::Overwrite,
        ..exporting(destination.path(), "default", false)
    };
    download(&server, &overwrite, future::pending())
        .await
        .result
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("report.docx")).unwrap(),
        "new"
    );
}

#[tokio::test]
async fn an_unchanged_export_is_not_exported_again() {
    let document = |modified: &str| {
        vec![
            FakeFile::native("doc", "report", "root", MIME_TYPE_DRIVE_DOCUMENT, modified)
                .modified(modified),
            // Without modifiedTime nothing tells that it is unchanged.
            FakeFile::native("undated", "undated", "root", MIME_TYPE_DRIVE_DOCUMENT, "x"),
        ]
    };
    let destination = tempfile::tempdir().unwrap();
    let report = local(destination.path(), "Root/report.docx");
    let overwrite = Config {
        existing_file_action: ExistingFileAction::Overwrite,
        ..exporting(destination.path(), "default", false)
    };

    let server = FakeDrive::start(document("2024-05-06T07:08:09.123Z"), Behavior::default()).await;
    download(&server, &overwrite, future::pending())
        .await
        .result
        .unwrap();
    assert_eq!(server.exports().len(), 2);

    // Unchanged on Drive: left alone, with --overwrite and without.
    for config in [&overwrite, &exporting(destination.path(), "default", true)] {
        let again = download(&server, config, future::pending()).await;
        let summary = again.err.lines().last().unwrap().clone();
        if config.existing_file_action == ExistingFileAction::Overwrite {
            again.result.unwrap();
            assert_eq!(
                summary,
                "Summary: 1 transferred (1 B), 1 unchanged, 0 skipped, 0 failed, 0 cancelled"
            );
            // Completed exports, not found ones.
            assert_eq!(
                again.out.lines().last().unwrap(),
                "Exported 1 Google Docs file; 1 unchanged, not exported again"
            );
        } else {
            // The undated export exists and cannot be told unchanged.
            assert_eq!(
                summary,
                "Summary: 0 transferred (0 B), 1 unchanged, 0 skipped, 1 failed, 0 cancelled"
            );
        }
    }
    let exported = server.exports();
    assert_eq!(
        exported.iter().filter(|(id, _)| id == "doc").count(),
        1,
        "{exported:?}"
    );

    // Edited on Drive: exported again with --overwrite, an error without.
    let edited = FakeDrive::start(document("2024-05-06T07:08:10Z"), Behavior::default()).await;
    let strict = download(
        &edited,
        &exporting(destination.path(), "default", true),
        future::pending(),
    )
    .await;
    assert!(strict
        .error()
        .starts_with("Error: the recursive download is incomplete: 2 files failed"));
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        "2024-05-06T07:08:09.123Z"
    );
    download(&edited, &overwrite, future::pending())
        .await
        .result
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        "2024-05-06T07:08:10Z"
    );
}

#[tokio::test]
async fn an_edit_in_the_same_second_as_the_last_export_is_exported_again() {
    let document = |modified: &str| {
        vec![
            FakeFile::native("doc", "report", "root", MIME_TYPE_DRIVE_DOCUMENT, modified)
                .modified(modified),
        ]
    };
    let destination = tempfile::tempdir().unwrap();
    let report = local(destination.path(), "Root/report.docx");
    let overwrite = Config {
        existing_file_action: ExistingFileAction::Overwrite,
        ..exporting(destination.path(), "default", false)
    };

    let first = FakeDrive::start(document("2024-05-06T07:08:09.123Z"), Behavior::default()).await;
    download(&first, &overwrite, future::pending())
        .await
        .result
        .unwrap();
    let modified = std::fs::metadata(&report).unwrap().modified().unwrap();
    assert_eq!(
        chrono::DateTime::<chrono::Utc>::from(modified),
        chrono::DateTime::parse_from_rfc3339("2024-05-06T07:08:09.123Z").unwrap(),
        "the export has the document's time to the millisecond"
    );

    let edited = FakeDrive::start(document("2024-05-06T07:08:09.456Z"), Behavior::default()).await;
    let again = download(&edited, &overwrite, future::pending()).await;

    again.result.unwrap();
    assert_eq!(edited.exports().len(), 1, "exported again");
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        "2024-05-06T07:08:09.456Z"
    );
    assert_eq!(
        again.err.lines().last().unwrap(),
        "Summary: 1 transferred (24 B), 0 unchanged, 0 skipped, 0 failed, 0 cancelled"
    );
}

#[tokio::test]
async fn a_rerun_with_overwrite_through_the_command_skips_the_unchanged_export() {
    let files = vec![
        // The root folder, for the command's `files.get`.
        FakeFile::folder("root", "Root", "none"),
        FakeFile::native("doc", "report", "root", MIME_TYPE_DRIVE_DOCUMENT, "doc!")
            .modified("2024-05-06T07:08:09.123Z"),
        FakeFile::blob("x", "x.txt", "root", "x"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let hub = server.hub();
    let destination = tempfile::tempdir().unwrap();
    let run = |existing_file_action| Config {
        existing_file_action,
        ..exporting(destination.path(), "default", false)
    };

    super::super::download_with_hub(&hub, run(ExistingFileAction::Abort))
        .await
        .unwrap();
    assert_eq!(server.exports().len(), 1);

    // Into an existing destination, only --overwrite gets past the existing top folder.
    let error = super::super::download_with_hub(&hub, run(ExistingFileAction::Abort))
        .await
        .unwrap_err();
    assert!(
        crate::error_message(&error).ends_with("already exists, use --overwrite to overwrite it"),
        "{error}"
    );

    super::super::download_with_hub(&hub, run(ExistingFileAction::Overwrite))
        .await
        .unwrap();
    assert_eq!(
        server.exports().len(),
        1,
        "the unchanged export is not requested"
    );
    assert_eq!(
        server.media(),
        ["x"],
        "the identical file is not downloaded either"
    );
    assert_eq!(
        std::fs::read_to_string(local(destination.path(), "Root/report.docx")).unwrap(),
        "doc!"
    );
}

/// The bytes reported for each file and export of a recursive download, by label, and whether
/// the file was in place already when they were.
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
async fn downloaded_and_exported_bytes_are_reported_as_they_are_written() {
    let big = "0123456789".repeat(30_000);
    let files = vec![
        FakeFile::blob("big", "big.bin", "root", &big),
        FakeFile::blob("small", "small.txt", "root", "small"),
        FakeFile::blob("same", "same.txt", "root", "same"),
        FakeFile::native("doc", "report", "root", MIME_TYPE_DRIVE_DOCUMENT, "doc!"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let destination = tempfile::tempdir().unwrap();
    let root = destination.path().join("Root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("same.txt"), "same").unwrap();
    let config = Config {
        existing_file_action: ExistingFileAction::Overwrite,
        ..exporting(destination.path(), "default", false)
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
    // Exports are labeled with the platform's separator.
    let mut expected = vec![
        (String::from("Root/big.bin"), big.len() as u64),
        (shown("Root/report.docx"), 4),
        (String::from("Root/small.txt"), 5),
    ];
    expected.sort();
    assert_eq!(totals, expected, "an unchanged file transfers nothing");
    for (label, reports) in reports.iter() {
        assert!(
            reports.iter().all(|(_, renamed)| !renamed),
            "{label}: reported while it was written: {reports:?}"
        );
    }
}

/// Where the plain line of a display and the lines of the run go to one stream: the line was
/// drawn and erased before the summary, and nothing is drawn after it.
fn assert_progress_gone_before_the_summary(written: &str) {
    let drawn = written.find(" files, ").expect("the progress is drawn");
    let erased = written
        .rfind(&format!("\r{}\r", " ".repeat(80)))
        .expect("the progress is erased");
    let summary = written.find("Summary: ").expect("a summary");
    assert!(drawn < erased && erased < summary, "{written:?}");
    assert!(!written[summary..].contains('\r'), "{written:?}");
}

#[tokio::test]
async fn the_progress_is_gone_before_the_summary() {
    let files = vec![
        FakeFile::blob("a", "a.txt", "root", "a"),
        FakeFile::blob("b", "b.txt", "root", "b"),
    ];
    // Long enough for the display to draw.
    let behavior = Behavior {
        media_delay: Duration::from_millis(500),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let destination = tempfile::tempdir().unwrap();
    let written = Captured::default();
    let printer = Printer::to(written.clone(), written.clone());

    download_directory(
        &server.hub(),
        root_folder(),
        &config(destination.path(), 2, false),
        &printer,
        ProgressDisplay::plain_on(written.clone()),
        future::pending(),
    )
    .await
    .unwrap();

    assert_progress_gone_before_the_summary(&written.text());
}
