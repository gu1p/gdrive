//! `files migrate` between two fake Drives: the source answers listings, `files.get`, media
//! downloads, exports and the patches that trash; the destination answers listings, creates and
//! `files.generateIds`, and refuses a create into a folder that does not exist yet.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future,
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use serde_json::json;

use super::{run, Error, Settings};
use crate::{
    common::drive_file::MIME_TYPE_DRIVE_FOLDER,
    transfer::{
        fake_drive::{Behavior, Created, FakeDrive, FakeFile},
        progress::{NoProgress, ProgressDisplay},
        relay::{RelayError, Unsupported, EXPORT_LIMIT},
        scheduler::{
            capture::{output, Captured},
            Options,
        },
        upload::UploadOptions,
    },
};

/// `photos` in the source's My Drive, with 4 files on three levels and an empty folder:
///
/// ```text
/// photos/a.txt b.txt  photos/2024/d.txt  photos/2024/jan/f.txt  photos/empty/
/// ```
fn source_tree() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("photos", "photos", "root"),
        FakeFile::blob("a", "a.txt", "photos", "aaaa"),
        FakeFile::blob("b", "b.txt", "photos", "bb"),
        FakeFile::folder("y2024", "2024", "photos"),
        FakeFile::blob("d", "d.txt", "y2024", "dddd"),
        FakeFile::folder("jan", "jan", "y2024"),
        FakeFile::blob("f", "f.txt", "jan", "ffffff"),
        FakeFile::folder("empty", "empty", "photos"),
    ]
}

/// What [`source_tree`] becomes in the destination: folders end with `/`, files have their
/// content.
fn migrated_tree() -> BTreeMap<String, String> {
    [
        ("photos/", ""),
        ("photos/a.txt", "aaaa"),
        ("photos/b.txt", "bb"),
        ("photos/2024/", ""),
        ("photos/2024/d.txt", "dddd"),
        ("photos/2024/jan/", ""),
        ("photos/2024/jan/f.txt", "ffffff"),
        ("photos/empty/", ""),
    ]
    .into_iter()
    .map(|(path, content)| (path.to_owned(), content.to_owned()))
    .collect()
}

fn settings(file_ids: &[&str]) -> Settings {
    Settings {
        file_ids: file_ids.iter().map(|&id| id.to_owned()).collect(),
        into: None,
        dry_run: false,
        trash_source: false,
        archive: false,
        contents: false,
        replace_smaller: false,
        ask: None,
        same_account: false,
        transfer: Options::new(NonZeroUsize::new(4).unwrap(), false),
    }
}

fn continuing(settings: Settings) -> Settings {
    Settings {
        transfer: Options::new(settings.transfer.jobs, true),
        ..settings
    }
}

struct Migrated {
    result: Result<(), Error>,
    out: Captured,
    err: Captured,
}

impl Migrated {
    fn error(&self) -> String {
        match &self.result {
            Ok(()) => panic!("the migration should fail; stderr: {}", self.err.text()),
            Err(error) => crate::error_message(error),
        }
    }

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

    /// The lines of stdout, sorted: parallel jobs print in any order.
    fn sorted_out(&self) -> Vec<String> {
        let mut lines = self.out.lines();
        lines.sort();
        lines
    }
}

async fn migrate(source: &FakeDrive, dest: &FakeDrive, settings: Settings) -> Migrated {
    let output = output();
    let (source, dest) = (source.hub(), dest.hub());
    let options = UploadOptions::default();
    let run = run(
        &source,
        &dest,
        &settings,
        &options,
        &output.printer,
        ProgressDisplay::reporting_to(Arc::new(NoProgress)),
        future::pending(),
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

/// A file or folder of the destination, created by the run or there before.
struct Node {
    name: String,
    parent: String,
    folder: bool,
    content: String,
}

/// The destination below its top folder (and below the folders of `stop`): what the run
/// created, placed in the folders of `before` (what was there before) and of the run.
fn tree_below(created: &[Created], before: &[FakeFile], stop: &[&str]) -> BTreeMap<String, String> {
    let mut nodes = HashMap::new();
    for file in before {
        nodes.insert(
            file.id.clone(),
            Node {
                name: file.name.clone(),
                parent: file.parents.first().cloned().unwrap_or_default(),
                folder: file.mime_type == MIME_TYPE_DRIVE_FOLDER,
                content: file.content.clone().unwrap_or_default(),
            },
        );
    }
    for file in created {
        nodes.insert(
            file.id.clone(),
            Node {
                name: file.name.clone(),
                parent: file.parents.first().cloned().unwrap_or_default(),
                folder: file.mime_type == MIME_TYPE_DRIVE_FOLDER,
                content: String::from_utf8_lossy(&file.content).into_owned(),
            },
        );
    }
    let path = |id: &str| {
        let mut names = Vec::new();
        let mut current = id.to_owned();
        while let Some(node) = nodes.get(&current) {
            names.push(node.name.clone());
            if stop.contains(&node.parent.as_str()) {
                break;
            }
            current = node.parent.clone();
        }
        names.reverse();
        names.join("/")
    };
    created
        .iter()
        .map(|file| {
            let node = &nodes[&file.id];
            if node.folder {
                (format!("{}/", path(&file.id)), String::new())
            } else {
                (path(&file.id), node.content.clone())
            }
        })
        .collect()
}

/// What the run created in the destination, by path below the top of My Drive.
fn created_tree(dest: &FakeDrive) -> BTreeMap<String, String> {
    tree_below(&dest.created(), &[], &["root"])
}

fn assert_no_secrets(migrated: &Migrated) {
    for text in [migrated.out.text(), migrated.err.text()] {
        for secret in ["mock-token", "upload-session", "Bearer"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
}

#[tokio::test]
async fn a_folder_tree_is_migrated_with_its_structure_and_content() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    migrated.assert_ok();
    assert_eq!(created_tree(&dest), migrated_tree());
    let created = dest.created();
    let photos = created.iter().find(|file| file.name == "photos").unwrap();
    assert_eq!(photos.parents, ["root"]);
    for file in created
        .iter()
        .filter(|file| file.mime_type != MIME_TYPE_DRIVE_FOLDER)
    {
        assert_eq!(file.mime_type, "text/plain", "{}", file.name);
    }
    assert_eq!(
        migrated.out.lines().first().map(String::as_str),
        Some("Found 4 files in 4 folders with a total size of 16 B")
    );
    let mut expected = vec![
        "Found 4 files in 4 folders with a total size of 16 B",
        "create photos/",
        "create photos/2024/",
        "create photos/2024/jan/",
        "create photos/empty/",
        "create photos/a.txt (4 B)",
        "create photos/b.txt (2 B)",
        "create photos/2024/d.txt (4 B)",
        "create photos/2024/jan/f.txt (6 B)",
    ];
    expected.sort_unstable();
    assert_eq!(migrated.sorted_out(), expected);
    assert_eq!(
        migrated.summary(),
        "Summary: 4 migrated (16 B), 0 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled; 4 folders created, 0 reused"
    );
    // The source is only read.
    assert_eq!(source.writes(), Vec::<String>::new());
    assert_eq!(source.patches(), []);
    assert!(source.created().is_empty());
    let mut media = source.media();
    media.sort();
    assert_eq!(media, ["a", "b", "d", "f"]);
    assert_no_secrets(&migrated);
}

#[tokio::test]
async fn an_id_given_twice_is_migrated_once() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos", "photos"])).await;

    migrated.assert_ok();
    assert_eq!(created_tree(&dest), migrated_tree());
    assert_eq!(dest.created().len(), migrated_tree().len());
}

#[tokio::test]
async fn files_and_folders_go_into_the_into_folder() {
    let mut files = source_tree();
    files.push(FakeFile::blob("notes", "notes.txt", "root", "n"));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let target = FakeFile::folder("target", "Archive", "root");
    let dest = FakeDrive::start(vec![target.clone()], Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            into: Some(String::from("target")),
            ..settings(&["notes", "photos"])
        },
    )
    .await;

    migrated.assert_ok();
    let mut expected = migrated_tree()
        .into_iter()
        .map(|(path, content)| (format!("Archive/{path}"), content))
        .collect::<BTreeMap<_, _>>();
    expected.insert(String::from("Archive/notes.txt"), String::from("n"));
    assert_eq!(tree_below(&dest.created(), &[target], &["root"]), expected);
    assert_eq!(dest.gets(), ["target"], "--into is checked to be a folder");
}

#[tokio::test]
async fn into_must_be_a_folder_of_the_destination() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(
        vec![FakeFile::blob("file", "file.txt", "root", "x")],
        Behavior::default(),
    )
    .await;

    for (into, expected) in [
        (
            "file",
            "Error: 'file' is not a folder of the destination account; --into takes a folder id",
        ),
        (
            "missing",
            "Error: unable to get the destination folder 'missing'",
        ),
    ] {
        let migrated = migrate(
            &source,
            &dest,
            Settings {
                into: Some(into.to_owned()),
                ..settings(&["photos"])
            },
        )
        .await;
        assert!(
            migrated.error().starts_with(expected),
            "{}",
            migrated.error()
        );
    }
    assert!(dest.created().is_empty());
}

