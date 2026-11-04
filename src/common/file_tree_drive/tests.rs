use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;

use super::{errors, Action, Child, ChildKind, ListedChild, ListedFolder, LocalTree, Totals};
use crate::{
    common::{drive_file::MIME_TYPE_DRIVE_FOLDER, local_name::InvalidName},
    files,
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile},
        scheduler::{JobKind, Outcome, SkipReason},
        walk::{
            Children, EntryKind, FolderError, FolderKey, FolderRef, Walk, WalkEntry, WalkItem,
            WalkOptions,
        },
    },
};

/// A folder, or a file with content (it has an md5).
fn drive_file(id: &str, name: &str, folder: bool) -> google_drive3::api::File {
    google_drive3::api::File {
        id: Some(id.to_string()),
        name: Some(name.to_string()),
        mime_type: Some(if folder {
            MIME_TYPE_DRIVE_FOLDER.to_string()
        } else {
            String::from("application/pdf")
        }),
        md5_checksum: (!folder).then(|| format!("{:x}", md5::compute(id))),
        size: (!folder).then_some(0),
        ..google_drive3::api::File::default()
    }
}

/// The children as the tree keeps them while their folder is listed; folders are listed under
/// the root's key (only their names matter here).
fn children(files: &[google_drive3::api::File]) -> Vec<Child> {
    files
        .iter()
        .map(|file| {
            Child::from_file(
                file.clone(),
                EntryKind::of(file),
                Children::Listed(FolderKey::ROOT),
            )
            .expect("a folder or a file with content")
        })
        .collect()
}

fn local_names(listed: &ListedFolder) -> Vec<Result<String, &errors::Folder>> {
    listed
        .children
        .iter()
        .map(|child| match child {
            Ok(ListedChild::Folder { local_name, .. }) => Ok(local_name.clone()),
            Ok(ListedChild::File(file)) => Ok(file.local_name.clone()),
            Err(failed) => Err(&failed.error),
        })
        .collect()
}

#[test]
fn children_get_safe_unique_names() {
    let files = [
        drive_file("id-b", "report.pdf", false),
        drive_file("id-a", "report.pdf", false),
        drive_file("id-c", "../escape", true),
        drive_file("id-d", "plain", false),
    ];

    let listed = ListedFolder::finish(children(&files));
    let names = local_names(&listed)
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();

    assert_eq!(names[1], "report.pdf");
    assert!(names[0].starts_with("report~"), "{}", names[0]);
    assert_eq!(&names[0][names[0].len() - 4..], ".pdf");
    assert_eq!(names[2], ".._escape");
    assert_eq!(names[3], "plain");
    assert_eq!(listed.changes.len(), 2, "the suffix and the sanitized name");
}

#[test]
fn an_unusable_name_fails_only_its_child() {
    let files = [
        drive_file("id-a", "..", true),
        drive_file("id-b", "b", false),
    ];

    let listed = ListedFolder::finish(children(&files));
    let names = local_names(&listed);

    assert_eq!(names[0].as_deref().ok(), Some("b"));
    assert!(matches!(
        names[1],
        Err(errors::Folder::UnusableName(name))
            if name.id == "id-a" && name.source == InvalidName::ParentDir
    ));
}

#[test]
fn a_child_without_name_fails_alone() {
    let files = [
        google_drive3::api::File {
            name: None,
            ..drive_file("id-a", "", false)
        },
        drive_file("id-b", "b", false),
    ];

    let listed = ListedFolder::finish(children(&files));
    let names = local_names(&listed);

    assert_eq!(names[0].as_deref().ok(), Some("b"));
    assert!(matches!(
        names[1],
        Err(errors::Folder::File {
            source: errors::File::MissingFileName,
            ..
        })
    ));
}

