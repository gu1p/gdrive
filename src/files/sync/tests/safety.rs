//! The safety properties of `files sync`, against the data-loss bugs of earlier sync attempts
//! (docs/triage/report-fork-toutizes.md, sections 3 and 5): temporary-name collisions, path
//! traversal, permanent deletion, deleting what was filtered out of a listing or could not be
//! listed, deleting after a failure, and what a dry run and Ctrl-C may touch.

use std::{num::NonZeroUsize, time::Duration};

use super::{
    config, deleting, drive, dry, local, paths, read, run_trash, snapshot, sync, sync_until, write,
    Config, DeleteMode, Direction,
};
use crate::{
    common::local_name,
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile},
        progress,
        scheduler::Options,
    },
};

/// The paths below `dir` that are not in its local trash.
fn outside_trash(dir: &std::path::Path) -> Vec<String> {
    paths(dir)
        .into_iter()
        .filter(|path| path != ".gdrive-trash" && !path.starts_with(".gdrive-trash/"))
        .collect()
}

fn temp_files(dir: &std::path::Path) -> Vec<String> {
    paths(dir)
        .into_iter()
        .filter(|path| {
            let name = path.rsplit('/').next().unwrap_or_default();
            local_name::is_reserved_temp_name(name)
        })
        .collect()
}

#[tokio::test]
async fn concurrent_downloads_with_similar_names_do_not_share_temporary_files() {
    // `report.pdf` and `report.docx` shared `report.incomplete` in an earlier sync, and
    // `archive.tar.gz` became `archive.tar.incomplete`.
    let files = drive(vec![
        FakeFile::blob("pdf", "report.pdf", "root", "pdf bytes"),
        FakeFile::blob("docx", "report.docx", "root", "docx bytes"),
        FakeFile::blob("tgz", "archive.tar.gz", "root", "tgz bytes"),
        FakeFile::blob("tar", "archive.tar", "root", "tar bytes"),
    ]);
    let behavior = Behavior {
        // All four are in flight at once.
        media_rendezvous: 4,
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    // Somebody else's temporary file, with the name the first download would take.
    let stale = local_name::temp_file_name("pdf", 0);
    write(dir.path(), &[(stale.as_str(), "not ours")]);
    let config = Config {
        transfer: Options::new(NonZeroUsize::new(4).unwrap(), false),
        ..deleting(config(Direction::Down, dir.path()))
    };

    let synced = sync(&server, &config).await;

    synced.ok();
    assert_eq!(server.media_max_in_flight(), 4);
    assert_eq!(read(dir.path(), "report.pdf"), "pdf bytes");
    assert_eq!(read(dir.path(), "report.docx"), "docx bytes");
    assert_eq!(read(dir.path(), "archive.tar.gz"), "tgz bytes");
    assert_eq!(read(dir.path(), "archive.tar"), "tar bytes");
    // The stale file is neither reused, nor removed, nor an extra.
    assert_eq!(read(dir.path(), &stale), "not ours");
    assert_eq!(temp_files(dir.path()), std::slice::from_ref(&stale));
    assert!(synced.lines().contains(&format!("skip:temp-file {stale}")));
    assert!(!dir.path().join(".gdrive-trash").exists());
}

#[tokio::test]
async fn a_file_named_like_a_temporary_file_is_neither_uploaded_nor_trashed() {
    // A user's file that `files upload -r` sent once; a download maps the Drive name to
    // `.gdrive-x.part_`, so the local file is never its counterpart.
    let files = drive(vec![FakeFile::blob(
        "twin",
        ".gdrive-x.part",
        "root",
        "data",
    )]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[(".gdrive-x.part", "data")]);

    let synced = sync(&server, &deleting(config(Direction::Up, dir.path()))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&[
            "skip:temp-file .gdrive-x.part",
            "skip:temp-file .gdrive-x.part_"
        ])
    );
    assert_eq!(server.patches(), []);
    assert_eq!(server.created().len(), 0);
}

