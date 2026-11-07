//! `files unzip` on a fake Drive. What the reader does with odd zip files is tested next to it, in
//! `transfer/zip/tests.rs`.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use super::{folder_name, mark, run, Error, PROPERTY};
use crate::transfer::{
    fake_drive::{Behavior, Created, FakeDrive, FakeFile},
    progress::{NoProgress, ProgressDisplay},
    scheduler::{
        capture::{output, Captured},
        Options,
    },
    upload::{UploadOptions, CHUNK_ALIGNMENT},
    zip::testing::{noise, zip_of, Written},
};

const FOLDER: &str = "application/vnd.google-apps.folder";

struct Ran {
    result: Result<(), Error>,
    out: Captured,
    err: Captured,
}

impl Ran {
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
}

/// A drive with the zip file `photos.zip` (id `zip`) of `zip` in the folder `home`.
async fn drive_with(zip: &[u8], others: Vec<FakeFile>, behavior: Behavior) -> FakeDrive {
    let mut files = vec![
        FakeFile::folder("home", "home", "root"),
        FakeFile::binary("zip", "photos.zip", "home", zip),
    ];
    files.extend(others);
    FakeDrive::start(files, behavior).await
}

async fn unzip_with(
    drive: &FakeDrive,
    id: &str,
    into: Option<&str>,
    continue_on_error: bool,
    options: &UploadOptions,
    cancel: impl Future<Output = ()> + Send,
) -> Ran {
    let output = output();
    let hub = drive.hub();
    let run = Box::pin(run(
        &hub,
        id,
        into,
        Options::new(NonZeroUsize::new(4).unwrap(), continue_on_error),
        options,
        &output.printer,
        ProgressDisplay::reporting_to(Arc::new(NoProgress)),
        cancel,
    ));
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("the run should end");
    Ran {
        result,
        out: output.out,
        err: output.err,
    }
}

async fn unzip(drive: &FakeDrive) -> Ran {
    unzip_with(
        drive,
        "zip",
        None,
        false,
        &UploadOptions::default(),
        std::future::pending(),
    )
    .await
}

async fn going_on(drive: &FakeDrive) -> Ran {
    unzip_with(
        drive,
        "zip",
        None,
        true,
        &UploadOptions::default(),
        std::future::pending(),
    )
    .await
}

fn named(drive: &FakeDrive, name: &str) -> Created {
    drive
        .created()
        .into_iter()
        .find(|file| file.name == name)
        .unwrap_or_else(|| panic!("no {name} in {:?}", drive.created()))
}

