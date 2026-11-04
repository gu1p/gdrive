pub(crate) mod errors;

use std::{collections::HashMap, iter, ops::Not, path::PathBuf, sync::Arc};

use futures::StreamExt;

use super::{file_tree_like, parse_md5_digest, FileLike, FileTreeLike, FolderInfoLike, FolderLike};
use crate::{
    common::{
        drive_file,
        file_tree_drive::errors::FileIdentifier,
        local_name::{self, NameChange, NameEntry, UnusableName},
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

/// The children of every listed folder, in listing order, with whether the walk listed them.
type Listing = HashMap<FolderKey, Vec<(google_drive3::api::File, Children)>>;

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
        };
        let mut listing = list_subtree(hub, root).await?;

        let info = Arc::new(FolderInfo {
            name,
            local_name,
            parent: None,
        });
        Folder::from_listing(info, FolderKey::ROOT, &mut listing)
    }

    /// Builds the folder listed under `key` and its subtree, taking their children out of
    /// `listing`.
    fn from_listing(
        info: Arc<FolderInfo>,
        key: FolderKey,
        listing: &mut Listing,
    ) -> Result<Folder, errors::Folder> {
        // Skip documents and shortcuts
        let (files, children): (Vec<_>, Vec<_>) = listing
            .remove(&key)
            .unwrap_or_default()
            .into_iter()
            .filter(|(file, _)| drive_file::is_directory(file) || drive_file::is_binary(file))
            .unzip();
        let local_names = children_local_names(&files, &info)?;

        let mut folder = Folder {
            info,
            children: Vec::with_capacity(files.len()),
        };

        for ((file, children), local_name) in files.into_iter().zip(children).zip(local_names) {
            let node = if drive_file::is_directory(&file) {
                Node::Folder(Folder::subfolder(
                    file,
                    children,
                    &folder.info,
                    local_name,
                    listing,
                )?)
            } else {
                let file = File::from_file(file, &folder, local_name)
                    .map_err(|(source, identifier)| errors::Folder::File { identifier, source })?;
                Node::File(file)
            };
            folder.children.push(node);
        }

        Ok(folder)
    }

    /// Builds a child folder. Its name and id were checked by [`children_local_names`].
    fn subfolder(
        file: google_drive3::api::File,
        children: Children,
        parent: &Arc<FolderInfo>,
        local_name: String,
        listing: &mut Listing,
    ) -> Result<Folder, errors::Folder> {
        let key = match children {
            Children::Listed(key) => key,
            Children::None => return Err(errors::Folder::MissingFileId),
            Children::Cycle => return Err(errors::Folder::Cycle(FileIdentifier::from(file))),
        };
        let name = file.name.ok_or(errors::Folder::MissingFileName)?;

        let info = Arc::new(FolderInfo {
            name,
            local_name,
            parent: Some(Arc::clone(parent)),
        });
        Folder::from_listing(info, key, listing)
    }
}