#[tokio::test]
async fn folders_are_created_before_what_goes_into_them() {
    let mut files = source_tree();
    for n in 0..6 {
        files.push(FakeFile::blob(
            &format!("j{n}"),
            &format!("j{n}.txt"),
            "jan",
            "j",
        ));
    }
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            upload_delay: Duration::from_millis(5),
            ..Behavior::default()
        },
    )
    .await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    migrated.assert_ok();
    let created = dest.created();
    let by_id = created
        .iter()
        .map(|file| (file.id.as_str(), file))
        .collect::<HashMap<_, _>>();
    for file in &created {
        let parent = &file.parents[0];
        if parent == "root" {
            continue;
        }
        let folder = by_id[parent.as_str()];
        assert!(
            folder.answered < file.arrived,
            "'{}' arrived before its folder '{}' was created",
            file.name,
            folder.name
        );
    }
    // Breadth-first: a folder's subfolders are created before the folders below them.
    let position = |name: &str| created.iter().position(|file| file.name == name).unwrap();
    assert!(position("photos") < position("2024"));
    assert!(position("2024") < position("jan"));
    assert_eq!(created.len(), 4 + 4 + 6);
}

#[tokio::test]
async fn a_second_run_finds_everything_already_there() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, settings(&["photos"]))
        .await
        .assert_ok();
    let created = dest.created().len();

    let again = migrate(&source, &dest, settings(&["photos"])).await;

    again.assert_ok();
    assert_eq!(dest.created().len(), created, "nothing is created twice");
    assert_eq!(
        again.summary(),
        "Summary: 0 migrated (0 B), 4 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled; 0 folders created, 4 reused"
    );
    let lines = again.sorted_out();
    assert!(
        lines.contains(&String::from("reuse photos/2024/jan/")),
        "{lines:?}"
    );
    assert!(
        lines.contains(&String::from("already-there photos/2024/jan/f.txt")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn existing_folders_are_reused_and_identical_files_skipped() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let before = vec![
        FakeFile::folder("old-photos", "photos", "root"),
        FakeFile::blob("old-a", "a.txt", "old-photos", "aaaa"),
        FakeFile::folder("old-2024", "2024", "old-photos"),
        // A folder of the same name is no file of that name.
        FakeFile::folder("old-d", "d.txt", "old-2024"),
        // Not the same name: `photos/b.txt` is created.
        FakeFile::blob("old-b", "B.txt", "old-photos", "bb"),
    ];
    let dest = FakeDrive::start(before.clone(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    migrated.assert_ok();
    let created = dest.created();
    assert!(!created.iter().any(|file| file.name == "photos"));
    assert!(!created.iter().any(|file| file.name == "2024"));
    assert!(!created.iter().any(|file| file.name == "a.txt"));
    let mut expected = migrated_tree();
    for reused in ["photos/", "photos/a.txt", "photos/2024/"] {
        expected.remove(reused);
    }
    assert_eq!(tree_below(&created, &before, &["root"]), expected);
    let b = created.iter().find(|file| file.name == "b.txt").unwrap();
    assert_eq!(b.parents, ["old-photos"]);
    assert_eq!(
        migrated.summary(),
        "Summary: 3 migrated (12 B), 1 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled; 2 folders created, 2 reused"
    );
    let lines = migrated.sorted_out();
    for line in [
        "reuse photos/",
        "reuse photos/2024/",
        "already-there photos/a.txt",
    ] {
        assert!(lines.contains(&line.to_owned()), "{line}: {lines:?}");
    }
}

/// The destination of the conflict tests: `photos/a.txt` with other content than the source's.
fn conflicting_destination() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("old-photos", "photos", "root"),
        FakeFile::blob("old-a", "a.txt", "old-photos", "not a"),
    ]
}

const CONFLICT: &str = "Error: unable to migrate 'photos/a.txt': it exists in the destination \
    with different content";

#[tokio::test]
async fn a_file_with_other_content_in_the_destination_fails_and_stops_the_run() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(conflicting_destination(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            transfer: Options::new(NonZeroUsize::new(1).unwrap(), false),
            ..settings(&["photos"])
        },
    )
    .await;

    let error = migrated.error();
    assert!(
        error.starts_with("Error: the migration is incomplete: 1 file failed"),
        "{error}"
    );
    assert!(error.contains("use --continue-on-error"), "{error}");
    assert!(migrated.err.lines().contains(&CONFLICT.to_owned()));
    assert!(!dest.created().iter().any(|file| file.name == "a.txt"));
    assert!(
        migrated.summary().contains("1 failed"),
        "{}",
        migrated.summary()
    );
}

#[tokio::test]
async fn with_continue_on_error_a_conflict_fails_alone() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let before = conflicting_destination();
    let dest = FakeDrive::start(before.clone(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, continuing(settings(&["photos"]))).await;

    let error = migrated.error();
    assert_eq!(error, "Error: the migration is incomplete: 1 file failed");
    assert!(migrated.err.lines().contains(&CONFLICT.to_owned()));
    let mut expected = migrated_tree();
    expected.remove("photos/");
    expected.remove("photos/a.txt");
    assert_eq!(tree_below(&dest.created(), &before, &["root"]), expected);
    assert_eq!(
        migrated.summary(),
        "Summary: 3 migrated (12 B), 0 already there, 0 skipped (unsupported), 1 failed, \
         0 cancelled; 3 folders created, 1 reused"
    );
}

#[tokio::test]
async fn a_dry_run_reads_the_destination_and_writes_nothing() {
    let mut files = source_tree();
    files.push(FakeFile::native(
        "budget",
        "Budget",
        "photos",
        "application/vnd.google-apps.spreadsheet",
        "a,b",
    ));
    files.push(FakeFile::shortcut(
        "link",
        "link",
        "photos",
        "a",
        "text/plain",
    ));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(
        vec![
            FakeFile::folder("old-photos", "photos", "root"),
            FakeFile::blob("old-a", "a.txt", "old-photos", "aaaa"),
        ],
        Behavior::default(),
    )
    .await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    // Only the listings of the top folder and of the folder that exists.
    assert!(dest.created().is_empty());
    assert_eq!(dest.generated_ids(), 0);
    assert!(dest.patches().is_empty() && dest.updates().is_empty() && dest.deletes().is_empty());
    assert_eq!(dest.chunks(), []);
    assert_eq!(dest.gets(), Vec::<String>::new());
    let mut listed = dest
        .requests()
        .into_iter()
        .flat_map(|request| request.parents)
        .collect::<Vec<_>>();
    listed.sort();
    assert_eq!(listed, ["old-photos", "root"]);
    // The source is only listed, also with --trash-source.
    assert!(source.media().is_empty() && source.exports().is_empty());
    assert_eq!(source.patches(), []);
    assert_eq!(source.writes(), Vec::<String>::new());
    assert_eq!(dest.writes(), Vec::<String>::new());

    let skip = format!("skip photos/link: {}", Unsupported::Shortcut);
    let mut expected = vec![
        "Found 6 files in 4 folders with a total size of 16 B",
        "reuse photos/",
        "already-there photos/a.txt",
        "create photos/b.txt (2 B)",
        "convert photos/Budget",
        &skip,
        "create photos/2024/",
        "create photos/2024/d.txt (4 B)",
        "create photos/2024/jan/",
        "create photos/2024/jan/f.txt (6 B)",
        "create photos/empty/",
        "trash photos/a.txt",
        "trash photos/b.txt",
        "trash photos/Budget",
        "trash photos/2024/d.txt",
        "trash photos/2024/jan/f.txt",
        "trash photos/2024/jan/",
        "trash photos/2024/",
        "trash photos/empty/",
    ];
    expected.sort_unstable();
    assert_eq!(migrated.sorted_out(), expected);
    assert_eq!(
        migrated.summary(),
        "Dry run, nothing was changed. Summary: 4 to migrate (12 B, 1 Google Workspace file to \
         convert), 1 already there, 1 skipped (unsupported), 0 failed, 0 cancelled; 3 folders to \
         create, 1 reused; 8 to move to the trash in the source"
    );
}

#[tokio::test]
async fn a_dry_run_reports_a_conflict_as_the_run_would() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(conflicting_destination(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        continuing(Settings {
            dry_run: true,
            ..settings(&["photos"])
        }),
    )
    .await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 file failed"
    );
    assert!(migrated.err.lines().contains(&CONFLICT.to_owned()));
    assert!(dest.created().is_empty());
}

