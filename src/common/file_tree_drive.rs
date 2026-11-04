pub(crate) mod errors;

use std::{collections::HashMap, iter, ops::Not, path::PathBuf, sync::Arc};

use futures::StreamExt;

use super::{file_tree_like, parse_md5_digest, FileLike, FileTreeLike, FolderInfoLike, FolderLike};
use crate::{
    common::{
        drive_file,
        file_tree_drive::errors::FileIdentifier,
        local_name::{self, NameChange, NameEntry, UniqueNames, UnusableName},
    },
    files::list,
    hub::Hub,
    transfer::walk::{Children, FolderKey, Walk, WalkItem, WalkOptions, WalkRoot},
};

#[derive(Debug, Clone)]
pub(crate) struct FileTreeDrive {
    pub(crate) root: Folder,
}

impl FileTreeDrive {
    /// Lists the subtree of the folder `file` and builds the tree. The listing is the
    /// [`transfer::walk`](crate::transfer::walk) with its default options: breadth-first, two
    /// list requests at a time, up to 50 folders per query.
    pub(crate) async fn from_file(
        hub: &Hub,
        file: google_drive3::api::File,
    ) -> Result<FileTreeDrive, errors::FileTreeDrive> {
        let root = Folder::from_file(hub, file)
            .await
            .map_err(errors::FileTreeDrive)?;
        Ok(FileTreeDrive { root })
    }
}

impl FileTreeLike for FileTreeDrive {
    type Folder = Folder;

    fn root(&self) -> &Self::Folder {
        &self.root
    }
}

type Node = file_tree_like::Node<Folder>;

#[derive(Debug, Clone)]
pub(crate) struct Folder {
    pub(crate) info: Arc<FolderInfo>,
    pub(crate) children: Vec<Node>,
}

#[derive(Debug, Clone)]
pub(crate) struct FolderInfo {
    /// Name on Drive.
    pub(crate) name: String,
    /// Safe name of the local directory, see [`local_name::unique_local_names`].
    pub(crate) local_name: String,
    pub(crate) parent: Option<Arc<Self>>,
}

impl Folder {
    /// Lists the subtree of the folder `file` and builds the folder from the listing.
    async fn from_file(
        hub: &Hub,
        file: google_drive3::api::File,
    ) -> Result<Folder, errors::Folder> {
        if drive_file::is_directory(&file).not() {
            return Err(errors::Folder::NotDirectory);
        }

        let name = file.name.ok_or(errors::Folder::MissingFileName)?;
        let file_id = file.id.ok_or(errors::Folder::MissingFileId)?;
        let local_name = root_local_name(&name, &file_id)?;

        let root = WalkRoot {
            id: file_id,
            name: name.clone(),
            drive_id: file.drive_id,
            resource_key: file.resource_key,
        };
        let mut listing = list_subtree(hub, root).await?;

        let info = Arc::new(FolderInfo {
            name,
            local_name,
            parent: None,
        });
        Ok(Folder::from_listing(info, FolderKey::ROOT, &mut listing))
    }

    /// Builds the folder listed under `key` and its subtree, taking them out of `listing`, and
    /// reports the local names that differ from the Drive names, now that the path is known.
    fn from_listing(info: Arc<FolderInfo>, key: FolderKey, listing: &mut Listing) -> Folder {
        // Every listed folder ended with `FolderDone` once the walk is complete.
        let ListedFolder { children, changes } = listing.remove(&key).unwrap_or_default();

        if changes.is_empty().not() {
            let folder_path = info.relative_path();
            for change in &changes {
                eprintln!("Warning: In '{}': {change}", folder_path.display());
            }
        }

        let mut folder = Folder {
            info,
            children: Vec::with_capacity(children.len()),
        };
        for child in children {
            let node = match child {
                ListedChild::Folder {
                    name,
                    local_name,
                    key,
                } => {
                    let info = Arc::new(FolderInfo {
                        name,
                        local_name,
                        parent: Some(Arc::clone(&folder.info)),
                    });
                    Node::Folder(Folder::from_listing(info, key, listing))
                }
                ListedChild::File(file) => Node::File(File {
                    name: file.name,
                    local_name: file.local_name,
                    size: file.size,
                    parent: Arc::clone(&folder.info),
                    drive_id: file.drive_id,
                    md5: file.md5,
                }),
            };
            folder.children.push(node);
        }

        folder
    }
}