#[test]
fn only_folders_and_files_with_content_are_kept_compact() {
    let kept = |file: google_drive3::api::File| {
        let kind = EntryKind::of(&file);
        Child::from_file(file, kind, Children::Listed(FolderKey::ROOT))
    };
    let document = google_drive3::api::File {
        mime_type: Some(String::from("application/vnd.google-apps.document")),
        md5_checksum: None,
        ..drive_file("doc", "notes", false)
    };
    let shortcut = google_drive3::api::File {
        mime_type: Some(String::from("application/vnd.google-apps.shortcut")),
        md5_checksum: None,
        ..drive_file("link", "link", false)
    };
    let without_md5 = google_drive3::api::File {
        md5_checksum: None,
        ..drive_file("raw", "raw.bin", false)
    };

    assert!(kept(document).is_none());
    assert!(kept(shortcut).is_none());
    assert!(kept(drive_file("a", "a", true)).is_some_and(|child| child.is_folder()));
    assert!(kept(drive_file("b", "b", false)).is_some_and(|child| !child.is_folder()));
    assert!(
        kept(without_md5)
            .is_some_and(|child| matches!(child.kind, ChildKind::File { md5: None, .. })),
        "a file with content is downloaded without an md5 too, checked by its size"
    );
    // A Drive `File` has every API field; the tree holds a small fraction of it per child.
    assert!(std::mem::size_of::<Child>() * 10 < std::mem::size_of::<google_drive3::api::File>());
}

fn root_folder() -> google_drive3::api::File {
    google_drive3::api::File {
        id: Some(String::from("root")),
        name: Some(String::from("Root")),
        mime_type: Some(MIME_TYPE_DRIVE_FOLDER.to_string()),
        ..google_drive3::api::File::default()
    }
}

/// The actions of a tree, sorted by kind.
#[derive(Debug, Default)]
struct Planned {
    /// In order.
    dirs: Vec<PathBuf>,
    /// Paths as `/`-joined components, sorted.
    downloads: Vec<String>,
    done: Vec<(JobKind, String, Outcome)>,
    warnings: Vec<String>,
    /// The downloads came after the directories they are in.
    ordered: bool,
}

impl Planned {
    fn add(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::CreateDir(path) => self.dirs.push(path),
                Action::Download(file) => {
                    let parent = file.path.parent().map(Path::to_path_buf);
                    self.ordered &= parent.is_some_and(|parent| self.dirs.contains(&parent));
                    self.downloads.push(slashed(&file.path));
                }
                Action::Done {
                    kind,
                    label,
                    outcome,
                } => self.done.push((kind, label, outcome)),
                Action::Warning(warning) => self.warnings.push(warning),
            }
        }
        self.downloads.sort();
    }

    fn dirs(&self) -> Vec<String> {
        self.dirs.iter().map(|dir| slashed(dir)).collect()
    }

    /// The labels of the decided items, with their kinds and a short form of the outcome.
    fn done(&self) -> Vec<(JobKind, &str, &str)> {
        let mut done = self
            .done
            .iter()
            .map(|(kind, label, outcome)| {
                let outcome = match outcome {
                    Outcome::Skipped {
                        reason: SkipReason::GoogleDocument,
                    } => "document",
                    Outcome::Skipped {
                        reason: SkipReason::Shortcut,
                    } => "shortcut",
                    Outcome::Failed { .. } => "failed",
                    Outcome::Cancelled => "cancelled",
                    Outcome::Transferred { .. } | Outcome::Unchanged => "transferred",
                };
                (*kind, label.as_str(), outcome)
            })
            .collect::<Vec<_>>();
        done.sort_unstable();
        done
    }
}

