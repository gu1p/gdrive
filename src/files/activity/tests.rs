//! `files activity` against the fake Drive, which answers the listings, `files.get`,
//! `permissions.list` and the audit log of the Reports API.

use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};

use super::{run, when::When, By, Error, Output, Scan, Settings};
use crate::{
    common::audit_log::rfc3339,
    files::info::format_date_time,
    transfer::{
        fake_drive::{Behavior, Bounds, FakeDrive, FakeFile},
        scheduler::capture::{output, Captured},
    },
};

/// The time the tests run at.
const NOW: &str = "2026-11-05T12:00:00Z";

/// The account of the tests.
const ADMIN: &str = "admin@example.com";

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    at(NOW)
}

/// A time as the report prints it: in the local time zone.
fn local(rfc3339: &str) -> String {
    format_date_time(at(rfc3339)).to_string()
}

/// `old` in My Drive, with a trashed file in it and a file outside it:
///
/// ```text
/// old/a.txt  old/sub/b.txt  old/sub/Budget  (old/gone.txt, trashed)  notes.txt
/// ```
fn tree() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("old", "old", "root"),
        FakeFile::blob("a", "a.txt", "old", "a"),
        FakeFile::folder("sub", "sub", "old"),
        FakeFile::blob("b", "b.txt", "sub", "b"),
        FakeFile::document("doc", "Budget", "sub"),
        FakeFile::blob("gone", "gone.txt", "old", "g").trashed(),
        FakeFile::blob("notes", "notes.txt", "root", "n"),
    ]
}

/// The shared drive `d1` (`Team`) with `Project/plan.txt` in it, and `other.txt` of the drive
/// `d2`.
fn shared_drive() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("d1", "Team", "").in_drive("d1"),
        FakeFile::folder("proj", "Project", "d1").in_drive("d1"),
        FakeFile::blob("plan", "plan.txt", "proj", "p").in_drive("d1"),
        FakeFile::blob("other", "other.txt", "d2", "o").in_drive("d2"),
    ]
}

/// An event `name` about `item`, without `primary_event`.
fn event(name: &str, item: &str) -> Value {
    json!({"type": "access", "name": name, "parameters": [
        {"name": "doc_id", "value": item},
        {"name": "doc_title", "value": "title"},
        {"name": "owner", "value": "old@example.com"},
    ]})
}

/// [`event`] with `primary_event` set to `primary`.
fn with_primary(mut event: Value, primary: bool) -> Value {
    event["parameters"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "primary_event", "boolValue": primary}));
    event
}

/// [`event`] in the shared drive `drive`.
fn in_drive(mut event: Value, drive: &str) -> Value {
    event["parameters"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "shared_drive_id", "value": drive}));
    event
}

static QUALIFIER: AtomicUsize = AtomicUsize::new(1);

/// A record of the audit log at `time` by `actor` (`None`: no address) with `events`, with a
/// `uniqueQualifier` of its own.
fn record(time: &str, actor: Option<&str>, events: Vec<Value>) -> Value {
    let qualifier = QUALIFIER.fetch_add(1, Ordering::SeqCst);
    let mut by = json!({"profileId": "1", "callerType": "USER"});
    if let Some(email) = actor {
        by["email"] = json!(email);
    }
    json!({
        "kind": "admin#reports#activity",
        "id": {
            "time": time,
            "uniqueQualifier": format!("-{qualifier}"),
            "applicationName": "drive",
            "customerId": "C01",
        },
        "actor": by,
        "ipAddress": "203.0.113.7",
        "events": Value::Array(events),
    })
}

fn permission(kind: &str, role: &str, who: &str) -> Value {
    match kind {
        "domain" => json!({"kind": "drive#permission", "type": kind, "role": role, "domain": who}),
        "anyone" => json!({"kind": "drive#permission", "type": kind, "role": role}),
        _ => json!({"kind": "drive#permission", "type": kind, "role": role, "emailAddress": who}),
    }
}

fn settings(file_ids: &[&str]) -> Settings {
    Settings {
        file_ids: file_ids.iter().map(|&id| id.to_owned()).collect(),
        account: ADMIN.to_owned(),
        since: When::Ago(TimeDelta::days(30)),
        until: None,
        by: By::User,
        exclude: Vec::new(),
        active_only: false,
        scan: Scan::Domain,
        jobs: NonZeroUsize::new(4).unwrap(),
        output: Output::Tsv { skip_header: false },
        progress_every: None,
    }
}