#[tokio::test]
async fn shortcuts_drawings_and_forms_are_skipped_and_listed() {
    let mut files = source_tree();
    files.push(FakeFile::shortcut(
        "link",
        "link to a",
        "photos",
        "a",
        "text/plain",
    ));
    files.push(FakeFile::native(
        "form",
        "Survey",
        "y2024",
        "application/vnd.google-apps.form",
        "",
    ));
    // Drive converts nothing back to a Drawing.
    files.push(FakeFile::native(
        "drawing",
        "Sketch",
        "photos",
        "application/vnd.google-apps.drawing",
        "png",
    ));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["photos"])).await;

    migrated.assert_ok();
    assert_eq!(created_tree(&dest), migrated_tree());
    let lines = migrated.sorted_out();
    let shortcut = Unsupported::Shortcut;
    assert!(
        lines.contains(&format!("skip photos/link to a: {shortcut}")),
        "{lines:?}"
    );
    let err = migrated.err.lines();
    let at = err
        .iter()
        .position(|line| line == "Skipped 3 unsupported items, which cannot be migrated:")
        .unwrap_or_else(|| panic!("{err:?}"));
    let form = Unsupported::NativeType(String::from("application/vnd.google-apps.form"));
    let drawing = Unsupported::NativeType(String::from("application/vnd.google-apps.drawing"));
    assert_eq!(err[at + 1], format!("  photos/2024/Survey: {form}"));
    assert_eq!(err[at + 2], format!("  photos/Sketch: {drawing}"));
    assert_eq!(err[at + 3], format!("  photos/link to a: {shortcut}"));
    assert_eq!(
        migrated.summary(),
        "Summary: 4 migrated (16 B), 0 already there, 3 skipped (unsupported), 0 failed, \
         0 cancelled; 4 folders created, 0 reused"
    );
    assert_eq!(source.exports(), []);
}

#[tokio::test]
async fn google_docs_are_exported_and_converted_back() {
    let files = vec![
        FakeFile::folder("docs", "docs", "root"),
        FakeFile::native(
            "report",
            "Report",
            "docs",
            "application/vnd.google-apps.document",
            "the report",
        ),
    ];
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, settings(&["docs"])).await;

    migrated.assert_ok();
    let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    assert_eq!(
        source.exports(),
        [(String::from("report"), docx.to_owned())]
    );
    let created = dest.created();
    let report = created.iter().find(|file| file.name == "Report").unwrap();
    assert_eq!(report.mime_type, "application/vnd.google-apps.document");
    assert_eq!(report.content, b"the report");
    assert!(migrated
        .out
        .lines()
        .contains(&String::from("convert docs/Report")));
    assert!(
        migrated
            .summary()
            .starts_with("Summary: 1 migrated (10 B, 1 Google Workspace file converted),"),
        "{}",
        migrated.summary()
    );

    // Again: a Workspace file of the same name and type is there.
    let again = migrate(&source, &dest, settings(&["docs"])).await;
    again.assert_ok();
    assert_eq!(dest.created().len(), created.len());
    assert!(again
        .out
        .lines()
        .contains(&String::from("already-there docs/Report")));
}

#[tokio::test]
async fn an_export_over_the_limit_is_a_failed_item() {
    let files = vec![
        FakeFile::folder("docs", "docs", "root"),
        FakeFile::native(
            "big",
            "Big",
            "docs",
            "application/vnd.google-apps.document",
            "x",
        ),
        FakeFile::blob("small", "small.txt", "docs", "s"),
    ];
    let source = FakeDrive::start(
        files,
        Behavior {
            export_too_large: HashSet::from([String::from("big")]),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, continuing(settings(&["docs"]))).await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 file failed"
    );
    let err = migrated.err.text();
    let too_large = RelayError::ExportTooLarge {
        limit: EXPORT_LIMIT,
    };
    assert!(
        err.contains(&format!("Error: unable to migrate 'docs/Big': {too_large}")),
        "{err}"
    );
    assert!(dest.created().iter().any(|file| file.name == "small.txt"));
    assert!(!dest.created().iter().any(|file| file.name == "Big"));
}

/// Without `--trash-source` the source account is only read, whatever a run meets: files that
/// are copied, converted, skipped, already there or in conflict, and a second run over all of
/// it. Not one request that could change, trash or delete anything goes to the source.
#[tokio::test]
async fn without_trash_source_the_source_is_only_read() {
    let mut files = source_tree();
    files.push(FakeFile::native(
        "budget",
        "Budget",
        "photos",
        "application/vnd.google-apps.spreadsheet",
        "a,b",
    ));
    files.push(FakeFile::shortcut(
        "link",
        "link",
        "photos",
        "a",
        "text/plain",
    ));
    let source = FakeDrive::start(files, Behavior::default()).await;
    // `a.txt` is in conflict, `b.txt` is already there.
    let mut before = conflicting_destination();
    before.push(FakeFile::blob("old-b", "b.txt", "old-photos", "bb"));
    let dest = FakeDrive::start(before, Behavior::default()).await;

    for run in ["first", "second"] {
        let migrated = migrate(&source, &dest, continuing(settings(&["photos"]))).await;

        assert_eq!(
            migrated.error(),
            "Error: the migration is incomplete: 1 file failed",
            "{run} run"
        );
        assert_eq!(source.writes(), Vec::<String>::new(), "{run} run");
        assert_eq!(source.patches(), [], "{run} run");
        assert_eq!(source.deletes(), Vec::<String>::new(), "{run} run");
        assert!(source.created().is_empty(), "{run} run");
        assert!(source.updates().is_empty(), "{run} run");
        assert_eq!(source.chunks(), [], "{run} run");
        assert_eq!(source.generated_ids(), 0, "{run} run");
        assert!(
            !migrated.out.text().contains("trash "),
            "{run} run: {}",
            migrated.out.text()
        );
        assert!(
            !migrated.summary().contains("trash"),
            "{run} run: {}",
            migrated.summary()
        );
    }
    // The run did its work: the copies read the source's content.
    let mut media = source.media();
    media.sort();
    media.dedup();
    assert_eq!(media, ["d", "f"]);
    assert_eq!(source.exports().len(), 1, "{:?}", source.exports());
    // And the destination never loses anything either.
    assert_eq!(dest.deletes(), Vec::<String>::new());
    assert_eq!(dest.patches(), []);
}

#[tokio::test]
async fn trash_source_trashes_each_file_and_then_its_folders() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert_eq!(created_tree(&dest), migrated_tree());
    let patches = source.patches();
    for (_, body) in &patches {
        assert_eq!(body, &json!({"trashed": true}));
    }
    let order = patches
        .iter()
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    let mut trashed = order.clone();
    trashed.sort_unstable();
    assert_eq!(
        trashed,
        ["a", "b", "d", "empty", "f", "jan", "photos", "y2024"]
    );
    let at = |id: &str| order.iter().position(|&trashed| trashed == id).unwrap();
    assert!(at("f") < at("jan"));
    assert!(at("d") < at("y2024") && at("jan") < at("y2024"));
    for child in ["a", "b", "y2024", "empty"] {
        assert!(at(child) < at("photos"), "{child} after photos: {order:?}");
    }
    assert!(
        migrated
            .summary()
            .ends_with("; 8 moved to the trash in the source"),
        "{}",
        migrated.summary()
    );
    // Never deleted for good: the trash is the only thing the source is asked for.
    assert_eq!(source.deletes(), Vec::<String>::new());
    let writes = source.writes();
    assert_eq!(writes.len(), patches.len(), "{writes:?}");
    assert!(
        writes.iter().all(|write| write.starts_with("PATCH ")),
        "{writes:?}"
    );
}

#[tokio::test]
async fn a_folder_with_anything_not_migrated_stays_in_the_source() {
    let mut files = source_tree();
    files.push(FakeFile::shortcut(
        "link",
        "link",
        "y2024",
        "a",
        "text/plain",
    ));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(conflicting_destination(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        continuing(Settings {
            trash_source: true,
            ..settings(&["photos"])
        }),
    )
    .await;

    // `photos/a.txt` conflicts and `photos/2024/link` is skipped: neither folder goes, nor
    // the conflicting file; everything else does.
    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 file failed"
    );
    let mut trashed = source
        .patches()
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    trashed.sort();
    assert_eq!(trashed, ["b", "d", "empty", "f", "jan"]);
}

#[tokio::test]
async fn a_failed_trash_fails_the_item_and_keeps_its_folder() {
    let source = FakeDrive::start(
        source_tree(),
        Behavior {
            patch_rejected: HashSet::from([String::from("f")]),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        continuing(Settings {
            trash_source: true,
            ..settings(&["photos"])
        }),
    )
    .await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 file failed"
    );
    let err = migrated.err.text();
    assert!(
        err.contains(
            "Error: unable to migrate 'photos/2024/jan/f.txt': it is in the destination, but it \
             could not be moved to the trash in the source"
        ),
        "{err}"
    );
    // The file is in the destination all the same.
    assert_eq!(created_tree(&dest), migrated_tree());
    let mut trashed = source
        .patches()
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    trashed.sort();
    assert_eq!(trashed, ["a", "b", "d", "empty", "f"], "f was tried");
}

