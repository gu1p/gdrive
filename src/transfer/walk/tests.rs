use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;

use super::{
    parents_query, Children, EntryKind, FolderKey, FolderRef, IncompleteSearch, Walk, WalkEntry,
    WalkItem, WalkOptions, WalkRoot, FIELDS, MAX_PARENTS_PER_QUERY,
};
use crate::{
    common::drive_file::MIME_TYPE_DRIVE_FOLDER,
    transfer::fake_drive::{Behavior, FakeDrive, FakeFile},
};

/// Upper bound for one walk, so that a hang fails the test.
const TIMEOUT: Duration = Duration::from_secs(20);

fn root() -> WalkRoot {
    WalkRoot {
        id: String::from("root"),
        name: String::from("Root"),
        drive_id: None,
        resource_key: None,
    }
}

fn options() -> WalkOptions {
    WalkOptions::default()
}

fn start(server: &FakeDrive, options: WalkOptions) -> Walk {
    Walk::start(Arc::new(server.hub()), root(), options)
}

async fn collect(walk: Walk) -> Vec<WalkItem> {
    tokio::time::timeout(TIMEOUT, walk.collect())
        .await
        .expect("walk should finish")
}

async fn walk_all(files: Vec<FakeFile>, behavior: Behavior, options: WalkOptions) -> Walked {
    let server = FakeDrive::start(files, behavior).await;
    let items = collect(start(&server, options)).await;
    let walked = Walked { server, items };
    walked.assert_item_order();
    walked
}

struct Walked {
    server: FakeDrive,
    items: Vec<WalkItem>,
}

impl Walked {
    fn entries(&self) -> impl Iterator<Item = &WalkEntry> {
        self.items.iter().filter_map(|item| match item {
            WalkItem::Entry(entry) => Some(entry),
            WalkItem::FolderDone(_) | WalkItem::FolderError(_) | WalkItem::IncompleteSearch(_) => {
                None
            }
        })
    }

    /// The sorted paths of all entries, components joined with `/`.
    fn paths(&self) -> Vec<String> {
        let mut paths = self
            .entries()
            .map(|entry| entry.components.join("/"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    /// The one entry at `path`.
    fn entry(&self, path: &str) -> &WalkEntry {
        let found = self
            .entries()
            .filter(|entry| entry.components.join("/") == path)
            .collect::<Vec<_>>();
        assert_eq!(found.len(), 1, "expected one entry at {path}: {found:#?}");
        found[0]
    }

    fn folder_key(&self, path: &str) -> FolderKey {
        match self.entry(path).children {
            Children::Listed(key) => key,
            children => panic!("{path} was not listed: {children:?}"),
        }
    }

    fn errors(&self) -> Vec<&super::FolderError> {
        self.items
            .iter()
            .filter_map(|item| match item {
                WalkItem::FolderError(error) => Some(error),
                WalkItem::Entry(_) | WalkItem::FolderDone(_) | WalkItem::IncompleteSearch(_) => {
                    None
                }
            })
            .collect()
    }

    /// The order invariants of a complete walk: a folder's entry comes before every item about
    /// its children, every listed key is new, and every listed folder ends with exactly one
    /// `FolderDone` or `FolderError`, after which nothing names it.
    fn assert_item_order(&self) {
        let mut open = HashSet::from([FolderKey::ROOT]);
        let mut ended = HashSet::new();
        let assert_open = |open: &HashSet<FolderKey>, ended: &HashSet<_>, key, what: &str| {
            assert!(open.contains(&key), "{what} for {key:?} before its folder");
            assert!(!ended.contains(&key), "{what} for {key:?} after its end");
        };
        for item in &self.items {
            match item {
                WalkItem::Entry(entry) => {
                    assert_open(&open, &ended, entry.parent, "entry");
                    if let Children::Listed(key) = entry.children {
                        assert!(open.insert(key), "key {key:?} given twice");
                    }
                }
                WalkItem::FolderDone(key) => {
                    assert_open(&open, &ended, *key, "FolderDone");
                    ended.insert(*key);
                }
                WalkItem::FolderError(error) => {
                    assert_open(&open, &ended, error.folder.key, "FolderError");
                    ended.insert(error.folder.key);
                }
                WalkItem::IncompleteSearch(incomplete) => {
                    for folder in &incomplete.folders {
                        assert_open(&open, &ended, folder.key, "IncompleteSearch");
                    }
                }
            }
        }
        assert_eq!(open, ended, "every listed folder ends once");
    }

    fn done(&self) -> Vec<FolderKey> {
        self.items
            .iter()
            .filter_map(|item| match item {
                WalkItem::FolderDone(key) => Some(*key),
                WalkItem::Entry(_) | WalkItem::FolderError(_) | WalkItem::IncompleteSearch(_) => {
                    None
                }
            })
            .collect()
    }

    /// The folder ids of each request, in order.
    fn queried_parents(&self) -> Vec<Vec<String>> {
        self.server
            .requests()
            .into_iter()
            .map(|request| request.parents)
            .collect()
    }
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|&id| id.to_owned()).collect()
}

#[tokio::test]
async fn empty_folder_yields_only_its_end() {
    let walked = walk_all(Vec::new(), Behavior::default(), options()).await;

    assert!(matches!(
        walked.items.as_slice(),
        [WalkItem::FolderDone(FolderKey::ROOT)]
    ));
    assert_eq!(walked.queried_parents(), [ids(&["root"])]);
}

#[tokio::test]
async fn nested_folders_are_listed_with_components_depths_and_kinds() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::blob("top", "top.txt", "root", "top"),
        FakeFile::document("doc", "notes", "root"),
        FakeFile::folder("b", "b", "a"),
        FakeFile::blob("a1", "a.txt", "a", "in a"),
        FakeFile::blob("deep", "deep.txt", "b", "deep"),
    ];
    let walked = walk_all(files, Behavior::default(), options()).await;

