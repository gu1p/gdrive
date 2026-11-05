//! `files diverged` between two fake Drives: the origin answers `files.get`, the walk and the
//! listings of the trash; the copy answers `files.get` of `--into`, its listings and
//! `revisions.list`. Neither is ever written to.

use std::{num::NonZeroUsize, time::Duration};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::{run, By, Error, Settings};
use crate::{
    files::{
        activity::{when::When, Output},
        info::format_date_time,
    },
    transfer::{
        fake_drive::{Behavior, FakeDrive, FakeFile, FakeUser},
        scheduler::capture::{output, Captured},
    },
};

/// The time the tests run at.
const NOW: &str = "2026-11-05T12:00:00Z";

/// When the migration copied the origin.
const MIGRATION: &str = "2026-11-01T00:00:00Z";

/// Long before the migration.
const BEFORE: &str = "2026-01-01T00:00:00Z";

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&Utc)
}

/// A time as the report prints it: in the local time zone.
fn local(rfc3339: &str) -> String {
    format_date_time(at(rfc3339)).to_string()
}

fn md5(content: &str) -> String {
    format!("{:x}", md5::compute(content))
}

fn owner() -> FakeUser {
    FakeUser::new("Old", "old@example.com")
}

fn ana() -> FakeUser {
    FakeUser::new("Ana", "Ana@Example.com")
}

fn bob() -> FakeUser {
    FakeUser::new("Bob", "bob@example.com")
}

fn carol() -> FakeUser {
    FakeUser::new("Carol", "carol@example.com")
}

/// A blob of `content`, created long ago by the owner and last modified at `modified`.
fn blob(id: &str, name: &str, parent: &str, content: &str, modified: &str) -> FakeFile {
    FakeFile::blob(id, name, parent, content)
        .created(BEFORE)
        .modified(modified)
        .owned_by(owner())
        .modified_by(owner())
}

/// A Google Docs document last modified at `modified`.
fn doc(id: &str, name: &str, parent: &str, modified: &str) -> FakeFile {
    FakeFile::document(id, name, parent)
        .created(BEFORE)
        .modified(modified)
        .owned_by(owner())
        .modified_by(owner())
}

fn folder(id: &str, name: &str, parent: &str) -> FakeFile {
    FakeFile::folder(id, name, parent)
        .created(BEFORE)
        .modified(BEFORE)
        .owned_by(owner())
}

/// The copy of `file` as the migration made it, under `id` in `parent`: the same name, content
/// and times, owned by the new account.
fn copy_of(file: &FakeFile, id: &str, parent: &str) -> FakeFile {
    FakeFile {
        id: id.to_owned(),
        parents: vec![parent.to_owned()],
        owners: vec![FakeUser::new("New", "new@example.com")],
        last_modifying_user: Some(FakeUser::new("New", "new@example.com")),
        ..file.clone()
    }
}

/// A revision of a blob of `content` at `modified`.
fn revision(content: &str, modified: &str) -> Value {
    json!({
        "kind": "drive#revision",
        "id": "1",
        "modifiedTime": modified,
        "md5Checksum": md5(content),
        "size": content.len().to_string(),
    })
}

/// A revision of a Google file at `modified`.
fn doc_revision(modified: &str) -> Value {
    json!({"kind": "drive#revision", "id": "1", "modifiedTime": modified})
}

/// `Projects` in the origin, after the migration: one item of each change.
///
/// ```text
/// Projects/plan.docx     edited by Ana after the migration
/// Projects/same.txt      as copied
/// Projects/Budget        a Google Docs file as copied
/// Projects/notes.txt     changed in the copy later; its revisions are gone
/// Projects/2026/         added by Bob, with new.pdf (uploaded by Bob, edited by Ana) and
///                        b.jpg, which was Projects/a.jpg
/// Projects/old.xlsx      moved to the trash by Ana
/// ```
fn origin_tree() -> Vec<FakeFile> {
    vec![
        folder("P", "Projects", "root"),
        blob("plan", "plan.docx", "P", "plan v2", "2026-11-05T10:12:44Z").modified_by(ana()),
        blob("same", "same.txt", "P", "same", "2026-10-01T00:00:00Z").modified_by(bob()),
        doc("budget", "Budget", "P", "2026-10-02T00:00:00Z"),
        blob(
            "notes",
            "notes.txt",
            "P",
            "notes v2",
            "2026-10-28T08:15:00Z",
        )
        .modified_by(carol()),
        FakeFile::folder("y2026", "2026", "P")
            .created("2026-11-04T08:00:00Z")
            .modified("2026-11-04T08:00:00Z")
            .owned_by(bob())
            .modified_by(bob()),
        blob("new", "new.pdf", "y2026", "pdf", "2026-11-04T10:00:00Z")
            .created("2026-11-04T09:00:02Z")
            .owned_by(bob())
            .modified_by(ana()),
        blob("b", "b.jpg", "y2026", "jpeg", "2026-01-03T00:00:00Z"),
        blob("oldx", "old.xlsx", "P", "x", "2026-01-04T00:00:00Z")
            .trashed_by(Some(ana()), Some("2026-11-03T16:40:10Z")),
    ]
}

/// The copy of `Projects` in the top of the new account's My Drive, as the migration left it,
/// and `notes.txt` changed there.
fn copy_tree() -> Vec<FakeFile> {
    let origin = origin_tree();
    let find = |id: &str| origin.iter().find(|file| file.id == id).unwrap().clone();
    vec![
        copy_of(&find("P"), "P2", "root"),
        copy_of(
            &blob("x", "plan.docx", "P", "plan v1", "2026-10-01T00:00:00Z"),
            "plan2",
            "P2",
        ),
        copy_of(&find("same"), "same2", "P2"),
        copy_of(&find("budget"), "budget2", "P2"),
        copy_of(
            &blob("x", "notes.txt", "P", "notes v3", "2026-11-02T00:00:00Z"),
            "notes2",
            "P2",
        ),
        copy_of(
            &blob("x", "a.jpg", "P", "jpeg", "2026-01-03T00:00:00Z"),
            "a2",
            "P2",
        ),
        copy_of(
            &blob("x", "old.xlsx", "P", "x", "2026-01-04T00:00:00Z"),
            "oldx2",
            "P2",
        ),
    ]
}

fn settings(file_ids: &[&str]) -> Settings {
    Settings {
        file_ids: file_ids.iter().map(|&id| id.to_owned()).collect(),
        into: None,
        since: None,
        by: By::Item,
        exclude: Vec::new(),
        jobs: NonZeroUsize::new(4).unwrap(),
        output: Output::Tsv { skip_header: false },
    }
}