#[tokio::test]
async fn a_relay_error_is_a_failed_item() {
    let source = FakeDrive::start(
        source_tree(),
        Behavior {
            media_missing: HashMap::from([(String::from("b"), Duration::ZERO)]),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            upload_rejected: HashSet::from([String::from("d.txt")]),
            ..Behavior::default()
        },
    )
    .await;

    let migrated = migrate(&source, &dest, continuing(settings(&["photos"]))).await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 2 files failed"
    );
    let err = migrated.err.lines();
    // The relay's own words are its business; the status Drive answered comes through.
    assert!(
        err.iter().any(
            |line| line.starts_with("Error: unable to migrate 'photos/b.txt': ")
                && line.contains("404 Not Found")
        ),
        "{err:?}"
    );
    assert!(
        err.iter().any(
            |line| line.starts_with("Error: unable to migrate 'photos/2024/d.txt': ")
                && line.contains("400 Bad Request")
        ),
        "{err:?}"
    );
    let mut expected = migrated_tree();
    expected.remove("photos/b.txt");
    expected.remove("photos/2024/d.txt");
    assert_eq!(created_tree(&dest), expected);
    assert_no_secrets(&migrated);
}

#[tokio::test]
async fn a_missing_source_id_is_a_failed_item() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, continuing(settings(&["nothing", "photos"]))).await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 file failed"
    );
    let err = migrated.err.text();
    assert!(
        err.contains(
            "Error: unable to migrate 'nothing': unable to get 'nothing' from the source account"
        ),
        "{err}"
    );
    assert_eq!(created_tree(&dest), migrated_tree());
}