/// What the tree keeps of a child while its folder is being listed: only folders and binary
/// files, and only the fields the tree uses. Documents and shortcuts, which a recursive download
/// skips, are dropped as they arrive.
#[derive(Debug, Clone)]
struct Child {
    name: Option<String>,
    id: Option<String>,
    kind: ChildKind,
}

#[derive(Debug, Clone)]
enum ChildKind {
    Folder(Children),
    File {
        size: Option<i64>,
        md5: Option<md5::Digest>,
    },
}

impl Child {
    /// `None` for a file the tree skips.
    fn from_file(file: google_drive3::api::File, children: Children) -> Option<Child> {
        let kind = if drive_file::is_directory(&file) {
            ChildKind::Folder(children)
        } else if drive_file::is_binary(&file) {
            ChildKind::File {
                size: file.size,
                md5: file.md5_checksum.as_deref().and_then(parse_md5_digest),
            }
        } else {
            return None;
        };
        Some(Child {
            name: file.name,
            id: file.id,
            kind,
        })
    }

    fn is_folder(&self) -> bool {
        matches!(self.kind, ChildKind::Folder(_))
    }

    fn identifier(&self) -> FileIdentifier {
        FileIdentifier::new(self.name.clone(), self.id.clone())
    }

    /// Checks the child and gives it its local name.
    fn finish(self, local_name: String) -> Result<ListedChild, errors::Folder> {
        let identifier = self.identifier();
        let Child { name, id, kind } = self;
        let (name, id) = match (name, id) {
            (Some(name), Some(id)) => (name, id),
            // Already reported by `children_local_names`.
            (name, _) => {
                let is_folder = matches!(kind, ChildKind::Folder(_));
                return Err(missing_name_or_id(identifier, is_folder, name.is_none()));
            }
        };

        match kind {
            ChildKind::Folder(children) => match children {
                Children::Listed(key) => Ok(ListedChild::Folder {
                    name,
                    local_name,
                    key,
                }),
                Children::None => Err(errors::Folder::MissingFileId),
                Children::Cycle => Err(errors::Folder::Cycle(identifier)),
            },
            ChildKind::File { size, md5 } => {
                let file_error = |source| errors::Folder::File {
                    identifier: FileIdentifier::Name(name.clone()),
                    source,
                };
                let size = size.ok_or_else(|| file_error(errors::File::MissingFileSize))?;
                let size = u64::try_from(size)
                    .map_err(|source| file_error(errors::File::InvalidFileSize(source)))?;
                Ok(ListedChild::File(ListedFile {
                    name,
                    local_name,
                    size,
                    drive_id: id,
                    md5,
                }))
            }
        }
    }
}

/// A child of a folder whose listing is complete, with its local name.
#[derive(Debug, Clone)]
enum ListedChild {
    Folder {
        name: String,
        local_name: String,
        key: FolderKey,
    },
    File(ListedFile),
}

#[derive(Debug, Clone)]
struct ListedFile {
    name: String,
    local_name: String,
    size: u64,
    drive_id: String,
    md5: Option<md5::Digest>,
}

/// A folder whose listing is complete.
#[derive(Debug, Clone, Default)]
struct ListedFolder {
    children: Vec<ListedChild>,
    /// Local names that differ from the Drive names, reported once the tree is built and the
    /// folder's local path is known.
    changes: Vec<NameChange>,
}