    assert_eq!(
        walked.paths(),
        ["a", "a/a.txt", "a/b", "a/b/deep.txt", "notes", "top.txt"]
    );

    let a = walked.entry("a");
    assert_eq!(
        (a.kind, a.depth, a.parent),
        (EntryKind::Folder, 1, FolderKey::ROOT)
    );
    let top = walked.entry("top.txt");
    assert_eq!(top.kind, EntryKind::Blob);
    assert_eq!(top.children, Children::None);
    assert_eq!(top.file.size, Some(3));
    assert_eq!(
        top.file.md5_checksum.as_deref(),
        Some(format!("{:x}", md5::compute("top")).as_str())
    );
    assert_eq!(walked.entry("notes").kind, EntryKind::Native);
    let deep = walked.entry("a/b/deep.txt");
    assert_eq!(deep.depth, 3);
    assert_eq!(deep.parent, walked.folder_key("a/b"));
    assert_eq!(deep.file.id.as_deref(), Some("deep"));

    assert_eq!(
        walked.queried_parents(),
        [ids(&["root"]), ids(&["a"]), ids(&["b"])]
    );
}

#[tokio::test]
async fn multi_page_folder_follows_page_tokens() {
    let files = (0..5)
        .map(|n| FakeFile::blob(&format!("f{n}"), &format!("{n}.txt"), "root", "x"))
        .collect();
    let behavior = Behavior {
        page_size: Some(2),
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    assert_eq!(
        walked.paths(),
        ["0.txt", "1.txt", "2.txt", "3.txt", "4.txt"]
    );
    let requests = walked.server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].param("pageToken"), None);
    assert!(requests[1].param("pageToken").is_some());
    assert!(requests[2].param("pageToken").is_some());
    assert_ne!(
        requests[1].param("pageToken"),
        requests[2].param("pageToken")
    );
    for request in &requests {
        assert_eq!(request.param("pageSize"), Some("1000"));
        assert_eq!(request.param("q"), requests[0].param("q"));
    }
}

#[tokio::test]
async fn folders_end_after_the_last_page_of_their_query() {
    let mut files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::blob("b1", "b.txt", "b", "b"),
    ];
    for n in 0..3 {
        files.push(FakeFile::blob(
            &format!("a{n}"),
            &format!("{n}.txt"),
            "a",
            "a",
        ));
    }
    let behavior = Behavior {
        page_size: Some(2),
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    // Pages of the batch [a, b]: a0 a1 | a2 b1; both folders end after the second page.
    let done = walked.done();
    assert_eq!(
        done,
        [
            FolderKey::ROOT,
            walked.folder_key("a"),
            walked.folder_key("b")
        ]
    );
    let position = |wanted: &WalkItem| {
        walked
            .items
            .iter()
            .position(|item| std::ptr::eq(item, wanted))
            .unwrap()
    };
    let last_entry = walked
        .items
        .iter()
        .rfind(|item| matches!(item, WalkItem::Entry(_)))
        .unwrap();
    let first_done_of_a = walked
        .items
        .iter()
        .find(|item| matches!(item, WalkItem::FolderDone(key) if *key == walked.folder_key("a")))
        .unwrap();
    assert!(position(last_entry) < position(first_done_of_a));
}