fn behavior(audit_log: Vec<Value>) -> Behavior {
    Behavior {
        audit_log,
        ..Behavior::default()
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

/// The `--by user` row of `who` on a sharing list with `role` and no activity.
fn idle(who: &str, role: &str) -> Vec<String> {
    row(&[who, role, "-", "0", "0", "0", "0", "0", "0"])
}

async fn activity(drive: &FakeDrive, settings: Settings) -> Ran {
    let output = output();
    let hub = drive.hub();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run(&hub, &settings, now(), &output.printer),
    )
    .await
    .expect("the report should end");
    Ran {
        result,
        out: output.out,
        err: output.err,
    }
}

/// The drive was only read.
fn assert_read_only(drive: &FakeDrive) {
    assert_eq!(drive.writes(), Vec::<String>::new());
}

/// Ana, Bob and anonymous users in `old`; Dave outside it and in the trash; Erin before the
/// time range.
fn people_log() -> Vec<Value> {
    vec![
        record(
            "2026-11-04T12:00:00Z",
            Some("ana@example.com"),
            vec![event("view", "a")],
        ),
        record(
            "2026-11-04T13:00:00Z",
            Some("ana@example.com"),
            vec![event("download", "b")],
        ),
        record(
            "2026-11-03T12:00:00Z",
            Some("ana@example.com"),
            vec![event("edit", "doc")],
        ),
        // Opening a folder is using it.
        record(
            "2026-11-02T12:00:00Z",
            Some("ana@example.com"),
            vec![event("view", "sub")],
        ),
        // The same person, another case.
        record(
            "2026-10-20T12:00:00Z",
            Some("Ana@Example.com"),
            vec![event("preview", "a")],
        ),
        record(
            "2026-10-30T12:00:00Z",
            Some("bob@example.com"),
            vec![event("view", "b")],
        ),
        record("2026-10-12T12:00:00Z", None, vec![event("view", "a")]),
        record("2026-10-13T12:00:00Z", Some(""), vec![event("view", "a")]),
        // Outside the tree, and in the trash: not counted.
        record(
            "2026-11-01T12:00:00Z",
            Some("dave@example.com"),
            vec![event("view", "notes")],
        ),
        record(
            "2026-11-01T12:00:00Z",
            Some("dave@example.com"),
            vec![event("view", "gone")],
        ),
        // Before --since.
        record(
            "2026-09-01T12:00:00Z",
            Some("erin@example.com"),
            vec![event("view", "a")],
        ),
    ]
}

/// The sharing list of `old`: everyone but Bob.
fn old_sharing() -> Vec<Value> {
    vec![
        permission("user", "owner", ADMIN),
        permission("user", "writer", "ana@example.com"),
        permission("user", "reader", "Carol@Example.com"),
        permission("group", "writer", "team@example.com"),
        permission("domain", "reader", "example.com"),
        permission("anyone", "reader", ""),
    ]
}

#[tokio::test]
async fn the_report_has_each_person_newest_first_then_who_has_access_and_did_nothing() {
    let (log, sharing) = (people_log(), old_sharing());
    let drive = FakeDrive::start(
        tree(),
        Behavior {
            permissions: [(String::from("old"), sharing)].into(),
            // Every listing and sharing list comes in pages.
            page_size: Some(2),
            ..behavior(log)
        },
    )
    .await;

    let ran = activity(&drive, settings(&["old"])).await;

    ran.assert_ok();
    assert_eq!(
        ran.out.lines()[0],
        "User\tAccess\tLast activity\tDays\tViews\tDownloads\tEdits\tOther\tFiles"
    );
    let ana = local("2026-11-04T13:00:00Z");
    let bob = local("2026-10-30T12:00:00Z");
    let anonymous = local("2026-10-13T12:00:00Z");
    assert_eq!(
        ran.rows(),
        [
            row(&[
                "ana@example.com",
                "writer",
                &ana,
                "4",
                "3",
                "1",
                "1",
                "0",
                "4"
            ]),
            row(&["bob@example.com", "-", &bob, "1", "1", "0", "0", "0", "1"]),
            row(&["(anonymous)", "-", &anonymous, "2", "2", "0", "0", "0", "1"]),
            idle(ADMIN, "owner"),
            idle("anyone", "reader"),
            idle("carol@example.com", "reader"),
            idle("domain:example.com", "reader"),
            idle("group:team@example.com", "writer"),
        ]
    );
    let since = local("2026-10-06T12:00:00Z");
    assert_eq!(
        ran.err.lines(),
        [
            String::from("Found 3 files in 2 folders"),
            format!(
                "Reading the audit log from {since} to {}: every Drive event of the \
                 organization, in 30 windows of 24 hours (--scan domain; --scan file is faster \
                 for a few items, --scan drive for items in shared drives)",
                local(NOW)
            ),
            String::from("Read 10 events of the audit log, 8 of them about these items"),
            format!(
                "Summary: 2 people active since {since}, and anonymous users; 5 with access and \
                 no activity"
            ),
        ]
    );
    let permission_requests = drive.permission_requests();
    assert_eq!(
        permission_requests.len(),
        3,
        "all pages of the sharing list"
    );
    for request in &permission_requests {
        assert_eq!(request.file_id, "old");
        assert_eq!(request.param("supportsAllDrives"), Some("true"));
    }
    assert_read_only(&drive);
    ran.assert_no_secrets();
}

#[tokio::test]
async fn event_names_go_to_their_columns_and_unknown_ones_to_other() {
    let names = [
        "view",
        "preview",
        "download",
        "print",
        "source_copy",
        "email_as_attachment",
        "edit",
        "create",
        "upload",
        "copy",
        "rename",
        "move",
        "trash",
        "untrash",
        "delete",
        "add_to_folder",
        "remove_from_folder",
        "change_user_access",
        "access_item_content",
        "a_name_from_next_year",
    ];
    let events = names.iter().map(|name| event(name, "a")).collect();
    let drive = FakeDrive::start(
        tree(),
        behavior(vec![record(
            "2026-11-01T12:00:00Z",
            Some("ana@example.com"),
            events,
        )]),
    )
    .await;

    let ran = activity(
        &drive,
        Settings {
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    let last = local("2026-11-01T12:00:00Z");
    assert_eq!(
        ran.rows(),
        [row(&[
            "ana@example.com",
            "-",
            &last,
            "1",
            "2",
            "4",
            "11",
            "3",
            "1"
        ])]
    );

    let ran = activity(
        &drive,
        Settings {
            by: By::Event,
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    let event_names = ran
        .rows()
        .into_iter()
        .map(|row| row[2].clone())
        .collect::<Vec<_>>();
    let mut sorted = names.map(String::from).to_vec();
    sorted.sort();
    assert_eq!(event_names, sorted, "the raw names, same time: by name");
}

#[tokio::test]
async fn only_the_primary_event_of_an_action_counts() {
    let drive = FakeDrive::start(
        tree(),
        behavior(vec![record(
            "2026-11-01T12:00:00Z",
            Some("ana@example.com"),
            vec![
                with_primary(event("edit", "a"), true),
                with_primary(event("view", "a"), false),
                event("download", "a"),
            ],
        )]),
    )
    .await;

    let ran = activity(
        &drive,
        Settings {
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    let last = local("2026-11-01T12:00:00Z");
    assert_eq!(
        ran.rows(),
        [row(&[
            "ana@example.com",
            "-",
            &last,
            "1",
            "0",
            "1",
            "1",
            "0",
            "1"
        ])]
    );
    assert!(ran.err.lines().contains(&String::from(
        "Read 3 events of the audit log, 2 of them about these items"
    )));
}

/// Ana and Bob, and the migration's account that downloaded everything.
fn migrated_log() -> Vec<Value> {
    vec![
        record(
            "2026-11-01T12:00:00Z",
            Some("migrator@example.com"),
            vec![event("download", "a")],
        ),
        record(
            "2026-11-01T12:00:01Z",
            Some("migrator@example.com"),
            vec![event("download", "b")],
        ),
        record(
            "2026-11-02T12:00:00Z",
            Some("ana@example.com"),
            vec![event("view", "b")],
        ),
        record(
            "2026-11-03T12:00:00Z",
            Some("ana@example.com"),
            vec![event("edit", "b")],
        ),
        record(
            "2026-11-02T10:00:00Z",
            Some("bob@example.com"),
            vec![event("view", "b")],
        ),
        record(
            "2026-10-20T12:00:00Z",
            Some("bob@example.com"),
            vec![event("view", "a")],
        ),
        record("2026-10-21T12:00:00Z", None, vec![event("print", "sub")]),
    ]
}

#[tokio::test]
async fn exclude_leaves_a_person_out_of_every_report() {
    let drive = FakeDrive::start(tree(), behavior(migrated_log())).await;
    let exclude = vec![String::from(" Migrator@Example.COM")];

    let ran = activity(
        &drive,
        Settings {
            exclude: exclude.clone(),
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    let users = ran
        .rows()
        .into_iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>();
    assert_eq!(users, ["ana@example.com", "bob@example.com", "(anonymous)"]);

    for by in [By::File, By::Event] {
        let ran = activity(
            &drive,
            Settings {
                by,
                exclude: exclude.clone(),
                ..settings(&["old"])
            },
        )
        .await;
        ran.assert_ok();
        assert!(!ran.out.text().contains("migrator"), "{}", ran.out.text());
        assert_eq!(ran.rows().len(), 5 - usize::from(by == By::File), "{by:?}");
    }
}

#[tokio::test]
async fn an_excluded_person_is_left_out_of_the_sharing_rows_and_the_summary_too() {
    let sharing = vec![
        // Another case than the --exclude below.
        permission("user", "owner", "MIGRATOR@example.com"),
        permission("group", "writer", "Migrator@Example.com"),
        permission("user", "reader", "carol@example.com"),
    ];
    let drive = FakeDrive::start(
        tree(),
        Behavior {
            permissions: [(String::from("old"), sharing)].into(),
            ..behavior(migrated_log())
        },
    )
    .await;

    let ran = activity(
        &drive,
        Settings {
            exclude: vec![String::from("migrator@EXAMPLE.com")],
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    let users = ran
        .rows()
        .into_iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        users,
        [
            "ana@example.com",
            "bob@example.com",
            "(anonymous)",
            "carol@example.com"
        ]
    );
    assert!(!ran.out.text().to_lowercase().contains("migrator"));
    assert_eq!(
        ran.summary(),
        format!(
            "Summary: 2 people active since {}, and anonymous users; 1 with access and no activity",
            local("2026-10-06T12:00:00Z")
        )
    );
}

#[tokio::test]
async fn by_file_has_a_row_per_item_and_person_by_path_then_newest_first() {
    let drive = FakeDrive::start(tree(), behavior(migrated_log())).await;

    let ran = activity(
        &drive,
        Settings {
            by: By::File,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(
        ran.out.lines()[0],
        "Path\tUser\tLast activity\tViews\tDownloads\tEdits\tOther"
    );
    let time = local;
    assert_eq!(
        ran.rows(),
        [
            row(&[
                "old/a.txt",
                "migrator@example.com",
                &time("2026-11-01T12:00:00Z"),
                "0",
                "1",
                "0",
                "0"
            ]),
            row(&[
                "old/a.txt",
                "bob@example.com",
                &time("2026-10-20T12:00:00Z"),
                "1",
                "0",
                "0",
                "0"
            ]),
            row(&[
                "old/sub",
                "(anonymous)",
                &time("2026-10-21T12:00:00Z"),
                "0",
                "1",
                "0",
                "0"
            ]),
            row(&[
                "old/sub/b.txt",
                "ana@example.com",
                &time("2026-11-03T12:00:00Z"),
                "1",
                "0",
                "1",
                "0"
            ]),
            row(&[
                "old/sub/b.txt",
                "bob@example.com",
                &time("2026-11-02T10:00:00Z"),
                "1",
                "0",
                "0",
                "0"
            ]),
            row(&[
                "old/sub/b.txt",
                "migrator@example.com",
                &time("2026-11-01T12:00:01Z"),
                "0",
                "1",
                "0",
                "0"
            ]),
        ]
    );
    assert!(drive.permission_requests().is_empty(), "no sharing rows");
    assert_eq!(
        ran.summary(),
        format!(
            "Summary: 3 people active since {}, and anonymous users",
            local("2026-10-06T12:00:00Z")
        )
    );
}

#[tokio::test]
async fn by_event_has_a_row_per_event_oldest_first() {
    let drive = FakeDrive::start(tree(), behavior(migrated_log())).await;

    let ran = activity(
        &drive,
        Settings {
            by: By::Event,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(ran.out.lines()[0], "Time\tUser\tEvent\tPath");
    let time = local;
    assert_eq!(
        ran.rows(),
        [
            row(&[
                &time("2026-10-20T12:00:00Z"),
                "bob@example.com",
                "view",
                "old/a.txt"
            ]),
            row(&[
                &time("2026-10-21T12:00:00Z"),
                "(anonymous)",
                "print",
                "old/sub"
            ]),
            row(&[
                &time("2026-11-01T12:00:00Z"),
                "migrator@example.com",
                "download",
                "old/a.txt"
            ]),
            row(&[
                &time("2026-11-01T12:00:01Z"),
                "migrator@example.com",
                "download",
                "old/sub/b.txt"
            ]),
            row(&[
                &time("2026-11-02T10:00:00Z"),
                "bob@example.com",
                "view",
                "old/sub/b.txt"
            ]),
            row(&[
                &time("2026-11-02T12:00:00Z"),
                "ana@example.com",
                "view",
                "old/sub/b.txt"
            ]),
            row(&[
                &time("2026-11-03T12:00:00Z"),
                "ana@example.com",
                "edit",
                "old/sub/b.txt"
            ]),
        ]
    );
}

#[tokio::test]
async fn active_only_reads_no_sharing_list_and_an_empty_report_is_its_header() {
    let drive = FakeDrive::start(
        tree(),
        Behavior {
            permissions: [(
                String::from("old"),
                vec![permission("user", "reader", "carol@example.com")],
            )]
            .into(),
            ..Behavior::default()
        },
    )
    .await;

    let ran = activity(
        &drive,
        Settings {
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(
        ran.out.lines(),
        ["User\tAccess\tLast activity\tDays\tViews\tDownloads\tEdits\tOther\tFiles"]
    );
    assert!(drive.permission_requests().is_empty());
    assert_eq!(
        ran.summary(),
        format!(
            "Summary: 0 people active since {}",
            local("2026-10-06T12:00:00Z")
        )
    );

    let ran = activity(
        &drive,
        Settings {
            output: Output::Tsv { skip_header: true },
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    assert_eq!(
        ran.out.lines(),
        ["carol@example.com\treader\t-\t0\t0\t0\t0\t0\t0"]
    );
    assert!(
        ran.summary().ends_with("; 1 with access and no activity"),
        "{}",
        ran.summary()
    );
}

#[tokio::test]
async fn the_strongest_role_of_the_given_items_is_the_access() {
    let drive = FakeDrive::start(
        tree(),
        Behavior {
            permissions: [
                (
                    String::from("old"),
                    vec![
                        permission("user", "writer", "ana@example.com"),
                        permission("user", "reader", "bob@example.com"),
                    ],
                ),
                (
                    String::from("notes"),
                    vec![
                        permission("user", "owner", "ana@example.com"),
                        permission("user", "commenter", "bob@example.com"),
                    ],
                ),
                // The sharing lists below the given items are not read.
                (
                    String::from("sub"),
                    vec![permission("user", "owner", "zoe@example.com")],
                ),
            ]
            .into(),
            ..Behavior::default()
        },
    )
    .await;

    let ran = activity(&drive, settings(&["old", "notes", "old"])).await;

    ran.assert_ok();
    let access = ran
        .rows()
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        access,
        [
            (String::from("ana@example.com"), String::from("owner")),
            (String::from("bob@example.com"), String::from("commenter")),
        ]
    );
    let mut asked = drive
        .permission_requests()
        .into_iter()
        .map(|request| request.file_id)
        .collect::<Vec<_>>();
    asked.sort();
    assert_eq!(asked, ["notes", "old"], "an id given twice is read once");
    assert_eq!(ran.err.lines()[0], "Found 4 files in 2 folders");
}

/// The windows of the requests, ordered by start: `(startTime, endTime, filters)`.
fn windows(drive: &FakeDrive) -> Vec<(String, String, Option<String>)> {
    let mut windows = drive
        .audit_requests()
        .into_iter()
        .filter(|request| request.param("pageToken").is_none())
        .map(|request| {
            assert_eq!(request.param("maxResults"), Some("1000"));
            (
                request.param("startTime").unwrap().to_owned(),
                request.param("endTime").unwrap().to_owned(),
                request.param("filters").map(String::from),
            )
        })
        .collect::<Vec<_>>();
    windows.sort();
    windows
}

/// 24-hour windows from `since` to `until` with `filters`, each reaching a second into the
/// next one.
fn expected_windows(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    filters: Option<&str>,
) -> Vec<(String, String, Option<String>)> {
    let mut windows = Vec::new();
    let mut start = since;
    while start < until {
        let end = (start + TimeDelta::days(1)).min(until);
        let reach = if end < until {
            end + TimeDelta::seconds(1)
        } else {
            end
        };
        windows.push((rfc3339(start), rfc3339(reach), filters.map(String::from)));
        start = end;
    }
    windows
}

#[tokio::test]
async fn the_domain_scan_reads_windows_of_a_day_without_filters() {
    let drive = FakeDrive::start(tree(), Behavior::default()).await;

    let ran = activity(&drive, settings(&["old"])).await;

    ran.assert_ok();
    let windows = windows(&drive);
    assert_eq!(windows.len(), 30);
    assert_eq!(
        windows,
        expected_windows(now() - TimeDelta::days(30), now(), None)
    );
    for request in drive.audit_requests() {
        assert_eq!(request.authorization.as_deref(), Some("Bearer mock-token"));
    }
    assert_read_only(&drive);
}

#[tokio::test]
async fn the_drive_scan_reads_the_shared_drive_of_the_items_on_request_only() {
    let log = vec![
        record(
            "2026-11-01T12:00:00Z",
            Some("ana@example.com"),
            vec![in_drive(event("view", "plan"), "d1")],
        ),
        record(
            "2026-11-01T12:00:00Z",
            Some("ana@example.com"),
            vec![in_drive(event("view", "other"), "d2")],
        ),
    ];
    let drive = FakeDrive::start(shared_drive(), behavior(log)).await;

    // The shared drive itself, and a folder in it.
    for id in ["d1", "proj"] {
        let ran = activity(
            &drive,
            Settings {
                scan: Scan::Drive,
                active_only: true,
                ..settings(&[id])
            },
        )
        .await;
        ran.assert_ok();
        assert_eq!(ran.rows().len(), 1, "{id}");
        assert!(ran
            .err
            .text()
            .contains("the events of 1 shared drive, in 30 windows of 24 hours (--scan drive)"));
    }
    let mut windows = windows(&drive);
    assert_eq!(windows.len(), 60, "30 for each run");
    windows.dedup();
    assert_eq!(
        windows,
        expected_windows(
            now() - TimeDelta::days(30),
            now(),
            Some("shared_drive_id==d1")
        )
    );
    assert_read_only(&drive);
}

#[tokio::test]
async fn the_default_scan_reads_the_whole_organization_also_for_shared_drives() {
    let drive = FakeDrive::start(shared_drive(), Behavior::default()).await;

    let ran = activity(&drive, settings(&["proj"])).await;

    ran.assert_ok();
    assert!(
        ran.err.text().contains(
            "(--scan domain; --scan file is faster for a few items, --scan drive for items in \
             shared drives)"
        ),
        "{}",
        ran.err.text()
    );
    assert_eq!(
        windows(&drive),
        expected_windows(now() - TimeDelta::days(30), now(), None)
    );
}

/// A file that was in My Drive, then in the shared drive `d0`, and is in `d1` since yesterday:
/// each of its events names the drive it was in then.
fn moved_file_log() -> Vec<Value> {
    vec![
        record(
            "2026-10-10T12:00:00Z",
            Some("ana@example.com"),
            vec![event("view", "plan")],
        ),
        record(
            "2026-10-20T12:00:00Z",
            Some("bob@example.com"),
            vec![in_drive(event("view", "plan"), "d0")],
        ),
        record(
            "2026-11-04T12:00:00Z",
            Some("carol@example.com"),
            vec![in_drive(event("view", "plan"), "d1")],
        ),
    ]
}

#[tokio::test]
async fn a_drive_scan_misses_what_happened_before_a_file_was_moved_into_its_shared_drive() {
    let drive = FakeDrive::start(shared_drive(), behavior(moved_file_log())).await;
    let users = |ran: &Ran| {
        ran.assert_ok();
        ran.rows()
            .into_iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>()
    };

    // Complete: domain reads everything, file selects by id, which a move keeps.
    for scan in [Scan::Domain, Scan::File] {
        let ran = activity(
            &drive,
            Settings {
                scan,
                active_only: true,
                ..settings(&["proj"])
            },
        )
        .await;
        assert_eq!(
            users(&ran),
            ["carol@example.com", "bob@example.com", "ana@example.com"],
            "{scan:?}"
        );
    }
    // The documented miss: only the events from the time the file was in d1.
    let ran = activity(
        &drive,
        Settings {
            scan: Scan::Drive,
            active_only: true,
            ..settings(&["proj"])
        },
    )
    .await;
    assert_eq!(users(&ran), ["carol@example.com"]);
}

#[tokio::test]
async fn the_drive_scan_reads_each_shared_drive_and_refuses_my_drive() {
    let mut files = tree();
    files.extend(shared_drive());
    let drive = FakeDrive::start(files, Behavior::default()).await;

    let ran = activity(
        &drive,
        Settings {
            scan: Scan::Drive,
            ..settings(&["proj", "other", "plan"])
        },
    )
    .await;
    ran.assert_ok();
    assert!(ran
        .err
        .text()
        .contains("the events of 2 shared drives, in 30 windows"));
    let mut filters = windows(&drive)
        .into_iter()
        .map(|(_, _, filters)| filters.unwrap())
        .collect::<Vec<_>>();
    filters.dedup();
    filters.sort();
    filters.dedup();
    assert_eq!(filters, ["shared_drive_id==d1", "shared_drive_id==d2"]);

    let drive = FakeDrive::start(tree(), Behavior::default()).await;
    let ran = activity(
        &drive,
        Settings {
            scan: Scan::Drive,
            ..settings(&["old"])
        },
    )
    .await;
    assert_eq!(
        ran.error(),
        "Error: 'old' is not in a shared drive, which --scan drive reads; use --scan domain or \
         --scan file"
    );
    assert!(drive.audit_requests().is_empty());
}

#[tokio::test]
async fn the_file_scan_reads_each_item_over_the_whole_range_and_counts_an_event_once() {
    // One record about two items: the requests of both return it.
    let both = record(
        "2026-11-01T12:00:00Z",
        Some("ana@example.com"),
        vec![event("view", "a"), event("view", "b")],
    );
    let drive = FakeDrive::start(tree(), behavior(vec![both])).await;

    let ran = activity(
        &drive,
        Settings {
            scan: Scan::File,
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    let last = local("2026-11-01T12:00:00Z");
    assert_eq!(
        ran.rows(),
        [row(&[
            "ana@example.com",
            "-",
            &last,
            "1",
            "2",
            "0",
            "0",
            "0",
            "2"
        ])]
    );
    let since = rfc3339(now() - TimeDelta::days(30));
    let until = rfc3339(now());
    let expected = ["a", "b", "doc", "old", "sub"]
        .iter()
        .map(|id| (since.clone(), until.clone(), Some(format!("doc_id=={id}"))))
        .collect::<Vec<_>>();
    let mut requested = windows(&drive);
    requested.sort_by(|a, b| a.2.cmp(&b.2));
    assert_eq!(requested, expected);
    assert!(ran
        .err
        .text()
        .contains("the events of each of the 5 items, one series of requests each (--scan file)"));
}

#[tokio::test]
async fn root_is_my_drive_with_everything_in_it() {
    let mut files = tree();
    files.push(FakeFile::folder("root", "My Drive", ""));
    let log = vec![record(
        "2026-11-01T12:00:00Z",
        Some("ana@example.com"),
        vec![event("view", "notes")],
    )];
    let drive = FakeDrive::start(files, behavior(log)).await;

    let ran = activity(
        &drive,
        Settings {
            by: By::File,
            ..settings(&["root"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(ran.err.lines()[0], "Found 4 files in 3 folders");
    assert_eq!(ran.rows()[0][0], "My Drive/notes.txt");
}

/// Five events a day for 30 days, one of them exactly where a window ends and the next one
/// starts, read in pages of 2.
fn busy_log(since: DateTime<Utc>) -> Vec<Value> {
    let mut log = Vec::new();
    for day in 0..30 {
        let start = since + TimeDelta::days(day);
        for offset in [
            TimeDelta::zero(),
            TimeDelta::milliseconds(1),
            TimeDelta::hours(6),
            TimeDelta::hours(23) + TimeDelta::minutes(59) + TimeDelta::seconds(59),
            TimeDelta::hours(23) + TimeDelta::minutes(59) + TimeDelta::milliseconds(59_999),
        ] {
            let time = rfc3339(start + offset);
            log.push(record(
                &time,
                Some("ana@example.com"),
                vec![event("view", "a")],
            ));
        }
    }
    log
}

#[tokio::test]
async fn windows_and_pages_lose_no_event_and_count_none_twice() {
    let since = at("2026-10-06T00:00:00Z");
    let until = at("2026-11-05T00:00:00Z");
    for bounds in [Bounds::StartIncluded, Bounds::BothExcluded] {
        let mut log = busy_log(since);
        if bounds == Bounds::BothExcluded {
            // A server that leaves out both ends loses the very first instant of the range.
            log.remove(0);
        }
        let total = log.len();
        // Calendar days of the local time zone.
        let days = log
            .iter()
            .map(|record| {
                at(record["id"]["time"].as_str().unwrap())
                    .with_timezone(&chrono::Local)
                    .date_naive()
            })
            .collect::<std::collections::HashSet<_>>()
            .len();
        let drive = FakeDrive::start(
            tree(),
            Behavior {
                audit_page_size: Some(2),
                audit_bounds: bounds,
                ..behavior(log)
            },
        )
        .await;

        let ran = activity(
            &drive,
            Settings {
                since: When::At(since),
                until: Some(When::At(until)),
                active_only: true,
                ..settings(&["old"])
            },
        )
        .await;

        ran.assert_ok();
        assert_eq!(ran.rows()[0][4], total.to_string(), "{bounds:?}: views");
        assert_eq!(ran.rows()[0][3], days.to_string(), "{bounds:?}: days");
        assert!(
            ran.err
                .text()
                .contains(&format!("{total} of them about these items")),
            "{bounds:?}: {}",
            ran.err.text()
        );
        if bounds == Bounds::StartIncluded {
            assert!(ran.err.text().contains(&format!("Read {total} events")));
        }
        assert_eq!(windows(&drive), expected_windows(since, until, None));
        assert!(drive.audit_requests().len() > 30 * 2, "pages are followed");
        assert_eq!(
            ran.summary(),
            format!(
                "Summary: 1 person active from {} to {}",
                format_date_time(since),
                format_date_time(until)
            )
        );
    }
}

#[tokio::test]
async fn a_since_older_than_the_audit_log_is_clamped_with_a_warning() {
    let drive = FakeDrive::start(tree(), Behavior::default()).await;

    let ran = activity(
        &drive,
        Settings {
            since: When::Ago(TimeDelta::days(200)),
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    let horizon = now() - TimeDelta::days(180);
    assert!(ran.err.lines().contains(&format!(
        "Warning: the audit log keeps 180 days; reading from {} instead of {}",
        format_date_time(horizon),
        format_date_time(now() - TimeDelta::days(200))
    )));
    let windows = windows(&drive);
    assert_eq!(windows.len(), 180);
    assert_eq!(windows[0].0, rfc3339(horizon));
}

#[tokio::test]
async fn an_empty_or_forgotten_time_range_is_an_error_before_any_request() {
    let drive = FakeDrive::start(tree(), Behavior::default()).await;
    let day = at("2026-11-01T00:00:00Z");

    let ran = activity(
        &drive,
        Settings {
            since: When::At(day),
            until: Some(When::At(day)),
            ..settings(&["old"])
        },
    )
    .await;
    let empty = format_date_time(day);
    assert_eq!(
        ran.error(),
        format!("Error: --since ({empty}) must be before --until ({empty})")
    );

    let ran = activity(
        &drive,
        Settings {
            since: When::Ago(TimeDelta::days(365)),
            until: Some(When::Ago(TimeDelta::days(200))),
            ..settings(&["old"])
        },
    )
    .await;
    assert_eq!(
        ran.error(),
        format!(
            "Error: the audit log keeps 180 days, from {}: --until must be later",
            format_date_time(now() - TimeDelta::days(180))
        )
    );
    assert!(drive.gets().is_empty() && drive.audit_requests().is_empty());
}

fn google_error(code: u16, message: &str, reason: &str) -> Value {
    json!({"error": {
        "code": code,
        "message": message,
        "errors": [{"domain": "global", "reason": reason, "message": message}],
    }})
}

#[tokio::test]
async fn each_failure_of_the_audit_log_names_what_to_do() {
    let enable = "Admin SDK API has not been used in project 123 before or it is disabled. Enable \
                  it by visiting https://console.developers.google.com/apis/api/admin.googleapis\
                  .com/overview?project=123 then retry.";
    let mut files = tree();
    files.extend(shared_drive());
    for (status, body, scan, expected) in [
        (
            403,
            google_error(
                403,
                "Request had insufficient authentication scopes.",
                "insufficientPermissions",
            ),
            Scan::Domain,
            String::from(
                "Error: unable to read the audit log: Google answered 403 Forbidden: Request had \
                 insufficient authentication scopes.\nHint: the account is not authorized to read \
                 the audit log; run 'gdrive account reauth admin@example.com --audit-log'",
            ),
        ),
        (
            403,
            google_error(403, enable, "accessNotConfigured"),
            Scan::Domain,
            format!(
                "Error: unable to read the audit log: Google answered 403 Forbidden: {enable}\n\
                 Hint: enable the Admin SDK API in the Google Cloud project of the account's \
                 OAuth client (Google's message names the project and has the link), wait a few \
                 minutes and run the command again"
            ),
        ),
        (
            401,
            google_error(
                401,
                "Access denied. You are not authorized to read activity records.",
                "authError",
            ),
            Scan::Domain,
            String::from(
                "Error: unable to read the audit log: Google answered 401 Unauthorized: Access \
                 denied. You are not authorized to read activity records.\nHint: the Google \
                 account must be an administrator of the Google Workspace organization, with the \
                 Reports privilege; personal Gmail accounts have no audit log",
            ),
        ),
        (
            400,
            google_error(400, "Invalid value 'filters'", "invalid"),
            Scan::Drive,
            String::from(
                "Error: unable to read the audit log: Google answered 400 Bad Request: Invalid \
                 value 'filters'\nHint: Google refused the filter of this scan; run the command \
                 again with --scan domain",
            ),
        ),
        (
            400,
            google_error(400, "Invalid value 'startTime'", "invalid"),
            Scan::Domain,
            String::from(
                "Error: unable to read the audit log: Google answered 400 Bad Request: Invalid \
                 value 'startTime'",
            ),
        ),
    ] {
        let drive = FakeDrive::start(
            files.clone(),
            Behavior {
                audit_error: Some((status, body)),
                ..Behavior::default()
            },
        )
        .await;
        let ran = activity(
            &drive,
            Settings {
                scan,
                ..settings(&["proj"])
            },
        )
        .await;
        assert_eq!(ran.error(), expected);
        ran.assert_no_secrets();
        assert!(
            drive.audit_requests().len() <= 4,
            "not retried: {}",
            drive.audit_requests().len()
        );
        assert!(ran.out.text().is_empty(), "no report: {}", ran.out.text());
    }
}

#[tokio::test]
async fn a_rate_limit_is_waited_for_and_retried() {
    let drive = FakeDrive::start(
        tree(),
        Behavior {
            audit_rate_limited: 1,
            ..behavior(vec![record(
                "2026-11-01T12:00:00Z",
                Some("ana@example.com"),
                vec![event("view", "a")],
            )])
        },
    )
    .await;

    let ran = activity(
        &drive,
        Settings {
            jobs: NonZeroUsize::new(1).unwrap(),
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    assert_eq!(drive.audit_requests().len(), 31);
    assert_eq!(ran.rows().len(), 1);
}

#[tokio::test]
async fn requests_run_up_to_jobs_at_once() {
    for jobs in [1, 3] {
        let drive = FakeDrive::start(
            tree(),
            Behavior {
                audit_rendezvous: jobs,
                ..Behavior::default()
            },
        )
        .await;
        let ran = activity(
            &drive,
            Settings {
                jobs: NonZeroUsize::new(jobs).unwrap(),
                active_only: true,
                ..settings(&["old"])
            },
        )
        .await;
        ran.assert_ok();
        assert_eq!(drive.audit_max_in_flight(), jobs);
    }
}

#[tokio::test]
async fn a_tree_that_is_not_listed_completely_fails_before_the_audit_log() {
    for (behavior, ids, expected) in [
        (
            Behavior {
                unreadable: [String::from("sub")].into(),
                ..Behavior::default()
            },
            &["old"][..],
            "Error: unable to list the folder 'old/sub'",
        ),
        (
            Behavior {
                incomplete: [String::from("sub")].into(),
                ..Behavior::default()
            },
            &["old"],
            "Error: Drive did not list 'old/sub' completely, the report could miss what is in \
             it; run the command again later",
        ),
        (
            Behavior::default(),
            &["old", "missing"],
            "Error: unable to get 'missing'",
        ),
        (
            Behavior {
                permissions_unreadable: [String::from("old")].into(),
                ..Behavior::default()
            },
            &["old"],
            "Error: unable to read who has access to 'old' (--active-only goes without it)",
        ),
    ] {
        let drive = FakeDrive::start(tree(), behavior).await;
        let ran = activity(&drive, settings(ids)).await;
        assert!(ran.error().starts_with(expected), "{}", ran.error());
        assert!(drive.audit_requests().is_empty());
        assert_eq!(ran.out.text(), "");
    }
}

#[tokio::test]
async fn hostile_names_and_addresses_are_escaped_in_every_output() {
    let mut files = tree();
    files.push(FakeFile::blob(
        "evil",
        "tab\there\nnew\u{1b}[2J\\.txt",
        "old",
        "x",
    ));
    let log = vec![
        record(
            "2026-11-01T12:00:00Z",
            Some("mallory\u{1b}]0;owned\u{7}@example.com"),
            vec![event("view\r\nforged", "evil")],
        ),
        record(
            "2026-11-02T12:00:00Z",
            Some("ana@example.com"),
            vec![event("view", "evil")],
        ),
    ];
    let drive = FakeDrive::start(files, behavior(log)).await;

    let ran = activity(
        &drive,
        Settings {
            by: By::Event,
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    let lines = ran.out.lines();
    assert_eq!(lines.len(), 3, "one line per row: {lines:?}");
    assert_eq!(
        ran.rows()[0][1..],
        [
            String::from("mallory\\u{1b}]0;owned\\u{7}@example.com"),
            String::from("view\\r\\nforged"),
            String::from("old/tab\\there\\nnew\\u{1b}[2J\\\\.txt"),
        ]
    );

    let ran = activity(
        &drive,
        Settings {
            by: By::Event,
            output: Output::Table {
                skip_header: false,
                field_separator: String::from("\t"),
            },
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    assert_eq!(ran.out.lines().len(), 3);
    assert!(ran.out.lines()[1].contains("old/tab\\there\\nnew\\u{1b}[2J\\.txt"));
    for text in [ran.out.text(), ran.err.text()] {
        assert!(!text.contains(['\u{1b}', '\u{7}', '\r']), "{text:?}");
    }
}

#[tokio::test]
async fn the_table_aligns_columns_and_takes_the_flags_of_files_list() {
    let drive = FakeDrive::start(tree(), behavior(migrated_log())).await;
    let table = |skip_header: bool, field_separator: &str| Output::Table {
        skip_header,
        field_separator: field_separator.to_owned(),
    };

    let ran = activity(
        &drive,
        Settings {
            output: table(false, "\t"),
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    let lines = ran.out.lines();
    assert_eq!(lines.len(), 5);
    assert!(
        lines[0].starts_with("User                   Access   Last activity"),
        "{}",
        lines[0]
    );
    // Nobody is on a sharing list: the Access column is `-` below its header.
    let access = lines[0].find("Access").unwrap();
    assert!(
        lines[1..].iter().all(|line| line.find('-') == Some(access)),
        "{lines:?}"
    );
    assert!(!ran.out.text().contains('\t'));

    let ran = activity(
        &drive,
        Settings {
            output: table(true, ","),
            active_only: true,
            ..settings(&["old"])
        },
    )
    .await;
    ran.assert_ok();
    let lines = ran.out.lines();
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[0],
        format!(
            "ana@example.com,-,{},2,1,0,1,0,1",
            local("2026-11-03T12:00:00Z")
        )
    );
}

#[tokio::test]
async fn a_long_scan_prints_how_far_it_got() {
    let drive = FakeDrive::start(tree(), behavior(migrated_log())).await;

    let ran = activity(
        &drive,
        Settings {
            progress_every: Some(Duration::ZERO),
            jobs: NonZeroUsize::new(1).unwrap(),
            ..settings(&["old"])
        },
    )
    .await;

    ran.assert_ok();
    let progress = ran
        .err
        .lines()
        .into_iter()
        .filter(|line| line.contains(" so far, "))
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 30, "{progress:?}");
    assert_eq!(
        progress[0],
        "Read 0 events so far, 0 about these items (0 of 30 windows done)"
    );
    assert_eq!(
        progress[29],
        "Read 7 events so far, 7 about these items (29 of 30 windows done)"
    );
}

#[test]
fn account_errors_name_the_account_and_the_commands_to_run() {
    use crate::common::hub_helper::GetHubError;

    let message = crate::error_message(&Error::Account {
        account: Some(String::from("old@example.com")),
        source: GetHubError::AccountNotFound(String::from("old@example.com")),
    });
    assert_eq!(
        message,
        "Error: unable to use the account 'old@example.com': account 'old@example.com' not \
         found; run 'gdrive account list' to see the accounts, or 'gdrive account add' to add one"
    );
}