impl ListedFolder {
    /// Assigns the local names of a folder's children and checks them.
    fn finish(children: Vec<Child>) -> Result<ListedFolder, errors::Folder> {
        let unique = children_local_names(&children)?;
        let children = children
            .into_iter()
            .zip(unique.names)
            .map(|(child, local_name)| child.finish(local_name))
            .collect::<Result<_, _>>()?;
        Ok(ListedFolder {
            children,
            changes: unique.changes,
        })
    }
}

/// Every folder of the tree whose listing is complete.
type Listing = HashMap<FolderKey, ListedFolder>;

/// Runs the walk to its end. The children of a folder are kept in compact form while it is
/// being listed, and get their local names when the walk reports its end. The first folder that
/// cannot be listed, or whose children cannot be mapped to local names, ends the listing with
/// its error, as before the walk existed.
async fn list_subtree(hub: &Hub, root: WalkRoot) -> Result<Listing, errors::Folder> {
    let mut walk = Walk::start(Arc::new(hub.clone()), root, WalkOptions::default());
    let mut in_progress = HashMap::<FolderKey, Vec<Child>>::new();
    let mut listing = Listing::new();

    while let Some(item) = walk.next().await {
        match item {
            WalkItem::Entry(entry) => {
                let children = in_progress.entry(entry.parent).or_default();
                children.extend(Child::from_file(*entry.file, entry.children));
            }
            WalkItem::FolderDone(key) => {
                let children = in_progress.remove(&key).unwrap_or_default();
                listing.insert(key, ListedFolder::finish(children)?);
            }
            WalkItem::FolderError(error) => {
                return Err(errors::Folder::ListFiles(list::Error::ListFiles(
                    error.error,
                )));
            }
            WalkItem::IncompleteSearch(incomplete) => {
                for folder in &incomplete.folders {
                    eprintln!(
                        "Warning: Drive did not search everything while listing '{}', some \
                        files may be missing",
                        folder.display_path()
                    );
                }
            }
        }
    }

    Ok(listing)
}

fn root_local_name(name: &str, file_id: &str) -> Result<String, errors::Folder> {
    let local_name = local_name::local_name(name, file_id)
        .map_err(|source| errors::Folder::UnusableName(UnusableName::new(name, file_id, source)))?;

    if local_name::is_renamed(name, &local_name) {
        let change = NameChange::Sanitized {
            id: file_id.to_owned(),
            drive_name: name.to_owned(),
            local_name: local_name.clone(),
        };
        eprintln!("Warning: {change}");
    }

    Ok(local_name)
}