#[tokio::test]
async fn ids_repeated_across_pages_are_emitted_once() {
    let mut files = Vec::new();
    for parent in ["root", "a", "b"] {
        for n in 0..3 {
            files.push(FakeFile::blob(
                &format!("{parent}{n}"),
                &format!("{n}.txt"),
                parent,
                "x",
            ));
        }
    }
    // Inserted after the root's files so that it is the repeated one on its second page.
    files.insert(2, FakeFile::folder("a", "a", "root"));
    files.push(FakeFile::folder("b", "b", "root"));
    let behavior = Behavior {
        page_size: Some(2),
        repeat_across_pages: true,
        ..Behavior::default()
    };
    for max_parents_per_query in [1, MAX_PARENTS_PER_QUERY] {
        let options = WalkOptions {
            max_parents_per_query,
            ..options()
        };
        let walked = walk_all(files.clone(), behavior.clone(), options).await;

        assert_eq!(
            walked.paths(),
            [
                "0.txt", "1.txt", "2.txt", "a", "a/0.txt", "a/1.txt", "a/2.txt", "b", "b/0.txt",
                "b/1.txt", "b/2.txt"
            ],
            "with max_parents_per_query = {max_parents_per_query}"
        );
        let listings_of_a = walked
            .server
            .requests()
            .iter()
            .filter(|request| request.parents.contains(&String::from("a")))
            .filter(|request| request.param("pageToken").is_none())
            .count();
        assert_eq!(listings_of_a, 1, "`a` is queued once");
    }
}

#[tokio::test]
async fn batched_query_routes_children_to_their_parents() {
    let mut files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::folder("c", "c", "root"),
    ];
    for folder in ["a", "b", "c"] {
        files.push(FakeFile::blob(
            &format!("{folder}-same"),
            "same.txt",
            folder,
            folder,
        ));
    }
    files.push(FakeFile::blob("b-only", "only-in-b.txt", "b", "b"));
    let walked = walk_all(files, Behavior::default(), options()).await;

    assert_eq!(
        walked.queried_parents(),
        [ids(&["root"]), ids(&["a", "b", "c"])]
    );
    assert_eq!(
        walked.server.requests()[1].param("q"),
        Some("('a' in parents or 'b' in parents or 'c' in parents) and trashed = false")
    );
    for folder in ["a", "b", "c"] {
        let same = walked.entry(&format!("{folder}/same.txt"));
        assert_eq!(
            same.file.id.as_deref(),
            Some(format!("{folder}-same").as_str())
        );
        assert_eq!(same.parent, walked.folder_key(folder));
        assert_eq!(same.depth, 2);
    }
    assert_eq!(
        walked.entry("b/only-in-b.txt").parent,
        walked.folder_key("b")
    );
    assert_eq!(walked.entries().count(), 7);
}

#[tokio::test]
async fn file_with_two_parents_in_one_batch_is_listed_under_both() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::blob("shared", "shared.txt", "a", "s").with_parents(&["a", "b"]),
    ];
    let walked = walk_all(files, Behavior::default(), options()).await;

    assert_eq!(walked.paths(), ["a", "a/shared.txt", "b", "b/shared.txt"]);
    assert_eq!(walked.entry("a/shared.txt").parent, walked.folder_key("a"));
    assert_eq!(walked.entry("b/shared.txt").parent, walked.folder_key("b"));
    assert_eq!(walked.server.request_count(), 2);
}

#[tokio::test]
async fn batch_size_is_capped_by_max_parents_per_query() {
    let files = (0..5)
        .map(|n| FakeFile::folder(&format!("f{n}"), &format!("f{n}"), "root"))
        .collect();
    let options = WalkOptions {
        listing_jobs: 1,
        max_parents_per_query: 2,
        ..options()
    };
    let walked = walk_all(files, Behavior::default(), options).await;

    assert_eq!(
        walked.queried_parents(),
        [
            ids(&["root"]),
            ids(&["f0", "f1"]),
            ids(&["f2", "f3"]),
            ids(&["f4"])
        ]
    );
}

#[tokio::test]
async fn more_than_the_query_limit_is_clamped() {
    let files = (0..60)
        .map(|n| FakeFile::folder(&format!("f{n}"), &format!("f{n}"), "root"))
        .collect();
    let options = WalkOptions {
        listing_jobs: 1,
        max_parents_per_query: 1000,
        ..options()
    };
    let walked = walk_all(files, Behavior::default(), options).await;

    let sizes = walked
        .queried_parents()
        .iter()
        .map(Vec::len)
        .collect::<Vec<_>>();
    assert_eq!(sizes, [1, MAX_PARENTS_PER_QUERY, 10]);
}

