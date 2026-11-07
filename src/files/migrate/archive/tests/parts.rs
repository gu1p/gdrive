//! `files migrate --archive --max-part-size`: every part unpacks alone, and a later run sends only
//! what no part holds.

use std::collections::HashMap;

use super::*;

/// With these entries, a part of at most this many bytes holds what [`photos`] promises.
const MAX: u64 = 5000;

const INDEX: &str = "photos.parts.json";

fn in_parts(file_ids: &[&str]) -> Settings {
    Settings {
        max_part_size: Some(MAX),
        ..settings(file_ids)
    }
}

fn going_on(settings: Settings) -> Settings {
    Settings {
        transfer: Options::new(NonZeroUsize::new(4).unwrap(), true),
        ..settings
    }
}

fn thousand(c: char) -> String {
    std::iter::repeat_n(c, 1000).collect()
}

fn blob_of(id: &str, name: &str, content: &str) -> FakeFile {
    FakeFile::blob(id, name, "photos", content).modified(FILE_TIME)
}

/// `photos` with five files of 1000 bytes, an entry of 1536 bytes each. The folder's entry has
/// 512, and the end of a tar 1024, so that [`MAX`] makes parts of: the folder, `a` and `b`; `c`
/// and `d`; `e`.
fn photos() -> Vec<FakeFile> {
    let mut files = vec![FakeFile::folder("photos", "photos", "root").modified(FOLDER_TIME)];
    for name in ['a', 'b', 'c', 'd', 'e'] {
        let id = name.to_string();
        files.push(blob_of(&id, &format!("{id}.bin"), &thousand(name)));
    }
    files
}

fn is_part(created: &Created) -> bool {
    created.name.contains(".part") && created.name.contains(".tar")
}

/// The parts `dest` got, in the order of their upload.
fn parts_of(dest: &FakeDrive) -> Vec<Created> {
    dest.created().into_iter().filter(is_part).collect()
}

fn names_of(dest: &FakeDrive) -> Vec<String> {
    parts_of(dest).into_iter().map(|part| part.name).collect()
}

fn paths(part: &Created) -> Vec<String> {
    unpack(&part.content)
        .into_iter()
        .map(|(path, ..)| path)
        .collect()
}

/// The part of that name.
fn part(dest: &FakeDrive, name: &str) -> Created {
    parts_of(dest)
        .into_iter()
        .find(|part| part.name == name)
        .unwrap_or_else(|| panic!("no part {name}"))
}

/// The index `dest` holds now.
fn index_of(dest: &FakeDrive) -> serde_json::Value {
    let created = dest.created();
    let file = created.iter().find(|file| file.name == INDEX).unwrap();
    let stored = dest.stored(&file.id).unwrap();
    serde_json::from_str(&stored.content.unwrap()).unwrap()
}

/// The parts the index names, with the paths of their entries.
fn listed(index: &serde_json::Value) -> Vec<(String, Vec<String>)> {
    index["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| {
            let paths = part["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["path"].as_str().unwrap().to_owned())
                .collect();
            (part["name"].as_str().unwrap().to_owned(), paths)
        })
        .collect()
}

fn missing_media(id: &str) -> Behavior {
    Behavior {
        media_missing: HashMap::from([(id.to_owned(), Duration::ZERO)]),
        ..Behavior::default()
    }
}