#[tokio::test]
async fn drive_names_cannot_escape_the_local_directory() {
    let files = drive(vec![
        FakeFile::folder("dotdot", "..", "root"),
        FakeFile::blob("in-dotdot", "escaped.txt", "dotdot", "escaped"),
        FakeFile::folder("slash", "/x", "root"),
        FakeFile::blob("in-slash", "inner.txt", "slash", "inner"),
        FakeFile::blob("traversal", "../evil.txt", "root", "evil"),
        FakeFile::blob("absolute", "/etc/passwd", "root", "passwd"),
        FakeFile::blob("dot", ".", "root", "dot"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("dest");
    write(parent.path(), &[("outside.txt", "keep")]);

    let synced = sync(&server, &deleting(config(Direction::Down, &dir))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&[
            "create .._evil.txt",
            "create _etc_passwd",
            "create _x/",
            "create _x/inner.txt",
            "skip:unusable-name .",
            "skip:unusable-name ../",
        ])
    );
    // Nothing was written next to the local directory.
    assert_eq!(
        paths(parent.path())
            .into_iter()
            .filter(|path| !path.starts_with("dest"))
            .collect::<Vec<_>>(),
        ["outside.txt"]
    );
    assert_eq!(read(parent.path(), "outside.txt"), "keep");
    assert_eq!(read(&dir, "_x/inner.txt"), "inner");
    assert_eq!(read(&dir, ".._evil.txt"), "evil");
    assert_eq!(
        server.media().len(),
        3,
        "the file in `..` is never downloaded"
    );
}

#[tokio::test]
async fn local_files_named_like_skipped_documents_and_shortcuts_are_never_deleted() {
    let files = drive(vec![
        FakeFile::document("doc", "Report", "root"),
        FakeFile::shortcut("link", "link", "root", "target", "application/pdf"),
        FakeFile::native(
            "form",
            "Survey",
            "root",
            "application/vnd.google-apps.form",
            "",
        ),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            // The document itself, and exports of it from an earlier `--export` run.
            ("Report", "local"),
            ("Report.docx", "docx"),
            ("Report.pdf", "pdf"),
            ("link", "the shortcut's target, downloaded once"),
            ("Survey", "local survey"),
            ("other.txt", "extra"),
        ],
    );

    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&[
            "extra->trash other.txt",
            "skip:document Report",
            "skip:document Report.docx",
            "skip:document Report.pdf",
            "skip:not-exportable Survey",
            "skip:shortcut link",
        ])
    );
    assert_eq!(
        outside_trash(dir.path()),
        ["Report", "Report.docx", "Report.pdf", "Survey", "link"]
    );
    assert_eq!(paths(&run_trash(dir.path())), ["other.txt"]);
}

#[tokio::test]
async fn an_ambiguous_folder_keeps_everything_in_it() {
    // Two Drive folders `photos`: the local `photos` is neither's, nothing in it is an extra.
    let files = drive(vec![
        FakeFile::folder("p1", "photos", "root"),
        FakeFile::blob("p1-a", "a.jpg", "p1", "a"),
        FakeFile::folder("p2", "photos", "root"),
        FakeFile::blob("p2-b", "b.jpg", "p2", "b"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("photos/old.jpg", "local only")]);

    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;

    synced.ok();
    // The two Drive folders, under the names a download gives them, and the local one.
    let out = synced.lines();
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(
        out.iter()
            .all(|line| line.starts_with("skip:ambiguous photos")),
        "{out:?}"
    );
    assert_eq!(read(dir.path(), "photos/old.jpg"), "local only");
    assert!(!dir.path().join(".gdrive-trash").exists());
    assert_eq!(server.media(), Vec::<String>::new());
}

/// Two local directories whose names differ only in case: the Drive folder is neither's.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drive_folder_matching_two_local_names_keeps_everything_in_it() {
    let files = drive(vec![
        FakeFile::folder("data", "data", "root"),
        FakeFile::blob("drive-only", "drive-only.txt", "data", "d"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("Data/x.txt", "x"), ("data/y.txt", "y")]);

    let synced = sync(&server, &deleting(config(Direction::Up, dir.path()))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&[
            "skip:ambiguous Data/",
            "skip:ambiguous data/",
            "skip:ambiguous data/",
        ])
    );
    assert_eq!(server.patches(), []);
    assert_eq!(server.created().len(), 0);
}