fn since(settings: Settings, time: &str) -> Settings {
    Settings {
        since: Some(When::At(at(time))),
        ..settings
    }
}

struct Ran {
    result: Result<(), Error>,
    out: Captured,
    err: Captured,
}

impl Ran {
    fn assert_ok(&self) {
        assert!(
            self.result.is_ok(),
            "{}\nstdout:\n{}\nstderr:\n{}",
            self.error(),
            self.out.text(),
            self.err.text()
        );
    }

    fn error(&self) -> String {
        match &self.result {
            Ok(()) => panic!("the command should fail; stderr: {}", self.err.text()),
            Err(error) => crate::error_message(error),
        }
    }

    /// The rows of a TSV report, without its header.
    fn rows(&self) -> Vec<Vec<String>> {
        self.out
            .lines()
            .into_iter()
            .skip(1)
            .map(|line| line.split('\t').map(String::from).collect())
            .collect()
    }

    fn summary(&self) -> String {
        self.err.lines().last().cloned().unwrap_or_default()
    }

    /// stdout, stderr and the error, if any, hold no token.
    fn assert_no_secrets(&self) {
        let error = self
            .result
            .as_ref()
            .err()
            .map(|error| crate::error_message(error))
            .unwrap_or_default();
        for text in [self.out.text(), self.err.text(), error] {
            for secret in ["mock-token", "Bearer"] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }
}

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|&cell| cell.to_owned()).collect()
}

/// A `--by item` row.
fn change(kind: &str, when: Option<&str>, who: &str, path: &str) -> Vec<String> {
    let when = when.map_or_else(|| String::from("-"), local);
    row(&[kind, &when, who, path])
}

async fn diverged(origin: &FakeDrive, copy: Option<&FakeDrive>, settings: Settings) -> Ran {
    let output = output();
    let origin_hub = origin.hub();
    let copy_hub = copy.map(FakeDrive::hub);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run(
            &origin_hub,
            copy_hub.as_ref(),
            &settings,
            at(NOW),
            &output.printer,
        ),
    )
    .await
    .expect("the report should end");
    Ran {
        result,
        out: output.out,
        err: output.err,
    }
}

/// Nothing was changed in any of `drives`.
fn assert_read_only(drives: &[&FakeDrive]) {
    for drive in drives {
        assert_eq!(drive.writes(), Vec::<String>::new());
        assert!(drive.patches().is_empty() && drive.created().is_empty());
    }
}

async fn start(files: Vec<FakeFile>) -> FakeDrive {
    FakeDrive::start(files, Behavior::default()).await
}

#[tokio::test]
async fn each_change_of_the_origin_after_the_migration_is_a_row_and_what_is_as_copied_is_not() {
    // Every listing comes in pages of two.
    let paged = Behavior {
        page_size: Some(2),
        ..Behavior::default()
    };
    let origin = FakeDrive::start(origin_tree(), paged.clone()).await;
    let copy = FakeDrive::start(copy_tree(), paged).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(ran.out.lines()[0], "Change\tWhen\tWho\tPath");
    assert_eq!(
        ran.rows(),
        [
            change(
                "edited",
                Some("2026-11-05T10:12:44Z"),
                "ana@example.com",
                "Projects/plan.docx"
            ),
            // Bob uploaded it, Ana edited it: the owner put it there.
            change(
                "added",
                Some("2026-11-04T09:00:02Z"),
                "bob@example.com",
                "Projects/2026/new.pdf"
            ),
            change(
                "added",
                Some("2026-11-04T08:00:00Z"),
                "bob@example.com",
                "Projects/2026"
            ),
            change(
                "trashed",
                Some("2026-11-03T16:40:10Z"),
                "ana@example.com",
                "Projects/old.xlsx"
            ),
            change(
                "differs",
                Some("2026-10-28T08:15:00Z"),
                "carol@example.com",
                "Projects/notes.txt"
            ),
            change(
                "moved",
                None,
                "-",
                "Projects/2026/b.jpg (was Projects/a.jpg)"
            ),
        ]
    );
    assert_eq!(
        ran.err.lines(),
        [
            "Found 6 files in 2 folders in the origin, and 1 item moved from them to the trash",
            "Found 6 files in 1 folder in the copy",
            "Compared by name with the copy in the top of My Drive: files by size and MD5 \
             checksum, Google Docs, Sheets and Slides by modification time",
            "Read the revisions of 1 file of the copy, newer than the origin's file of the same \
             name; they do not tell whether 1 file of the origin changed after the migration \
             (differs)",
            "Summary: 6 items diverged in the origin (2 added, 1 edited, 1 trashed, 1 differs, \
             1 moved) by 3 people; 1 item without a known person",
        ]
    );
    // Only the file that changed on both sides has its revisions read.
    let revisions = copy.revision_requests();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].file_id, "notes2");
    assert_eq!(
        revisions[0].param("fields"),
        Some("revisions(id,modifiedTime,md5Checksum,size)")
    );
    assert_asks_for_the_users_and_the_trash(&origin);
    assert_read_only(&[&origin, &copy]);
    ran.assert_no_secrets();
}

/// The walk of the origin asks who modified and owns each item, and its folders are listed for
/// the trash with one query.
fn assert_asks_for_the_users_and_the_trash(origin: &FakeDrive) {
    let walk = &origin.requests()[0];
    let fields = walk.param("fields").unwrap();
    assert!(
        fields.contains("lastModifyingUser(displayName,emailAddress)"),
        "{fields}"
    );
    assert!(
        fields.contains("owners(displayName,emailAddress)"),
        "{fields}"
    );
    let trash = origin
        .requests()
        .into_iter()
        .find(|request| {
            request
                .param("q")
                .is_some_and(|q| q.contains("trashed = true"))
        })
        .unwrap();
    assert_eq!(
        trash.param("q"),
        Some("('P' in parents or 'y2026' in parents) and trashed = true")
    );
    let fields = trash.param("fields").unwrap();
    for field in [
        "explicitlyTrashed",
        "trashedTime",
        "trashingUser(displayName,emailAddress)",
    ] {
        assert!(fields.contains(field), "{field} in {fields}");
    }
}