#[tokio::test]
async fn a_folder_arrives_in_parts_that_each_unpack_alone() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    migrated.assert_ok();
    let parts = parts_of(&dest);
    let sizes = parts.iter().map(|p| p.content.len()).collect::<Vec<_>>();
    assert_eq!(sizes, [4608, 4096, 2560]);
    assert_eq!(
        names_of(&dest),
        [
            "photos.part00001.tar",
            "photos.part00002.tar",
            "photos.part00003.tar"
        ]
    );
    assert_eq!(
        paths(&parts[0]),
        ["photos/", "photos/a.bin", "photos/b.bin"]
    );
    assert_eq!(paths(&parts[1]), ["photos/c.bin", "photos/d.bin"]);
    assert_eq!(paths(&parts[2]), ["photos/e.bin"]);
    assert_eq!(unpack(&parts[2].content)[0].3, thousand('e').as_bytes());
    for part in &parts {
        assert_eq!(part.mime_type, MEDIA_TYPE);
        assert_eq!(part.parents, ["root"]);
        assert!(part.app_properties.is_empty(), "marked once it is checked");
        let stored = dest.stored(&part.id).unwrap().app_properties;
        let md5 = format!("{:x}", md5::compute(&part.content));
        assert_eq!(stored.get(PROPERTY_MD5), Some(&md5));
        assert_eq!(stored[PROPERTY].len(), 64);
    }

    let index = index_of(&dest);
    assert_eq!(index["source"], "photos");
    let expected = parts
        .iter()
        .map(|part| (part.name.clone(), paths(part)))
        .map(|(name, mut paths)| {
            paths[0] = paths[0].trim_end_matches('/').to_owned();
            (name, paths)
        })
        .collect::<Vec<_>>();
    assert_eq!(listed(&index), expected);
    assert_eq!(
        dest.updates().len(),
        2,
        "the one index, written again for the later parts"
    );

    assert_eq!(
        migrated.out.lines()[1..],
        [
            "create photos.parts.json",
            "create photos.part00001.tar (4.6 KB)",
            "create photos.part00002.tar (4.1 KB)",
            "create photos.part00003.tar (2.6 KB)",
        ]
    );
    assert_eq!(
        migrated.summary(),
        "Summary: 3 archive parts migrated (11.3 KB), 0 already there, 0 skipped (unsupported), 0 \
         failed, 0 cancelled"
    );
    assert_eq!(source.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn a_later_part_extracts_alone_into_an_empty_folder() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();
    let last = part(&dest, "photos.part00003.tar");
    let dir = tempfile::tempdir().unwrap();

    tar::Archive::new(last.content.as_slice())
        .unpack(dir.path())
        .unwrap();

    let extracted = std::fs::read(dir.path().join("photos").join("e.bin")).unwrap();
    assert_eq!(extracted, thousand('e').as_bytes());
    assert_eq!(files_in(&dir.path().join("photos")), ["e.bin"]);
}

#[tokio::test]
async fn a_second_run_finds_every_part_there_and_sends_nothing_whatever_the_maximum() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();
    let (media, writes) = (source.media().len(), dest.writes().len());

    for max in [MAX, 100] {
        let settings = Settings {
            max_part_size: Some(max),
            ..settings(&["photos"])
        };
        let again = migrate(&source, &dest, settings).await;

        again.assert_ok();
        assert_eq!(
            again.out.lines()[1..],
            [
                "already-there photos.part00001.tar",
                "already-there photos.part00002.tar",
                "already-there photos.part00003.tar",
            ]
        );
        assert_eq!(
            again.summary(),
            "Summary: 0 archive parts migrated (0 B), 3 already there, 0 skipped (unsupported), \
             0 failed, 0 cancelled"
        );
        assert_eq!(source.media().len(), media, "no file read");
        assert_eq!(dest.writes().len(), writes, "nothing written");
    }
}

#[tokio::test]
async fn after_a_failure_only_what_no_part_holds_goes_up() {
    let failing = FakeDrive::start(photos(), missing_media("c")).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let first = migrate(&failing, &dest, in_parts(&["photos"])).await;

    assert!(first.result.is_err());
    assert_eq!(names_of(&dest), ["photos.part00001.tar"]);
    assert!(
        first
            .err
            .text()
            .contains("Error: unable to migrate 'photos.part00002.tar'"),
        "{}",
        first.err.text()
    );
    assert!(
        first.summary().starts_with(
            "Summary: 1 archive part migrated (4.6 KB), 0 already there, 0 skipped (unsupported), \
             1 failed"
        ),
        "{}",
        first.summary()
    );
    // Named before it went up.
    let named = listed(&index_of(&dest));
    assert_eq!(named.len(), 2, "{named:?}");
    assert_eq!(named[1].0, "photos.part00002.tar");

    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let second = migrate(&source, &dest, in_parts(&["photos"])).await;

    second.assert_ok();
    assert_eq!(source.media(), ["c", "d", "e"], "not a, not b");
    assert_eq!(
        second.out.lines()[1..],
        [
            "already-there photos.part00001.tar",
            "create photos.part00002.tar (4.1 KB)",
            "create photos.part00003.tar (2.6 KB)",
        ]
    );
    assert_eq!(
        second.summary(),
        "Summary: 2 archive parts migrated (6.7 KB), 1 already there, 0 skipped (unsupported), 0 \
         failed, 0 cancelled"
    );
    assert_eq!(
        names_of(&dest),
        [
            "photos.part00001.tar",
            "photos.part00002.tar",
            "photos.part00003.tar"
        ]
    );
    assert_eq!(listed(&index_of(&dest)).len(), 3);
}

