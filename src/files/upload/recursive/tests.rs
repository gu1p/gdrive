//! `files upload --recursive` against the fake Drive, which answers creates concurrently and
//! refuses a create into a folder that does not exist yet.

use std::{
    collections::HashSet,
    future,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use super::upload_directory;
use crate::{
    common::{drive_file::MIME_TYPE_DRIVE_FOLDER, table::FailingWriter},
    files::upload::{CommonConfig, Error, UploadDirectoryConfig},
    transfer::{
        fake_drive::{Behavior, Created, FakeDrive, FakeFile},
        progress::{Mode, NoProgress, ProgressDisplay, ProgressEvent, ProgressSink},
        scheduler::{
            capture::{output, Captured},
            JobKind, Options, Printer,
        },
        upload::UploadOptions,
    },
};

/// `photos` with 6 files on three levels and an empty folder:
///
/// ```text
/// photos/a.txt b.txt c.txt  photos/2024/d.txt e.txt  photos/2024/jan/f.txt  photos/empty/
/// ```
fn local_tree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    for folder in ["", "2024", "2024/jan", "empty"] {
        std::fs::create_dir_all(root.join(folder)).unwrap();
    }
    for (path, content) in [
        ("a.txt", "aaaa"),
        ("b.txt", "bb"),
        ("c.txt", "c"),
        ("2024/d.txt", "dddd"),
        ("2024/e.txt", "eee"),
        ("2024/jan/f.txt", "ffffff"),
    ] {
        std::fs::write(root.join(path), content).unwrap();
    }
    (dir, root)
}

/// The total size of the files of [`local_tree`].
const TREE_SIZE: usize = 4 + 2 + 1 + 4 + 3 + 6;

fn config(path: &Path, jobs: usize, continue_on_error: bool) -> UploadDirectoryConfig {
    UploadDirectoryConfig {
        file_path: path.to_path_buf(),
        common: CommonConfig {
            name: None,
            mime_type: None,
            parents: None,
            upload_directories: true,
            print_only_id: false,
            transfer: Options::new(NonZeroUsize::new(jobs).unwrap(), continue_on_error),
            progress: Mode::Never,
        },
    }
}

struct Uploaded {
    result: Result<(), Error>,
    out: Captured,
    err: Captured,
}

impl Uploaded {
    fn error(&self) -> String {
        match &self.result {
            Ok(()) => panic!("the upload should fail"),
            Err(error) => crate::error_message(error),
        }
    }
}

async fn upload(server: &FakeDrive, config: UploadDirectoryConfig) -> Uploaded {
    let output = output();
    let result = upload_through(server, config, &output.printer).await;
    Uploaded {
        result,
        out: output.out,
        err: output.err,
    }
}

async fn upload_through(
    server: &FakeDrive,
    config: UploadDirectoryConfig,
    printer: &Printer,
) -> Result<(), Error> {
    upload_with(
        server,
        config,
        &UploadOptions::default(),
        printer,
        future::pending(),
    )
    .await
}

async fn upload_with(
    server: &FakeDrive,
    config: UploadDirectoryConfig,
    options: &UploadOptions,
    printer: &Printer,
    cancel: impl std::future::Future<Output = ()> + Send,
) -> Result<(), Error> {
    upload_reporting(
        server,
        config,
        options,
        printer,
        Arc::new(NoProgress),
        cancel,
    )
    .await
}

/// [`upload_with`], reporting to `progress`.
async fn upload_reporting(
    server: &FakeDrive,
    config: UploadDirectoryConfig,
    options: &UploadOptions,
    printer: &Printer,
    progress: Arc<dyn ProgressSink>,
    cancel: impl std::future::Future<Output = ()> + Send,
) -> Result<(), Error> {
    let hub = server.hub();
    let run = upload_directory(
        &hub,
        config,
        options,
        printer,
        ProgressDisplay::reporting_to(progress),
        cancel,
    );
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the upload should end")
}

fn is_folder(created: &Created) -> bool {
    created.mime_type == MIME_TYPE_DRIVE_FOLDER
}

/// The Drive path of every created item, `/`-joined from the top folder, sorted.
fn created_paths(created: &[Created]) -> Vec<String> {
    let path_of = |item: &Created| {
        let mut names = vec![item.name.clone()];
        let mut parent = item.parents.first();
        while let Some(folder) = parent.and_then(|id| created.iter().find(|c| &c.id == id)) {
            names.push(folder.name.clone());
            parent = folder.parents.first();
        }
        names.reverse();
        names.join("/")
    };
    let mut paths = created.iter().map(path_of).collect::<Vec<_>>();
    paths.sort();
    paths
}