#[tokio::test]
async fn an_origin_as_it_was_copied_is_an_empty_report_of_its_header() {
    let files = vec![
        folder("P", "Projects", "root"),
        blob("a", "a.txt", "P", "a", "2026-10-01T00:00:00Z"),
        doc("d", "Budget", "P", "2026-10-02T00:00:00Z"),
        folder("s", "sub", "P"),
        blob("b", "b.txt", "s", "b", "2026-10-03T00:00:00Z"),
    ];
    let copies = vec![
        copy_of(&files[0], "P2", "root"),
        // The same content, whatever its time.
        copy_of(&files[1], "a2", "P2").modified("2026-11-01T00:00:00Z"),
        copy_of(&files[2], "d2", "P2"),
        copy_of(&files[3], "s2", "P2"),
        copy_of(&files[4], "b2", "s2"),
    ];
    let origin = start(files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(ran.out.text(), "Change\tWhen\tWho\tPath\n");
    assert_eq!(ran.summary(), "Summary: 0 items diverged in the origin");
    assert!(copy.revision_requests().is_empty());

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            by: By::User,
            ..settings(&["P"])
        },
    )
    .await;
    ran.assert_ok();
    assert_eq!(
        ran.out.text(),
        "User\tLast change\tAdded\tEdited\tTrashed\tOther\tItems\n"
    );
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn google_files_are_compared_by_their_times() {
    let files = vec![
        folder("P", "Projects", "root"),
        // As copied.
        doc("budget", "Budget", "P", "2026-10-02T00:00:00Z"),
        // Edited in the origin after the migration.
        doc("plan", "Plan", "P", "2026-11-03T00:00:00Z").modified_by(ana()),
        // Edited in the copy only.
        doc("notes", "Notes", "P", "2026-10-03T00:00:00Z"),
        // Edited in both, the copy last.
        doc("minutes", "Minutes", "P", "2026-11-02T00:00:00Z").modified_by(bob()),
        // Edited in the copy, whose revisions are gone.
        doc("agenda", "Agenda", "P", "2026-10-04T00:00:00Z"),
    ];
    let copies = vec![
        copy_of(&files[0], "P2", "root"),
        copy_of(&files[1], "budget2", "P2"),
        copy_of(&files[2], "plan2", "P2").modified("2026-10-05T00:00:00Z"),
        copy_of(&files[3], "notes2", "P2").modified("2026-11-04T00:00:00Z"),
        copy_of(&files[4], "minutes2", "P2").modified("2026-11-04T00:00:00Z"),
        copy_of(&files[5], "agenda2", "P2").modified("2026-11-04T00:00:00Z"),
    ];
    let origin = start(files).await;
    let copy = FakeDrive::start(
        copies,
        Behavior {
            revisions: [
                // The first revision has the time the migration gave the copy.
                (
                    String::from("notes2"),
                    vec![
                        doc_revision("2026-10-03T00:00:00Z"),
                        doc_revision("2026-11-04T00:00:00Z"),
                    ],
                ),
                (
                    String::from("minutes2"),
                    vec![
                        doc_revision(MIGRATION),
                        doc_revision("2026-11-04T00:00:00Z"),
                    ],
                ),
            ]
            .into(),
            ..Behavior::default()
        },
    )
    .await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "edited",
                Some("2026-11-03T00:00:00Z"),
                "ana@example.com",
                "Projects/Plan"
            ),
            change(
                "edited",
                Some("2026-11-02T00:00:00Z"),
                "bob@example.com",
                "Projects/Minutes"
            ),
            change(
                "differs",
                Some("2026-10-04T00:00:00Z"),
                "old@example.com",
                "Projects/Agenda"
            ),
        ]
    );
    let mut asked = copy
        .revision_requests()
        .into_iter()
        .map(|request| request.file_id)
        .collect::<Vec<_>>();
    asked.sort();
    assert_eq!(asked, ["agenda2", "minutes2", "notes2"]);
    assert_read_only(&[&origin, &copy]);
}

/// The revisions of the copies of `revisions_settle_a_file_changed_on_both_sides_or_it_differs`:
/// the migration's and a later one, none, refused, gone, and only later ones.
fn revisions_behavior() -> Behavior {
    Behavior {
        revisions: [
            (
                String::from("kept2"),
                vec![
                    revision("v1", MIGRATION),
                    revision("v9", "2026-11-04T00:00:00Z"),
                ],
            ),
            (
                String::from("both2"),
                vec![
                    revision("v1", MIGRATION),
                    revision("v9", "2026-11-04T00:00:00Z"),
                ],
            ),
            (
                String::from("pruned2"),
                vec![
                    revision("v5", "2026-11-03T00:00:00Z"),
                    revision("v9", "2026-11-04T00:00:00Z"),
                ],
            ),
        ]
        .into(),
        revisions_refused: [
            (String::from("refused2"), 403),
            (String::from("gone2"), 404),
        ]
        .into(),
        ..Behavior::default()
    }
}

