//! Phase 3 on its own: what a local extra must still be when it is moved to the trash or
//! removed, against an inventory taken before something else changed it.

use std::path::{Path, PathBuf};

use super::{drive, paths, read, write, DeleteMode};
use crate::{
    files::sync::{
        delete, inventory,
        plan::{self, Extra, ExtraTarget},
    },
    transfer::{
        fake_drive::{Behavior, FakeDrive},
        scheduler::{capture::output, Options},
    },
};

/// The extra `name` at the top of `root` (canonical), as the inventory sees it now.
async fn extra_of(root: &Path, name: &str) -> Extra {
    let output = output();
    let mut problems = Vec::new();
    let nodes = inventory::local(root.to_path_buf(), &output.printer, &mut problems)
        .await
        .unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let node = nodes.iter().find(|node| node.name == name).unwrap();
    Extra {
        label: name.to_owned(),
        target: ExtraTarget::Local {
            path: node.path.clone(),
            rel: PathBuf::from(name),
            snapshot: plan::snapshot(node),
        },
    }
}

/// Phase 3 for `extra` below `root`: how many failed and how many were removed, and stderr.
async fn delete(root: &Path, extra: &Extra, mode: DeleteMode) -> (u64, u64, String) {
    let output = output();
    let server = FakeDrive::start(drive(Vec::new()), Behavior::default()).await;
    let (report, removed) = delete::run(
        &server.hub(),
        vec![extra.clone()],
        mode,
        root,
        Options::default(),
        &output.printer,
        std::future::pending(),
    )
    .await;
    output.printer.flush().await.unwrap();
    (report.folders.failed, removed, output.err.text())
}

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    (dir, root)
}

/// Everything in the local trash of `root`, below the run's directory.
fn trashed(root: &Path) -> Vec<String> {
    let trash = root.join(".gdrive-trash");
    if !trash.exists() {
        return Vec::new();
    }
    paths(&trash)
        .into_iter()
        .filter_map(|path| path.split_once('/').map(|(_, rest)| rest.to_owned()))
        .collect()
}

#[tokio::test]
async fn an_extra_file_that_changed_since_the_inventory_stays() {
    let (_dir, root) = canonical_tempdir();
    write(&root, &[("old.txt", "old")]);
    let extra = extra_of(&root, "old.txt").await;
    std::fs::write(root.join("old.txt"), "edited since").unwrap();

    for mode in [DeleteMode::Trash, DeleteMode::Permanently] {
        let (failed, removed, err) = delete(&root, &extra, mode).await;
        assert_eq!((failed, removed), (1, 0), "{mode:?}");
        assert!(err.contains("changed since the inventory"), "{err}");
        assert_eq!(read(&root, "old.txt"), "edited since");
    }
    assert_eq!(trashed(&root), Vec::<String>::new());
}