/// `path` with the platform's separator, as gdrive prints local paths on stdout. Warnings and
/// errors show paths from the uploaded folder with `/`.
fn shown(path: &str) -> String {
    path.split('/')
        .fold(PathBuf::new(), |path, name| path.join(name))
        .display()
        .to_string()
}

#[tokio::test]
async fn folders_exist_before_their_contents_and_uploads_are_bounded() {
    let (_dir, root) = local_tree();
    let behavior = Behavior {
        // The first two uploads wait for each other, so both are in flight at once however slow
        // the machine is.
        upload_rendezvous: 2,
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;

    let uploaded = upload(&server, config(&root, 2, false)).await;

    uploaded.result.unwrap();
    let created = server.created();
    assert_eq!(
        created_paths(&created),
        [
            "photos",
            "photos/2024",
            "photos/2024/d.txt",
            "photos/2024/e.txt",
            "photos/2024/jan",
            "photos/2024/jan/f.txt",
            "photos/a.txt",
            "photos/b.txt",
            "photos/c.txt",
            "photos/empty",
        ]
    );
    for item in &created {
        assert!(item.id.starts_with("gen-"), "a pre-generated id: {item:?}");
        for parent in &item.parents {
            let folder = created.iter().find(|folder| &folder.id == parent).unwrap();
            assert!(is_folder(folder));
            assert!(
                folder.answered < item.arrived,
                "{} was created before {}",
                folder.name,
                item.name
            );
        }
    }
    let file = |name: &str| created.iter().find(|item| item.name == name).unwrap();
    assert_eq!(file("f.txt").content, b"ffffff");
    assert_eq!(file("a.txt").content, b"aaaa");
    assert_eq!(server.uploads_max_in_flight(), 2);

    let out = uploaded.out.lines();
    assert_eq!(
        out[0],
        format!("Found 6 files in 4 directories with a total size of {TREE_SIZE} B")
    );
    let photos = file("photos");
    assert_eq!(
        out[1],
        format!("Creating directory 'photos' with id: {}", photos.id)
    );
    let f = file("f.txt");
    assert!(out.contains(&format!(
        "Uploading file '{}' with id: {}",
        shown("photos/2024/jan/f.txt"),
        f.id
    )));
    assert_eq!(
        out.last().unwrap(),
        &format!("Uploaded 6 files in 4 directories with a total size of {TREE_SIZE} B")
    );
    assert_eq!(out.len(), 2 + 4 + 6);
    assert_eq!(
        uploaded.err.lines(),
        [format!(
            "Summary: 6 transferred ({TREE_SIZE} B), 0 unchanged, 0 skipped, 0 failed, 0 cancelled"
        )]
    );
}

#[tokio::test]
async fn print_only_id_prints_one_line_per_item_after_it_exists() {
    let (_dir, root) = local_tree();
    let server = FakeDrive::start(
        vec![FakeFile::folder("dest", "dest", "root")],
        Behavior::default(),
    )
    .await;
    let mut config = config(&root, 3, false);
    config.common.print_only_id = true;
    config.common.parents = Some(vec![String::from("dest")]);

    let uploaded = upload(&server, config).await;

    uploaded.result.unwrap();
    let created = server.created();
    let photos = created.iter().find(|item| item.name == "photos").unwrap();
    assert_eq!(photos.parents, ["dest"]);

    let out = uploaded.out.lines();
    assert_eq!(out.len(), 10, "{out:?}");
    let position = |path: &str| {
        let item = created
            .iter()
            .find(|item| item.name == path.rsplit('/').next().unwrap())
            .unwrap();
        out.iter()
            .position(|line| *line == format!("{}: {}", shown(path), item.id))
            .unwrap_or_else(|| panic!("{path} in {out:?}"))
    };
    assert_eq!(position("photos"), 0);
    assert!(position("photos/2024") < position("photos/2024/jan"));
    assert!(position("photos/2024/jan") < position("photos/2024/jan/f.txt"));
    assert!(position("photos/2024") < position("photos/2024/d.txt"));
    position("photos/empty");
}

#[tokio::test]
async fn a_failing_file_stops_the_upload_by_default() {
    let (_dir, root) = local_tree();
    let behavior = Behavior {
        upload_delay: Duration::from_millis(50),
        upload_rejected: HashSet::from([String::from("b.txt")]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;

    let uploaded = upload(&server, config(&root, 2, false)).await;

    let error = uploaded.error();
    assert!(
        error.starts_with("Error: the recursive upload is incomplete: 1 file failed, "),
        "{error}"
    );
    assert!(
        error.ends_with("; use --continue-on-error to finish the other transfers after a failure"),
        "{error}"
    );
    let err = uploaded.err.lines();
    // The path from the uploaded folder, with `/` on every platform.
    assert!(
        err[0].starts_with("Error: unable to upload 'photos/b.txt': unable to upload file"),
        "{err:?}"
    );
    let files = server
        .created()
        .iter()
        .filter(|item| !is_folder(item))
        .count();
    assert!(files < 5, "the upload stopped: {files} files were uploaded");
    assert!(!uploaded.out.text().contains("Uploaded "));
}

#[tokio::test]
async fn continue_on_error_uploads_the_other_files_and_still_fails() {
    let (_dir, root) = local_tree();
    let behavior = Behavior {
        upload_delay: Duration::from_millis(20),
        upload_rejected: HashSet::from([String::from("b.txt")]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;

    let uploaded = upload(&server, config(&root, 2, true)).await;

    assert_eq!(
        uploaded.error(),
        "Error: the recursive upload is incomplete: 1 file failed"
    );
    let created = created_paths(&server.created());
    assert_eq!(created.len(), 9);
    assert!(!created.contains(&String::from("photos/b.txt")));
    let size = TREE_SIZE - 2;
    assert_eq!(
        uploaded.err.lines().last().unwrap(),
        &format!(
            "Summary: 5 transferred ({size} B), 0 unchanged, 0 skipped, 1 failed, 0 cancelled"
        )
    );
}

/// The totals a run planned, the bytes it confirmed, and what failed folders took along.
#[derive(Debug, Default, PartialEq, Eq)]
struct Totals {
    planned: (u64, u64),
    finished: u64,
    confirmed: u64,
    cancelled: (u64, u64),
}

#[derive(Default)]
struct Recorder(Mutex<Totals>);

impl ProgressSink for Recorder {
    fn event(&self, event: &ProgressEvent<'_>) {
        let mut totals = self.0.lock().unwrap();
        match *event {
            ProgressEvent::Planned { files, bytes } => totals.planned = (files, bytes),
            ProgressEvent::Bytes { bytes, .. } => totals.confirmed += bytes,
            ProgressEvent::Finished {
                kind: JobKind::Media,
                ..
            } => totals.finished += 1,
            ProgressEvent::SubtreeCancelled { files, bytes } => {
                totals.cancelled.0 += files;
                totals.cancelled.1 += bytes;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_folder_that_cannot_be_created_cancels_everything_in_it() {
    let (_dir, root) = local_tree();
    let behavior = Behavior {
        upload_rejected: HashSet::from([String::from("2024")]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;
    let output = output();
    let recorder = Arc::new(Recorder::default());

    let result = upload_reporting(
        &server,
        config(&root, 2, true),
        &UploadOptions::default(),
        &output.printer,
        Arc::clone(&recorder) as Arc<dyn ProgressSink>,
        future::pending(),
    )
    .await;
    let uploaded = Uploaded {
        result,
        out: output.out,
        err: output.err,
    };

    assert_eq!(
        uploaded.error(),
        "Error: the recursive upload is incomplete: 1 folder failed, 4 cancelled"
    );
    assert_eq!(
        created_paths(&server.created()),
        [
            "photos",
            "photos/a.txt",
            "photos/b.txt",
            "photos/c.txt",
            "photos/empty"
        ]
    );
    let err = uploaded.err.lines();
    assert!(
        err[0].starts_with("Error: unable to upload 'photos/2024': unable to create directory"),
        "{err:?}"
    );
    assert_eq!(
        err.last().unwrap(),
        "Summary: 3 transferred (7 B), 0 unchanged, 0 skipped, 0 failed, 3 cancelled; \
        1 folder failed"
    );
    // The progress's totals: the three files of 2024 leave them with their folder, so the
    // run ends at what it planned.
    assert_eq!(
        *recorder.0.lock().unwrap(),
        Totals {
            planned: (6, TREE_SIZE as u64),
            finished: 3,
            confirmed: 7,
            cancelled: (3, 13),
        }
    );
}

#[tokio::test]
async fn ids_that_cannot_be_printed_fail_the_upload() {
    let (_dir, root) = local_tree();
    let server = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let mut config = config(&root, 2, false);
    config.common.print_only_id = true;
    let err = Captured::default();
    let printer = Printer::to(FailingWriter(std::io::ErrorKind::StorageFull), err.clone());

    let result = upload_through(&server, config, &printer).await;

    let error = crate::error_message(&result.unwrap_err());
    assert!(
        error.starts_with(
            "Error: unable to write the output of the recursive upload: unable to write to \
            stdout: "
        ),
        "{error}"
    );
    // The upload stopped once the first id was lost.
    assert!(server.created().len() < 10, "{:?}", server.created());
    assert!(err.text().contains("Summary: "));
}

/// [`local_tree`] with a broken symlink and, where permissions apply, a directory that cannot
/// be read. Returns whether the directory is unreadable.
#[cfg(unix)]
fn with_unreadable_entries(root: &Path) -> bool {
    use std::os::unix::fs::{symlink, PermissionsExt};

    symlink("missing", root.join("2024").join("dangling.jpg")).unwrap();
    let locked = root.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("hidden.txt"), "h").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::read_dir(&locked).is_err()
}

#[cfg(unix)]
fn unlock(root: &Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_local_entry_stops_the_upload_before_anything_is_created() {
    let (_dir, root) = local_tree();
    with_unreadable_entries(&root);
    let server = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let uploaded = upload(&server, config(&root, 2, false)).await;
    unlock(&root);

    let error = uploaded.error();
    assert!(
        error.starts_with("Error: the recursive upload is incomplete: 1 f"),
        "{error}"
    );
    assert!(error.contains("--continue-on-error"), "{error}");
    assert_eq!(server.created().len(), 0, "nothing was created");
    let err = uploaded.err.lines();
    assert!(
        err[0].starts_with("Error: unable to upload '")
            && err[0].contains("': unable to read it from the local disk: "),
        "{err:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn continue_on_error_uploads_everything_that_can_be_read() {
    let (_dir, root) = local_tree();
    let locked = with_unreadable_entries(&root);
    let server = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let uploaded = upload(&server, config(&root, 2, true)).await;
    unlock(&root);

    let created = created_paths(&server.created());
    let readable = if locked { 10 } else { 12 };
    assert_eq!(
        created.len(),
        readable,
        "every readable folder and file: {created:?}"
    );
    let err = uploaded.err.lines();
    assert!(
        err.iter().any(|line| line.starts_with(
            "Error: unable to upload 'photos/2024/dangling.jpg': unable to read it from the local \
            disk: symbolic link '"
        )),
        "{err:?}"
    );
    if locked {
        assert_eq!(
            uploaded.error(),
            "Error: the recursive upload is incomplete: 1 file and 1 folder failed"
        );
        assert!(err.iter().any(|line| line.starts_with(
            "Error: unable to upload 'photos/locked': unable to read it from the local disk: \
            unable to read directory content"
        )));
        assert_eq!(
            err.last().unwrap(),
            &format!(
                "Summary: 6 transferred ({TREE_SIZE} B), 0 unchanged, 0 skipped, 1 failed, 0 \
                cancelled; 1 folder failed"
            )
        );
    } else {
        assert!(uploaded.error().ends_with("1 file failed"));
    }
}

/// `big` with three files of 600 KiB, each with its own content.
fn big_tree() -> (tempfile::TempDir, PathBuf, Vec<(String, Vec<u8>)>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("big");
    std::fs::create_dir(&root).unwrap();
    let files = ["a.bin", "b.bin", "c.bin"]
        .iter()
        .zip(1_u32..)
        .map(|(name, seed)| {
            let content = (0..600 * 1024_u32)
                .map(|i| u8::try_from((i * seed + i / 7) % 251).unwrap())
                .collect::<Vec<_>>();
            std::fs::write(root.join(name), &content).unwrap();
            ((*name).to_owned(), content)
        })
        .collect();
    (dir, root, files)
}

/// Every upload resumable, in chunks of 256 KiB: three per file of [`big_tree`].
fn chunked() -> UploadOptions {
    UploadOptions {
        chunk_size: 256 * 1024,
        multipart_threshold: 0,
        ..UploadOptions::default()
    }
}

#[tokio::test]
async fn concurrent_multi_chunk_uploads_retry_a_failed_chunk() {
    let (_dir, root, files) = big_tree();
    let behavior = Behavior {
        // The first chunks of the first two sessions wait for each other: both sessions are open
        // at once.
        upload_rendezvous: 2,
        chunk_unavailable: std::collections::HashMap::from([(String::from("b.bin"), 262_144)]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;
    let output = output();

    let result = upload_with(
        &server,
        config(&root, 2, false),
        &chunked(),
        &output.printer,
        future::pending(),
    )
    .await;

    result.unwrap();
    let created = server.created();
    for (name, content) in &files {
        let file = created.iter().find(|item| &item.name == name).unwrap();
        assert!(file.content == *content, "{name} arrived whole");
    }
    assert_eq!(server.sessions_max_active(), 2, "two files at a time");

    let chunks = server.chunks();
    let of = |name: &str| {
        chunks
            .iter()
            .filter(|chunk| chunk.name == name && chunk.first.is_some())
            .map(|chunk| (chunk.first.unwrap(), chunk.status))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        of("a.bin"),
        [(0, 308), (262_144, 308), (524_288, 200)],
        "three chunks"
    );
    let b = of("b.bin");
    let failed = b.iter().position(|chunk| *chunk == (262_144, 503)).unwrap();
    assert!(
        b[failed + 1..].contains(&(262_144, 308)),
        "the chunk was sent again: {b:?}"
    );
    assert_eq!(b.last(), Some(&(524_288, 200)));

    let total = files
        .iter()
        .map(|(_, content)| content.len() as u64)
        .sum::<u64>();
    assert_eq!(
        output.err.lines().last().unwrap(),
        &format!(
            "Summary: 3 transferred ({}), 0 unchanged, 0 skipped, 0 failed, 0 cancelled",
            crate::common::human_size::human_size(total)
        )
    );
}

#[tokio::test]
async fn ctrl_c_stops_multi_chunk_uploads_in_the_middle_of_their_sessions() {
    let (_dir, root, _files) = big_tree();
    let behavior = Behavior {
        // Every upload gets its first chunk on Drive, and the fake holds its second without a
        // reply, so no file can be completed before Ctrl-C, however slow the machine is.
        chunks_stalled_from: Some(256 * 1024),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;
    let output = output();
    // Ctrl-C once the fake holds the second chunk of both running uploads: both have their first
    // chunk on Drive and are in the middle of their sessions. The fake says when, so nothing
    // here depends on how long the uploads take.
    let cancel = server.wait_for_stalled_chunks(2);

    let result = upload_with(
        &server,
        config(&root, 2, false),
        &chunked(),
        &output.printer,
        cancel,
    )
    .await;

    let error = crate::error_message(&result.unwrap_err());
    assert_eq!(
        error,
        "Error: the recursive upload is incomplete: interrupted, 3 cancelled"
    );
    let created = created_paths(&server.created());
    assert_eq!(created, ["big"], "no file was completed");
    assert!(output.err.lines().contains(&String::from(
        "Interrupted: cancelling the transfers in progress"
    )));
    // Drive answered the first chunk of the two uploads and holds their second.
    let answered = server.chunks();
    assert_eq!(
        answered
            .iter()
            .map(|chunk| (chunk.first, chunk.status))
            .collect::<Vec<_>>(),
        [(Some(0), 308), (Some(0), 308)],
        "{answered:?}"
    );
    assert_eq!(
        answered
            .iter()
            .map(|chunk| &chunk.name)
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(server.stalled_chunks(), 2);
    // The aborted uploads send nothing more.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.chunks(), answered);
    assert_eq!(server.stalled_chunks(), 2);
}

#[tokio::test]
async fn the_progress_is_gone_before_the_summary() {
    let (_dir, root) = local_tree();
    // Long enough for the display to draw.
    let behavior = Behavior {
        upload_delay: Duration::from_millis(150),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;
    let written = Captured::default();
    let printer = Printer::to(written.clone(), written.clone());

    let hub = server.hub();
    upload_directory(
        &hub,
        config(&root, 2, false),
        &UploadOptions::default(),
        &printer,
        ProgressDisplay::plain_on(written.clone()),
        future::pending(),
    )
    .await
    .unwrap();

    // The line was drawn and erased before the last lines, and nothing is drawn after them.
    let written = written.text();
    let drawn = written.find(" files, ").expect("the progress is drawn");
    let erased = written
        .rfind(&format!("\r{}\r", " ".repeat(80)))
        .expect("the progress is erased");
    let uploaded = written
        .find("Uploaded 6 files")
        .expect("the end of the upload");
    assert!(drawn < erased && erased < uploaded, "{written:?}");
    assert!(!written[uploaded..].contains('\r'), "{written:?}");
}