#[tokio::test]
async fn results_without_parents_are_listed_again_one_folder_per_query() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::blob("a1", "x.txt", "a", "a"),
        FakeFile::blob("b1", "x.txt", "b", "b"),
    ];
    let behavior = Behavior {
        omit_parents: true,
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    assert_eq!(walked.paths(), ["a", "a/x.txt", "b", "b/x.txt"]);
    assert_eq!(walked.entry("a/x.txt").file.id.as_deref(), Some("a1"));
    assert_eq!(walked.entry("b/x.txt").file.id.as_deref(), Some("b1"));
    assert_eq!(
        walked.queried_parents(),
        [ids(&["root"]), ids(&["a", "b"]), ids(&["a"]), ids(&["b"])]
    );
}

#[tokio::test]
async fn unreadable_folder_yields_an_error_and_the_walk_continues() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
        FakeFile::folder("c", "c", "root"),
        FakeFile::blob("a1", "x.txt", "a", "a"),
        FakeFile::blob("b1", "x.txt", "b", "b"),
        FakeFile::folder("b-sub", "sub", "b"),
        FakeFile::blob("c1", "x.txt", "c", "c"),
    ];
    let behavior = Behavior {
        unreadable: HashSet::from([String::from("b")]),
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    assert_eq!(walked.paths(), ["a", "a/x.txt", "b", "c", "c/x.txt"]);
    let errors = walked.errors();
    assert_eq!(errors.len(), 1);
    let error = errors[0];
    assert_eq!(
        error.folder,
        FolderRef {
            key: walked.folder_key("b"),
            id: String::from("b"),
            name: String::from("b"),
            components: ids(&["b"]),
        }
    );
    assert_eq!(error.to_string(), "unable to list folder 'b' with id 'b'");
    let mut done = walked.done();
    done.sort();
    assert_eq!(
        done,
        [
            FolderKey::ROOT,
            walked.folder_key("a"),
            walked.folder_key("c")
        ]
    );
    assert!(matches!(*error.error, google_drive3::Error::BadRequest(_)));
    assert_eq!(
        walked.queried_parents(),
        [
            ids(&["root"]),
            ids(&["a", "b", "c"]),
            ids(&["a"]),
            ids(&["b"]),
            ids(&["c"])
        ]
    );
}

#[tokio::test]
async fn failed_later_page_of_a_batch_does_not_emit_entries_twice() {
    let mut files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("b", "b", "root"),
    ];
    for folder in ["a", "b"] {
        for n in 1..=3 {
            files.push(FakeFile::blob(
                &format!("{folder}{n}"),
                &format!("{n}.txt"),
                folder,
                "x",
            ));
        }
    }
    let behavior = Behavior {
        page_size: Some(2),
        fail_after_first_page: HashSet::from([String::from("a")]),
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    // The batch's first page had a1 and a2; listing `a` alone fails on its second page again.
    assert_eq!(
        walked.paths(),
        ["a", "a/1.txt", "a/2.txt", "b", "b/1.txt", "b/2.txt", "b/3.txt"]
    );
    let errors = walked.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].folder.id, "a");
    assert_eq!(errors[0].folder.key, walked.folder_key("a"));
    assert!(walked.done().contains(&walked.folder_key("b")));
    assert!(!walked.done().contains(&walked.folder_key("a")));
}

#[tokio::test]
async fn root_that_cannot_be_listed_is_reported() {
    let behavior = Behavior {
        unreadable: HashSet::from([String::from("root")]),
        ..Behavior::default()
    };
    let walked = walk_all(Vec::new(), behavior, options()).await;

    assert_eq!(walked.items.len(), 1);
    let errors = walked.errors();
    assert_eq!(
        errors[0].folder,
        FolderRef {
            key: FolderKey::ROOT,
            id: String::from("root"),
            name: String::from("Root"),
            components: Vec::new(),
        }
    );
    assert_eq!(
        errors[0].to_string(),
        "unable to list folder 'Root' with id 'root'"
    );
}

fn shortcut_tree() -> Vec<FakeFile> {
    vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::shortcut("back", "back", "a", "root", MIME_TYPE_DRIVE_FOLDER),
        FakeFile::shortcut("out", "out", "a", "ext", MIME_TYPE_DRIVE_FOLDER),
        FakeFile::shortcut("to-file", "file link", "a", "e1", "text/plain"),
        // `ext` is outside the walked tree.
        FakeFile::folder("ext", "external", "elsewhere"),
        FakeFile::blob("e1", "e.txt", "ext", "e"),
        FakeFile::shortcut("loop", "loop", "ext", "a", MIME_TYPE_DRIVE_FOLDER),
        FakeFile::shortcut("self", "self", "ext", "ext", MIME_TYPE_DRIVE_FOLDER),
    ]
}