#[cfg(unix)]
#[tokio::test]
async fn an_extra_directory_replaced_by_a_symlink_is_not_followed() {
    use std::os::unix::fs::symlink;

    // The inventory sees `gone/x.txt` below the root. Then `gone` moves outside the root, with
    // the very same files (same inodes, sizes, times), and a symlink takes its place.
    let (dir, root) = canonical_tempdir();
    write(&root, &[("gone/x.txt", "x"), ("gone/sub/y.txt", "y")]);
    let extra = extra_of(&root, "gone").await;
    let outside = dir.path().join("outside");
    std::fs::rename(root.join("gone"), &outside).unwrap();
    symlink(&outside, root.join("gone")).unwrap();

    for mode in [DeleteMode::Permanently, DeleteMode::Trash] {
        let (failed, removed, err) = delete(&root, &extra, mode).await;
        assert_eq!((failed, removed), (1, 0), "{mode:?}: {err}");
        assert!(err.contains("changed since the inventory"), "{err}");
        assert_eq!(read(&outside, "x.txt"), "x", "{mode:?}");
        assert_eq!(read(&outside, "sub/y.txt"), "y", "{mode:?}");
        assert!(std::fs::symlink_metadata(root.join("gone"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    assert_eq!(trashed(&root), Vec::<String>::new());
}

#[cfg(unix)]
#[tokio::test]
async fn a_directory_above_an_extra_replaced_by_a_symlink_is_not_followed() {
    use std::os::unix::fs::symlink;

    // The extra is `a/old.txt`, its directory `a` stays: `a` becomes a symlink to outside.
    let (dir, root) = canonical_tempdir();
    write(&root, &[("a/old.txt", "old")]);
    let output = output();
    let mut problems = Vec::new();
    let nodes = inventory::local(root.clone(), &output.printer, &mut problems)
        .await
        .unwrap();
    let inventory::LocalKind::Dir(children) = &nodes[0].kind else {
        panic!("a is a directory");
    };
    let extra = Extra {
        label: String::from("a/old.txt"),
        target: ExtraTarget::Local {
            path: children[0].path.clone(),
            rel: PathBuf::from("a/old.txt"),
            snapshot: plan::snapshot(&children[0]),
        },
    };
    let outside = dir.path().join("outside");
    std::fs::rename(root.join("a"), &outside).unwrap();
    symlink(&outside, root.join("a")).unwrap();

    for mode in [DeleteMode::Permanently, DeleteMode::Trash] {
        let (failed, removed, err) = delete(&root, &extra, mode).await;
        assert_eq!((failed, removed), (1, 0), "{mode:?}: {err}");
        assert_eq!(read(&outside, "old.txt"), "old", "{mode:?}");
    }
    assert_eq!(trashed(&root), Vec::<String>::new());
}

#[tokio::test]
async fn an_extra_directory_whose_entries_changed_stays() {
    // An entry added, one removed, one rewritten: each leaves the directory in place.
    let changes = ["added", "removed", "rewritten"];
    for change in changes {
        for mode in [DeleteMode::Trash, DeleteMode::Permanently] {
            let (_dir, root) = canonical_tempdir();
            write(&root, &[("gone/x.txt", "x"), ("gone/sub/y.txt", "y")]);
            let extra = extra_of(&root, "gone").await;
            let gone = root.join("gone");
            match change {
                "added" => std::fs::write(gone.join("new.txt"), "new").unwrap(),
                "removed" => std::fs::remove_file(gone.join("sub/y.txt")).unwrap(),
                _ => std::fs::write(gone.join("x.txt"), "longer").unwrap(),
            }
            let before = paths(&root);

            let (failed, removed, err) = delete(&root, &extra, mode).await;

            assert_eq!((failed, removed), (1, 0), "{change} {mode:?}: {err}");
            assert!(err.contains("changed since the inventory"), "{err}");
            let after = paths(&root)
                .into_iter()
                .filter(|path| !path.starts_with(".gdrive-trash"))
                .collect::<Vec<_>>();
            assert_eq!(after, before, "{change} {mode:?}");
            assert_eq!(trashed(&root), Vec::<String>::new(), "{change} {mode:?}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_extra_file_replaced_by_another_with_the_same_size_and_time_stays() {
    let (_dir, root) = canonical_tempdir();
    write(&root, &[("gone/x.txt", "x")]);
    let extra = extra_of(&root, "gone").await;
    let path = root.join("gone/x.txt");
    let time = std::fs::metadata(&path).unwrap().modified().unwrap();
    // Another file under the name, with the same size and time. It is created while the first
    // one still exists, so it cannot get its inode.
    std::fs::write(root.join("gone/other"), "z").unwrap();
    std::fs::rename(root.join("gone/other"), &path).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(time)
        .unwrap();

    let (failed, removed, _) = delete(&root, &extra, DeleteMode::Permanently).await;

    assert_eq!((failed, removed), (1, 0));
    assert_eq!(read(&root, "gone/x.txt"), "z");
}

#[tokio::test]
async fn an_unchanged_extra_directory_goes_whole() {
    for mode in [DeleteMode::Trash, DeleteMode::Permanently] {
        let (_dir, root) = canonical_tempdir();
        write(
            &root,
            &[
                ("gone/x.txt", "x"),
                ("gone/sub/y.txt", "y"),
                ("kept.txt", "k"),
            ],
        );
        let extra = extra_of(&root, "gone").await;

        let (failed, removed, err) = delete(&root, &extra, mode).await;

        assert_eq!((failed, removed), (0, 1), "{mode:?}: {err}");
        let left = paths(&root)
            .into_iter()
            .filter(|path| !path.starts_with(".gdrive-trash"))
            .collect::<Vec<_>>();
        assert_eq!(left, ["kept.txt"], "{mode:?}");
        let expected = match mode {
            DeleteMode::Trash => vec!["gone", "gone/sub", "gone/sub/y.txt", "gone/x.txt"],
            DeleteMode::Permanently => Vec::new(),
        };
        assert_eq!(trashed(&root), expected, "{mode:?}");
    }
}