#[tokio::test]
async fn an_incomplete_listing_suppresses_deletion() {
    let files = drive(vec![
        FakeFile::blob("a", "a.txt", "root", "a"),
        FakeFile::folder("sub", "sub", "root"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[("a.txt", "a"), ("extra.txt", "x"), ("sub/extra.txt", "y")],
    );

    // Drive says it did not search everything, or a folder cannot be listed at all.
    let incomplete = Behavior {
        incomplete: [String::from("root")].into(),
        ..Behavior::default()
    };
    let unlisted = Behavior {
        unreadable: [String::from("sub")].into(),
        ..Behavior::default()
    };
    for behavior in [incomplete, unlisted] {
        for direction in [Direction::Down, Direction::Up] {
            let server = FakeDrive::start(files.clone(), behavior.clone()).await;
            let before = snapshot(dir.path());
            let synced = sync(&server, &deleting(config(direction, dir.path()))).await;

            let error = synced.error();
            assert!(error.contains("the inventory is incomplete"), "{error}");
            assert!(error.contains("nothing was deleted"), "{error}");
            assert!(
                synced.err.text().contains("Deletion skipped"),
                "{}",
                synced.err.text()
            );
            assert!(
                !synced.out.text().contains("extra->"),
                "{direction:?}: {}",
                synced.out.text()
            );
            assert_eq!(snapshot(dir.path()), before, "{direction:?}");
            assert_eq!(server.patches(), [], "{direction:?}");
            assert_eq!(server.deletes(), Vec::<String>::new());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_local_entry_suppresses_deletion() {
    use std::os::unix::fs::PermissionsExt;

    let files = drive(vec![
        FakeFile::blob("a", "a.txt", "root", "a"),
        FakeFile::blob("old", "old.txt", "root", "old"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("a.txt", "a"), ("locked/secret.txt", "s")]);
    let locked = local(dir.path(), "locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        // Permissions do not apply to root.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let synced = sync(&server, &deleting(config(Direction::Up, dir.path()))).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        synced.error().contains("the inventory is incomplete"),
        "{}",
        synced.error()
    );
    assert!(synced
        .lines()
        .contains(&String::from("skip:unreadable locked/")));
    assert!(synced.lines().contains(&String::from("extra old.txt")));
    assert_eq!(server.patches(), []);
}

#[tokio::test]
async fn a_failed_transfer_suppresses_deletion() {
    // down: a download fails.
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
    write(dir.path(), &[("extra.txt", "keep me")]);
    let continuing = |config: Config| Config {
        transfer: Options::new(NonZeroUsize::new(2).unwrap(), true),
        ..config
    };

    let synced = sync(
        &server,
        &continuing(deleting(config(Direction::Down, dir.path()))),
    )
    .await;

    let error = synced.error();
    assert!(
        error.contains("1 item failed") && error.contains("nothing was deleted"),
        "{error}"
    );
    assert!(synced
        .err
        .text()
        .contains("Deletion skipped: not everything was transferred"));
    assert!(synced.lines().contains(&String::from("extra extra.txt")));
    assert_eq!(read(dir.path(), "extra.txt"), "keep me");
    assert_eq!(read(dir.path(), "a.txt"), "a");
    assert!(!dir.path().join(".gdrive-trash").exists());

    // up: an update fails.
    let files = drive(vec![
        FakeFile::blob("changed", "changed.txt", "root", "old"),
        FakeFile::blob("old", "old.txt", "root", "old"),
    ]);
    let behavior = Behavior {
        update_rejected: [String::from("changed")].into(),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("changed.txt", "new content")]);

    let synced = sync(
        &server,
        &continuing(deleting(config(Direction::Up, dir.path()))),
    )
    .await;

    assert!(
        synced.error().contains("1 item failed"),
        "{}",
        synced.error()
    );
    assert!(synced.lines().contains(&String::from("extra old.txt")));
    assert_eq!(server.patches(), [], "nothing goes to the trash");
}

#[tokio::test]
async fn a_dry_run_only_lists_and_writes_nothing() {
    let files = drive(vec![
        FakeFile::blob("same", "same.txt", "root", "same"),
        FakeFile::blob("changed", "changed.txt", "root", "drive"),
        FakeFile::blob("new", "new.txt", "root", "new"),
        FakeFile::folder("folder", "folder", "root"),
        FakeFile::blob("inner", "inner.txt", "folder", "inner"),
        FakeFile::document("doc", "Doc", "root"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("same.txt", "same"),
            ("changed.txt", "local"),
            ("local-only.txt", "l"),
            ("dir/x.txt", "x"),
        ],
    );

    for direction in [Direction::Down, Direction::Up] {
        for delete in [None, Some(DeleteMode::Trash), Some(DeleteMode::Permanently)] {
            let server = FakeDrive::start(files.clone(), Behavior::default()).await;
            let before = snapshot(dir.path());
            let config = Config {
                delete,
                ..dry(config(direction, dir.path()))
            };
            let synced = sync(&server, &config).await;

            synced.ok();
            assert_eq!(snapshot(dir.path()), before, "{direction:?} {delete:?}");
            assert_eq!(server.gets(), ["root"], "only the folder's metadata");
            assert!(server.request_count() > 0);
            assert_eq!(server.media(), Vec::<String>::new());
            assert_eq!(server.exports(), Vec::<(String, String)>::new());
            assert_eq!(server.created().len(), 0);
            assert_eq!(server.updates(), []);
            assert_eq!(server.patches(), []);
            assert_eq!(server.deletes(), Vec::<String>::new());
            assert_eq!(server.generated_ids(), 0);
            assert_eq!(server.chunks(), []);
            assert!(synced.err.text().contains("Dry run, nothing was changed"));
        }
    }

    // A local directory that does not exist is not created.
    let server = FakeDrive::start(files, Behavior::default()).await;
    let missing = dir.path().join("missing");
    let synced = sync(&server, &dry(config(Direction::Down, &missing))).await;
    synced.ok();
    assert!(synced
        .lines()
        .contains(&String::from("create folder/inner.txt")));
    assert!(!missing.exists());
}

#[tokio::test]
async fn a_dry_run_prints_the_lines_of_the_real_run() {
    let files = drive(vec![
        FakeFile::blob("same", "same.txt", "root", "same"),
        FakeFile::blob("changed", "changed.txt", "root", "drive"),
        FakeFile::blob("new", "new.txt", "root", "new"),
        FakeFile::folder("folder", "folder", "root"),
        FakeFile::blob("inner", "inner.txt", "folder", "inner"),
        FakeFile::document("doc", "Doc", "root"),
        FakeFile::folder("gone", "gone", "root"),
        FakeFile::blob("gone-file", "g.txt", "gone", "g"),
    ]);
    let local_files = [
        ("same.txt", "same"),
        ("changed.txt", "local"),
        ("local-only.txt", "l"),
        ("dir/x.txt", "x"),
        ("dir/y/z.txt", "z"),
    ];

    for direction in [Direction::Down, Direction::Up] {
        for delete in [None, Some(DeleteMode::Trash)] {
            let dry_dir = tempfile::tempdir().unwrap();
            let real_dir = tempfile::tempdir().unwrap();
            write(dry_dir.path(), &local_files);
            write(real_dir.path(), &local_files);

            let server = FakeDrive::start(files.clone(), Behavior::default()).await;
            let config_of = |dir| Config {
                delete,
                ..config(direction, dir)
            };
            let planned = sync(&server, &dry(config_of(dry_dir.path()))).await;
            let server = FakeDrive::start(files.clone(), Behavior::default()).await;
            let done = sync(&server, &config_of(real_dir.path())).await;

            planned.ok();
            done.ok();
            assert_eq!(planned.lines(), done.lines(), "{direction:?} {delete:?}");
            assert!(planned.lines().len() >= 6, "{:?}", planned.lines());
        }
    }
}

#[tokio::test]
async fn delete_uses_the_drive_trash_and_the_local_trash() {
    // up: the Drive trash, never files.delete.
    let files = drive(vec![FakeFile::blob("old", "old.txt", "root", "old")]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let synced = sync(&server, &deleting(config(Direction::Up, dir.path()))).await;
    synced.ok();
    assert_eq!(
        server.patches(),
        [(String::from("old"), serde_json::json!({"trashed": true}))]
    );
    assert_eq!(server.deletes(), Vec::<String>::new());
    assert!(
        !dir.path().join(".gdrive-trash").exists(),
        "up never touches the local side"
    );

    // down: `.gdrive-trash/<time>/` with the relative path, ready to be moved back.
    let server = FakeDrive::start(drive(Vec::new()), Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("a/b/old.txt", "old"), ("a/keep.txt", "k")]);
    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;
    synced.ok();
    assert_eq!(synced.lines(), super::lines(&["extra->trash a/"]));
    let trash = run_trash(dir.path());
    assert_eq!(read(&trash, "a/b/old.txt"), "old");
    assert_eq!(outside_trash(dir.path()), Vec::<String>::new());
    std::fs::rename(trash.join("a"), dir.path().join("a")).unwrap();
    assert_eq!(read(dir.path(), "a/keep.txt"), "k");

    // The local trash is never synced: a second run leaves it alone in both directions.
    for direction in [Direction::Down, Direction::Up] {
        let server = FakeDrive::start(drive(Vec::new()), Behavior::default()).await;
        let synced = sync(&server, &dry(deleting(config(direction, dir.path())))).await;
        synced.ok();
        assert!(
            !synced.out.text().contains(".gdrive-trash"),
            "{}",
            synced.out.text()
        );
    }
}

#[tokio::test]
async fn permanent_deletion_needs_its_own_flag() {
    let files = drive(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("old.txt", "old"), ("gone/x.txt", "x")]);

    // --delete alone keeps a copy.
    let server = FakeDrive::start(files.clone(), Behavior::default()).await;
    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;
    synced.ok();
    assert_eq!(
        paths(&run_trash(dir.path())),
        ["gone", "gone/x.txt", "old.txt"]
    );
    std::fs::remove_dir_all(dir.path().join(".gdrive-trash")).unwrap();

    // --delete-permanently removes.
    write(dir.path(), &[("old.txt", "old"), ("gone/x.txt", "x")]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let permanently = Config {
        delete: Some(DeleteMode::Permanently),
        ..config(Direction::Down, dir.path())
    };
    let synced = sync(&server, &permanently).await;
    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&["extra->delete gone/", "extra->delete old.txt"])
    );
    assert_eq!(paths(dir.path()), Vec::<String>::new());
}

#[test]
fn the_command_line_asks_for_permanent_deletion_explicitly() {
    use clap::Parser;

    let parse = |args: &[&str]| {
        let mut argv = vec!["gdrive", "files", "sync"];
        argv.extend(args);
        crate::Cli::try_parse_from(argv).map(|cli| match cli.command {
            crate::Command::Files {
                command: crate::FileCommand::Sync { command },
            } => crate::sync_config(command),
            _ => panic!("expected files sync"),
        })
    };
    let down = parse(&["down", "id", "dir", "--delete"]).unwrap();
    assert_eq!(down.delete, Some(DeleteMode::Trash));
    let down = parse(&["down", "id", "dir", "--delete", "--delete-permanently"]).unwrap();
    assert_eq!(down.delete, Some(DeleteMode::Permanently));
    assert!(parse(&["down", "id", "dir", "--delete-permanently"]).is_err());
    // Drive extras always go to the Drive trash.
    assert!(parse(&["up", "dir", "id", "--delete", "--delete-permanently"]).is_err());
    assert_eq!(parse(&["up", "dir", "id"]).unwrap().delete, None);
    let up = parse(&["up", "dir", "id", "--dry-run", "--jobs", "8"]).unwrap();
    assert!(up.dry_run);
    assert_eq!(up.transfer.jobs.get(), 8);
    assert_eq!(up.direction, Direction::Up);
    // The progress flags of the other transfer commands, the last one winning.
    for direction in [["up", "dir", "id"], ["down", "id", "dir"]] {
        let mode = |flags: &[&str]| {
            let mut args = direction.to_vec();
            args.extend(flags);
            parse(&args).unwrap().progress
        };
        assert_eq!(mode(&["--progress"]), progress::Mode::Always);
        assert_eq!(mode(&["--no-progress"]), progress::Mode::Never);
        assert_eq!(
            mode(&["--no-progress", "--progress"]),
            progress::Mode::Always
        );
    }
}

#[tokio::test]
async fn ctrl_c_leaves_no_temporary_files_and_deletes_nothing() {
    let files = drive(vec![
        FakeFile::blob("stalled", "stalled.bin", "root", &"x".repeat(64 * 1024)),
        FakeFile::blob("a", "a.txt", "root", "a"),
    ]);
    let behavior = Behavior {
        media_stalled: [String::from("stalled")].into(),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("extra.txt", "keep me")]);
    let (interrupt, interrupted) = tokio::sync::oneshot::channel::<()>();
    let cancel = async move {
        let _ = interrupted.await;
    };

    let config = deleting(config(Direction::Down, dir.path()));
    let run = sync_until(&server, &config, cancel);
    let watch = async {
        // Ctrl-C once the stalled download has written part of its temporary file: it writes
        // only after its cleanup took note of the file.
        let part = dir.path().join(local_name::temp_file_name("stalled", 0));
        while !std::fs::metadata(&part).is_ok_and(|metadata| metadata.len() > 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = interrupt.send(());
    };
    let (synced, ()) = tokio::join!(run, watch);

    let error = synced.error();
    assert!(error.contains("interrupted"), "{error}");
    assert_eq!(temp_files(dir.path()), Vec::<String>::new());
    assert_eq!(read(dir.path(), "extra.txt"), "keep me");
    assert!(!dir.path().join(".gdrive-trash").exists());
    assert!(!dir.path().join("stalled.bin").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn down_never_writes_through_or_deletes_symlinks() {
    use std::os::unix::fs::symlink;

    let files = drive(vec![
        FakeFile::folder("linked", "linked", "root"),
        FakeFile::blob("inside", "inside.txt", "linked", "from drive"),
        FakeFile::blob("file-link", "file-link.txt", "root", "from drive"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("dest");
    write(
        parent.path(),
        &[("outside/inside.txt", "outside"), ("target.txt", "target")],
    );
    std::fs::create_dir(&dir).unwrap();
    symlink(parent.path().join("outside"), dir.join("linked")).unwrap();
    symlink(parent.path().join("target.txt"), dir.join("file-link.txt")).unwrap();
    symlink(parent.path().join("outside"), dir.join("extra-link")).unwrap();

    let synced = sync(&server, &deleting(config(Direction::Down, &dir))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&[
            "skip:symlink extra-link/",
            "skip:symlink file-link.txt",
            "skip:symlink linked/",
        ])
    );
    assert_eq!(read(parent.path(), "outside/inside.txt"), "outside");
    assert_eq!(read(parent.path(), "target.txt"), "target");
    assert!(std::fs::symlink_metadata(dir.join("extra-link"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(server.media(), Vec::<String>::new());
}

/// Starts `config`'s sync against `server`, whose content replies wait for `gate`; once the
/// request for `id` arrived (in `requests`), puts `appeared` at `path` and lets it go on.
async fn sync_while_appearing(
    server: &FakeDrive,
    config: &Config,
    gate: &std::sync::Arc<tokio::sync::Semaphore>,
    arrived: impl Fn() -> bool,
    path: &std::path::Path,
) -> super::Synced {
    let run = sync(server, config);
    let meanwhile = async {
        while !arrived() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::write(path, "someone else's").unwrap();
        gate.add_permits(100);
    };
    let (synced, ()) = tokio::join!(run, meanwhile);
    synced
}

#[tokio::test]
async fn a_new_file_never_replaces_one_that_appeared_during_its_download() {
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let behavior = Behavior {
        content_gate: Some(std::sync::Arc::clone(&gate)),
        ..Behavior::default()
    };
    let files = drive(vec![FakeFile::blob("new", "new.txt", "root", "from drive")]);
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.txt");

    let synced = sync_while_appearing(
        &server,
        &config(Direction::Down, dir.path()),
        &gate,
        || server.media().contains(&String::from("new")),
        &path,
    )
    .await;

    assert!(
        synced.error().contains("1 item failed"),
        "{}",
        synced.error()
    );
    assert!(
        synced
            .err
            .text()
            .contains("appeared while the file was downloaded"),
        "{}",
        synced.err.text()
    );
    assert_eq!(read(dir.path(), "new.txt"), "someone else's");
    assert_eq!(temp_files(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn a_new_export_never_replaces_a_file_that_appeared_during_it() {
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let behavior = Behavior {
        content_gate: Some(std::sync::Arc::clone(&gate)),
        ..Behavior::default()
    };
    let files = drive(vec![FakeFile::native(
        "doc",
        "Report",
        "root",
        "application/vnd.google-apps.document",
        "exported",
    )]);
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Report.docx");
    let config = Config {
        export: Some(crate::files::export::ExportFormats::DEFAULT),
        ..config(Direction::Down, dir.path())
    };

    let synced = sync_while_appearing(
        &server,
        &config,
        &gate,
        || !server.exports().is_empty(),
        &path,
    )
    .await;

    assert!(
        synced.error().contains("1 item failed"),
        "{}",
        synced.error()
    );
    assert_eq!(read(dir.path(), "Report.docx"), "someone else's");
    assert_eq!(temp_files(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn a_changed_file_still_replaces_the_local_one() {
    // Only new files refuse to replace: an update is meant to.
    let files = drive(vec![FakeFile::blob("a", "a.txt", "root", "from drive")]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("a.txt", "local")]);

    let synced = sync(&server, &config(Direction::Down, dir.path())).await;

    synced.ok();
    assert_eq!(synced.lines(), super::lines(&["update a.txt"]));
    assert_eq!(read(dir.path(), "a.txt"), "from drive");
}

/// Names that differ only in case, inside a directory that is only local.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_extra_local_directory_with_ambiguous_names_is_not_deleted_whole() {
    let server = FakeDrive::start(drive(Vec::new()), Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("gone/A.txt", "upper"),
            ("gone/a.txt", "lower"),
            ("gone/other.txt", "o"),
            ("deep/sub/X", "upper"),
            ("deep/sub/x", "lower"),
            ("deep/keep.txt", "k"),
        ],
    );

    let synced = sync(&server, &deleting(config(Direction::Down, dir.path()))).await;

    synced.ok();
    assert_eq!(
        synced.lines(),
        super::lines(&[
            "extra->trash deep/keep.txt",
            "extra->trash gone/other.txt",
            "skip:ambiguous deep/sub/X",
            "skip:ambiguous deep/sub/x",
            "skip:ambiguous gone/A.txt",
            "skip:ambiguous gone/a.txt",
            "skip:protected deep/",
            "skip:protected deep/sub/",
            "skip:protected gone/",
        ])
    );
    assert_eq!(
        outside_trash(dir.path()),
        [
            "deep",
            "deep/sub",
            "deep/sub/X",
            "deep/sub/x",
            "gone",
            "gone/A.txt",
            "gone/a.txt"
        ]
    );
    assert_eq!(
        paths(&run_trash(dir.path())),
        ["deep", "deep/keep.txt", "gone", "gone/other.txt"]
    );
}

#[tokio::test]
async fn an_extra_drive_folder_with_duplicate_names_is_not_trashed_whole() {
    let files = drive(vec![
        FakeFile::folder("old", "old", "root"),
        FakeFile::blob("dup1", "dup.txt", "old", "one"),
        FakeFile::blob("dup2", "dup.txt", "old", "two"),
        FakeFile::blob("other", "other.txt", "old", "o"),
        FakeFile::folder("inner", "inner", "old"),
        FakeFile::blob("case1", "Case.txt", "inner", "1"),
        FakeFile::blob("case2", "case.txt", "inner", "2"),
        FakeFile::blob("keep", "keep.txt", "inner", "k"),
    ]);
    let server = FakeDrive::start(files, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let synced = sync(&server, &deleting(config(Direction::Up, dir.path()))).await;

    synced.ok();
    let out = synced.lines();
    for line in [
        "skip:protected old/",
        "skip:protected old/inner/",
        "skip:ambiguous old/dup.txt",
        "skip:ambiguous old/inner/Case.txt",
        "skip:ambiguous old/inner/case.txt",
        "extra->trash old/other.txt",
        "extra->trash old/inner/keep.txt",
    ] {
        assert!(out.contains(&String::from(line)), "{line}: {out:?}");
    }
    assert_eq!(out.len(), 8, "{out:?}");
    let mut trashed = server
        .patches()
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    trashed.sort();
    assert_eq!(trashed, ["keep", "other"]);
}

#[tokio::test]
async fn the_suffixed_exports_of_duplicate_documents_are_never_deleted() {
    // Two documents `Report`, and a file `Notes.docx` next to the document `Notes`: an earlier
    // `--export` gave one of each pair a suffixed name. Without --export they are skipped, and
    // so is every file a download could have made of them.
    let files = drive(vec![
        FakeFile::document("doc1", "Report", "root"),
        FakeFile::document("doc2", "Report", "root"),
        FakeFile::document("notes", "Notes", "root"),
        FakeFile::blob("blob", "Notes.docx", "root", "a real docx"),
    ]);
    let exported = |entries: &[(&str, &str)]| {
        let entries = entries
            .iter()
            .map(|&(id, name)| local_name::NameEntry { id, name })
            .collect::<Vec<_>>();
        local_name::unique_local_names(&entries).unwrap().names
    };
    let reports = exported(&[("doc1", "Report.docx"), ("doc2", "Report.docx")]);
    let reports_pdf = exported(&[("doc1", "Report.pdf"), ("doc2", "Report.pdf")]);
    let notes = exported(&[("notes", "Notes.docx"), ("blob", "Notes.docx")]);
    let mut local_files = reports.clone();
    local_files.extend(reports_pdf.clone());
    local_files.extend(notes.clone());
    local_files.sort();
    local_files.dedup();
    assert!(local_files.len() >= 6, "{local_files:?}");
    let dir = tempfile::tempdir().unwrap();
    let contents = local_files
        .iter()
        .map(|name| (name.as_str(), "an export"))
        .collect::<Vec<_>>();
    write(dir.path(), &contents);
    // The blob's own file is the plain name, with its content.
    write(dir.path(), &[("Notes.docx", "a real docx")]);
    write(dir.path(), &[("extra.txt", "x")]);

    let synced = sync(
        &server_of(files).await,
        &deleting(config(Direction::Down, dir.path())),
    )
    .await;

    synced.ok();
    let out = synced.lines();
    assert_eq!(
        out.iter()
            .filter(|line| line.starts_with("extra"))
            .collect::<Vec<_>>(),
        ["extra->trash extra.txt"],
        "{out:?}"
    );
    assert_eq!(outside_trash(dir.path()), local_files);
}

async fn server_of(files: Vec<FakeFile>) -> FakeDrive {
    FakeDrive::start(files, Behavior::default()).await
}

#[tokio::test]
async fn a_file_edited_after_its_md5_was_compared_keeps_its_time() {
    // One transfer at a time: `same.txt`, unchanged by md5, gets Drive's time only after the
    // download of `a.bin`, which the test holds while it edits `same.txt`.
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let behavior = Behavior {
        content_gate: Some(std::sync::Arc::clone(&gate)),
        ..Behavior::default()
    };
    let files = drive(vec![
        FakeFile::blob("a", "a.bin", "root", "a"),
        FakeFile::blob("same", "same.txt", "root", "same").modified("2024-05-06T07:08:09.123Z"),
    ]);
    let server = FakeDrive::start(files, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("same.txt", "same")]);
    let config = Config {
        transfer: Options::new(NonZeroUsize::new(1).unwrap(), true),
        ..config(Direction::Down, dir.path())
    };
    let path = dir.path().join("same.txt");

    let run = sync(&server, &config);
    let meanwhile = async {
        while !server.media().contains(&String::from("a")) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::write(&path, "edited locally").unwrap();
        gate.add_permits(100);
    };
    let (synced, ()) = tokio::join!(run, meanwhile);

    assert!(
        synced.error().contains("1 item failed"),
        "{}",
        synced.error()
    );
    assert!(
        synced.err.text().contains("changed since the inventory"),
        "{}",
        synced.err.text()
    );
    assert_eq!(read(dir.path(), "same.txt"), "edited locally");
    let stamped = chrono::DateTime::parse_from_rfc3339("2024-05-06T07:08:09.123Z").unwrap();
    let modified = chrono::DateTime::<chrono::Utc>::from(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    assert_ne!(modified, stamped);
}