#[tokio::test]
async fn an_incomplete_listing_fails_the_run_and_keeps_the_folder_in_the_source() {
    let source = FakeDrive::start(
        source_tree(),
        Behavior {
            incomplete: HashSet::from([String::from("jan")]),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    assert!(
        migrated
            .error()
            .starts_with("Error: Drive did not search everything while listing the source"),
        "{}",
        migrated.error()
    );
    assert!(migrated.err.lines().contains(&String::from(
        "Warning: Drive did not search everything while listing 'photos/2024/jan', some files \
         may be missing"
    )));
    assert_eq!(created_tree(&dest), migrated_tree());
    let mut trashed = source
        .patches()
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    trashed.sort();
    assert_eq!(trashed, ["a", "b", "d", "empty", "f"]);
}

#[tokio::test]
async fn an_unlisted_source_folder_fails_with_what_is_below_it() {
    let source = FakeDrive::start(
        source_tree(),
        Behavior {
            unreadable: HashSet::from([String::from("y2024")]),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, continuing(settings(&["photos"]))).await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 folder failed"
    );
    let err = migrated.err.text();
    assert!(
        err.contains(
            "Error: unable to migrate 'photos/2024': unable to list the folder in the source"
        ),
        "{err}"
    );
    let created = created_tree(&dest);
    assert!(created.contains_key("photos/b.txt"));
    assert!(!created.keys().any(|path| path.starts_with("photos/2024")));
}

#[test]
fn account_errors_name_the_account_and_the_commands_to_run() {
    use crate::common::hub_helper::GetHubError;

    let message = crate::error_message(&Error::DestinationAccount {
        account: String::from("new@example.com"),
        source: GetHubError::AccountNotFound(String::from("new@example.com")),
    });
    assert_eq!(
        message,
        "Error: unable to use the destination account 'new@example.com': account \
         'new@example.com' not found; run 'gdrive account list' to see the accounts, or 'gdrive \
         account add' to add one"
    );
    let message = crate::error_message(&Error::SourceAccount {
        account: Some(String::from("old@example.com")),
        source: GetHubError::AccountNotFound(String::from("old@example.com")),
    });
    assert!(
        message.starts_with("Error: unable to use the source account 'old@example.com': account"),
        "{message}"
    );
    assert_eq!(
        crate::error_message(&Error::SameAccount(String::from("me@example.com"))),
        "Error: 'me@example.com' is both the source and the destination account; --to must name \
         another account, see 'gdrive account list' (inside one account only --archive works)"
    );
}

/// A server in front of a fake Drive: requests that `answer` replies to get its JSON, the others
/// go to the fake. For what the fake cannot do, such as a source that changes during a run.
struct Interceptor {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

type Answer = dyn Fn(&hyper::Method, &str, &HashMap<String, String>) -> Option<serde_json::Value>
    + Send
    + Sync;

impl Interceptor {
    async fn start(
        upstream: &FakeDrive,
        answer: impl Fn(&hyper::Method, &str, &HashMap<String, String>) -> Option<serde_json::Value>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        use http_body_util::{BodyExt, Full};
        use hyper_util::{client::legacy::Client, rt::TokioExecutor};

        let root = upstream
            .hub()
            .drive_base_url()
            .trim_end_matches("drive/v3/")
            .trim_end_matches('/')
            .to_owned();
        let answer: Arc<Answer> = Arc::new(answer);
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<bytes::Bytes>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (answer, client, root) = (Arc::clone(&answer), client.clone(), root.clone());
                let service = hyper::service::service_fn(
                    move |request: hyper::Request<hyper::body::Incoming>| {
                        let (answer, client, root) =
                            (Arc::clone(&answer), client.clone(), root.clone());
                        async move {
                            let params = request
                                .uri()
                                .query()
                                .map(|query| {
                                    url::form_urlencoded::parse(query.as_bytes())
                                        .into_owned()
                                        .collect::<HashMap<_, _>>()
                                })
                                .unwrap_or_default();
                            let path = request.uri().path().to_owned();
                            if let Some(reply) = answer(request.method(), &path, &params) {
                                let response = hyper::Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Full::new(bytes::Bytes::from(reply.to_string())))
                                    .unwrap();
                                return Ok::<_, std::convert::Infallible>(response);
                            }
                            let (parts, body) = request.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            let path_and_query =
                                parts.uri.path_and_query().map_or("/", |path| path.as_str());
                            let mut forwarded = hyper::Request::builder()
                                .method(parts.method)
                                .uri(format!("{root}{path_and_query}"));
                            // The Host stays this server's, so that a session URL leads here.
                            for (name, value) in &parts.headers {
                                forwarded = forwarded.header(name, value);
                            }
                            let reply = client
                                .request(forwarded.body(Full::new(body)).unwrap())
                                .await
                                .unwrap();
                            let (mut parts, body) = reply.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            parts.headers.remove("transfer-encoding");
                            Ok(hyper::Response::from_parts(parts, Full::new(body)))
                        }
                    },
                );
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Interceptor { addr, task }
    }

    fn hub(&self) -> crate::hub::Hub {
        crate::hub::Hub::for_mock_server(&format!("http://{}/", self.addr)).unwrap()
    }
}

impl Drop for Interceptor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// [`migrate`] with the source behind `source`.
async fn migrate_through(source: &Interceptor, dest: &FakeDrive, settings: Settings) -> Migrated {
    let output = output();
    let (source, dest) = (source.hub(), dest.hub());
    let options = UploadOptions::default();
    let run = run(
        &source,
        &dest,
        &settings,
        &options,
        &output.printer,
        ProgressDisplay::reporting_to(Arc::new(NoProgress)),
        future::pending(),
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

fn trashed_ids(source: &FakeDrive) -> Vec<String> {
    let mut trashed = source
        .patches()
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    trashed.sort();
    trashed
}

#[tokio::test]
async fn a_folder_that_both_accounts_see_is_never_its_own_copy() {
    // Both accounts serve the same objects, as for a folder shared with the destination.
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(source_tree(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    assert!(
        migrated
            .error()
            .starts_with("Error: the migration is incomplete: 1 folder failed"),
        "{}",
        migrated.error()
    );
    assert!(migrated.err.lines().contains(&String::from(
        "Error: unable to migrate 'photos': the source and the destination are the same folder; \
         nothing to do"
    )));
    assert_eq!(trashed_ids(&source), Vec::<String>::new());
    assert_eq!(dest.created().len(), 0);
}

#[tokio::test]
async fn a_file_that_both_accounts_see_is_never_its_own_copy() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(source_tree(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            into: Some(String::from("photos")),
            trash_source: true,
            ..settings(&["a"])
        },
    )
    .await;

    assert!(
        migrated
            .error()
            .starts_with("Error: the migration is incomplete: 1 file failed"),
        "{}",
        migrated.error()
    );
    assert!(migrated.err.lines().contains(&String::from(
        "Error: unable to migrate 'a.txt': the source and the destination are the same file; \
         nothing to do"
    )));
    assert_eq!(trashed_ids(&source), Vec::<String>::new());
    assert_eq!(dest.created().len(), 0);
}

#[tokio::test]
async fn into_must_not_be_a_folder_that_is_migrated() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(source_tree(), Behavior::default()).await;

    for into in ["photos", "jan"] {
        let migrated = migrate(
            &source,
            &dest,
            Settings {
                into: Some(into.to_owned()),
                trash_source: true,
                ..settings(&["photos"])
            },
        )
        .await;
        assert_eq!(
            migrated.error(),
            format!(
                "Error: the destination folder '{into}' is one of the folders to migrate, or \
                 inside one; --into must name a folder outside them"
            )
        );
    }
    assert_eq!(trashed_ids(&source), Vec::<String>::new());
    assert_eq!(dest.created().len(), 0);
}

#[tokio::test]
async fn a_folder_that_got_a_new_file_during_the_run_stays_in_the_source() {
    let fake = FakeDrive::start(source_tree(), Behavior::default()).await;
    // The check right before `jan` goes to the trash finds a file that the walk did not.
    let source = Interceptor::start(&fake, |method, path, params| {
        let leftover = *method == hyper::Method::GET
            && path == "/drive/v3/files"
            && params.get("fields").map(String::as_str) == Some(super::LEFTOVER_FIELDS)
            && params
                .get("q")
                .is_some_and(|q| q.contains("'jan' in parents"));
        leftover.then(|| json!({"files": [{"id": "late"}]}))
    })
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate_through(
        &source,
        &dest,
        Settings {
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert_eq!(created_tree(&dest), migrated_tree());
    assert!(migrated.out.lines().contains(&String::from(
        "keep photos/2024/jan/: it has files that were not migrated"
    )));
    // `jan` stays, and so do the folders around it.
    assert_eq!(trashed_ids(&fake), ["a", "b", "d", "empty", "f"]);
    assert!(
        migrated
            .summary()
            .ends_with("; 5 moved to the trash in the source, 1 kept there"),
        "{}",
        migrated.summary()
    );
}

#[tokio::test]
async fn a_source_file_that_changed_during_the_run_stays_in_the_source() {
    let fake = FakeDrive::start(source_tree(), Behavior::default()).await;
    // The check right before `f.txt` goes to the trash finds other content.
    let source = Interceptor::start(&fake, |method, path, params| {
        let recheck = *method == hyper::Method::GET
            && path == "/drive/v3/files/f"
            && params.get("fields").map(String::as_str) == Some(super::RECHECK_FIELDS);
        recheck.then(|| json!({"id": "f", "size": "7", "md5Checksum": "0123456789abcdef"}))
    })
    .await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate_through(
        &source,
        &dest,
        Settings {
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert!(migrated.out.lines().contains(&String::from(
        "keep photos/2024/jan/f.txt: it changed in the source during the migration"
    )));
    assert_eq!(trashed_ids(&fake), ["a", "b", "d", "empty"]);
}

#[tokio::test]
async fn an_incomplete_destination_listing_fails_that_folder() {
    let mut files = source_tree();
    files.push(FakeFile::blob("notes", "notes.txt", "root", "n"));
    let source = FakeDrive::start(files.clone(), Behavior::default()).await;
    let dest = FakeDrive::start(
        vec![FakeFile::folder("old-photos", "photos", "root")],
        Behavior {
            incomplete: HashSet::from([String::from("old-photos")]),
            ..Behavior::default()
        },
    )
    .await;

    let migrated = migrate(&source, &dest, continuing(settings(&["notes", "photos"]))).await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 folder failed, 7 cancelled"
    );
    let err = migrated.err.text();
    assert!(
        err.contains(
            "Error: unable to migrate 'photos': Drive did not list the destination folder \
             completely"
        ),
        "{err}"
    );
    let created = dest.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].name, "notes.txt");

    // The top folder of the destination: nothing can be compared, so nothing starts.
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            incomplete: HashSet::from([String::from("root")]),
            ..Behavior::default()
        },
    )
    .await;
    let migrated = migrate(&source, &dest, continuing(settings(&["notes", "photos"]))).await;
    assert!(
        migrated
            .error()
            .starts_with("Error: Drive did not list the destination folder completely"),
        "{}",
        migrated.error()
    );
    assert_eq!(dest.created().len(), 0);
}

#[tokio::test]
async fn control_characters_in_names_are_escaped_in_the_output() {
    let mut files = source_tree();
    files.push(FakeFile::blob("nl", "bad\nname.txt", "photos", "x"));
    files.push(FakeFile::folder("esc", "esc\u{1b}[2J", "photos"));
    files.push(FakeFile::blob("cr", "c\rr.txt", "esc", "y"));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let before = vec![
        FakeFile::folder("old-photos", "photos", "root"),
        FakeFile::blob("old-nl", "bad\nname.txt", "old-photos", "not x"),
    ];
    let dest = FakeDrive::start(before, Behavior::default()).await;

    let migrated = migrate(&source, &dest, continuing(settings(&["photos"]))).await;

    assert_eq!(
        migrated.error(),
        "Error: the migration is incomplete: 1 file failed"
    );
    let out = migrated.sorted_out();
    assert!(
        out.contains(&String::from("create photos/esc\\u{1b}[2J/")),
        "{out:?}"
    );
    assert!(
        out.contains(&String::from("create photos/esc\\u{1b}[2J/c\\rr.txt (1 B)")),
        "{out:?}"
    );
    assert!(migrated.err.lines().contains(&String::from(
        "Error: unable to migrate 'photos/bad\\nname.txt': it exists in the destination with \
         different content"
    )));
    for text in [migrated.out.text(), migrated.err.text()] {
        assert!(!text.contains('\u{1b}') && !text.contains('\r'), "{text:?}");
        assert!(
            !text.lines().any(|line| line.starts_with("name.txt")),
            "{text}"
        );
    }
    // Drive gets the names as they are.
    let created = dest.created();
    assert!(created.iter().any(|file| file.name == "esc\u{1b}[2J"));
    assert!(created.iter().any(|file| file.name == "c\rr.txt"));
}

/// `D/x(A)/x(B)/x(C)`, `C` empty, seen by both accounts.
fn nested_same_names() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("D", "D", "root"),
        FakeFile::folder("A", "x", "D"),
        FakeFile::folder("B", "x", "A"),
        FakeFile::folder("C", "x", "B"),
    ]
}

#[tokio::test]
async fn a_destination_folder_inside_the_source_is_never_reused() {
    let source = FakeDrive::start(nested_same_names(), Behavior::default()).await;
    let dest = FakeDrive::start(nested_same_names(), Behavior::default()).await;

    // `B` goes into `D` and reuses `A`, which is outside the source; `C` would then reuse `B`,
    // which is the source itself, and trashing `C` and `B` would remove what was reused.
    let migrated = migrate(
        &source,
        &dest,
        Settings {
            into: Some(String::from("D")),
            trash_source: true,
            ..settings(&["B"])
        },
    )
    .await;

    assert!(
        migrated
            .error()
            .starts_with("Error: the migration is incomplete: 1 folder failed"),
        "{}",
        migrated.error()
    );
    assert!(
        migrated.err.lines().contains(&String::from(
            "Error: unable to migrate 'x/x': the folder of this name in the destination is one \
             of the source items that are migrated; nothing to do"
        )),
        "{}",
        migrated.err.text()
    );
    assert!(migrated.out.lines().contains(&String::from("reuse x/")));
    assert_eq!(trashed_ids(&source), Vec::<String>::new());
    assert_eq!(dest.created().len(), 0);
}

#[tokio::test]
async fn a_destination_file_that_is_a_source_file_is_never_matched() {
    // `k` has two parents: `A` (outside the source) and `S` (inside it). `h` would match `k` as
    // already there, and trashing `k` with the rest would remove what `h` matched.
    let files = vec![
        FakeFile::folder("D", "D", "root"),
        FakeFile::folder("A", "p", "D"),
        FakeFile::folder("B", "p", "root"),
        FakeFile::blob("h", "n.txt", "B", "same"),
        FakeFile::folder("S", "s", "B"),
        FakeFile::blob("k", "n.txt", "S", "same").with_parents(&["S", "A"]),
    ];
    let source = FakeDrive::start(files.clone(), Behavior::default()).await;
    let dest = FakeDrive::start(files, Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        continuing(Settings {
            into: Some(String::from("D")),
            trash_source: true,
            ..settings(&["B"])
        }),
    )
    .await;

    assert!(
        migrated.err.lines().contains(&String::from(
            "Error: unable to migrate 'p/n.txt': the file of this name in the destination is one \
             of the source items that are migrated; nothing to do"
        )),
        "{}",
        migrated.err.text()
    );
    assert!(
        migrated
            .error()
            .starts_with("Error: the migration is incomplete"),
        "{}",
        migrated.error()
    );
    assert!(!trashed_ids(&source).contains(&String::from("h")));
    assert!(!trashed_ids(&source).contains(&String::from("B")));
}

#[tokio::test]
async fn the_relay_gets_the_resource_key_and_creation_time_of_each_source_file() {
    use super::source::{discover, SourceNode};

    const CREATED: &str = "2020-01-02T03:04:05.000Z";
    let md5 = |content: &str| format!("{:x}", md5::compute(content));
    let fake = FakeDrive::start(
        vec![
            FakeFile::folder("shared", "shared", "root").with_resource_key("rk-folder"),
            FakeFile::blob("a", "a.txt", "shared", "aaaa").with_resource_key("rk-a"),
            FakeFile::blob("b", "b.txt", "root", "bb").with_resource_key("rk-b"),
        ],
        Behavior::default(),
    )
    .await;
    // The fake keeps no `createdTime`: the listing of `shared` and the `files.get` of `b` that ask
    // for it get it here. A mask without it would go to the fake and find none.
    let source = Interceptor::start(&fake, move |method, path, params| {
        let asks = *method == hyper::Method::GET
            && params
                .get("fields")
                .is_some_and(|fields| fields.contains("createdTime"));
        let listing = path == "/drive/v3/files"
            && params
                .get("q")
                .is_some_and(|q| q.contains("'shared' in parents"));
        match path {
            _ if asks && listing => Some(json!({"files": [{
                "id": "a", "name": "a.txt", "mimeType": "text/plain", "size": "4",
                "md5Checksum": md5("aaaa"), "parents": ["shared"], "resourceKey": "rk-a",
                "createdTime": CREATED,
            }]})),
            "/drive/v3/files/b" if asks => Some(json!({
                "id": "b", "name": "b.txt", "mimeType": "text/plain", "size": "2",
                "md5Checksum": md5("bb"), "resourceKey": "rk-b", "createdTime": CREATED,
            })),
            _ => None,
        }
    })
    .await;
    let created = Some(CREATED.parse::<chrono::DateTime<chrono::Utc>>().unwrap());

    let output = output();
    let hub = source.hub();
    let Ok(SourceNode::Folder(folder)) = discover(&hub, "shared", 2, &output.printer).await else {
        panic!("shared is a folder");
    };
    let [SourceNode::File(a)] = folder.children.as_slice() else {
        panic!("{:?}", folder.children);
    };
    let a = a.file.to_drive_file();
    assert_eq!(a.resource_key.as_deref(), Some("rk-a"));
    assert_eq!(a.created_time, created);
    let Ok(SourceNode::File(b)) = discover(&hub, "b", 2, &output.printer).await else {
        panic!("b is a file");
    };
    let b = b.file.to_drive_file();
    assert_eq!(b.resource_key.as_deref(), Some("rk-b"));
    assert_eq!(b.created_time, created);

    // And the migration of both goes through.
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let migrated = migrate_through(&source, &dest, settings(&["shared", "b"])).await;
    migrated.assert_ok();
    let mut names = dest
        .created()
        .into_iter()
        .map(|file| file.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt", "shared"]);
}

/// What [`source_tree`] becomes in the folder `Archive` with `--contents`: no `photos`.
fn contents_tree() -> BTreeMap<String, String> {
    migrated_tree()
        .into_iter()
        .filter_map(|(path, content)| {
            let inside = path.strip_prefix("photos/")?;
            (!inside.is_empty()).then(|| (format!("Archive/{inside}"), content))
        })
        .collect()
}

#[tokio::test]
async fn with_contents_what_is_in_a_folder_goes_straight_into_the_destination() {
    let mut files = source_tree();
    files.push(FakeFile::blob("notes", "notes.txt", "root", "n"));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let target = FakeFile::folder("target", "Archive", "root");
    let dest = FakeDrive::start(vec![target.clone()], Behavior::default()).await;
    // A file given by its id is migrated as without the flag.
    let contents = || Settings {
        into: Some(String::from("target")),
        contents: true,
        ..settings(&["notes", "photos"])
    };

    let migrated = migrate(&source, &dest, contents()).await;

    migrated.assert_ok();
    let mut expected = contents_tree();
    expected.insert(String::from("Archive/notes.txt"), String::from("n"));
    let created = dest.created();
    assert_eq!(
        tree_below(&created, std::slice::from_ref(&target), &["root"]),
        expected
    );
    assert!(created.iter().all(|file| file.name != "photos"));
    // The lines name the items as they are placed in the destination folder.
    let mut lines = vec![
        "Found 5 files in 4 folders with a total size of 17 B",
        "create 2024/",
        "create 2024/jan/",
        "create empty/",
        "create a.txt (4 B)",
        "create b.txt (2 B)",
        "create 2024/d.txt (4 B)",
        "create 2024/jan/f.txt (6 B)",
        "create notes.txt (1 B)",
    ];
    lines.sort_unstable();
    assert_eq!(migrated.sorted_out(), lines);
    assert_eq!(
        migrated.summary(),
        "Summary: 5 migrated (17 B), 0 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled; 3 folders created, 0 reused"
    );
    assert_eq!(source.writes(), Vec::<String>::new());

    let again = migrate(&source, &dest, contents()).await;

    again.assert_ok();
    assert_eq!(dest.created().len(), created.len(), "nothing twice");
    assert_eq!(
        again.summary(),
        "Summary: 0 migrated (0 B), 5 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled; 0 folders created, 3 reused"
    );
    let lines = again.sorted_out();
    for line in ["reuse 2024/jan/", "already-there 2024/jan/f.txt"] {
        assert!(lines.contains(&String::from(line)), "{lines:?}");
    }
}

#[tokio::test]
async fn with_contents_a_dry_run_shows_the_same_and_writes_nothing() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            contents: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    let lines = migrated.sorted_out();
    for line in ["create 2024/", "create a.txt (4 B)"] {
        assert!(lines.contains(&String::from(line)), "{lines:?}");
    }
    assert!(
        lines.iter().all(|line| !line.contains("photos")),
        "{lines:?}"
    );
    assert_eq!(dest.writes(), Vec::<String>::new());
    assert_eq!(source.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn with_contents_the_given_folder_stays_in_the_source_when_its_items_go_to_the_trash() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let target = FakeFile::folder("target", "Archive", "root");
    let dest = FakeDrive::start(vec![target.clone()], Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            into: Some(String::from("target")),
            trash_source: true,
            contents: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert_eq!(
        tree_below(&dest.created(), &[target], &["root"]),
        contents_tree()
    );
    // Everything in `photos`, and not `photos`.
    assert_eq!(
        trashed_ids(&source),
        ["a", "b", "d", "empty", "f", "jan", "y2024"]
    );
    assert!(
        migrated
            .summary()
            .ends_with("; 7 moved to the trash in the source"),
        "{}",
        migrated.summary()
    );
}

#[tokio::test]
async fn with_contents_a_folder_that_could_not_be_listed_fails_and_nothing_is_created() {
    let behavior = Behavior {
        unreadable: HashSet::from([String::from("photos")]),
        ..Behavior::default()
    };
    let source = FakeDrive::start(source_tree(), behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            contents: true,
            ..settings(&["photos"])
        },
    )
    .await;

    assert!(migrated.result.is_err());
    assert!(
        migrated
            .err
            .text()
            .contains("Error: unable to migrate 'photos'"),
        "{}",
        migrated.err.text()
    );
    assert!(dest.created().is_empty());
}

/// [`migrate`] between any two hubs, with `options`.
async fn migrate_hubs(
    source: &crate::hub::Hub,
    dest: &crate::hub::Hub,
    settings: Settings,
    options: &UploadOptions,
) -> Migrated {
    let output = output();
    let run = run(
        source,
        dest,
        &settings,
        options,
        &output.printer,
        ProgressDisplay::reporting_to(Arc::new(NoProgress)),
        future::pending(),
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

/// `photos` in the destination with other content than [`source_tree`] under three names.
fn smaller_and_larger_destination() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("old-photos", "photos", "root"),
        // Smaller than the source's `aaaa`.
        FakeFile::blob("old-a", "a.txt", "old-photos", "a"),
        // Larger than the source's `bb`.
        FakeFile::blob("old-b", "b.txt", "old-photos", "bbb"),
        FakeFile::folder("old-2024", "2024", "old-photos"),
        // As large as the source's `dddd`.
        FakeFile::blob("old-d", "d.txt", "old-2024", "DDDD"),
    ]
}

fn replacing(file_ids: &[&str]) -> Settings {
    continuing(Settings {
        replace_smaller: true,
        ..settings(file_ids)
    })
}

fn conflict(label: &str) -> String {
    format!(
        "Error: unable to migrate '{label}': it exists in the destination with different content"
    )
}

#[tokio::test]
async fn replace_smaller_replaces_a_smaller_file_of_the_name_and_no_other() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, replacing(&["photos"])).await;

    // The larger file and the one of the same size are conflicts, as without the flag.
    assert!(migrated.result.is_err());
    let errors = migrated.err.text();
    for label in ["photos/b.txt", "photos/2024/d.txt"] {
        assert!(errors.contains(&conflict(label)), "{errors}");
    }
    assert!(!errors.contains(&conflict("photos/a.txt")), "{errors}");
    // The smaller one has the source's content now: the same file, where it was.
    assert_eq!(dest.updates(), [(String::from("old-a"), b"aaaa".to_vec())]);
    let replaced = dest.stored("old-a").unwrap();
    assert_eq!(replaced.content.as_deref(), Some("aaaa"));
    assert_eq!(replaced.parents, ["old-photos"]);
    assert!(
        dest.gets().contains(&String::from("old-a")),
        "checked first"
    );
    let created = dest.created();
    let mut names = created
        .iter()
        .map(|file| file.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["empty", "f.txt", "jan"]);
    assert!(migrated
        .out
        .lines()
        .contains(&String::from("replace photos/a.txt (4 B, was 1 B)")));
    assert_eq!(
        migrated.summary(),
        "Summary: 2 migrated (10 B), 0 already there, 0 skipped (unsupported), 2 failed, \
         0 cancelled; 2 folders created, 2 reused; 1 smaller file replaced"
    );
    assert_eq!(source.writes(), Vec::<String>::new());

    let again = migrate(&source, &dest, replacing(&["photos"])).await;

    assert!(again
        .out
        .lines()
        .contains(&String::from("already-there photos/a.txt")));
    assert_eq!(dest.updates().len(), 1, "nothing is replaced twice");
    assert!(
        again.summary().ends_with("; 0 smaller files replaced"),
        "{}",
        again.summary()
    );
}