#[tokio::test]
async fn revisions_settle_a_file_changed_on_both_sides_or_it_differs() {
    // Each origin file was copied as `v1` at the migration; the copy changed to `v9` later.
    let origin_files = vec![
        folder("P", "Projects", "root"),
        // Not touched since the migration.
        blob("kept", "kept.txt", "P", "v1", "2026-10-20T00:00:00Z"),
        // Changed after the migration, before the copy was.
        blob("both", "both.txt", "P", "v2", "2026-11-02T00:00:00Z").modified_by(ana()),
        // No revisions, refused, gone, and a first revision that is not the migration's.
        blob("none", "none.txt", "P", "v2", "2026-10-21T00:00:00Z"),
        blob("refused", "refused.txt", "P", "v2", "2026-10-22T00:00:00Z"),
        blob("gone", "gone.txt", "P", "v2", "2026-10-23T00:00:00Z"),
        blob("pruned", "pruned.txt", "P", "v2", "2026-10-24T00:00:00Z"),
    ];
    let mut copies = vec![copy_of(&origin_files[0], "P2", "root")];
    for file in &origin_files[1..] {
        copies.push(
            FakeFile {
                id: format!("{}2", file.id),
                parents: vec![String::from("P2")],
                ..FakeFile::blob("x", &file.name, "P2", "v9")
            }
            .modified("2026-11-04T00:00:00Z"),
        );
    }
    let origin = start(origin_files).await;
    let copy = FakeDrive::start(copies, revisions_behavior()).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "edited",
                Some("2026-11-02T00:00:00Z"),
                "ana@example.com",
                "Projects/both.txt"
            ),
            change(
                "differs",
                Some("2026-10-24T00:00:00Z"),
                "old@example.com",
                "Projects/pruned.txt"
            ),
            change(
                "differs",
                Some("2026-10-23T00:00:00Z"),
                "old@example.com",
                "Projects/gone.txt"
            ),
            change(
                "differs",
                Some("2026-10-22T00:00:00Z"),
                "old@example.com",
                "Projects/refused.txt"
            ),
            change(
                "differs",
                Some("2026-10-21T00:00:00Z"),
                "old@example.com",
                "Projects/none.txt"
            ),
        ]
    );
    assert!(ran.err.lines().contains(&String::from(
        "Read the revisions of 6 files of the copy, newer than the origin's file of the same \
             name; they do not tell whether 4 files of the origin changed after the migration \
             (differs)"
    )));
    assert_eq!(copy.revision_requests().len(), 6);
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn a_renamed_or_moved_blob_is_moved_once_and_a_google_file_never() {
    let origin_files = vec![
        folder("P", "Projects", "root"),
        // Two files with the content of one file of the copy that is not in the origin.
        blob("x1", "x1.bin", "P", "same", BEFORE),
        blob("x2", "x2.bin", "P", "same", BEFORE),
        // A file of the copy with this content and a file of its name in the origin is no source.
        blob("keep", "keep.bin", "P", "same", BEFORE),
        // A Google file has no md5: it is added, never moved.
        doc("report", "Report", "P", BEFORE),
        folder("sub", "sub", "P"),
        blob("y", "y.bin", "sub", "other", BEFORE),
    ];
    let copies = vec![
        copy_of(&origin_files[0], "P2", "root"),
        copy_of(&blob("x", "old1.bin", "P", "same", BEFORE), "old1", "P2"),
        copy_of(&origin_files[3], "keep2", "P2"),
        copy_of(&doc("x", "Old report", "P", BEFORE), "report2", "P2"),
        copy_of(&origin_files[5], "sub2", "P2"),
        // Moved from sub/ to the top in the copy's terms: the origin moved it into sub/.
        copy_of(&blob("x", "z.bin", "P", "other", BEFORE), "z2", "P2"),
    ];
    let origin = start(origin_files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change("added", Some(BEFORE), "old@example.com", "Projects/Report"),
            change("added", Some(BEFORE), "old@example.com", "Projects/x2.bin"),
            change(
                "moved",
                None,
                "-",
                "Projects/sub/y.bin (was Projects/z.bin)"
            ),
            change(
                "moved",
                None,
                "-",
                "Projects/x1.bin (was Projects/old1.bin)"
            ),
        ]
    );
    assert!(copy.revision_requests().is_empty());
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn an_added_folder_is_a_row_with_a_row_for_everything_in_it() {
    let origin_files = vec![
        folder("P", "Projects", "root"),
        FakeFile::folder("new", "New", "P")
            .created("2026-11-02T00:00:00Z")
            .owned_by(bob()),
        FakeFile::folder("deep", "deeper", "new")
            .created("2026-11-02T01:00:00Z")
            .owned_by(bob()),
        blob("d", "d.txt", "deep", "d", "2026-11-02T02:00:00Z")
            .created("2026-11-02T02:00:00Z")
            .owned_by(ana()),
        // A file in a new folder that someone moved there, created long ago.
        blob("o", "old.txt", "new", "o", BEFORE),
        // The copy has a folder `New` too, but a file is no folder.
        FakeFile::folder("file", "a.txt", "P")
            .created("2026-11-02T03:00:00Z")
            .owned_by(carol()),
    ];
    let copies = vec![
        copy_of(&origin_files[0], "P2", "root"),
        copy_of(&blob("x", "New", "P", "n", BEFORE), "n2", "P2"),
        copy_of(&blob("x", "a.txt", "P", "n", BEFORE), "a2", "P2"),
    ];
    let origin = start(origin_files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "added",
                Some("2026-11-02T03:00:00Z"),
                "carol@example.com",
                "Projects/a.txt"
            ),
            change(
                "added",
                Some("2026-11-02T02:00:00Z"),
                "ana@example.com",
                "Projects/New/deeper/d.txt"
            ),
            change(
                "added",
                Some("2026-11-02T01:00:00Z"),
                "bob@example.com",
                "Projects/New/deeper"
            ),
            change(
                "added",
                Some("2026-11-02T00:00:00Z"),
                "bob@example.com",
                "Projects/New"
            ),
            change(
                "added",
                Some(BEFORE),
                "old@example.com",
                "Projects/New/old.txt"
            ),
        ]
    );
    assert_read_only(&[&origin, &copy]);
}

/// `Projects` with a shortcut and a Drawing, which a migration does not copy, and a file that
/// is in the copy.
fn unsupported_tree() -> Vec<FakeFile> {
    vec![
        folder("P", "Projects", "root"),
        FakeFile::shortcut("link", "link", "P", "x", "text/plain")
            .created("2026-11-03T00:00:00Z")
            .owned_by(ana()),
        FakeFile::native(
            "draw",
            "Sketch",
            "P",
            "application/vnd.google-apps.drawing",
            "",
        )
        .created(BEFORE)
        .modified("2026-11-04T00:00:00Z")
        .modified_by(bob()),
        FakeFile::native(
            "form",
            "Survey",
            "P",
            "application/vnd.google-apps.form",
            "",
        )
        .created(BEFORE)
        .modified(BEFORE),
        blob("a", "a.txt", "P", "a", BEFORE),
    ]
}