#[tokio::test]
async fn with_continue_on_error_the_parts_after_a_failed_one_go_up() {
    let failing = FakeDrive::start(photos(), missing_media("c")).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let first = migrate(&failing, &dest, going_on(in_parts(&["photos"]))).await;

    assert!(first.result.is_err(), "the run still fails");
    assert_eq!(
        names_of(&dest),
        ["photos.part00001.tar", "photos.part00003.tar"]
    );
    assert_eq!(
        paths(&part(&dest, "photos.part00003.tar")),
        ["photos/e.bin"]
    );
    assert!(
        first
            .summary()
            .contains("2 archive parts migrated (7.2 KB)")
            && first.summary().contains(", 1 failed,"),
        "{}",
        first.summary()
    );

    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let second = migrate(&source, &dest, going_on(in_parts(&["photos"]))).await;

    second.assert_ok();
    assert_eq!(source.media(), ["c", "d"]);
    assert_eq!(
        second.out.lines()[1..],
        [
            "already-there photos.part00001.tar",
            "already-there photos.part00003.tar",
            "create photos.part00004.tar (4.1 KB)",
        ]
    );
    let named = listed(&index_of(&dest))
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    assert_eq!(
        named,
        [
            "photos.part00001.tar",
            "photos.part00003.tar",
            "photos.part00004.tar"
        ]
    );
}