#[tokio::test]
async fn shortcuts_are_not_followed_by_default() {
    let walked = walk_all(shortcut_tree(), Behavior::default(), options()).await;

    assert_eq!(walked.paths(), ["a", "a/back", "a/file link", "a/out"]);
    for path in ["a/back", "a/file link", "a/out"] {
        let shortcut = walked.entry(path);
        assert_eq!(shortcut.kind, EntryKind::Shortcut);
        assert_eq!(shortcut.children, Children::None);
    }
    assert_eq!(walked.queried_parents(), [ids(&["root"]), ids(&["a"])]);
}

#[tokio::test]
async fn followed_shortcut_lists_its_target_and_cycles_stop() {
    let options = WalkOptions {
        follow_shortcuts: true,
        ..options()
    };
    let walked = walk_all(shortcut_tree(), Behavior::default(), options).await;

    assert_eq!(
        walked.paths(),
        [
            "a",
            "a/back",
            "a/file link",
            "a/out",
            "a/out/e.txt",
            "a/out/loop",
            "a/out/self"
        ]
    );
    assert_eq!(walked.entry("a/back").children, Children::Cycle);
    assert_eq!(walked.entry("a/file link").children, Children::None);
    let e = walked.entry("a/out/e.txt");
    assert_eq!(e.parent, walked.folder_key("a/out"));
    assert_eq!(e.depth, 3);
    assert_eq!(walked.entry("a/out/loop").children, Children::Cycle);
    assert_eq!(walked.entry("a/out/self").children, Children::Cycle);
    assert_eq!(
        walked.queried_parents(),
        [ids(&["root"]), ids(&["a"]), ids(&["ext"])]
    );
}

#[tokio::test]
async fn folder_that_is_its_own_ancestor_is_not_listed_again() {
    // A broken server: folder `a` contains the root.
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::folder("root", "again", "a"),
    ];
    let walked = walk_all(files, Behavior::default(), options()).await;

    assert_eq!(walked.paths(), ["a", "a/again"]);
    assert_eq!(walked.entry("a/again").children, Children::Cycle);
    assert_eq!(walked.server.request_count(), 2);
}

#[tokio::test]
async fn incomplete_search_is_reported_before_the_page_entries() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::blob("x", "x.txt", "a", "x"),
    ];
    let behavior = Behavior {
        incomplete: HashSet::from([String::from("a")]),
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    let positions = walked
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            WalkItem::IncompleteSearch(incomplete) => Some((index, incomplete)),
            WalkItem::Entry(_) | WalkItem::FolderDone(_) | WalkItem::FolderError(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(positions.len(), 1, "{:#?}", walked.items);
    let (index, incomplete) = positions[0];
    assert_eq!(
        *incomplete,
        IncompleteSearch {
            folders: vec![FolderRef {
                key: walked.folder_key("a"),
                id: String::from("a"),
                name: String::from("a"),
                components: ids(&["a"]),
            }]
        }
    );
    let x = walked
        .items
        .iter()
        .position(
            |item| matches!(item, WalkItem::Entry(entry) if entry.components == ["a", "x.txt"]),
        )
        .expect("a/x.txt is listed");
    assert!(index < x);
}

fn resource_keys(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|&(id, key)| (id.to_owned(), key.to_owned()))
        .collect()
}