#[tokio::test]
async fn items_a_migration_does_not_copy_are_left_out_unless_since_reports_them() {
    let files = unsupported_tree();
    let copies = vec![
        copy_of(&files[0], "P2", "root"),
        copy_of(&files[4], "a2", "P2"),
    ];
    let origin = start(files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert!(ran.rows().is_empty(), "{:?}", ran.rows());
    let err = ran.err.lines();
    let left_out = err
        .iter()
        .position(|line| line.starts_with("Left out"))
        .unwrap();
    assert_eq!(
        err[left_out..],
        [
            "Left out 3 items, which a migration does not copy: they are never in the copy \
             (--since reports those created or modified since a time):",
            "  Projects/Sketch: Google Drawings cannot be copied: Drive converts no export back \
             to a drawing",
            "  Projects/Survey: Google Forms cannot be exported",
            "  Projects/link: shortcuts have no content",
            "Summary: 0 items diverged in the origin",
        ]
    );

    let ran = diverged(
        &origin,
        Some(&copy),
        since(settings(&["P"]), "2026-11-02T00:00:00Z"),
    )
    .await;
    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "edited",
                Some("2026-11-04T00:00:00Z"),
                "bob@example.com",
                "Projects/Sketch"
            ),
            change(
                "added",
                Some("2026-11-03T00:00:00Z"),
                "ana@example.com",
                "Projects/link"
            ),
        ]
    );
    assert!(!ran.err.text().contains("Left out"), "{}", ran.err.text());
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn since_alone_reports_what_was_created_or_modified_at_or_after_it() {
    const WHEN: &str = "2026-11-01T00:00:00Z";
    let files = vec![
        folder("P", "Projects", "root"),
        blob("c", "created.txt", "P", "c", "2026-11-03T00:00:00Z")
            .created(WHEN)
            .owned_by(bob()),
        blob("m", "modified.txt", "P", "m", WHEN).modified_by(ana()),
        blob("o", "older.txt", "P", "o", "2026-10-31T23:59:59Z").created("2026-10-31T23:59:59Z"),
        doc("d", "Budget", "P", "2026-11-02T00:00:00Z").modified_by(carol()),
        FakeFile::folder("n", "New", "P")
            .created("2026-11-02T00:00:00Z")
            .owned_by(bob()),
        blob("i", "in.txt", "n", "i", BEFORE),
        blob("t", "gone.txt", "P", "t", BEFORE)
            .trashed_by(Some(ana()), Some("2026-11-04T00:00:00Z")),
        blob("u", "early.txt", "P", "u", BEFORE)
            .trashed_by(Some(ana()), Some("2026-10-30T00:00:00Z")),
    ];
    let origin = start(files).await;

    let ran = diverged(&origin, None, since(settings(&["P"]), WHEN)).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "trashed",
                Some("2026-11-04T00:00:00Z"),
                "ana@example.com",
                "Projects/gone.txt"
            ),
            change(
                "edited",
                Some("2026-11-02T00:00:00Z"),
                "carol@example.com",
                "Projects/Budget"
            ),
            change(
                "added",
                Some("2026-11-02T00:00:00Z"),
                "bob@example.com",
                "Projects/New"
            ),
            change(
                "added",
                Some(WHEN),
                "bob@example.com",
                "Projects/created.txt"
            ),
            change(
                "edited",
                Some(WHEN),
                "ana@example.com",
                "Projects/modified.txt"
            ),
        ]
    );
    assert_eq!(
        ran.err.lines(),
        [
            String::from(
                "Found 5 files in 2 folders in the origin, and 2 items moved from them to the trash"
            ),
            format!(
                "Reporting what was created or modified in the origin since {}, and what was \
                 moved from its folders to the trash; nothing is compared with a copy (--to)",
                local(WHEN)
            ),
            format!(
                "Summary: 5 items changed in the origin since {} (2 added, 2 edited, 1 trashed) \
                 by 3 people",
                local(WHEN)
            ),
        ]
    );
    assert_read_only(&[&origin]);
}

#[tokio::test]
async fn since_filters_the_rows_of_a_comparison() {
    let origin = start(origin_tree()).await;
    let copy = start(copy_tree()).await;

    let ran = diverged(
        &origin,
        Some(&copy),
        since(settings(&["P"]), "2026-11-04T08:00:00Z"),
    )
    .await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "edited",
                Some("2026-11-05T10:12:44Z"),
                "ana@example.com",
                "Projects/plan.docx"
            ),
            change(
                "added",
                Some("2026-11-04T09:00:02Z"),
                "bob@example.com",
                "Projects/2026/new.pdf"
            ),
            change(
                "added",
                Some("2026-11-04T08:00:00Z"),
                "bob@example.com",
                "Projects/2026"
            ),
            change(
                "moved",
                None,
                "-",
                "Projects/2026/b.jpg (was Projects/a.jpg)"
            ),
        ]
    );
    assert_eq!(
        ran.summary(),
        format!(
            "Summary: 4 items diverged in the origin since {} (2 added, 1 edited, 1 moved) by 2 \
             people; 1 item without a known person",
            local("2026-11-04T08:00:00Z")
        )
    );
    assert_read_only(&[&origin, &copy]);
}

/// `Projects` with items moved to the trash: a file without a time or person (outside shared
/// drives), a folder with a file in it, and a file of a subfolder.
fn trash_tree() -> Vec<FakeFile> {
    vec![
        folder("P", "Projects", "root"),
        blob("x", "old.xlsx", "P", "x", BEFORE)
            .owned_by(ana())
            .trashed_by(None, None),
        folder("bin", "bin", "P").trashed_by(Some(bob()), Some("2026-11-02T00:00:00Z")),
        // Trashed with its folder, not by itself.
        blob("inner", "inner.txt", "bin", "i", BEFORE).in_trashed_folder(),
        folder("s", "sub", "P"),
        blob("g", "gone.txt", "s", "g", BEFORE)
            .trashed_by(Some(carol()), Some("2026-11-04T00:00:00Z")),
    ]
}