#[tokio::test]
async fn without_replace_smaller_a_smaller_file_is_a_conflict_and_a_dry_run_replaces_nothing() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, continuing(settings(&["photos"]))).await;

    assert!(migrated.err.text().contains(&conflict("photos/a.txt")));
    assert!(!migrated.summary().contains("replace"));
    assert_eq!(dest.updates(), []);

    let dest = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;
    let dry = migrate(
        &source,
        &dest,
        Settings {
            dry_run: true,
            ..replacing(&["photos"])
        },
    )
    .await;

    assert!(dry
        .out
        .lines()
        .contains(&String::from("replace photos/a.txt (4 B, was 1 B)")));
    assert!(
        dry.summary().ends_with("; 1 smaller file to replace"),
        "{}",
        dry.summary()
    );
    assert_eq!(dest.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn replace_smaller_picks_no_file_among_several_and_no_google_file() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(
        vec![
            FakeFile::folder("old-photos", "photos", "root"),
            // Two of the name, both smaller.
            FakeFile::blob("old-a1", "a.txt", "old-photos", "a"),
            FakeFile::blob("old-a2", "a.txt", "old-photos", "aa"),
            // A Google Docs file of the name.
            FakeFile::document("old-b", "b.txt", "old-photos"),
        ],
        Behavior::default(),
    )
    .await;

    let migrated = migrate(&source, &dest, replacing(&["photos"])).await;

    let errors = migrated.err.text();
    for label in ["photos/a.txt", "photos/b.txt"] {
        assert!(errors.contains(&conflict(label)), "{errors}");
    }
    assert_eq!(dest.updates(), []);
    assert!(dest
        .created()
        .iter()
        .all(|file| file.name != "a.txt" && file.name != "b.txt"));
}