fn names(drive: &FakeDrive) -> Vec<String> {
    let mut names = drive
        .created()
        .into_iter()
        .map(|file| file.name)
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// A file a first run extracted from `written`, as it lies in the folder `parent`.
fn extracted(id: &str, written: &Written, parent: &str) -> FakeFile {
    let name = String::from_utf8(written.name.clone()).unwrap();
    let name = name.rsplit('/').next().unwrap();
    let mut file = FakeFile::binary(id, name, parent, &written.content);
    file.app_properties
        .insert(PROPERTY.to_owned(), mark(crc32fast::hash(&written.content)));
    file
}

fn tree() -> Vec<Written> {
    vec![
        Written::deflated("a.txt", b"aaaaaaaaaaaaaaaaaaaaaaaa"),
        Written::stored("sub/b.bin", &[0, 159, 146, 150]),
        Written::deflated("sub/deep/c.txt", b"cccc"),
        Written::folder("empty/"),
        Written::stored("zero.dat", b""),
    ]
}

#[tokio::test]
async fn a_zip_arrives_as_a_tree_in_a_new_folder_next_to_it() {
    let zip = zip_of(&tree());
    let drive = drive_with(&zip, vec![], Behavior::default()).await;

    let ran = unzip(&drive).await;
    ran.assert_ok();

    assert_eq!(
        names(&drive),
        ["a.txt", "b.bin", "c.txt", "deep", "empty", "photos", "sub", "zero.dat"]
    );
    let top = named(&drive, "photos");
    assert_eq!(top.parents, ["home"]);
    assert_eq!(top.mime_type, FOLDER);
    let sub = named(&drive, "sub");
    assert_eq!(sub.parents, [top.id.as_str()]);
    assert_eq!(named(&drive, "empty").parents, [top.id.as_str()]);
    let deep = named(&drive, "deep");
    assert_eq!(deep.parents, [sub.id.as_str()]);

    let a = named(&drive, "a.txt");
    assert_eq!(a.parents, [top.id.as_str()]);
    assert_eq!(a.content, b"aaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(a.mime_type, "text/plain");
    assert_eq!(a.modified_time.as_deref(), Some("2021-02-03T04:05:06.000Z"));
    let b = named(&drive, "b.bin");
    assert_eq!(b.parents, [sub.id]);
    assert_eq!(b.content, [0, 159, 146, 150]);
    let c = named(&drive, "c.txt");
    assert_eq!(c.parents, [deep.id]);
    assert_eq!(c.content, b"cccc");
    assert_eq!(named(&drive, "zero.dat").content, b"");

    let out = ran.out.text();
    assert!(out.contains("Found 4 files in 4 folders"), "{out}");
    assert!(out.contains("create photos/\n"), "{out}");
    assert!(out.contains("create photos/sub/deep/c.txt (4 B)"), "{out}");
    assert!(
        out.contains(&format!(
            "Folder: https://drive.google.com/drive/folders/{}",
            top.id
        )),
        "{out}"
    );
    assert_eq!(
        ran.summary(),
        "Summary: 4 extracted (32 B), 0 already there, 0 failed, 0 cancelled; 4 folders created, \
         0 reused"
    );
}

#[tokio::test]
async fn the_zip_file_itself_is_only_read() {
    let zip = zip_of(&tree());
    let drive = drive_with(&zip, vec![], Behavior::default()).await;
    unzip(&drive).await.assert_ok();

    assert_eq!(drive.deletes().len(), 0);
    assert_eq!(drive.patches().len(), 0);
    assert_eq!(drive.updates().len(), 0);
    assert_eq!(drive.stored("zip").unwrap().binary.unwrap(), zip);
}

#[tokio::test]
async fn into_names_the_folder_of_the_new_folder() {
    let zip = zip_of(&tree());
    let others = vec![FakeFile::folder("dest", "dest", "root")];
    let drive = drive_with(&zip, others, Behavior::default()).await;

    let ran = unzip_with(
        &drive,
        "zip",
        Some("dest"),
        false,
        &UploadOptions::default(),
        std::future::pending(),
    )
    .await;
    ran.assert_ok();
    assert_eq!(named(&drive, "photos").parents, ["dest"]);
}

#[tokio::test]
async fn into_must_be_a_folder() {
    let zip = zip_of(&tree());
    let others = vec![FakeFile::blob("file", "file", "root", "x")];
    let drive = drive_with(&zip, others, Behavior::default()).await;

    let ran = unzip_with(
        &drive,
        "zip",
        Some("file"),
        false,
        &UploadOptions::default(),
        std::future::pending(),
    )
    .await;
    assert!(matches!(ran.result, Err(Error::NotAFolder(_))));
    assert_eq!(drive.created().len(), 0);
}

#[tokio::test]
async fn a_zip_in_no_folder_goes_to_the_top_of_my_drive() {
    let zip = zip_of(&tree());
    let file = FakeFile::binary("zip", "photos.zip", "x", &zip).with_parents(&[]);
    let drive = FakeDrive::start(vec![file], Behavior::default()).await;

    unzip(&drive).await.assert_ok();
    assert_eq!(named(&drive, "photos").parents, ["root"]);
}

#[tokio::test]
async fn a_second_run_uploads_nothing_and_makes_no_second_folder() {
    let zip = zip_of(&tree());
    let drive = drive_with(&zip, vec![], Behavior::default()).await;
    unzip(&drive).await.assert_ok();
    let first = drive.created().len();

    let ran = unzip(&drive).await;
    ran.assert_ok();

    assert_eq!(drive.created().len(), first);
    let out = ran.out.text();
    assert!(out.contains("reuse photos/\n"), "{out}");
    assert!(out.contains("reuse photos/sub/deep/\n"), "{out}");
    assert!(out.contains("already-there photos/a.txt"), "{out}");
    assert!(out.contains("already-there photos/zero.dat"), "{out}");
    assert_eq!(
        ran.summary(),
        "Summary: 0 extracted (0 B), 4 already there, 0 failed, 0 cancelled; 0 folders created, \
         4 reused"
    );
}

#[tokio::test]
async fn a_run_after_a_failure_uploads_only_what_is_missing() {
    let written = tree();
    let zip = zip_of(&written);
    let others = vec![
        FakeFile::folder("top", "photos", "home"),
        extracted("old-a", &written[0], "top"),
    ];
    let drive = drive_with(&zip, others, Behavior::default()).await;

    unzip(&drive).await.assert_ok();

    let names = names(&drive);
    assert!(!names.contains(&String::from("a.txt")), "{names:?}");
    assert!(!names.contains(&String::from("photos")), "{names:?}");
    assert_eq!(named(&drive, "b.bin").content, [0, 159, 146, 150]);
    assert_eq!(named(&drive, "sub").parents, ["top"]);
}

#[tokio::test]
async fn a_file_of_that_name_with_other_content_is_left_alone_and_fails_the_entry() {
    let written = tree();
    let zip = zip_of(&written);
    // One of another size, one with the size but no mark, and one marked with another CRC-32.
    let mut other_crc = extracted("old-c", &written[4], "top");
    other_crc.name = String::from("zero.dat");
    other_crc
        .app_properties
        .insert(PROPERTY.to_owned(), mark(1));
    let others = vec![
        FakeFile::folder("top", "photos", "home"),
        FakeFile::blob("old-a", "a.txt", "top", "other"),
        other_crc,
    ];
    let drive = drive_with(&zip, others, Behavior::default()).await;

    let ran = going_on(&drive).await;

    assert!(matches!(ran.result, Err(Error::Incomplete(_))));
    let err = ran.err.text();
    assert!(err.contains("photos/a.txt"), "{err}");
    assert!(err.contains("another file of this name"), "{err}");
    let names = names(&drive);
    assert!(!names.contains(&String::from("a.txt")), "{names:?}");
    assert!(!names.contains(&String::from("zero.dat")), "{names:?}");
    assert_eq!(drive.stored("old-a").unwrap().content.unwrap(), "other");
    assert_eq!(drive.updates().len(), 0);
    assert_eq!(drive.deletes().len(), 0);
    // What had no conflict arrived all the same.
    assert_eq!(named(&drive, "c.txt").content, b"cccc");
    assert!(ran.summary().contains(", 2 failed,"), "{}", ran.summary());
}

#[tokio::test]
async fn the_first_failure_stops_the_run_unless_told_to_go_on() {
    let zip = zip_of(&[
        Written::stored("a.txt", b"a"),
        Written::stored("b.txt", b"b"),
    ]);
    let rejecting = || Behavior {
        upload_rejected: HashSet::from([String::from("a.txt")]),
        ..Behavior::default()
    };

    let drive = drive_with(&zip, vec![], rejecting()).await;
    let ran = unzip_with(
        &drive,
        "zip",
        None,
        false,
        &UploadOptions {
            verify_md5: true,
            ..UploadOptions::default()
        },
        std::future::pending(),
    )
    .await;
    assert!(matches!(ran.result, Err(Error::Incomplete(_))));
    assert!(
        ran.err.text().contains("photos/a.txt"),
        "{}",
        ran.err.text()
    );

    let drive = drive_with(&zip, vec![], rejecting()).await;
    let ran = going_on(&drive).await;
    assert!(matches!(ran.result, Err(Error::Incomplete(_))));
    assert_eq!(named(&drive, "b.txt").content, b"b");
}

#[tokio::test]
async fn an_entry_that_cannot_be_read_fails_and_the_others_arrive() {
    let zip = zip_of(&[
        Written {
            flags: 1,
            ..Written::stored("secret.txt", b"x")
        },
        Written {
            method: 12,
            ..Written::stored("bzip2.txt", b"x")
        },
        Written::deflated("fine.txt", b"fine"),
    ]);
    let drive = drive_with(&zip, vec![], Behavior::default()).await;

    let ran = going_on(&drive).await;

    assert!(matches!(ran.result, Err(Error::Incomplete(_))));
    let err = ran.err.text();
    assert!(err.contains("photos/secret.txt"), "{err}");
    assert!(err.contains("encrypted"), "{err}");
    assert!(err.contains("photos/bzip2.txt"), "{err}");
    assert!(err.contains("method 12"), "{err}");
    assert_eq!(names(&drive), ["fine.txt", "photos"]);
}

#[tokio::test]
async fn damaged_content_never_becomes_a_file() {
    let content = noise(600_000, 11);
    let options = UploadOptions {
        chunk_size: CHUNK_ALIGNMENT,
        multipart_threshold: 1000,
        ..UploadOptions::default()
    };
    for written in [
        // One request.
        Written::deflated("bad.bin", b"small"),
        // Several chunks, the last of which never goes up.
        Written::deflated("bad.bin", &content),
        Written::stored("bad.bin", &content),
    ] {
        let zip = zip_of(&[Written {
            crc32: Some(1),
            ..written
        }]);
        let drive = drive_with(&zip, vec![], Behavior::default()).await;

        let ran = unzip_with(&drive, "zip", None, false, &options, std::future::pending()).await;

        assert!(matches!(ran.result, Err(Error::Incomplete(_))));
        assert!(ran.err.text().contains("CRC-32"), "{}", ran.err.text());
        assert_eq!(names(&drive), ["photos"]);
    }
}

#[tokio::test]
async fn a_copy_that_fails_the_md5_check_is_not_taken_for_extracted_later() {
    let zip = zip_of(&[Written::deflated("a.bin", &noise(600_000, 19))]);
    let behavior = Behavior {
        corrupt_session: Some(0),
        ..Behavior::default()
    };
    let drive = drive_with(&zip, vec![], behavior).await;
    let options = UploadOptions {
        chunk_size: CHUNK_ALIGNMENT,
        multipart_threshold: 1000,
        ..UploadOptions::default()
    };

    let ran = unzip_with(&drive, "zip", None, false, &options, std::future::pending()).await;
    assert!(matches!(ran.result, Err(Error::Incomplete(_))));

    let ran = unzip_with(&drive, "zip", None, false, &options, std::future::pending()).await;
    assert!(matches!(ran.result, Err(Error::Incomplete(_))));
    assert!(
        !ran.out.text().contains("already-there"),
        "{}",
        ran.out.text()
    );
    assert!(
        ran.err.text().contains("another file of this name"),
        "{}",
        ran.err.text()
    );
}

#[tokio::test]
async fn a_large_entry_goes_up_in_chunks_and_a_failed_chunk_downloads_nothing_again() {
    let chunk = usize::try_from(CHUNK_ALIGNMENT).unwrap();
    let deflated = noise(chunk * 3 + 1000, 13);
    let stored = noise(chunk * 2 + 500, 17);
    let zip = zip_of(&[
        Written::deflated("big.bin", &deflated),
        Written::stored("plain.bin", &stored),
    ]);
    let options = UploadOptions {
        chunk_size: CHUNK_ALIGNMENT,
        multipart_threshold: 1000,
        ..UploadOptions::default()
    };
    let unzipped = async |behavior| {
        let drive = drive_with(&zip, vec![], behavior).await;
        let ran = unzip_with(&drive, "zip", None, false, &options, std::future::pending()).await;
        ran.assert_ok();
        assert_eq!(named(&drive, "big.bin").content, deflated);
        assert_eq!(named(&drive, "plain.bin").content, stored);
        // A chunked upload carries the mark as a single request does.
        let properties = named(&drive, "big.bin").app_properties;
        assert_eq!(properties[PROPERTY], mark(crc32fast::hash(&deflated)));
        (drive.media().len(), drive.chunks().len())
    };

    let (downloads, chunks) = unzipped(Behavior::default()).await;
    let (downloads_on_retry, chunks_on_retry) = unzipped(Behavior {
        chunk_unavailable: HashMap::from([(String::from("big.bin"), CHUNK_ALIGNMENT)]),
        ..Behavior::default()
    })
    .await;

    assert!(chunks_on_retry > chunks, "{chunks_on_retry} {chunks}");
    assert_eq!(downloads_on_retry, downloads);
}

#[tokio::test]
async fn the_zip_of_a_real_tool_arrives_whole() {
    let zip = include_bytes!("../../transfer/zip/testdata/infozip.zip");
    let drive = drive_with(zip, vec![], Behavior::default()).await;

    unzip(&drive).await.assert_ok();

    let tree = named(&drive, "tree");
    assert_eq!(tree.parents, [named(&drive, "photos").id]);
    assert_eq!(
        named(&drive, "a\u{e7}\u{e3}o \u{e9}.txt").content,
        b"accent\n"
    );
    assert_eq!(
        named(&drive, "text.txt").content,
        b"hello hello hello hello hello hello hello hello\n"
    );
    assert_eq!(
        named(&drive, "text.txt").modified_time.as_deref(),
        Some("2022-03-04T05:06:08.000Z")
    );
    assert_eq!(named(&drive, "empty").mime_type, FOLDER);
}

#[tokio::test]
async fn of_two_entries_of_one_path_the_last_counts_also_in_a_later_run() {
    let written = [
        Written::stored("same.txt", b"old"),
        Written::stored("same.txt", b"new!"),
    ];
    let zip = zip_of(&written);

    let drive = drive_with(&zip, vec![], Behavior::default()).await;
    unzip(&drive).await.assert_ok();
    assert_eq!(names(&drive), ["photos", "same.txt"]);
    assert_eq!(named(&drive, "same.txt").content, b"new!");

    let ran = unzip(&drive).await;
    ran.assert_ok();
    assert!(ran.out.text().contains("already-there photos/same.txt"));
}

#[tokio::test]
async fn a_pointer_to_a_google_file_goes_up_as_the_bytes_it_is() {
    let zip = zip_of(&[Written::stored("notes.gdoc", b"{\"url\": \"x\"}")]);
    let drive = drive_with(&zip, vec![], Behavior::default()).await;

    unzip(&drive).await.assert_ok();

    let file = named(&drive, "notes.gdoc");
    assert_eq!(file.mime_type, "application/octet-stream");
    assert_eq!(file.content, b"{\"url\": \"x\"}");
}

#[tokio::test]
async fn a_zip_without_entries_is_an_empty_folder() {
    let zip = zip_of(&[]);
    let drive = drive_with(&zip, vec![], Behavior::default()).await;
    unzip(&drive).await.assert_ok();
    assert_eq!(names(&drive), ["photos"]);
}

#[tokio::test]
async fn what_is_no_zip_changes_nothing() {
    let others = vec![
        FakeFile::folder("folder", "a folder", "root"),
        FakeFile::document("doc", "a doc", "root"),
        FakeFile::blob("text", "notes.zip", "root", "no zip at all"),
    ];
    let drive = drive_with(&zip_of(&tree()), others, Behavior::default()).await;

    for (id, expected) in [
        ("folder", "has no content to read as a zip"),
        ("doc", "has no content to read as a zip"),
        ("text", "unable to read 'notes.zip'"),
    ] {
        let ran = unzip_with(
            &drive,
            id,
            None,
            false,
            &UploadOptions::default(),
            std::future::pending(),
        )
        .await;
        let error = ran.result.unwrap_err();
        assert!(error.to_string().contains(expected), "{id}: {error}");
    }
    assert_eq!(drive.created().len(), 0);
}

#[tokio::test]
async fn control_characters_of_a_name_stay_out_of_the_output_but_keep_the_name() {
    let zip = zip_of(&[Written::stored("bad\u{1b}[31m.txt", b"x")]);
    let drive = drive_with(&zip, vec![], Behavior::default()).await;

    let ran = unzip(&drive).await;
    ran.assert_ok();

    assert!(!ran.out.text().contains('\u{1b}'), "{}", ran.out.text());
    assert_eq!(named(&drive, "bad\u{1b}[31m.txt").content, b"x");
}

#[test]
fn the_folder_is_the_name_without_zip() {
    assert_eq!(folder_name("photos.zip"), "photos");
    assert_eq!(folder_name("PHOTOS.ZIP"), "PHOTOS");
    assert_eq!(folder_name("a\u{e7}\u{e3}o.zip"), "a\u{e7}\u{e3}o");
    assert_eq!(folder_name("old.backup"), "old.backup");
    assert_eq!(folder_name(".zip"), ".zip");
    assert_eq!(folder_name("zip"), "zip");
    // The last four bytes are no whole characters.
    assert_eq!(folder_name("\u{e7}\u{e3}"), "\u{e7}\u{e3}");
}