/// Components joined with `/`: the separator is `\` on Windows.
fn slashed(path: &Path) -> String {
    path.components()
        .map(|component| match component {
            Component::Normal(name) => name.to_string_lossy().into_owned(),
            other => panic!("{other:?} in {}", path.display()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Walks `root` on `server` into a local tree.
async fn plan_from(server: &FakeDrive, root: google_drive3::api::File) -> (Planned, Totals) {
    let (mut tree, root) = LocalTree::new(root).unwrap();
    let mut planned = Planned {
        ordered: true,
        ..Planned::default()
    };
    planned.add(tree.take_actions());
    let mut walk = Walk::start(Arc::new(server.hub()), root, WalkOptions::default());
    let walked = async {
        while let Some(item) = walk.next().await {
            tree.push(item);
            planned.add(tree.take_actions());
        }
    };
    tokio::time::timeout(Duration::from_secs(20), walked)
        .await
        .expect("the walk should end");
    (planned, tree.totals())
}

async fn plan(files: Vec<FakeFile>, behavior: Behavior) -> (FakeDrive, Planned, Totals) {
    let server = FakeDrive::start(files, behavior).await;
    let (planned, totals) = plan_from(&server, root_folder()).await;
    (server, planned, totals)
}

#[tokio::test]
async fn three_levels_of_8_folders_take_3_batched_requests() {
    let folders = [
        ("a", "root"),
        ("b", "root"),
        ("c", "root"),
        ("a1", "a"),
        ("a2", "a"),
        ("b1", "b"),
        ("c1", "c"),
    ];
    let mut files = folders
        .iter()
        .map(|(id, parent)| FakeFile::folder(id, id, parent))
        .collect::<Vec<_>>();
    for folder in ["root", "a", "b", "c", "a1", "a2", "b1", "c1"] {
        files.push(FakeFile::blob(
            &format!("{folder}-file"),
            "file.txt",
            folder,
            folder,
        ));
    }
    // Slow enough that requests would overlap if the walk sent more than it may.
    let behavior = Behavior {
        delay: Duration::from_millis(20),
        ..Behavior::default()
    };

    let (server, planned, totals) = plan(files, behavior).await;

    assert_eq!(
        planned.downloads,
        [
            "Root/a/a1/file.txt",
            "Root/a/a2/file.txt",
            "Root/a/file.txt",
            "Root/b/b1/file.txt",
            "Root/b/file.txt",
            "Root/c/c1/file.txt",
            "Root/c/file.txt",
            "Root/file.txt",
        ]
    );
    assert!(
        planned.ordered,
        "every directory is created before its files"
    );
    assert_eq!(planned.dirs().len(), 8);
    assert_eq!(planned.dirs()[0], "Root");
    assert_eq!(
        totals,
        Totals {
            files: 8,
            folders: 8,
            bytes: 4 + 1 + 1 + 1 + 2 + 2 + 2 + 2,
            incomplete: false,
        }
    );
    assert!(server.max_in_flight() <= WalkOptions::DEFAULT_LISTING_JOBS);
    // Batched, each level is one query: the root, then a, b and c, then a1, a2, b1 and c1.
    let parents = server
        .requests()
        .into_iter()
        .map(|request| request.parents)
        .collect::<Vec<_>>();
    assert_eq!(
        parents,
        [
            vec!["root"],
            vec!["a", "b", "c"],
            vec!["a1", "a2", "b1", "c1"]
        ]
    );
}

#[tokio::test]
async fn listed_children_get_safe_unique_names_and_documents_are_skipped() {
    let files = vec![
        FakeFile::blob("id-b", "report.pdf", "root", "b"),
        FakeFile::blob("id-a", "report.pdf", "root", "a"),
        FakeFile::folder("id-c", "../escape", "root"),
        FakeFile::blob("id-d", "a/b", "id-c", "d"),
        FakeFile::document("id-e", "notes", "root"),
        FakeFile::shortcut("id-f", "link", "root", "id-a", "application/pdf"),
    ];

    let (_server, planned, _totals) = plan(files, Behavior::default()).await;

    assert_eq!(planned.downloads.len(), 3, "{:?}", planned.downloads);
    assert_eq!(planned.downloads[0], "Root/.._escape/a_b");
    assert_eq!(planned.downloads[1], "Root/report.pdf");
    let suffixed = planned.downloads[2].strip_prefix("Root/").unwrap();
    assert!(suffixed.starts_with("report~"), "{suffixed}");
    assert_eq!(&suffixed[suffixed.len() - 4..], ".pdf");
    assert_eq!(
        planned.done(),
        [
            (JobKind::Media, "Root/link", "shortcut"),
            (JobKind::Media, "Root/notes", "document"),
        ]
    );
    // The duplicate and the two sanitized names, each with its folder.
    assert_eq!(planned.warnings.len(), 3, "{:?}", planned.warnings);
    assert!(planned
        .warnings
        .iter()
        .any(|warning| warning.starts_with("Warning: In 'Root/.._escape': 'a/b' (id id-d)")));
}

#[tokio::test]
async fn an_unusable_folder_name_fails_it_and_cancels_what_is_in_it() {
    let files = vec![
        FakeFile::folder("bad", "..", "root"),
        FakeFile::blob("x", "x.txt", "bad", "x"),
        FakeFile::folder("deeper", "deeper", "bad"),
        FakeFile::blob("z", "z.txt", "deeper", "z"),
        FakeFile::folder("ok", "ok", "root"),
        FakeFile::blob("y", "y.txt", "ok", "y"),
    ];

    let (_server, planned, totals) = plan(files, Behavior::default()).await;

    assert_eq!(planned.downloads, ["Root/ok/y.txt"]);
    assert_eq!(planned.dirs(), ["Root", "Root/ok"]);
    assert_eq!(
        planned.done(),
        [
            (JobKind::Media, "Root/../deeper/z.txt", "cancelled"),
            (JobKind::Media, "Root/../x.txt", "cancelled"),
            (JobKind::Metadata, "Root/..", "failed"),
        ]
    );
    assert_eq!((totals.files, totals.folders), (1, 2));
    let failure = planned
        .done
        .iter()
        .find_map(|(_, _, outcome)| match outcome {
            Outcome::Failed { error } => Some(crate::error_message(error.as_ref())),
            _ => None,
        })
        .unwrap();
    assert!(
        failure.starts_with(
            "Error: in folder 'Root': unable to map a drive name to a local name: the name '..' \
            of file bad cannot be used as a local file name, rename it with `gdrive files rename \
            bad <NEW_NAME>`"
        ),
        "{failure}"
    );
}

#[tokio::test]
async fn an_unreadable_folder_fails_alone() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::blob("a1", "a1.txt", "a", "a1"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::blob("b1", "b1.txt", "b", "b1"),
    ];
    let behavior = Behavior {
        unreadable: HashSet::from([String::from("b")]),
        ..Behavior::default()
    };

    let (_server, planned, _totals) = plan(files, behavior).await;

    assert_eq!(planned.downloads, ["Root/a/a1.txt"]);
    assert_eq!(planned.done(), [(JobKind::Metadata, "Root/b", "failed")]);
}

#[tokio::test]
async fn a_folder_listed_inside_itself_fails() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("a", "a", "a"),
        FakeFile::blob("x", "x.txt", "a", "x"),
    ];

    let (_server, planned, _totals) = plan(files, Behavior::default()).await;

    assert_eq!(planned.downloads, ["Root/a/x.txt"]);
    assert_eq!(planned.done(), [(JobKind::Metadata, "Root/a/a", "failed")]);
    assert!(matches!(
        &planned.done[0].2,
        Outcome::Failed { error } if error.to_string() == "in folder 'Root/a'"
    ));
}

#[tokio::test]
async fn incomplete_search_warns_and_marks_the_totals() {
    let files = vec![FakeFile::blob("x", "x.txt", "root", "x")];
    let behavior = Behavior {
        incomplete: HashSet::from([String::from("root")]),
        ..Behavior::default()
    };

    let (_server, planned, totals) = plan(files, behavior).await;

    assert!(totals.incomplete);
    assert_eq!(planned.downloads, ["Root/x.txt"]);
    assert_eq!(
        planned.warnings,
        [
            "Warning: Drive did not search everything while listing 'Root', some files may be \
        missing"
        ]
    );
}

#[tokio::test]
async fn shared_drive_folder_from_its_metadata_is_listed_in_its_drive() {
    let files = vec![
        FakeFile::folder("root", "Root", "drive-root").in_drive("drv"),
        FakeFile::folder("a", "a", "root").in_drive("drv"),
        FakeFile::blob("a1", "x.txt", "a", "x").in_drive("drv"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;

    // The metadata request of `files download`; the fake answers only the fields it asks for.
    let file = files::info::get_file(&server.hub(), "root").await.unwrap();
    assert_eq!(file.drive_id.as_deref(), Some("drv"));
    let (planned, _totals) = plan_from(&server, file).await;

    assert_eq!(planned.downloads, ["Root/a/x.txt"]);
    assert_eq!(server.gets(), ["root"]);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.param("corpora"), Some("drive"));
        assert_eq!(request.param("driveId"), Some("drv"));
    }
}

#[test]
fn a_root_that_is_not_a_usable_folder_is_rejected() {
    let file = google_drive3::api::File {
        mime_type: Some(String::from("text/plain")),
        ..root_folder()
    };
    assert!(matches!(
        LocalTree::new(file),
        Err(errors::Folder::NotDirectory)
    ));

    let file = google_drive3::api::File {
        name: Some(String::from("..")),
        ..root_folder()
    };
    assert!(matches!(
        LocalTree::new(file),
        Err(errors::Folder::UnusableName(_))
    ));
}

#[test]
fn a_renamed_root_warns_before_its_directory_is_created() {
    let file = google_drive3::api::File {
        name: Some(String::from("a/b")),
        ..root_folder()
    };
    let (mut tree, root) = LocalTree::new(file).unwrap();

    assert_eq!(root.name, "a/b");
    let actions = tree.take_actions();
    assert!(matches!(
        actions.as_slice(),
        [Action::Warning(warning), Action::CreateDir(dir)]
            if warning.starts_with("Warning: 'a/b' (id root) is saved as 'a_b'")
                && dir == Path::new("a_b")
    ));
}

/// The entry of `file` at `path` (below the root) in the listing of `parent`.
fn entry(
    file: google_drive3::api::File,
    path: &str,
    parent: FolderKey,
    children: Children,
) -> WalkItem {
    let components = path.split('/').map(String::from).collect::<Vec<_>>();
    WalkItem::Entry(WalkEntry {
        depth: components.len(),
        components,
        kind: EntryKind::of(&file),
        file: Box::new(file),
        parent,
        children,
    })
}

fn started_tree() -> LocalTree {
    let (mut tree, _root) = LocalTree::new(root_folder()).unwrap();
    assert!(matches!(
        tree.take_actions().as_slice(),
        [Action::CreateDir(dir)] if dir == Path::new("Root")
    ));
    tree
}

#[test]
fn files_of_a_folder_wait_for_the_names_of_its_parent() {
    let a = FolderKey::for_tests(1);
    let mut tree = started_tree();

    tree.push(entry(
        drive_file("a", "a", true),
        "a",
        FolderKey::ROOT,
        Children::Listed(a),
    ));
    tree.push(entry(
        drive_file("x", "x.pdf", false),
        "a/x.pdf",
        a,
        Children::None,
    ));
    // The subfolder's listing ends before the root's: its own path is not known yet.
    tree.push(WalkItem::FolderDone(a));
    assert!(tree.take_actions().is_empty());

    tree.push(WalkItem::FolderDone(FolderKey::ROOT));
    let actions = tree.take_actions();
    assert!(
        matches!(
            actions.as_slice(),
            [Action::CreateDir(dir), Action::Download(file)]
                if dir == Path::new("Root").join("a").as_path()
                    && file.path == Path::new("Root").join("a").join("x.pdf")
                    && file.id == "x"
        ),
        "{actions:?}"
    );
}

#[test]
fn a_folder_that_fails_after_some_entries_cancels_them_and_its_subfolders() {
    let sub = FolderKey::for_tests(1);
    let mut tree = started_tree();

    let root = FolderKey::ROOT;
    tree.push(entry(
        drive_file("x", "x.pdf", false),
        "x.pdf",
        root,
        Children::None,
    ));
    tree.push(entry(
        drive_file("sub", "sub", true),
        "sub",
        root,
        Children::Listed(sub),
    ));
    tree.push(entry(
        drive_file("y", "y.pdf", false),
        "sub/y.pdf",
        sub,
        Children::None,
    ));
    tree.push(WalkItem::FolderError(FolderError {
        folder: FolderRef {
            key: FolderKey::ROOT,
            id: String::from("root"),
            name: String::from("Root"),
            components: Vec::new(),
        },
        error: Box::new(google_drive3::Error::Cancelled),
    }));
    tree.push(entry(
        drive_file("z", "z.pdf", false),
        "sub/z.pdf",
        sub,
        Children::None,
    ));
    tree.push(WalkItem::FolderDone(sub));

    let mut planned = Planned {
        ordered: true,
        ..Planned::default()
    };
    planned.add(tree.take_actions());
    assert!(planned.downloads.is_empty() && planned.dirs.is_empty());
    assert_eq!(
        planned.done(),
        [
            (JobKind::Media, "Root/sub/y.pdf", "cancelled"),
            (JobKind::Media, "Root/sub/z.pdf", "cancelled"),
            (JobKind::Media, "Root/x.pdf", "cancelled"),
            (JobKind::Metadata, "Root", "failed"),
        ]
    );
}