#[tokio::test]
async fn two_source_files_of_one_name_replace_a_smaller_file_once() {
    let source = FakeDrive::start(
        vec![
            FakeFile::folder("photos", "photos", "root"),
            FakeFile::blob("s1", "same.txt", "photos", "first1"),
            FakeFile::blob("s2", "same.txt", "photos", "second22"),
        ],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(
        vec![
            FakeFile::folder("old-photos", "photos", "root"),
            FakeFile::blob("old", "same.txt", "old-photos", "x"),
        ],
        Behavior::default(),
    )
    .await;

    let migrated = migrate(&source, &dest, replacing(&["photos"])).await;

    assert!(migrated.result.is_err());
    let updates = dest.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0].0, "old");
    assert!(migrated.err.text().contains(&conflict("photos/same.txt")));
    assert!(dest.created().is_empty());
    assert!(
        migrated.summary().contains("1 migrated") && migrated.summary().contains("1 failed"),
        "{}",
        migrated.summary()
    );
}

/// Whether a request lists the destination's files named `name` again.
fn lists_again(
    method: &hyper::Method,
    path: &str,
    params: &HashMap<String, String>,
    name: &str,
) -> bool {
    *method == hyper::Method::GET
        && path == "/drive/v3/files"
        && params
            .get("q")
            .is_some_and(|q| q.contains(&format!("name = '{name}'")))
}

#[tokio::test]
async fn a_destination_file_that_is_not_what_was_listed_is_not_replaced() {
    let listed = |id: &str, content: &str| {
        json!({
            "id": id,
            "name": "a.txt",
            "mimeType": "text/plain",
            "size": content.len().to_string(),
            "md5Checksum": format!("{:x}", md5::compute(content)),
        })
    };
    for now in [
        // Other content than the listing had.
        json!([listed("old-a", "zz")]),
        // A second file of its name.
        json!([listed("old-a", "a"), listed("other", "a")]),
        // Renamed, moved or in the trash: not in the folder under its name.
        json!([]),
    ] {
        let source = FakeDrive::start(source_tree(), Behavior::default()).await;
        let fake = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;
        // The listing right before `a.txt` is replaced.
        let dest = Interceptor::start(&fake, move |method, path, params| {
            lists_again(method, path, params, "a.txt")
                .then(|| json!({"kind": "drive#fileList", "files": now.clone()}))
        })
        .await;

        let migrated = migrate_hubs(
            &source.hub(),
            &dest.hub(),
            replacing(&["photos"]),
            &UploadOptions::default(),
        )
        .await;

        assert!(
            migrated.err.text().contains(
                "Error: unable to migrate 'photos/a.txt': the file of this name in the \
                 destination changed during the migration; it was not replaced"
            ),
            "{}",
            migrated.err.text()
        );
        assert_eq!(fake.updates(), []);
        assert_eq!(fake.stored("old-a").unwrap().content.as_deref(), Some("a"));
    }
}

#[tokio::test]
async fn a_replace_that_the_destination_does_not_hold_fails_and_keeps_the_source() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let fake = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;
    // The file as it is once the upload returned: other content than was sent, as after a
    // session that an earlier run completed and a change since.
    let dest = Interceptor::start(&fake, |method, path, params| {
        let confirm = *method == hyper::Method::GET
            && path == "/drive/v3/files/old-a"
            && params.get("fields").map(String::as_str) == Some(super::REPLACED_FIELDS);
        confirm.then(|| json!({"id": "old-a", "size": "4", "md5Checksum": "0123456789abcdef"}))
    })
    .await;

    let migrated = migrate_hubs(
        &source.hub(),
        &dest.hub(),
        Settings {
            trash_source: true,
            ..replacing(&["photos"])
        },
        &UploadOptions::default(),
    )
    .await;

    assert!(
        migrated.err.text().contains(
            "Error: unable to migrate 'photos/a.txt': the file in the destination does not hold \
             the content that was sent to replace it; run the command again"
        ),
        "{}",
        migrated.err.text()
    );
    assert!(!trashed_ids(&source).contains(&String::from("a")));
}

#[tokio::test]
async fn a_file_that_one_item_found_its_content_in_is_not_replaced_by_another() {
    // Two source files of one name: one is in the destination already, the other is larger.
    let source = FakeDrive::start(
        vec![
            FakeFile::folder("photos", "photos", "root"),
            FakeFile::blob("s1", "x.txt", "photos", "bbbb"),
            FakeFile::blob("s2", "x.txt", "photos", "a"),
        ],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(
        vec![
            FakeFile::folder("old-photos", "photos", "root"),
            FakeFile::blob("old", "x.txt", "old-photos", "a"),
        ],
        Behavior::default(),
    )
    .await;

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            trash_source: true,
            ..replacing(&["photos"])
        },
    )
    .await;

    // Whichever came first: the file is what `s2` is migrated to, or it has the content of
    // `s1`. Never both, which would leave the content of `s2` nowhere but in the trash.
    assert!(migrated.result.is_err());
    assert!(migrated.err.text().contains(&conflict("photos/x.txt")));
    let trashed = trashed_ids(&source);
    if dest.updates().is_empty() {
        assert_eq!(trashed, ["s2"]);
        assert_eq!(dest.stored("old").unwrap().content.as_deref(), Some("a"));
    } else {
        assert_eq!(trashed, ["s1"]);
        assert_eq!(dest.stored("old").unwrap().content.as_deref(), Some("bbbb"));
    }
}

#[tokio::test]
async fn a_requested_file_that_could_not_be_read_is_never_replaced() {
    // Both accounts see `shared`, which the source cannot read now; `b` has its name.
    let source = FakeDrive::start(
        vec![FakeFile::blob("b", "x.txt", "root", "bbbb")],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(
        vec![FakeFile::blob("shared", "x.txt", "root", "a")],
        Behavior::default(),
    )
    .await;

    let migrated = migrate(&source, &dest, replacing(&["shared", "b"])).await;

    assert!(migrated.result.is_err());
    assert_eq!(dest.updates(), []);
    assert!(
        migrated.err.text().contains(
            "Error: unable to migrate 'x.txt': the file of this name in the destination is one \
             of the source items that are migrated; nothing to do"
        ),
        "{}",
        migrated.err.text()
    );
}

#[tokio::test]
async fn replace_smaller_sends_a_large_file_through_a_session_and_trashes_its_source() {
    let content = "0123456789".repeat(60_000);
    let source = FakeDrive::start(
        vec![
            FakeFile::folder("photos", "photos", "root"),
            FakeFile::blob("big", "big.bin", "photos", &content),
        ],
        Behavior::default(),
    )
    .await;
    let dest = FakeDrive::start(
        vec![
            FakeFile::folder("old-photos", "photos", "root"),
            FakeFile::blob("old-big", "big.bin", "old-photos", "0123456789"),
        ],
        Behavior::default(),
    )
    .await;
    // Everything through a resumable session, two chunks.
    let options = UploadOptions {
        chunk_size: 256 * 1024,
        multipart_threshold: 0,
        ..UploadOptions::default()
    };

    let migrated = migrate_hubs(
        &source.hub(),
        &dest.hub(),
        Settings {
            replace_smaller: true,
            trash_source: true,
            ..settings(&["photos"])
        },
        &options,
    )
    .await;

    migrated.assert_ok();
    assert_eq!(
        dest.updates(),
        [(String::from("old-big"), content.clone().into_bytes())]
    );
    assert!(dest.created().is_empty());
    let sent = dest
        .chunks()
        .iter()
        .filter(|chunk| chunk.first.is_some())
        .count();
    assert!(sent >= 2, "{:?}", dest.chunks());
    // Replaced and checked by md5: the source file went, and its folder after it.
    assert_eq!(trashed_ids(&source), ["big", "photos"]);
}

/// Answers the questions of `--ask` with `answer`, and keeps them.
struct Scripted<F> {
    answer: F,
    questions: std::sync::Mutex<Vec<String>>,
}

impl<F> std::fmt::Debug for Scripted<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scripted")
    }
}