#[tokio::test]
async fn every_part_that_fails_is_told_and_counted() {
    let behavior = Behavior {
        media_missing: HashMap::from([
            (String::from("c"), Duration::ZERO),
            (String::from("e"), Duration::ZERO),
        ]),
        ..Behavior::default()
    };
    let failing = FakeDrive::start(photos(), behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let first = migrate(&failing, &dest, going_on(in_parts(&["photos"]))).await;

    assert!(first.result.is_err());
    let text = first.err.text();
    for part in ["photos.part00002.tar", "photos.part00003.tar"] {
        assert!(
            text.contains(&format!("Error: unable to migrate '{part}'")),
            "{text}"
        );
    }
    assert!(first.summary().contains(", 2 failed,"), "{text}");
}

#[tokio::test]
async fn a_failure_is_told_and_counted_even_when_a_ctrl_c_follows() {
    let behavior = Behavior {
        media_missing: HashMap::from([(String::from("c"), Duration::ZERO)]),
        media_stalled: HashSet::from([String::from("e")]),
        ..Behavior::default()
    };
    let failing = FakeDrive::start(photos(), behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let first = migrate_with(
        &failing,
        &dest,
        going_on(in_parts(&["photos"])),
        &UploadOptions::default(),
        tokio::time::sleep(Duration::from_millis(500)),
    )
    .await;

    assert!(first.result.is_err());
    let text = first.err.text();
    assert!(
        text.contains("Error: unable to migrate 'photos.part00002.tar'"),
        "{text}"
    );
    assert!(
        first
            .summary()
            .contains(", 1 failed, 1 cancelled; interrupted"),
        "{}",
        first.summary()
    );
}

#[tokio::test]
async fn a_skip_counts_also_when_the_part_fails() {
    let mut files = photos();
    files
        .push(FakeFile::native("doc", "notes", "photos", DOCUMENT, "too much").modified(FILE_TIME));
    let behavior = Behavior {
        export_too_large: HashSet::from([String::from("doc")]),
        media_missing: HashMap::from([(String::from("a"), Duration::ZERO)]),
        ..Behavior::default()
    };
    let failing = FakeDrive::start(files, behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let settings = Settings {
        max_part_size: Some(1_000_000),
        ..settings(&["photos"])
    };

    let first = migrate(&failing, &dest, settings).await;

    assert!(first.result.is_err());
    assert!(
        first
            .summary()
            .contains(", 1 skipped (unsupported), 1 failed"),
        "{}",
        first.summary()
    );
}

#[tokio::test]
async fn what_was_added_or_edited_later_goes_into_a_new_part() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();

    // `bb.bin` is new, `d.bin` was edited, and `e.bin` is gone from the source.
    let mut files = photos();
    files.retain(|file| file.id != "d" && file.id != "e");
    files.push(blob_of("bb", "bb.bin", &thousand('n')));
    files.push(blob_of("d", "d.bin", &thousand('x')));
    let later = FakeDrive::start(files, Behavior::default()).await;

    let second = migrate(&later, &dest, in_parts(&["photos"])).await;

    second.assert_ok();
    let mut media = later.media();
    media.sort();
    assert_eq!(media, ["bb", "d"], "only those two");
    assert_eq!(
        paths(&part(&dest, "photos.part00004.tar")),
        ["photos/bb.bin", "photos/d.bin"]
    );

    // Extracted in their order, the parts give the newest content.
    let extracted = extracted(&dest);
    assert_eq!(extracted["photos/d.bin"], thousand('x').as_bytes());
    assert_eq!(extracted["photos/bb.bin"], thousand('n').as_bytes());
    assert_eq!(
        extracted["photos/e.bin"],
        thousand('e').as_bytes(),
        "what left the source stays in its part"
    );
}

/// The content of each path once `dest`'s parts are extracted in the order of their names.
fn extracted(dest: &FakeDrive) -> BTreeMap<String, Vec<u8>> {
    let mut names = names_of(dest);
    names.sort();
    let mut extracted = BTreeMap::new();
    for name in names {
        for (path, _, _, content) in unpack(&part(dest, &name).content) {
            extracted.insert(path, content);
        }
    }
    extracted
}

#[tokio::test]
async fn a_file_edited_back_to_what_an_older_part_holds_goes_up_again() {
    let edited = |c: char| {
        let mut files = photos();
        files.retain(|file| file.id != "d");
        files.push(blob_of("d", "d.bin", &thousand(c)));
        files
    };
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    for content in ['d', 'x', 'd'] {
        let source = FakeDrive::start(edited(content), Behavior::default()).await;
        migrate(&source, &dest, in_parts(&["photos"]))
            .await
            .assert_ok();
        assert_eq!(
            extracted(&dest)["photos/d.bin"],
            thousand(content).as_bytes(),
            "the source's content, after the edit to {content}"
        );
    }
    assert_eq!(names_of(&dest).len(), 5, "one part for each edit");

    // Nothing changed since.
    let source = FakeDrive::start(edited('d'), Behavior::default()).await;
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();
    assert_eq!(source.media(), Vec::<String>::new());
}

#[tokio::test]
async fn a_free_number_is_never_filled_so_that_the_names_give_the_order_of_the_uploads() {
    let failing = FakeDrive::start(photos(), missing_media("c")).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    // Part 2 fails, part 3 holds `e`.
    let first = migrate(&failing, &dest, going_on(in_parts(&["photos"]))).await;
    assert!(first.result.is_err());

    // Then `e` is edited, and goes up with what failed.
    let mut files = photos();
    files.retain(|file| file.id != "e");
    files.push(blob_of("e", "e.bin", &thousand('x')));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let second = migrate(&source, &dest, in_parts(&["photos"])).await;

    second.assert_ok();
    assert_eq!(
        extracted(&dest)["photos/e.bin"],
        thousand('x').as_bytes(),
        "not the older content of part 3"
    );
}

#[tokio::test]
async fn a_file_larger_than_the_maximum_is_alone_in_its_part() {
    let mut files = vec![FakeFile::folder("photos", "photos", "root").modified(FOLDER_TIME)];
    files.push(blob_of("a", "a.bin", &thousand('a')));
    let huge = std::iter::repeat_n('h', 6000).collect::<String>();
    files.push(blob_of("h", "h.bin", &huge));
    files.push(blob_of("z", "z.bin", &thousand('z')));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    migrated.assert_ok();
    let parts = parts_of(&dest);
    assert_eq!(paths(&parts[0]), ["photos/", "photos/a.bin"]);
    assert_eq!(paths(&parts[1]), ["photos/h.bin"]);
    assert_eq!(paths(&parts[2]), ["photos/z.bin"]);
    assert_eq!(parts[1].content.len(), 7680);
    assert_eq!(
        migrated.out.lines()[2..],
        [
            "create photos.part00001.tar (3.1 KB)",
            "create photos.part00002.tar (7.7 KB, larger than the maximum: a file is never split)",
            "create photos.part00003.tar (2.6 KB)",
        ]
    );
}

#[tokio::test]
async fn a_part_that_is_gone_or_has_no_mark_proves_nothing() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();

    // Part 1 was deleted, part 2 lost its mark.
    let stored = |name: &str| {
        let created = dest.created();
        let file = created.iter().find(|file| file.name == name).unwrap();
        dest.stored(&file.id).unwrap()
    };
    let mut unmarked = stored("photos.part00002.tar");
    unmarked.app_properties.clear();
    let unmarked_id = unmarked.id.clone();
    let mut index = stored(INDEX);
    index.id = String::from("index");
    let files = vec![index, unmarked, stored("photos.part00003.tar")];
    let later = FakeDrive::start(files, Behavior::default()).await;

    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let second = migrate(&source, &later, in_parts(&["photos"])).await;

    second.assert_ok();
    assert_eq!(source.media(), ["a", "b", "c", "d"], "not e");
    assert_eq!(
        second.out.lines()[1..],
        [
            "already-there photos.part00003.tar",
            "create photos.part00004.tar (4.6 KB)",
            "create photos.part00005.tar (4.1 KB)",
        ]
    );
    let text = second.err.text();
    assert!(
        text.contains(
            "Warning: photos.part00002.tar has no mark of a checked part: what it was to hold goes \
             into a new part, and it can be deleted"
        ),
        "{text}"
    );
    assert!(
        later.stored(&unmarked_id).is_some() && later.deletes().is_empty(),
        "nothing is deleted"
    );
    let named = listed(&index_of_stored(&later))
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    assert_eq!(
        named,
        [
            "photos.part00003.tar",
            "photos.part00004.tar",
            "photos.part00005.tar"
        ],
        "the index no longer names what proved nothing"
    );

    // The third run finds everything.
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let third = migrate(&source, &later, in_parts(&["photos"])).await;
    third.assert_ok();
    assert_eq!(source.media(), Vec::<String>::new());
}

/// The index of a drive that was started with it.
fn index_of_stored(dest: &FakeDrive) -> serde_json::Value {
    serde_json::from_str(&dest.stored("index").unwrap().content.unwrap()).unwrap()
}

#[tokio::test]
async fn a_part_that_does_not_hold_what_the_index_says_proves_nothing() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();

    // The index claims that part 3 also holds what part 2 holds.
    let created = dest.created();
    let file = created.iter().find(|file| file.name == INDEX).unwrap();
    let mut stored = dest.stored(&file.id).unwrap();
    stored.id = String::from("index");
    let mut index =
        serde_json::from_str::<serde_json::Value>(stored.content.as_ref().unwrap()).unwrap();
    let second_part = index["parts"][1]["entries"].as_array().unwrap().clone();
    index["parts"][2]["entries"]
        .as_array_mut()
        .unwrap()
        .extend(second_part);
    stored.content = Some(index.to_string());
    let mut files = vec![stored];
    for part in parts_of(&dest) {
        files.push(dest.stored(&part.id).unwrap());
    }
    let later = FakeDrive::start(files, Behavior::default()).await;

    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let second = migrate(&source, &later, in_parts(&["photos"])).await;

    second.assert_ok();
    let reads = source.media();
    assert!(reads.contains(&String::from("e")), "{reads:?}");
    assert!(!reads.contains(&String::from("a")), "{reads:?}");
    assert_eq!(listed(&index_of_stored(&later)).len(), 3);
}