/// Assigns safe and unique local names to the children of one folder. The changes are the names
/// that differ from the Drive names.
fn children_local_names(children: &[Child]) -> Result<UniqueNames, errors::Folder> {
    let entries = children
        .iter()
        .map(|child| match (child.name.as_deref(), child.id.as_deref()) {
            (Some(name), Some(id)) => Ok(NameEntry { id, name }),
            (name, _) => Err(missing_name_or_id(
                child.identifier(),
                child.is_folder(),
                name.is_none(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;

    local_name::unique_local_names(&entries).map_err(errors::Folder::UnusableName)
}

fn missing_name_or_id(
    identifier: FileIdentifier,
    is_folder: bool,
    missing_name: bool,
) -> errors::Folder {
    if is_folder {
        if missing_name {
            errors::Folder::MissingFileName
        } else {
            errors::Folder::MissingFileId
        }
    } else {
        let source = if missing_name {
            errors::File::MissingFileName
        } else {
            errors::File::MissingFileId
        };
        errors::Folder::File { identifier, source }
    }
}

impl FolderLike for Folder {
    type File = File;

    type Info = FolderInfo;

    fn children(&self) -> &[file_tree_like::Node<Self>] {
        &self.children
    }

    fn info(&self) -> &Arc<Self::Info> {
        &self.info
    }
}

impl FolderInfo {
    /// Local path relative to the download destination, built from the local names.
    #[must_use]
    pub(crate) fn relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();

        for folder in self.ancestors() {
            path = local_name::safe_join(&path, &folder.local_name);
        }

        local_name::safe_join(&path, &self.local_name)
    }

    fn ancestors(&self) -> Vec<Arc<FolderInfo>> {
        let mut folders = iter::successors(self.parent.as_ref(), |folder_info| {
            folder_info.parent.as_ref()
        })
        .cloned()
        .collect::<Vec<_>>();

        folders.reverse();
        folders
    }
}

impl FolderInfoLike for FolderInfo {
    fn name(&self) -> &str {
        &self.name
    }

    fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct File {
    /// Name on Drive.
    pub(crate) name: String,
    /// Safe name of the local file, see [`local_name::unique_local_names`].
    pub(crate) local_name: String,
    pub(crate) size: u64,
    pub(crate) parent: Arc<FolderInfo>,
    pub(crate) drive_id: String,
    pub(crate) md5: Option<md5::Digest>,
}

impl File {
    /// Local path relative to the download destination, built from the local names.
    #[must_use]
    pub(crate) fn relative_path(&self) -> PathBuf {
        local_name::safe_join(&self.parent.relative_path(), &self.local_name)
    }
}

impl FileLike for File {
    fn name(&self) -> &str {
        &self.name
    }

    fn size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Component, Path},
        sync::Arc,
    };

    use super::{children_local_names, errors, Child, File, FileTreeDrive, FolderInfo};
    use crate::{
        common::{
            drive_file::MIME_TYPE_DRIVE_FOLDER, local_name::InvalidName, FileTreeLike, FolderLike,
        },
        files::list,
        transfer::{
            fake_drive::{Behavior, FakeDrive, FakeFile},
            walk::WalkOptions,
        },
    };

    fn folder_info(name: &str, parent: Option<&Arc<FolderInfo>>) -> Arc<FolderInfo> {
        Arc::new(FolderInfo {
            name: name.to_string(),
            local_name: name.to_string(),
            parent: parent.map(Arc::clone),
        })
    }

    /// A folder, or a binary file (it has an md5).
    fn drive_file(id: &str, name: &str, folder: bool) -> google_drive3::api::File {
        google_drive3::api::File {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            mime_type: folder.then(|| MIME_TYPE_DRIVE_FOLDER.to_string()),
            md5_checksum: (!folder).then(|| format!("{:x}", md5::compute(id))),
            size: (!folder).then_some(0),
            ..google_drive3::api::File::default()
        }
    }

    /// The children as the tree keeps them while their folder is listed.
    fn children(files: &[google_drive3::api::File]) -> Vec<Child> {
        files
            .iter()
            .map(|file| {
                let children = super::Children::Listed(super::FolderKey::ROOT);
                Child::from_file(file.clone(), children).expect("a folder or a binary file")
            })
            .collect()
    }

    #[test]
    fn folder_info_ancestors() {
        let folder_a = folder_info("a", None);
        let folder_b = folder_info("b", Some(&folder_a));
        let folder_c = folder_info("c", Some(&folder_b));
        let folder_d = folder_info("d", Some(&folder_c));

        let ancestors = folder_d.ancestors();
        let ancestors = ancestors.iter().map(|folder| folder.name.as_str());
        assert!(ancestors.eq(["a", "b", "c"]));
    }

    #[test]
    fn relative_paths_use_local_names() {
        let root = Arc::new(FolderInfo {
            name: "../root".to_string(),
            local_name: ".._root".to_string(),
            parent: None,
        });
        let sub = Arc::new(FolderInfo {
            name: "a/b".to_string(),
            local_name: "a_b".to_string(),
            parent: Some(Arc::clone(&root)),
        });
        let file = File {
            name: "/etc/passwd".to_string(),
            local_name: "_etc_passwd".to_string(),
            size: 0,
            parent: Arc::clone(&sub),
            drive_id: "id".to_string(),
            md5: None,
        };

        assert_eq!(sub.relative_path(), Path::new(".._root/a_b"));
        assert_eq!(file.relative_path(), Path::new(".._root/a_b/_etc_passwd"));
        assert!(file
            .relative_path()
            .components()
            .all(|component| matches!(component, Component::Normal(_))));
    }

    #[test]
    fn children_get_safe_unique_names() {
        let files = [
            drive_file("id-b", "report.pdf", false),
            drive_file("id-a", "report.pdf", false),
            drive_file("id-c", "../escape", true),
            drive_file("id-d", "plain", false),
        ];

        let names = children_local_names(&children(&files)).unwrap().names;
        assert_eq!(names[1], "report.pdf");
        assert!(names[0].starts_with("report~"), "{}", names[0]);
        assert_eq!(&names[0][names[0].len() - 4..], ".pdf");
        assert_eq!(names[2], ".._escape");
        assert_eq!(names[3], "plain");
    }

    #[test]
    fn unusable_child_name_is_an_error() {
        let files = [drive_file("id-a", "..", true)];

        let err = children_local_names(&children(&files)).unwrap_err();
        assert!(matches!(
            err,
            errors::Folder::UnusableName(ref name)
                if name.id == "id-a" && name.source == InvalidName::ParentDir
        ));
    }

    #[test]
    fn child_without_name_is_an_error() {
        let files = [google_drive3::api::File {
            name: None,
            ..drive_file("id-a", "", false)
        }];

        let err = children_local_names(&children(&files)).unwrap_err();
        assert!(matches!(
            err,
            errors::Folder::File {
                source: errors::File::MissingFileName,
                ..
            }
        ));
    }

    fn root_folder() -> google_drive3::api::File {
        google_drive3::api::File {
            id: Some(String::from("root")),
            name: Some(String::from("Root")),
            mime_type: Some(MIME_TYPE_DRIVE_FOLDER.to_string()),
            ..google_drive3::api::File::default()
        }
    }

    /// The relative paths of every file in the tree as path components, sorted. Components, not
    /// strings: the separator is `\` on Windows.
    fn file_paths(tree: &FileTreeDrive) -> Vec<Vec<String>> {
        let mut paths = tree
            .folders()
            .iter()
            .flat_map(|folder| folder.files())
            .map(|file| {
                file.relative_path()
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    /// The components of a `/`-separated expected path.
    fn components(path: &str) -> Vec<String> {
        path.split('/').map(String::from).collect()
    }

    async fn tree_from(
        files: Vec<FakeFile>,
        behavior: Behavior,
    ) -> (FakeDrive, Result<FileTreeDrive, errors::FileTreeDrive>) {
        let server = FakeDrive::start(files, behavior).await;
        let tree = FileTreeDrive::from_file(&server.hub(), root_folder()).await;
        (server, tree)
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
            delay: std::time::Duration::from_millis(20),
            ..Behavior::default()
        };

        let (server, tree) = tree_from(files, behavior).await;
        let tree = tree.unwrap();

        let info = tree.info();
        assert_eq!((info.folder_count, info.file_count), (8, 8));
        assert_eq!(
            file_paths(&tree),
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
            .map(components)
        );
        assert!(server.max_in_flight() <= WalkOptions::DEFAULT_LISTING_JOBS);
        // The listing before the walk sent one request per folder: 8. Batched, each level is
        // one query: the root, then a, b and c, then a1, a2, b1 and c1.
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
        assert!(parents.len() < 8);
    }

    #[tokio::test]
    async fn listed_children_get_safe_unique_names() {
        let files = vec![
            FakeFile::blob("id-b", "report.pdf", "root", "b"),
            FakeFile::blob("id-a", "report.pdf", "root", "a"),
            FakeFile::folder("id-c", "../escape", "root"),
            FakeFile::blob("id-d", "a/b", "id-c", "d"),
            FakeFile::document("id-e", "notes", "root"),
            FakeFile::shortcut("id-f", "link", "root", "id-a", "application/pdf"),
        ];

        let (_server, tree) = tree_from(files, Behavior::default()).await;
        let tree = tree.unwrap();

        let paths = file_paths(&tree);
        assert_eq!(
            paths.len(),
            3,
            "documents and shortcuts are skipped: {paths:?}"
        );
        assert_eq!(paths[0], components("Root/.._escape/a_b"));
        assert_eq!(paths[1], components("Root/report.pdf"));
        let [root, suffixed] = paths[2].as_slice() else {
            panic!("{:?} is not a child of the root", paths[2]);
        };
        assert_eq!(root, "Root");
        assert!(suffixed.starts_with("report~"), "{suffixed}");
        assert_eq!(&suffixed[suffixed.len() - 4..], ".pdf");
        let plain = tree
            .root
            .files()
            .into_iter()
            .find(|file| file.local_name == "report.pdf")
            .unwrap();
        assert_eq!(plain.drive_id, "id-a");
        assert_eq!(plain.size, 1);
        assert_eq!(plain.md5, Some(md5::compute("a")));
    }

    #[test]
    fn listing_keeps_only_compact_folders_and_binary_files() {
        let listed = |file: google_drive3::api::File| {
            Child::from_file(file, super::Children::Listed(super::FolderKey::ROOT))
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

        assert!(listed(document).is_none());
        assert!(listed(shortcut).is_none());
        assert!(listed(drive_file("a", "a", true)).is_some_and(|child| child.is_folder()));
        assert!(listed(drive_file("b", "b", false)).is_some_and(|child| !child.is_folder()));
        // A Drive `File` has every API field; the tree holds a small fraction of it per child.
        assert!(
            std::mem::size_of::<Child>() * 10 < std::mem::size_of::<google_drive3::api::File>()
        );
    }

    #[tokio::test]
    async fn unusable_name_stops_the_listing_when_its_folder_ends() {
        let mut files = vec![FakeFile::folder("bad", "..", "root")];
        let mut parent = String::from("root");
        for level in 0..4 {
            let id = format!("level{level}");
            files.push(FakeFile::folder(&id, &id, &parent));
            parent = id;
        }
        let behavior = Behavior {
            delay: std::time::Duration::from_millis(50),
            ..Behavior::default()
        };

        let (server, tree) = tree_from(files, behavior).await;

        assert!(matches!(
            tree.unwrap_err(),
            errors::FileTreeDrive(errors::Folder::UnusableName(ref name)) if name.id == "bad"
        ));
        // The root's children are named as soon as the root's listing ends: of the 5 requests of
        // the whole tree, only the root's and at most the next one were sent.
        assert!(server.request_count() <= 2, "{}", server.request_count());
    }

    #[tokio::test]
    async fn unreadable_subfolder_fails_the_tree() {
        let files = vec![
            FakeFile::folder("a", "a", "root"),
            FakeFile::folder("b", "b", "root"),
        ];
        let behavior = Behavior {
            unreadable: std::collections::HashSet::from([String::from("b")]),
            ..Behavior::default()
        };

        let (_server, tree) = tree_from(files, behavior).await;

        assert!(matches!(
            tree.unwrap_err(),
            errors::FileTreeDrive(errors::Folder::ListFiles(list::Error::ListFiles(_)))
        ));
    }

    #[tokio::test]
    async fn folder_listed_inside_itself_is_an_error() {
        let files = vec![
            FakeFile::folder("a", "a", "root"),
            FakeFile::folder("a", "a", "a"),
        ];

        let (_server, tree) = tree_from(files, Behavior::default()).await;

        assert!(matches!(
            tree.unwrap_err(),
            errors::FileTreeDrive(errors::Folder::Cycle(_))
        ));
    }

    #[tokio::test]
    async fn root_that_is_not_a_folder_is_rejected_without_listing() {
        let server = FakeDrive::start(Vec::new(), Behavior::default()).await;
        let file = google_drive3::api::File {
            mime_type: Some(String::from("text/plain")),
            ..root_folder()
        };

        let tree = FileTreeDrive::from_file(&server.hub(), file).await;

        assert!(matches!(
            tree.unwrap_err(),
            errors::FileTreeDrive(errors::Folder::NotDirectory)
        ));
        assert_eq!(server.request_count(), 0);
    }
}