/// Runs the walk to its end and groups the entries by the folder that listed them. The first
/// folder that cannot be listed ends the listing with its error, as before the walk existed.
async fn list_subtree(hub: &Hub, root: WalkRoot) -> Result<Listing, errors::Folder> {
    let mut walk = Walk::start(Arc::new(hub.clone()), root, WalkOptions::default());
    let mut listing = Listing::new();

    while let Some(item) = walk.next().await {
        match item {
            WalkItem::Entry(entry) => listing
                .entry(entry.parent)
                .or_default()
                .push((*entry.file, entry.children)),
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

/// Assigns safe and unique local names to the children of one folder, warning about every name
/// that differs from the Drive name.
fn children_local_names(
    files: &[google_drive3::api::File],
    parent: &FolderInfo,
) -> Result<Vec<String>, errors::Folder> {
    let entries = files
        .iter()
        .map(|file| match (file.name.as_deref(), file.id.as_deref()) {
            (Some(name), Some(id)) => Ok(NameEntry { id, name }),
            (name, _) => Err(missing_name_or_id(file, name.is_none())),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let unique = local_name::unique_local_names(&entries).map_err(errors::Folder::UnusableName)?;

    if unique.changes.is_empty().not() {
        let folder_path = parent.relative_path();
        for change in &unique.changes {
            eprintln!("Warning: In '{}': {change}", folder_path.display());
        }
    }

    Ok(unique.names)
}

fn missing_name_or_id(file: &google_drive3::api::File, missing_name: bool) -> errors::Folder {
    if drive_file::is_directory(file) {
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
        errors::Folder::File {
            identifier: FileIdentifier::from(file),
            source,
        }
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
    fn from_file(
        file: google_drive3::api::File,
        parent: &Folder,
        local_name: String,
    ) -> Result<File, (errors::File, FileIdentifier)> {
        let name = file
            .name
            .ok_or((errors::File::MissingFileName, FileIdentifier::None))?;
        let Some(size) = file.size else {
            return Err((errors::File::MissingFileSize, FileIdentifier::Name(name)));
        };
        let size = match size.try_into() {
            Ok(size) => size,
            Err(source) => {
                return Err((
                    errors::File::InvalidFileSize(source),
                    FileIdentifier::Name(name),
                ))
            }
        };
        let Some(file_id) = file.id else {
            return Err((errors::File::MissingFileId, FileIdentifier::Name(name)));
        };
        let md5 = file.md5_checksum.as_deref().and_then(parse_md5_digest);

        let file = File {
            name,
            local_name,
            size,
            parent: Arc::clone(&parent.info),
            drive_id: file_id,
            md5,
        };

        Ok(file)
    }

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

    use super::{children_local_names, errors, File, FileTreeDrive, FolderInfo};
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

    fn drive_file(id: &str, name: &str, folder: bool) -> google_drive3::api::File {
        google_drive3::api::File {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            mime_type: folder.then(|| MIME_TYPE_DRIVE_FOLDER.to_string()),
            ..google_drive3::api::File::default()
        }
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
        let parent = folder_info("parent", None);
        let files = [
            drive_file("id-b", "report.pdf", false),
            drive_file("id-a", "report.pdf", false),
            drive_file("id-c", "../escape", true),
            drive_file("id-d", "plain", false),
        ];

        let names = children_local_names(&files, &parent).unwrap();
        assert_eq!(names[1], "report.pdf");
        assert!(names[0].starts_with("report~"), "{}", names[0]);
        assert_eq!(&names[0][names[0].len() - 4..], ".pdf");
        assert_eq!(names[2], ".._escape");
        assert_eq!(names[3], "plain");
    }

    #[test]
    fn unusable_child_name_is_an_error() {
        let parent = folder_info("parent", None);
        let files = [drive_file("id-a", "..", true)];

        let err = children_local_names(&files, &parent).unwrap_err();
        assert!(matches!(
            err,
            errors::Folder::UnusableName(ref name)
                if name.id == "id-a" && name.source == InvalidName::ParentDir
        ));
    }

    #[test]
    fn child_without_name_is_an_error() {
        let parent = folder_info("parent", None);
        let files = [google_drive3::api::File {
            id: Some("id-a".to_string()),
            ..google_drive3::api::File::default()
        }];

        let err = children_local_names(&files, &parent).unwrap_err();
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

    /// The relative paths of every file in the tree, sorted.
    fn file_paths(tree: &FileTreeDrive) -> Vec<String> {
        let mut paths = tree
            .folders()
            .iter()
            .flat_map(|folder| folder.files())
            .map(|file| file.relative_path().display().to_string())
            .collect::<Vec<_>>();
        paths.sort();
        paths
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
        assert_eq!(paths[0], "Root/.._escape/a_b");
        assert_eq!(paths[1], "Root/report.pdf");
        assert!(paths[2].starts_with("Root/report~"), "{}", paths[2]);
        assert_eq!(&paths[2][paths[2].len() - 4..], ".pdf");
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