#[tokio::test]
async fn a_file_of_the_name_of_the_index_that_is_none_fails_and_stays() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let theirs = FakeFile::blob("theirs", INDEX, "root", "my own notes");
    let dest = FakeDrive::start(vec![theirs], Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    assert!(migrated.result.is_err());
    let text = migrated.err.text();
    assert!(
        text.contains(
            "'photos.parts.json' is in the destination and is no list of the parts of an archive; \
             rename or remove it"
        ),
        "{text}"
    );
    assert!(dest.created().is_empty() && dest.updates().is_empty());
    assert_eq!(source.media(), Vec::<String>::new());
}

#[tokio::test]
async fn the_index_of_another_source_of_the_same_name_is_left_alone() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let other = json!({ "source": "another", "parts": [] }).to_string();
    let theirs = FakeFile::blob("theirs", INDEX, "root", &other);
    let dest = FakeDrive::start(vec![theirs], Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    migrated.assert_ok();
    assert_eq!(dest.stored("theirs").unwrap().content.unwrap(), other);
    assert_eq!(index_of(&dest)["source"], "photos");
    assert_eq!(
        dest.updates()
            .iter()
            .filter(|(id, _)| id == "theirs")
            .count(),
        0
    );
}

#[tokio::test]
async fn a_file_with_the_name_of_a_part_stays_and_the_part_takes_the_next_number() {
    let source = FakeDrive::start(photos(), Behavior::default()).await;
    let theirs = FakeFile::blob("theirs", "photos.part00001.tar", "root", "my own");
    let dest = FakeDrive::start(vec![theirs], Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    migrated.assert_ok();
    assert_eq!(
        names_of(&dest),
        [
            "photos.part00002.tar",
            "photos.part00003.tar",
            "photos.part00004.tar"
        ]
    );
    assert_eq!(dest.stored("theirs").unwrap().content.unwrap(), "my own");
    assert!(dest.stored("theirs").unwrap().app_properties.is_empty());
}

#[tokio::test]
async fn a_dry_run_tells_the_parts_and_writes_nothing() {
    let mut files = photos();
    files
        .push(FakeFile::native("doc", "notes", "photos", DOCUMENT, "exported").modified(FILE_TIME));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;
    let dry = |ids: &[&str]| Settings {
        dry_run: true,
        ..in_parts(ids)
    };

    let first = migrate(&source, &dest, dry(&["photos"])).await;

    first.assert_ok();
    assert_eq!(
        first.out.lines()[1..],
        [
            "create photos.parts.json",
            "create photos.part00001.tar (4.6 KB)",
            "create photos.part00002.tar (4.1 KB)",
            "create photos.part00003.tar (3.1 KB without the exports of 1 Google Workspace file)",
        ]
    );
    assert_eq!(
        first.summary(),
        "Dry run, nothing was changed. Summary: 3 archive parts to migrate (11.8 KB without the \
         exports of 1 Google Workspace file), 0 already there, 0 skipped (unsupported), 0 failed, \
         0 cancelled"
    );
    assert_eq!(dest.writes(), Vec::<String>::new());
    assert_eq!(source.writes(), Vec::<String>::new());
    assert_eq!(source.exports(), Vec::<(String, String)>::new());
    assert_eq!(source.media(), Vec::<String>::new());

    // What a real run made is told as already there, and the index is not new anymore.
    migrate(&source, &dest, in_parts(&["photos"]))
        .await
        .assert_ok();
    let mut files = photos();
    files.push(blob_of("z", "z.bin", &thousand('z')));
    files
        .push(FakeFile::native("doc", "notes", "photos", DOCUMENT, "exported").modified(FILE_TIME));
    let later = FakeDrive::start(files, Behavior::default()).await;
    let writes = dest.writes().len();

    let second = migrate(&later, &dest, dry(&["photos"])).await;

    second.assert_ok();
    assert_eq!(
        second.out.lines()[1..],
        [
            "already-there photos.part00001.tar",
            "already-there photos.part00002.tar",
            "already-there photos.part00003.tar",
            "create photos.part00004.tar (2.6 KB)",
        ]
    );
    assert_eq!(dest.writes().len(), writes);
}

#[tokio::test]
async fn an_export_counts_with_its_real_size_and_is_made_once() {
    let export = std::iter::repeat_n('x', 3000).collect::<String>();
    let mut files = vec![FakeFile::folder("photos", "photos", "root").modified(FOLDER_TIME)];
    files.push(blob_of("a", "a.bin", &thousand('a')));
    files.push(FakeFile::native("doc", "notes", "photos", DOCUMENT, &export).modified(FILE_TIME));
    files.push(blob_of("z", "z.bin", &thousand('z')));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    migrated.assert_ok();
    let parts = parts_of(&dest);
    assert_eq!(paths(&parts[0]), ["photos/", "photos/a.bin"]);
    assert_eq!(paths(&parts[1]), ["photos/notes.docx"]);
    assert_eq!(paths(&parts[2]), ["photos/z.bin"]);
    assert_eq!(unpack(&parts[1].content)[0].3, export.as_bytes());
    assert!(
        migrated
            .summary()
            .contains("3 archive parts migrated (10.2 KB, 1 Google Workspace file exported)"),
        "{}",
        migrated.summary()
    );
    assert_eq!(source.exports().len(), 1);

    let again = migrate(&source, &dest, in_parts(&["photos"])).await;
    again.assert_ok();
    assert_eq!(
        source.exports().len(),
        1,
        "a part holds it: not exported again"
    );
}

#[tokio::test]
async fn an_export_over_the_limit_is_left_out_of_the_parts_and_counted() {
    let mut files = photos();
    files
        .push(FakeFile::native("doc", "notes", "photos", DOCUMENT, "too much").modified(FILE_TIME));
    let behavior = Behavior {
        export_too_large: HashSet::from([String::from("doc")]),
        ..Behavior::default()
    };
    let source = FakeDrive::start(files, behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["photos"])).await;

    migrated.assert_ok();
    let too_large = RelayError::ExportTooLarge {
        limit: EXPORT_LIMIT,
    };
    let skip = format!("skip photos/notes: {too_large}");
    assert!(
        migrated.out.lines().contains(&skip),
        "{}",
        migrated.out.text()
    );
    let all = parts_of(&dest).iter().flat_map(paths).collect::<Vec<_>>();
    assert!(!all.iter().any(|path| path.contains("notes")), "{all:?}");
    assert!(
        migrated.summary().contains(", 1 skipped (unsupported)"),
        "{}",
        migrated.summary()
    );
}