impl<F: Fn(&str) -> bool + Send + Sync> super::ask::Ask for Scripted<F> {
    fn ask(&self, question: String) -> futures::future::BoxFuture<'_, bool> {
        let answer = (self.answer)(&question);
        self.questions.lock().unwrap().push(question);
        Box::pin(async move { answer })
    }
}

fn scripted<F: Fn(&str) -> bool + Send + Sync + 'static>(answer: F) -> Arc<Scripted<F>> {
    Arc::new(Scripted {
        answer,
        questions: std::sync::Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn ask_puts_each_conflict_to_the_user_and_a_no_is_no_failure() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;
    // Yes to `b.txt`, whose file in the destination is the larger one.
    let user = scripted(|question| question.contains("photos/b.txt"));

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            ask: Some(user.clone()),
            ..settings(&["photos"])
        },
    )
    .await;

    // Nothing failed: the two that got a no stay as they are.
    migrated.assert_ok();
    let mut questions = user.questions.lock().unwrap().clone();
    questions.sort();
    assert_eq!(
        questions,
        [
            "\nphotos/2024/d.txt is in the destination with other content:\n  source:      4 B\n  \
             \x20            https://drive.google.com/file/d/d/view\n  destination: 4 B\n  \
             \x20            https://drive.google.com/file/d/old-d/view\n\
             Replace the destination's file? [y/N] ",
            "\nphotos/a.txt is in the destination with other content:\n  source:      4 B\n  \
             \x20            https://drive.google.com/file/d/a/view\n  destination: 1 B\n  \
             \x20            https://drive.google.com/file/d/old-a/view\n\
             Replace the destination's file? [y/N] ",
            "\nphotos/b.txt is in the destination with other content:\n  source:      2 B\n  \
             \x20            https://drive.google.com/file/d/b/view\n  destination: 3 B\n  \
             \x20            https://drive.google.com/file/d/old-b/view\n\
             Replace the destination's file? [y/N] ",
        ]
    );
    assert_eq!(dest.updates(), [(String::from("old-b"), b"bb".to_vec())]);
    let lines = migrated.out.lines();
    for line in [
        "replace photos/b.txt (2 B, was 3 B)",
        "keep-destination photos/a.txt",
        "keep-destination photos/2024/d.txt",
    ] {
        assert!(lines.contains(&String::from(line)), "{lines:?}");
    }
    assert_eq!(
        migrated.summary(),
        "Summary: 2 migrated (8 B), 0 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled; 2 folders created, 2 reused; 1 file replaced, 2 conflicts left alone"
    );
    assert_eq!(source.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn ask_with_replace_smaller_asks_only_about_what_is_not_smaller() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;
    let user = scripted(|_| false);

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            ask: Some(user.clone()),
            replace_smaller: true,
            trash_source: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert_eq!(dest.updates(), [(String::from("old-a"), b"aaaa".to_vec())]);
    let questions = user.questions.lock().unwrap().clone();
    assert_eq!(questions.len(), 2, "{questions:?}");
    assert!(questions.iter().all(|question| !question.contains("a.txt")));
    assert!(
        migrated
            .summary()
            .contains("; 1 file replaced, 2 conflicts left alone"),
        "{}",
        migrated.summary()
    );
    // What was left as it is stays in the source, and so do its folders.
    let trashed = trashed_ids(&source);
    for kept in ["b", "d", "y2024", "photos"] {
        assert!(!trashed.contains(&String::from(kept)), "{trashed:?}");
    }
    assert!(trashed.contains(&String::from("a")), "{trashed:?}");
}

#[tokio::test]
async fn ask_in_a_dry_run_asks_nothing_and_tells_what_it_would_ask_about() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(smaller_and_larger_destination(), Behavior::default()).await;
    let user = scripted(|_| true);

    let migrated = migrate(
        &source,
        &dest,
        Settings {
            ask: Some(user.clone()),
            dry_run: true,
            ..settings(&["photos"])
        },
    )
    .await;

    migrated.assert_ok();
    assert!(user.questions.lock().unwrap().is_empty());
    let lines = migrated.out.lines();
    for line in [
        "ask photos/a.txt (4 B, the destination's has 1 B)",
        "ask photos/b.txt (2 B, the destination's has 3 B)",
        "ask photos/2024/d.txt (4 B, the destination's has 4 B)",
    ] {
        assert!(lines.contains(&String::from(line)), "{lines:?}");
    }
    assert!(
        migrated
            .summary()
            .ends_with("; 0 files to replace, 3 conflicts to ask about"),
        "{}",
        migrated.summary()
    );
    assert_eq!(dest.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn ask_does_not_ask_about_what_no_single_file_can_settle() {
    let source = FakeDrive::start(source_tree(), Behavior::default()).await;
    let dest = FakeDrive::start(
        vec![
            FakeFile::folder("old-photos", "photos", "root"),
            FakeFile::blob("old-a1", "a.txt", "old-photos", "a"),
            FakeFile::blob("old-a2", "a.txt", "old-photos", "aa"),
        ],
        Behavior::default(),
    )
    .await;
    let user = scripted(|_| true);

    let migrated = migrate(
        &source,
        &dest,
        continuing(Settings {
            ask: Some(user.clone()),
            ..settings(&["photos"])
        }),
    )
    .await;

    assert!(migrated.err.text().contains(&conflict("photos/a.txt")));
    assert!(user.questions.lock().unwrap().is_empty());
    assert_eq!(dest.updates(), []);
}

#[tokio::test]
async fn the_my_drive_of_one_account_goes_into_the_my_drive_of_another() {
    // `root` is each account's name for its own top folder, whose id is another one.
    let files = source_tree()
        .into_iter()
        .map(|file| {
            if file.parents == ["root"] {
                file.with_parents(&["source-top"])
            } else {
                file
            }
        })
        .collect();
    let behavior = Behavior {
        my_drive: Some(String::from("source-top")),
        ..Behavior::default()
    };
    let source = FakeDrive::start(files, behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, replacing(&["root"])).await;

    migrated.assert_ok();
    let expected = migrated_tree()
        .into_iter()
        .map(|(path, content)| (format!("My Drive/{path}"), content))
        .chain([(String::from("My Drive/"), String::new())])
        .collect::<BTreeMap<_, _>>();
    assert_eq!(created_tree(&dest), expected);
}

#[tokio::test]
async fn a_file_that_one_item_created_is_not_replaced_by_another() {
    // Two source folders of one name go into the same destination folder. Its listing is
    // answered for one of them at once, and for the other one a second later: by then the
    // first one has created its `x.txt` and trashed its source.
    let source = FakeDrive::start(
        vec![
            FakeFile::folder("p1", "photos", "root"),
            FakeFile::blob("s1", "x.txt", "p1", "first"),
            FakeFile::folder("p2", "photos", "root"),
            FakeFile::blob("s2", "x.txt", "p2", "second one"),
        ],
        Behavior::default(),
    )
    .await;
    let behavior = Behavior {
        unavailable_first: HashMap::from([(String::from("old-photos"), 1)]),
        unavailable_retry_after: Some(1),
        ..Behavior::default()
    };
    let dest = FakeDrive::start(
        vec![FakeFile::folder("old-photos", "photos", "root")],
        behavior,
    )
    .await;
    // Yes to every question: whichever folder comes second would replace the file of the first.
    let user = scripted(|_| true);

    let migrated = migrate(
        &source,
        &dest,
        continuing(Settings {
            ask: Some(user.clone()),
            trash_source: true,
            ..settings(&["p1", "p2"])
        }),
    )
    .await;

    assert_eq!(dest.updates(), [], "{}", migrated.err.text());
    let created = dest.created();
    assert_eq!(created.len(), 1, "{created:?}");
    // The one whose content is in the destination went to the trash, and only that one.
    let content = String::from_utf8_lossy(&created[0].content).into_owned();
    let (migrated_file, other) = if content == "first" {
        ("s1", "s2")
    } else {
        ("s2", "s1")
    };
    let trashed = trashed_ids(&source);
    assert!(
        trashed.contains(&String::from(migrated_file)),
        "{trashed:?}"
    );
    assert!(!trashed.contains(&String::from(other)), "{trashed:?}");
    assert!(migrated.err.text().contains(&conflict("photos/x.txt")));
}
