//! `files upload --recursive` against the fake Drive, which answers creates concurrently and
//! refuses a create into a folder that does not exist yet.

use std::{
    collections::HashSet,
    future,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use super::upload_directory;
use crate::{
    common::{
        delegate::UploadDelegateConfig, drive_file::MIME_TYPE_DRIVE_FOLDER, retry::RetryPolicy,
    },
    files::upload::{CommonConfig, Error, UploadDirectoryConfig},
    transfer::{
        fake_drive::{Behavior, Created, FakeDrive, FakeFile},
        scheduler::{
            capture::{output, Captured},
            Options,
        },
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
            mime_type: None,
            parents: None,
            upload_directories: true,
            print_only_id: false,
            transfer: Options::new(NonZeroUsize::new(jobs).unwrap(), continue_on_error),
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
    let hub = server.hub();
    let delegate_config = UploadDelegateConfig {
        retry_policy: RetryPolicy::for_tests(),
        ..UploadDelegateConfig::default()
    };
    let run = upload_directory(
        &hub,
        config,
        &delegate_config,
        &output.printer,
        future::pending(),
    );
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the upload should end");
    Uploaded {
        result,
        out: output.out,
        err: output.err,
    }
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

/// `path` with the platform's separator, as gdrive prints it.
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
        upload_delay: Duration::from_millis(30),
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
    assert!(
        err[0].starts_with(&format!(
            "Error: unable to upload '{}': unable to upload file",
            shown("photos/b.txt")
        )),
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

#[tokio::test]
async fn a_folder_that_cannot_be_created_cancels_everything_in_it() {
    let (_dir, root) = local_tree();
    let behavior = Behavior {
        upload_rejected: HashSet::from([String::from("2024")]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(Vec::new(), behavior).await;

    let uploaded = upload(&server, config(&root, 2, true)).await;

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
        err[0].starts_with(&format!(
            "Error: unable to upload '{}': unable to create directory",
            shown("photos/2024")
        )),
        "{err:?}"
    );
    assert_eq!(
        err.last().unwrap(),
        "Summary: 3 transferred (7 B), 0 unchanged, 0 skipped, 0 failed, 3 cancelled; \
        1 folder failed"
    );
}