#[tokio::test]
async fn a_lone_doc_over_the_limit_is_one_skipped_item_and_makes_nothing() {
    let doc = FakeFile::native("doc", "notes", "root", DOCUMENT, "too much").modified(FILE_TIME);
    let behavior = Behavior {
        export_too_large: HashSet::from([String::from("doc")]),
        ..Behavior::default()
    };
    let source = FakeDrive::start(vec![doc], behavior).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let migrated = migrate(&source, &dest, in_parts(&["doc"])).await;

    migrated.assert_ok();
    assert_eq!(dest.writes(), Vec::<String>::new());
    assert!(
        migrated.summary().contains(", 1 skipped (unsupported)"),
        "{}",
        migrated.summary()
    );
}

/// `big`: a first part below the first chunk of an upload, then a second one with a Doc whose
/// exports differ every time, above it.
fn big() -> Vec<FakeFile> {
    let content = |c: char, size: usize| std::iter::repeat_n(c, size).collect::<String>();
    vec![
        FakeFile::folder("big", "big", "root"),
        FakeFile::blob("first", "0.bin", "big", "first bytes"),
        FakeFile::blob("one", "1.bin", "big", &content('1', 200_000)),
        FakeFile::native("doc", "notes", "big", DOCUMENT, &content('n', 250_000))
            .modified(FILE_TIME),
        FakeFile::blob("z", "z.bin", "big", &content('z', 100_000)),
    ]
}