#[tokio::test]
async fn followed_shortcut_target_is_listed_with_its_resource_key() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::shortcut("link", "link", "a", "ext", MIME_TYPE_DRIVE_FOLDER)
            .with_target_resource_key("rk-ext"),
        // Shared by link only, outside the walked tree.
        FakeFile::folder("ext", "external", "elsewhere").with_resource_key("rk-ext"),
        FakeFile::blob("e1", "e.txt", "ext", "e"),
        FakeFile::blob("e2", "f.txt", "ext", "f"),
        FakeFile::folder("sub", "sub", "ext").with_resource_key("rk-sub"),
        FakeFile::blob("s1", "s.txt", "sub", "s"),
    ];
    let behavior = Behavior {
        // One file per page, so that the header is also needed with a page token.
        page_size: Some(1),
        resource_keys: resource_keys(&[("ext", "rk-ext"), ("sub", "rk-sub")]),
        ..Behavior::default()
    };
    let options = WalkOptions {
        follow_shortcuts: true,
        ..options()
    };
    let walked = walk_all(files, behavior, options).await;

    assert!(walked.errors().is_empty(), "{:#?}", walked.errors());
    assert_eq!(
        walked.paths(),
        [
            "a",
            "a/link",
            "a/link/e.txt",
            "a/link/f.txt",
            "a/link/sub",
            "a/link/sub/s.txt"
        ]
    );
    let sent = walked
        .server
        .requests()
        .into_iter()
        .map(|request| {
            let page = request.param("pageToken").is_some();
            (request.parents, page, request.resource_keys)
        })
        .collect::<Vec<_>>();
    let key = |key: &str| Some(String::from(key));
    assert_eq!(
        sent,
        [
            (ids(&["root"]), false, None),
            (ids(&["a"]), false, None),
            (ids(&["ext"]), false, key("ext/rk-ext")),
            (ids(&["ext"]), true, key("ext/rk-ext")),
            (ids(&["ext"]), true, key("ext/rk-ext")),
            (ids(&["sub"]), false, key("sub/rk-sub")),
        ]
    );
    let request = &walked.server.requests()[2];
    assert_eq!(request.param("fields"), Some(FIELDS));
    assert_eq!(request.param("pageSize"), Some("1000"));
    assert_eq!(request.param("supportsAllDrives"), Some("true"));
    assert_eq!(request.param("includeItemsFromAllDrives"), Some("true"));
    assert_eq!(
        request.param("q"),
        Some("'ext' in parents and trashed = false")
    );
}

#[tokio::test]
async fn batch_sends_the_resource_keys_of_its_folders() {
    let files = vec![
        FakeFile::folder("x", "x", "root").with_resource_key("kx"),
        FakeFile::folder("y", "y", "root").with_resource_key("ky"),
        FakeFile::folder("z", "z", "root"),
        FakeFile::folder("w", "w", "root").with_resource_key("stale"),
        FakeFile::blob("x1", "1.txt", "x", "x"),
        FakeFile::blob("y1", "1.txt", "y", "y"),
        FakeFile::blob("z1", "1.txt", "z", "z"),
        FakeFile::blob("w1", "1.txt", "w", "w"),
    ];
    let behavior = Behavior {
        resource_keys: resource_keys(&[
            ("root", "rk-root"),
            ("x", "kx"),
            ("y", "ky"),
            ("w", "current"),
        ]),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;
    let root = WalkRoot {
        resource_key: Some(String::from("rk-root")),
        ..root()
    };
    let walked = Walked {
        items: collect(Walk::start(Arc::new(server.hub()), root, options())).await,
        server,
    };
    walked.assert_item_order();

    // `w`'s key is refused: the batch is split and only `w` fails, through the same error
    // mapping as google-drive3's.
    assert_eq!(
        walked.paths(),
        ["w", "x", "x/1.txt", "y", "y/1.txt", "z", "z/1.txt"]
    );
    let errors = walked.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].folder.id, "w");
    assert!(matches!(
        *errors[0].error,
        google_drive3::Error::BadRequest(_)
    ));
    let mut sent = walked
        .server
        .requests()
        .into_iter()
        .map(|request| (request.parents, request.resource_keys))
        .collect::<Vec<_>>();
    // The queries of the split batch run two at a time, in any order.
    sent[2..].sort();
    let key = |key: &str| Some(String::from(key));
    assert_eq!(
        sent,
        [
            (ids(&["root"]), key("root/rk-root")),
            (ids(&["x", "y", "z", "w"]), key("x/kx,y/ky,w/stale")),
            (ids(&["w"]), key("w/stale")),
            (ids(&["x"]), key("x/kx")),
            (ids(&["y"]), key("y/ky")),
            (ids(&["z"]), None),
        ]
    );
}

#[tokio::test]
async fn shared_drive_root_searches_the_drive_corpus() {
    let files = vec![
        FakeFile::folder("a", "a", "root").in_drive("drv"),
        FakeFile::blob("a1", "x.txt", "a", "x").in_drive("drv"),
        FakeFile::shortcut("out", "out", "a", "ext", MIME_TYPE_DRIVE_FOLDER).in_drive("drv"),
        // The shortcut's target is in My Drive.
        FakeFile::folder("ext", "external", "elsewhere"),
        FakeFile::blob("e1", "e.txt", "ext", "e"),
    ];
    let server = FakeDrive::start(files, Behavior::default()).await;
    let root = WalkRoot {
        drive_id: Some(String::from("drv")),
        ..root()
    };
    let options = WalkOptions {
        follow_shortcuts: true,
        ..options()
    };
    let walked = Walked {
        items: collect(Walk::start(Arc::new(server.hub()), root, options)).await,
        server,
    };

    assert_eq!(walked.paths(), ["a", "a/out", "a/out/e.txt", "a/x.txt"]);
    let requests = walked.server.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.param("supportsAllDrives"), Some("true"));
        assert_eq!(request.param("includeItemsFromAllDrives"), Some("true"));
    }
    for request in &requests[..2] {
        assert_eq!(request.param("corpora"), Some("drive"));
        assert_eq!(request.param("driveId"), Some("drv"));
    }
    assert_eq!(requests[2].parents, ids(&["ext"]));
    assert_eq!(requests[2].param("corpora"), None);
    assert_eq!(requests[2].param("driveId"), None);
}