#[tokio::test]
async fn what_was_moved_to_the_trash_is_a_row_in_both_modes_a_folder_once() {
    let files = trash_tree();
    let copies = vec![
        copy_of(&files[0], "P2", "root"),
        copy_of(&files[1], "x2", "P2"),
        copy_of(&files[2], "bin2", "P2"),
        copy_of(&files[3], "inner2", "bin2"),
        copy_of(&files[4], "s2", "P2"),
        copy_of(&files[5], "g2", "s2"),
    ];
    let origin = start(files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "trashed",
                Some("2026-11-04T00:00:00Z"),
                "carol@example.com",
                "Projects/sub/gone.txt"
            ),
            change(
                "trashed",
                Some("2026-11-02T00:00:00Z"),
                "bob@example.com",
                "Projects/bin"
            ),
            // Outside shared drives Drive tells neither; only the owner can trash it there.
            change("trashed", None, "ana@example.com", "Projects/old.xlsx"),
        ]
    );

    // By time: what was trashed before is left out, what has no time stays.
    let ran = diverged(
        &origin,
        None,
        since(settings(&["P"]), "2026-11-03T00:00:00Z"),
    )
    .await;
    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            change(
                "trashed",
                Some("2026-11-04T00:00:00Z"),
                "carol@example.com",
                "Projects/sub/gone.txt"
            ),
            change("trashed", None, "ana@example.com", "Projects/old.xlsx"),
        ]
    );

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            by: By::User,
            ..settings(&["P"])
        },
    )
    .await;
    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [
            row(&[
                "carol@example.com",
                &local("2026-11-04T00:00:00Z"),
                "0",
                "0",
                "1",
                "0",
                "1"
            ]),
            row(&[
                "bob@example.com",
                &local("2026-11-02T00:00:00Z"),
                "0",
                "0",
                "1",
                "0",
                "1"
            ]),
            // A person without a time comes after those with one.
            row(&["ana@example.com", "-", "0", "0", "1", "0", "1"]),
        ]
    );
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn who_is_the_address_else_the_name_else_the_owner_and_the_owner_for_what_was_added() {
    const WHEN: &str = "2026-11-01T00:00:00Z";
    let edit = "2026-11-02T00:00:00Z";
    let files = vec![
        folder("P", "Projects", "root"),
        blob("a", "a.txt", "P", "a", edit).modified_by(FakeUser::new("Ana", " ANA@example.COM ")),
        blob("b", "b.txt", "P", "b", edit).modified_by(FakeUser::named("Bob Builder")),
        FakeFile {
            last_modifying_user: None,
            ..blob("c", "c.txt", "P", "c", edit).owned_by(carol())
        },
        blob("d", "d.txt", "P", "d", edit)
            .created("2026-11-02T00:00:00Z")
            .modified_by(ana())
            .owned_by(FakeUser::new("Dave", "dave@example.com")),
        FakeFile {
            last_modifying_user: None,
            owners: Vec::new(),
            ..blob("e", "e.txt", "P", "e", edit)
        },
        blob("f", "f.txt", "P", "f", edit)
            .modified_by(FakeUser::default())
            .owned_by(FakeUser::named("Erin")),
        // Added in a shared drive, which has no owners: who modified it last.
        FakeFile {
            owners: Vec::new(),
            ..blob("g", "g.txt", "P", "g", edit)
                .created("2026-11-02T00:00:00Z")
                .modified_by(bob())
        },
    ];
    let origin = start(files).await;

    let ran = diverged(&origin, None, since(settings(&["P"]), WHEN)).await;

    ran.assert_ok();
    let who = ran
        .rows()
        .into_iter()
        .map(|row| (row[3].clone(), row[0].clone(), row[2].clone()))
        .collect::<Vec<_>>();
    let expected = [
        ("a.txt", "edited", "ana@example.com"),
        ("b.txt", "edited", "Bob Builder"),
        ("c.txt", "edited", "carol@example.com"),
        ("d.txt", "added", "dave@example.com"),
        ("e.txt", "edited", "-"),
        ("f.txt", "edited", "Erin"),
        ("g.txt", "added", "bob@example.com"),
    ]
    .map(|(path, kind, who)| (format!("Projects/{path}"), kind.to_owned(), who.to_owned()));
    assert_eq!(who, expected);
    assert!(
        ran.summary()
            .ends_with("by 6 people; 1 item without a known person"),
        "{}",
        ran.summary()
    );
    assert_read_only(&[&origin]);
}

#[tokio::test]
async fn exclude_leaves_out_what_a_person_did_in_any_case() {
    let origin = start(origin_tree()).await;
    let copy = start(copy_tree()).await;

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            exclude: vec![
                String::from(" ANA@example.com"),
                String::from("nobody@example.com"),
            ],
            ..settings(&["P"])
        },
    )
    .await;

    ran.assert_ok();
    let paths = ran
        .rows()
        .into_iter()
        .map(|row| row[3].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "Projects/2026/new.pdf",
            "Projects/2026",
            "Projects/notes.txt",
            "Projects/2026/b.jpg (was Projects/a.jpg)",
        ]
    );
    let err = ran.err.lines();
    assert_eq!(
        err[err.len() - 2..],
        [
            "Left out 2 changes of the people of --exclude",
            "Summary: 4 items diverged in the origin (2 added, 1 differs, 1 moved) by 2 people; \
             1 item without a known person",
        ]
    );
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn by_user_has_a_row_per_person_most_recent_first_and_nobody_last() {
    let origin = start(origin_tree()).await;
    let copy = start(copy_tree()).await;

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            by: By::User,
            ..settings(&["P"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(
        ran.out.lines()[0],
        "User\tLast change\tAdded\tEdited\tTrashed\tOther\tItems"
    );
    assert_eq!(
        ran.rows(),
        [
            row(&[
                "ana@example.com",
                &local("2026-11-05T10:12:44Z"),
                "0",
                "1",
                "1",
                "0",
                "2"
            ]),
            row(&[
                "bob@example.com",
                &local("2026-11-04T09:00:02Z"),
                "2",
                "0",
                "0",
                "0",
                "2"
            ]),
            row(&[
                "carol@example.com",
                &local("2026-10-28T08:15:00Z"),
                "0",
                "0",
                "0",
                "1",
                "1"
            ]),
            row(&["-", "-", "0", "0", "0", "1", "1"]),
        ]
    );
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn hostile_names_and_addresses_are_escaped_in_every_output() {
    let hostile = "tab\there\nnew\u{1b}[2J\\.txt";
    let files = vec![
        folder("P", "Projects", "root"),
        blob("evil", hostile, "P", "x", BEFORE)
            .created("2026-11-02T00:00:00Z")
            .owned_by(FakeUser::named("mallory\u{1b}]0;owned\u{7}\r\nforged")),
    ];
    let copies = vec![copy_of(&files[0], "P2", "root")];
    let origin = start(files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;
    ran.assert_ok();
    let lines = ran.out.lines();
    assert_eq!(lines.len(), 2, "one line per row: {lines:?}");
    assert_eq!(
        ran.rows()[0][2..],
        [
            String::from("mallory\\u{1b}]0;owned\\u{7}\\r\\nforged"),
            String::from("Projects/tab\\there\\nnew\\u{1b}[2J\\\\.txt"),
        ]
    );

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            output: Output::Table {
                skip_header: false,
                field_separator: String::from("\t"),
            },
            ..settings(&["P"])
        },
    )
    .await;
    ran.assert_ok();
    assert_eq!(ran.out.lines().len(), 2);
    assert!(ran.out.lines()[1].contains("Projects/tab\\there\\nnew\\u{1b}[2J\\.txt"));
    for text in [ran.out.text(), ran.err.text()] {
        assert!(!text.contains(['\u{1b}', '\u{7}', '\r']), "{text:?}");
    }
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn the_table_aligns_columns_and_takes_the_flags_of_files_list() {
    let origin = start(origin_tree()).await;
    let copy = start(copy_tree()).await;
    let table = |skip_header: bool, field_separator: &str| Output::Table {
        skip_header,
        field_separator: field_separator.to_owned(),
    };

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            output: table(false, "\t"),
            ..settings(&["P"])
        },
    )
    .await;
    ran.assert_ok();
    let lines = ran.out.lines();
    assert_eq!(lines.len(), 7);
    assert!(
        lines[0].starts_with("Change    When                  Who                 Path"),
        "{}",
        lines[0]
    );
    assert!(!ran.out.text().contains('\t'));

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            output: table(true, ","),
            ..settings(&["P"])
        },
    )
    .await;
    ran.assert_ok();
    let lines = ran.out.lines();
    assert_eq!(lines.len(), 6);
    assert_eq!(
        lines[0],
        format!(
            "edited,{},ana@example.com,Projects/plan.docx",
            local("2026-11-05T10:12:44Z")
        )
    );
}