#[tokio::test]
async fn an_interrupted_part_is_continued_with_its_exports_and_the_parts_before_it_stay() {
    let dir = tempfile::tempdir().unwrap();
    let uploads = dir.path().join("uploads");
    let options = continuing(&uploads);
    let source = FakeDrive::start(
        big(),
        Behavior {
            exports_vary: true,
            account_email: Some(String::from("old@example.com")),
            ..Behavior::default()
        },
    )
    .await;
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            chunks_stalled_from: Some(CHUNK_ALIGNMENT),
            account_email: Some(String::from("new@example.com")),
            ..Behavior::default()
        },
    )
    .await;
    let settings = || Settings {
        max_part_size: Some(400_000),
        ..settings(&["big"])
    };

    let first = migrate_with(
        &source,
        &dest,
        settings(),
        &options,
        dest.wait_for_stalled_chunks(1),
    )
    .await;

    assert!(
        first.summary().ends_with("; interrupted"),
        "{}",
        first.summary()
    );
    dest.stop_stalling();
    assert_eq!(names_of(&dest), ["big.part00001.tar"]);
    assert_eq!(source.exports().len(), 1);
    let chunks_before = dest.chunks().len();
    let media_before = source.media_ranges().len();

    let second = migrate_with(&source, &dest, settings(), &options, std::future::pending()).await;

    second.assert_ok();
    assert_eq!(
        second.out.lines()[1..],
        [
            "already-there big.part00001.tar",
            "create big.part00002.tar (352.8 KB)",
        ]
    );
    assert_eq!(source.exports().len(), 1, "the first run's export");
    let last = part(&dest, "big.part00002.tar");
    assert_eq!(paths(&last), ["big/notes.docx", "big/z.bin"]);
    let export = &unpack(&last.content)[0].3;
    assert!(export.ends_with(b"#0"), "the export of the first run");
    // The session is asked where it stands and continued there; the index is sent whole.
    let firsts = dest.chunks()[chunks_before..]
        .iter()
        .map(|chunk| chunk.first)
        .collect::<Vec<_>>();
    assert_eq!(firsts, [Some(0), None, Some(CHUNK_ALIGNMENT)]);
    // Only the file of that part is read: again, to check the whole part.
    assert_eq!(
        source.media_ranges()[media_before..],
        [(String::from("z"), None)]
    );
    assert_eq!(
        files_in(&uploads),
        Vec::<String>::new(),
        "nothing left behind"
    );
}