#[tokio::test]
async fn requests_ask_for_the_walk_fields_and_exclude_trashed_files() {
    let files = vec![
        FakeFile::blob("kept", "kept.txt", "root", "k"),
        FakeFile::blob("gone", "gone.txt", "root", "g").trashed(),
    ];
    let walked = walk_all(files, Behavior::default(), options()).await;

    assert_eq!(walked.paths(), ["kept.txt"]);
    let request = &walked.server.requests()[0];
    assert_eq!(request.param("fields"), Some(FIELDS));
    assert_eq!(request.param("pageSize"), Some("1000"));
    assert_eq!(request.param("orderBy"), None);
    assert_eq!(
        request.param("q"),
        Some("'root' in parents and trashed = false")
    );
    for field in [
        "id",
        "name",
        "mimeType",
        "size",
        "md5Checksum",
        "modifiedTime",
        "shortcutDetails",
        "trashed",
        "parents",
        "driveId",
        "resourceKey",
        "targetResourceKey",
        "incompleteSearch",
        "nextPageToken",
    ] {
        assert!(FIELDS.contains(field), "{field} missing from {FIELDS}");
    }
}

#[tokio::test]
async fn trashed_file_returned_anyway_is_dropped() {
    let files = vec![
        FakeFile::blob("kept", "kept.txt", "root", "k"),
        FakeFile::folder("gone", "gone", "root").trashed(),
    ];
    let behavior = Behavior {
        ignore_trashed_filter: true,
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    assert_eq!(walked.paths(), ["kept.txt"]);
    assert_eq!(walked.server.request_count(), 1);
}

/// A root with `count` folders of one file each.
fn wide_tree(count: usize) -> Vec<FakeFile> {
    (0..count)
        .flat_map(|n| {
            let folder = format!("f{n}");
            [
                FakeFile::folder(&folder, &folder, "root"),
                FakeFile::blob(&format!("{folder}-file"), "x.txt", &folder, "x"),
            ]
        })
        .collect()
}

#[tokio::test]
async fn in_flight_requests_never_exceed_listing_jobs() {
    for listing_jobs in [1, 3] {
        let behavior = Behavior {
            delay: Duration::from_millis(40),
            ..Behavior::default()
        };
        let options = WalkOptions {
            listing_jobs,
            max_parents_per_query: 1,
            ..options()
        };
        let walked = walk_all(wide_tree(12), behavior, options).await;

        assert_eq!(walked.entries().count(), 24);
        assert_eq!(walked.server.request_count(), 13);
        assert_eq!(
            walked.server.max_in_flight(),
            listing_jobs,
            "with listing_jobs = {listing_jobs}"
        );
    }
}

#[tokio::test]
async fn walk_does_not_run_ahead_of_a_slow_consumer() {
    let server = FakeDrive::start(wide_tree(30), Behavior::default()).await;
    let options = WalkOptions {
        buffer: 2,
        max_parents_per_query: 1,
        ..options()
    };
    let mut walk = start(&server, options);

    let first = tokio::time::timeout(TIMEOUT, walk.next()).await.unwrap();
    assert!(matches!(first, Some(WalkItem::Entry(_))));
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The root's page has 30 entries; the walk waits for room before it lists any subfolder.
    assert_eq!(server.request_count(), 1);

    let rest = collect(walk).await;
    // 30 folders and 30 files, and the end of 31 folders.
    assert_eq!(rest.len(), 60 + 31 - 1);
    assert_eq!(server.request_count(), 31);
}

#[tokio::test]
async fn dropping_the_walk_stops_listing() {
    let behavior = Behavior {
        delay: Duration::from_millis(50),
        ..Behavior::default()
    };
    let server = FakeDrive::start(wide_tree(30), behavior).await;
    let hub = Arc::new(server.hub());
    let options = WalkOptions {
        buffer: 1,
        max_parents_per_query: 1,
        ..options()
    };
    let mut walk = Walk::start(Arc::clone(&hub), root(), options);

    // Wait until subfolders are being listed, then hang up.
    while server.request_count() < 3 {
        tokio::time::timeout(TIMEOUT, walk.next())
            .await
            .unwrap()
            .expect("the walk is not finished yet");
    }
    drop(walk);

    // The producer task and every request in flight hold the hub: once it is the last
    // reference, nothing of the walk is left running.
    tokio::time::timeout(TIMEOUT, async {
        while Arc::strong_count(&hub) > 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the producer should stop");
    let requests = server.request_count();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.request_count(), requests);
    assert!(requests < 31);
}

#[test]
fn parents_query_quotes_and_joins_ids() {
    assert_eq!(
        parents_query(["a"].into_iter()),
        "'a' in parents and trashed = false"
    );
    assert_eq!(
        parents_query(["a", r"b'c\d"].into_iter()),
        r"('a' in parents or 'b\'c\\d' in parents) and trashed = false"
    );
}

#[test]
fn entry_kind_follows_the_mime_type() {
    let file = |mime: Option<&str>| google_drive3::api::File {
        mime_type: mime.map(String::from),
        ..google_drive3::api::File::default()
    };

    assert_eq!(
        EntryKind::of(&file(Some(MIME_TYPE_DRIVE_FOLDER))),
        EntryKind::Folder
    );
    assert_eq!(
        EntryKind::of(&file(Some("application/vnd.google-apps.shortcut"))),
        EntryKind::Shortcut
    );
    assert_eq!(
        EntryKind::of(&file(Some("application/vnd.google-apps.spreadsheet"))),
        EntryKind::Native
    );
    assert_eq!(
        EntryKind::of(&file(Some("application/pdf"))),
        EntryKind::Blob
    );
    assert_eq!(EntryKind::of(&file(None)), EntryKind::Blob);
}

#[test]
fn options_are_normalized() {
    let options = WalkOptions {
        listing_jobs: 0,
        buffer: 0,
        max_parents_per_query: 0,
        follow_shortcuts: true,
    }
    .normalized();
    assert_eq!(
        (
            options.listing_jobs,
            options.buffer,
            options.max_parents_per_query
        ),
        (1, 1, 1)
    );
    assert_eq!(
        WalkOptions {
            max_parents_per_query: 51,
            ..WalkOptions::default()
        }
        .normalized()
        .max_parents_per_query,
        MAX_PARENTS_PER_QUERY
    );
}

#[tokio::test]
async fn a_page_that_gets_503_is_retried() {
    let files = vec![
        FakeFile::folder("a", "a", "root"),
        FakeFile::blob("a1", "1.txt", "a", "x"),
        FakeFile::blob("r1", "top.txt", "root", "x"),
    ];
    let behavior = Behavior {
        unavailable_first: HashMap::from([(String::from("a"), 1)]),
        ..Behavior::default()
    };
    let walked = walk_all(files, behavior, options()).await;

    assert_eq!(walked.paths(), ["a", "a/1.txt", "top.txt"]);
    assert!(
        walked.errors().is_empty(),
        "a transient error is not a folder error"
    );
    let parents = walked
        .server
        .requests()
        .into_iter()
        .map(|request| request.parents)
        .collect::<Vec<_>>();
    let a = vec![String::from("a")];
    assert_eq!(
        parents,
        [vec![String::from("root")], a.clone(), a],
        "the same query again, not a split"
    );
}

#[tokio::test]
async fn a_page_with_resource_keys_that_gets_503_honors_retry_after() {
    // The request google-drive3 cannot build goes through the same retries, with the
    // Retry-After of its reply.
    let files = vec![
        FakeFile::folder("x", "x", "root").with_resource_key("kx"),
        FakeFile::blob("x1", "1.txt", "x", "x"),
    ];
    let behavior = Behavior {
        resource_keys: resource_keys(&[("x", "kx")]),
        unavailable_first: HashMap::from([(String::from("x"), 1)]),
        unavailable_retry_after: Some(1),
        ..Behavior::default()
    };
    let server = FakeDrive::start(files, behavior).await;

    let started = std::time::Instant::now();
    let walked = Walked {
        items: collect(start(&server, options())).await,
        server,
    };
    let elapsed = started.elapsed();
    walked.assert_item_order();

    assert_eq!(walked.paths(), ["x", "x/1.txt"]);
    assert!(walked.errors().is_empty());
    let requests = walked.server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].resource_keys.as_deref(), Some("x/kx"));
    assert_eq!(requests[2].resource_keys.as_deref(), Some("x/kx"));
    // The test policy's own backoff is at most 10 ms: only Retry-After explains a second.
    assert!(
        elapsed >= Duration::from_millis(950),
        "Retry-After: 1 must be honored, the walk took {elapsed:?}"
    );
}