#[tokio::test]
async fn a_given_item_without_a_copy_fails_and_names_the_ways_out() {
    let origin = start(origin_tree()).await;
    let mut copies = copy_tree();
    // The copy has a file named `Projects`, which is no folder.
    copies[0] = copy_of(&blob("x", "Projects", "root", "p", BEFORE), "P2", "root");
    copies.push(folder("F", "From old", "root"));
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;
    assert_eq!(
        ran.error(),
        "Error: 'Projects' has no copy in the copy's account: there is no folder of this name in \
         the top of its My Drive; give the folder that holds the copy with --into, or run \
         without --to and with --since <WHEN>, the time of the migration"
    );
    assert_eq!(ran.out.text(), "");

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            into: Some(String::from("F")),
            ..settings(&["plan"])
        },
    )
    .await;
    assert_eq!(
        ran.error(),
        "Error: 'plan.docx' has no copy in the copy's account: there is no file of this name in \
         the folder 'From old' ('F'); give the folder that holds the copy with --into, or run \
         without --to and with --since <WHEN>, the time of the migration"
    );
    assert_eq!(ran.out.text(), "");
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn into_names_the_folder_that_holds_the_copy() {
    let origin = start(origin_tree()).await;
    let mut copies = copy_tree();
    copies[0].parents = vec![String::from("F")];
    copies.push(folder("F", "From old", "root"));
    copies.push(blob("t", "not a folder", "root", "t", BEFORE));
    let copy = start(copies).await;

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            into: Some(String::from("F")),
            ..settings(&["P"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(ran.rows().len(), 6);
    assert!(ran.err.lines().contains(&String::from(
        "Compared by name with the copy in the folder 'From old' (--into): files by size and \
         MD5 checksum, Google Docs, Sheets and Slides by modification time"
    )));
    assert!(copy.gets().contains(&String::from("F")));

    for (into, expected) in [
        (
            "t",
            "Error: 't' is not a folder of the copy's account; --into takes the id of the folder \
             that holds the copy",
        ),
        (
            "missing",
            "Error: unable to get the folder 'missing' of the copy's account (--into)",
        ),
    ] {
        let ran = diverged(
            &origin,
            Some(&copy),
            Settings {
                into: Some(into.to_owned()),
                ..settings(&["P"])
            },
        )
        .await;
        assert!(ran.error().starts_with(expected), "{}", ran.error());
        assert_eq!(ran.out.text(), "");
    }
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn a_copy_in_a_shared_drive_is_listed_in_its_corpus() {
    let origin = start(origin_tree()).await;
    let mut copies = copy_tree()
        .into_iter()
        .map(|file| file.in_drive("d1"))
        .collect::<Vec<_>>();
    copies[0].parents = vec![String::from("d1")];
    copies.push(FakeFile {
        parents: Vec::new(),
        ..folder("d1", "Team", "").in_drive("d1")
    });
    let copy = start(copies).await;

    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            into: Some(String::from("d1")),
            ..settings(&["P"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(ran.rows().len(), 6);
    let requests = copy.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.param("corpora"), Some("drive"));
        assert_eq!(request.param("driveId"), Some("d1"));
        assert_eq!(request.param("includeItemsFromAllDrives"), Some("true"));
    }
    assert_read_only(&[&origin, &copy]);
}

#[tokio::test]
async fn root_is_compared_with_the_folder_named_my_drive() {
    let files = vec![
        FakeFile {
            parents: Vec::new(),
            ..folder("root", "My Drive", "")
        },
        blob("a", "a.txt", "root", "a2", "2026-11-03T00:00:00Z").modified_by(ana()),
    ];
    let copies = vec![
        folder("M", "My Drive", "root"),
        copy_of(&blob("x", "a.txt", "root", "a1", BEFORE), "a2", "M"),
    ];
    let origin = start(files).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["root"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.rows(),
        [change(
            "edited",
            Some("2026-11-03T00:00:00Z"),
            "ana@example.com",
            "My Drive/a.txt"
        )]
    );
}

#[tokio::test]
async fn an_item_that_both_accounts_see_is_never_compared_with_itself() {
    let files = vec![
        folder("P", "Projects", "root"),
        folder("S", "shared", "P"),
        blob("s1", "s.txt", "S", "s", BEFORE),
        blob("d", "doc.txt", "P", "d", BEFORE),
        blob("e", "e.txt", "P", "e", BEFORE),
    ];
    let copies = vec![
        copy_of(&files[0], "P2", "root"),
        // The copy's folder holds the origin folder itself.
        FakeFile {
            parents: vec![String::from("P2")],
            ..files[1].clone()
        },
        // A file of the name of doc.txt that is another origin item.
        FakeFile {
            id: String::from("e"),
            parents: vec![String::from("P2")],
            ..files[3].clone()
        },
    ];
    let origin = start(files.clone()).await;
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;

    ran.assert_ok();
    assert!(ran.rows().is_empty(), "{:?}", ran.rows());
    let err = ran.err.lines();
    let left_out = err
        .iter()
        .position(|line| line.starts_with("Left out"))
        .unwrap();
    assert_eq!(
        err[left_out..],
        [
            "Left out 3 items that both accounts see: the folder of the copy holds the item \
             itself, or one of the origin's items has its name there, which is not compared \
             with itself:",
            "  Projects/doc.txt",
            "  Projects/e.txt",
            "  Projects/shared",
            "Summary: 0 items diverged in the origin",
        ]
    );

    // The copy of a given item that is the item itself, or another origin item, is no copy.
    let copy = start(vec![files[0].clone()]).await;
    let ran = diverged(&origin, Some(&copy), settings(&["P"])).await;
    assert_eq!(
        ran.error(),
        "Error: the folder 'Projects' in the copy's account is the item itself, which both \
         accounts see, not a copy; give the folder that holds the copy with --into, or run \
         without --to and with --since <WHEN>, the time of the migration"
    );
    let copy = start(vec![FakeFile {
        parents: vec![String::from("root")],
        name: String::from("e.txt"),
        ..files[4].clone()
    }])
    .await;
    let ran = diverged(&origin, Some(&copy), settings(&["P", "d"])).await;
    assert!(
        ran.error().starts_with("Error: 'Projects' has no copy"),
        "{}",
        ran.error()
    );
    let ran = diverged(&origin, Some(&copy), settings(&["e"])).await;
    assert_eq!(
        ran.error(),
        "Error: the file 'e.txt' in the copy's account is the item itself, which both accounts \
         see, not a copy; give the folder that holds the copy with --into, or run without --to \
         and with --since <WHEN>, the time of the migration"
    );
    // An --into among the origin's folders.
    let copy = start(vec![files[0].clone(), files[1].clone()]).await;
    let ran = diverged(
        &origin,
        Some(&copy),
        Settings {
            into: Some(String::from("S")),
            ..settings(&["P"])
        },
    )
    .await;
    assert_eq!(
        ran.error(),
        "Error: the folder 'S' of the copy's account is one of the origin's folders, or inside \
         one; --into must name the folder that holds the copy"
    );
    assert_eq!(ran.out.text(), "");
    assert_read_only(&[&origin, &copy]);
}

/// The behaviors of the origin and the copy that make a listing fail, the ids given and the
/// start of the error.
fn listing_failures() -> Vec<(Behavior, Behavior, &'static [&'static str], &'static str)> {
    let behavior = |configure: fn(&mut Behavior)| {
        let mut behavior = Behavior::default();
        configure(&mut behavior);
        behavior
    };
    vec![
        (
            behavior(|b| b.unreadable = [String::from("sub")].into()),
            Behavior::default(),
            &["P"][..],
            "Error: unable to list the folder 'Projects/sub' of the origin",
        ),
        (
            // The walk lists `2026`, `sub` and `other` with one query, and names the first.
            behavior(|b| b.incomplete = [String::from("y2026")].into()),
            Behavior::default(),
            &["P"],
            "Error: Drive did not list the folder 'Projects/2026' of the origin completely, the \
             report could call what is missing added or unchanged; run the command again later",
        ),
        (
            behavior(|b| b.unreadable_trash = [String::from("other")].into()),
            Behavior::default(),
            &["P"],
            "Error: unable to list the folder 'Projects/other' of the origin",
        ),
        (
            Behavior::default(),
            // In a batch with `sub2`: listed again alone to name it.
            behavior(|b| b.unreadable = [String::from("other2")].into()),
            &["P"],
            "Error: unable to list the folder 'Projects/other' of the copy",
        ),
        (
            Behavior::default(),
            behavior(|b| b.incomplete = [String::from("P2")].into()),
            &["P"],
            "Error: Drive did not list the folder 'Projects' of the copy completely",
        ),
        (
            Behavior::default(),
            behavior(|b| b.unreadable = [String::from("root")].into()),
            &["P"],
            "Error: unable to list the folder 'My Drive' of the copy",
        ),
        (
            Behavior::default(),
            Behavior::default(),
            &["P", "missing"],
            "Error: unable to get 'missing'",
        ),
    ]
}

#[tokio::test]
async fn a_listing_that_fails_or_is_incomplete_on_either_side_fails_with_nothing_on_stdout() {
    let mut files = origin_tree();
    files.push(folder("sub", "sub", "P"));
    files.push(folder("other", "other", "P"));
    let mut copies = copy_tree();
    copies.push(folder("sub2", "sub", "P2"));
    copies.push(folder("other2", "other", "P2"));
    for (origin_behavior, copy_behavior, ids, expected) in listing_failures() {
        let origin = FakeDrive::start(files.clone(), origin_behavior).await;
        let copy = FakeDrive::start(copies.clone(), copy_behavior).await;
        let ran = diverged(&origin, Some(&copy), settings(ids)).await;
        assert!(ran.error().starts_with(expected), "{}", ran.error());
        assert_eq!(ran.out.text(), "");
        ran.assert_no_secrets();
        assert_read_only(&[&origin, &copy]);
    }
}

#[tokio::test]
async fn a_given_item_in_the_trash_is_refused() {
    let mut files = origin_tree();
    files.push(folder("bin", "bin", "root").trashed());
    // Drive reports what is in a trashed folder as trashed, not explicitly.
    files.push(blob("kept", "kept.txt", "bin", "k", BEFORE).in_trashed_folder());
    let origin = start(files).await;

    for (ids, expected) in [
        (
            &["oldx"][..],
            "Error: 'old.xlsx' ('oldx') is in the trash; restore it with `gdrive files untrash \
             oldx`, or leave it out",
        ),
        (
            &["kept"],
            "Error: 'kept.txt' ('kept') is in a folder that is in the trash; restore that folder \
             with `gdrive files untrash <FOLDER_ID>`, or leave it out",
        ),
    ] {
        let ran = diverged(&origin, None, since(settings(ids), MIGRATION)).await;
        assert_eq!(ran.error(), expected, "{ids:?}");
        assert_eq!(ran.out.text(), "");
    }
    assert_read_only(&[&origin]);
}

#[tokio::test]
async fn without_a_copy_or_a_time_there_is_nothing_to_compare() {
    let origin = start(origin_tree()).await;

    let ran = diverged(&origin, None, settings(&["P"])).await;
    assert!(
        ran.error()
            .starts_with("Error: nothing to compare with: give --to <ACCOUNT>"),
        "{}",
        ran.error()
    );
    let ran = diverged(
        &origin,
        None,
        Settings {
            into: Some(String::from("F")),
            ..since(settings(&["P"]), MIGRATION)
        },
    )
    .await;
    assert!(ran.error().contains("--into"), "{}", ran.error());
    assert!(origin.requests().is_empty() && origin.gets().is_empty());
}

#[tokio::test]
async fn an_item_given_twice_or_inside_a_given_folder_is_one_row() {
    let origin = start(origin_tree()).await;
    let mut copies = copy_tree();
    copies.push(copy_of(
        &blob("x", "plan.docx", "root", "plan v1", "2026-10-01T00:00:00Z"),
        "plan3",
        "root",
    ));
    let copy = start(copies).await;

    let ran = diverged(&origin, Some(&copy), settings(&["P", "plan", "P"])).await;

    ran.assert_ok();
    let plans = ran
        .rows()
        .into_iter()
        .filter(|row| row[3].ends_with("plan.docx"))
        .collect::<Vec<_>>();
    assert_eq!(
        plans,
        [change(
            "edited",
            Some("2026-11-05T10:12:44Z"),
            "ana@example.com",
            "plan.docx"
        )]
    );
    assert_eq!(ran.rows().len(), 6);
}

#[test]
fn the_walk_of_the_origin_asks_for_the_walk_fields_and_the_users() {
    use crate::transfer::walk::FIELDS;

    let base = FIELDS.strip_suffix(')').unwrap();
    assert_eq!(
        super::origin::WALK_FIELDS,
        format!(
            "{base},lastModifyingUser(displayName,emailAddress),owners(displayName,emailAddress))"
        )
    );
}