#[tokio::test]
async fn a_file_added_elsewhere_does_not_cost_an_interrupted_part_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let uploads = dir.path().join("uploads");
    let options = continuing(&uploads);
    let source_behavior = || Behavior {
        exports_vary: true,
        account_email: Some(String::from("old@example.com")),
        ..Behavior::default()
    };
    let source = FakeDrive::start(big(), source_behavior()).await;
    let dest = FakeDrive::start(
        Vec::new(),
        Behavior {
            chunks_stalled_from: Some(CHUNK_ALIGNMENT),
            account_email: Some(String::from("new@example.com")),
            ..Behavior::default()
        },
    )
    .await;
    let settings = || Settings {
        max_part_size: Some(400_000),
        ..settings(&["big"])
    };
    let first = migrate_with(
        &source,
        &dest,
        settings(),
        &options,
        dest.wait_for_stalled_chunks(1),
    )
    .await;
    assert!(first.summary().ends_with("; interrupted"));
    dest.stop_stalling();
    let chunks_before = dest.chunks().len();

    // A file that sorts after that part, too large to fit in it.
    let mut files = big();
    let added = std::iter::repeat_n('a', 100_000).collect::<String>();
    files.push(FakeFile::blob("zz", "zz.bin", "big", &added));
    let later = FakeDrive::start(files, source_behavior()).await;

    let second = migrate_with(&later, &dest, settings(), &options, std::future::pending()).await;

    second.assert_ok();
    assert_eq!(
        later.exports(),
        Vec::<(String, String)>::new(),
        "the first run's export"
    );
    let firsts = dest.chunks()[chunks_before..]
        .iter()
        .map(|chunk| chunk.first)
        .collect::<Vec<_>>();
    assert_eq!(
        firsts[..3],
        [Some(0), None, Some(CHUNK_ALIGNMENT)],
        "{firsts:?}"
    );
    let continued = part(&dest, "big.part00002.tar");
    assert_eq!(paths(&continued), ["big/notes.docx", "big/z.bin"]);
    assert_eq!(paths(&part(&dest, "big.part00003.tar")), ["big/zz.bin"]);
    assert_eq!(
        files_in(&uploads),
        Vec::<String>::new(),
        "nothing left behind"
    );
}

#[tokio::test]
async fn inside_one_account_the_parts_go_next_to_the_folder() {
    let drive = FakeDrive::start(photos(), Behavior::default()).await;

    let first = migrate(&drive, &drive, in_one_account(in_parts(&["photos"]))).await;
    first.assert_ok();
    assert_eq!(parts_of(&drive).len(), 3);

    let second = migrate(&drive, &drive, in_one_account(in_parts(&["photos"]))).await;
    second.assert_ok();
    assert_eq!(parts_of(&drive).len(), 3, "already there");
    assert!(
        second.summary().contains("3 already there"),
        "{}",
        second.summary()
    );
}

#[tokio::test]
async fn two_sources_of_one_name_keep_to_their_own_parts() {
    let mut files = photos();
    files.push(FakeFile::folder("other", "photos", "root").modified(FOLDER_TIME));
    files.push(FakeFile::blob("o", "o.bin", "other", "other bytes").modified(FILE_TIME));
    let source = FakeDrive::start(files, Behavior::default()).await;
    let dest = FakeDrive::start(Vec::new(), Behavior::default()).await;

    let first = migrate(&source, &dest, in_parts(&["photos", "other"])).await;
    first.assert_ok();
    let made = dest.created().len();
    let media = source.media().len();

    let second = migrate(&source, &dest, in_parts(&["photos", "other"])).await;

    second.assert_ok();
    assert_eq!(dest.created().len(), made);
    assert_eq!(source.media().len(), media);
    assert!(
        second.summary().contains("4 already there"),
        "{}",
        second.summary()
    );
}
