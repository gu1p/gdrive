pub(crate) mod errors;

use std::{
    borrow::Cow,
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_recursion::async_recursion;

use super::{FileLike, FileTreeLike, FolderInfoLike, FolderLike};
use crate::common::{file_info::FileInfo, file_tree_like, id_gen::IdGen};

#[derive(Debug)]
pub(crate) struct FileTree {
    pub(crate) root: Folder,
    /// The entries below the root that cannot be uploaded, in walk order. The tree holds the
    /// rest.
    pub(crate) unreadable: Vec<Unreadable>,
}

/// An entry of the local tree that cannot be uploaded: the walk goes on without it, and a
/// recursive upload reports it as failed.
#[derive(Debug)]
pub(crate) struct Unreadable {
    /// Relative to the parent of the walked directory, like [`File::relative_path`].
    pub(crate) relative_path: PathBuf,
    /// A directory (its contents are unknown), else a file or an entry of unknown type.
    pub(crate) is_dir: bool,
    pub(crate) error: errors::Folder,
}

impl FileTree {
    /// Walks the local directory `path` for a recursive upload.
    ///
    /// Symlinks are followed, to files and to directories, so an upload contains what `cp -rL`
    /// would copy, under the names of the links. A directory whose canonical path is one of the
    /// directories being walked above it (a symlink cycle) is skipped with a warning. A broken
    /// symlink, an entry whose metadata cannot be read, an unreadable file or directory and an
    /// entry that is neither a file nor a directory are left out and listed in
    /// [`FileTree::unreadable`]. Only a root that cannot be read and a failure to get Drive ids
    /// are errors.
    pub(crate) async fn from_path(
        path: &Path,
        ids: &mut IdGen<'_>,
    ) -> Result<FileTree, errors::FileTree> {
        let canonical_path = path
            .canonicalize()
            .map_err(errors::FileTree::Canonicalize)?;

        let mut walk = Walk {
            ancestors: HashSet::from([canonical_path.clone()]),
            unreadable: Vec::new(),
        };
        let root = Folder::from_path(&canonical_path, None, ids, &mut walk)
            .await
            .map_err(errors::FileTree::Folder)?;
        Ok(FileTree {
            root,
            unreadable: walk.unreadable,
        })
    }
}

/// The state of one walk of a local tree.
struct Walk {
    /// The canonical paths of the directory being walked and of the directories above it.
    ancestors: HashSet<PathBuf>,
    unreadable: Vec<Unreadable>,
}

impl Walk {
    /// Notes an entry of `folder` that cannot be uploaded.
    fn unreadable(&mut self, folder: &Folder, path: &Path, is_dir: bool, error: errors::Folder) {
        self.unreadable.push(Unreadable {
            relative_path: get_relative_path(path, &folder.info).to_path_buf(),
            is_dir,
            error,
        });
    }
}

impl FileTreeLike for FileTree {
    type Folder = Folder;

    fn root(&self) -> &Self::Folder {
        &self.root
    }
}

type Node = file_tree_like::Node<Folder>;

#[derive(Debug, Clone)]
pub(crate) struct FolderInfo {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) parent: Option<Arc<FolderInfo>>,
    pub(crate) drive_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Folder {
    pub(crate) info: Arc<FolderInfo>,
    pub(crate) children: Vec<Node>,
}

impl Folder {
    /// Walks the directory `path`. Its entries that cannot be uploaded go to
    /// [`Walk::unreadable`]; the errors are about the directory itself (it cannot be read, or no
    /// Drive id could be had), which the caller notes or returns.
    #[async_recursion]
    async fn from_path(
        path: &Path,
        parent: Option<&'async_recursion Folder>,
        ids: &mut IdGen<'_>,
        walk: &mut Walk,
    ) -> Result<Folder, errors::Folder> {
        use errors::Folder as E;

        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or(E::InvalidPath)?;

        let entries = fs::read_dir(path).map_err(E::ReadDir)?;
        let drive_id = ids.next().await.map_err(E::GenerateId)?;

        let mut folder = Folder {
            info: Arc::new(FolderInfo {
                name,
                path: path.to_path_buf(),
                parent: parent.map(|folder| Arc::clone(&folder.info)),
                drive_id,
            }),
            children: Vec::new(),
        };

        for e in entries {
            let entry = match e {
                Ok(entry) => entry,
                Err(source) => {
                    walk.unreadable(&folder, path, true, E::ReadDirEntry(source));
                    continue;
                }
            };
            let path = entry.path();

            // fs::metadata follows symlinks: a link is uploaded as what it points to.
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(source) => {
                    let is_symlink = entry.file_type().is_ok_and(|ty| ty.is_symlink());
                    let error = if is_symlink {
                        E::BrokenSymlink {
                            path: path.clone(),
                            source,
                        }
                    } else {
                        E::Metadata {
                            path: path.clone(),
                            source,
                        }
                    };
                    walk.unreadable(&folder, &path, false, error);
                    continue;
                }
            };

            if metadata.is_dir() {
                let canonical_path = match path.canonicalize() {
                    Ok(canonical_path) => canonical_path,
                    Err(source) => {
                        let error = E::Canonicalize {
                            path: path.clone(),
                            source,
                        };
                        walk.unreadable(&folder, &path, true, error);
                        continue;
                    }
                };

                if walk.ancestors.contains(&canonical_path) {
                    eprintln!(
                        "Warning: Skipping directory '{}': it resolves to '{}', which contains \
                        it (symlink cycle)",
                        path.display(),
                        canonical_path.display(),
                    );
                    continue;
                }

                walk.ancestors.insert(canonical_path.clone());
                let nested = Folder::from_path(&path, Some(&folder), ids, walk).await;
                walk.ancestors.remove(&canonical_path);

                match nested {
                    Ok(nested) => folder.children.push(Node::Folder(nested)),
                    // Without Drive ids nothing can be uploaded.
                    Err(source @ E::GenerateId(_)) => {
                        return Err(E::Nested {
                            path,
                            source: Box::new(source),
                        });
                    }
                    Err(error) => walk.unreadable(&folder, &path, true, error),
                }
            } else if metadata.is_file() {
                match File::from_path(&path, &folder, ids).await {
                    Ok(file) => folder.children.push(Node::File(file)),
                    Err(source @ errors::File::GenerateId(_)) => {
                        return Err(E::File { path, source });
                    }
                    Err(source) => {
                        let error = E::File {
                            path: path.clone(),
                            source,
                        };
                        walk.unreadable(&folder, &path, false, error);
                    }
                }
            } else {
                walk.unreadable(&folder, &path, false, E::UnknownFileType(path.clone()));
            }
        }

        Ok(folder)
    }

    #[must_use]
    pub(crate) fn relative_path(&self) -> &Path {
        get_relative_path(&self.info.path, &self.info)
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
    fn root(&self) -> &Self {
        let mut folder = self;
        while let Some(parent) = &folder.parent {
            folder = parent;
        }
        folder
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
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) mime_type: mime::Mime,
    pub(crate) parent: Arc<FolderInfo>,
    pub(crate) drive_id: String,
}

impl File {
    async fn from_path(
        path: &Path,
        parent: &Folder,
        ids: &mut IdGen<'_>,
    ) -> Result<File, errors::File> {
        use errors::File as E;

        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or(E::InvalidPath)?;

        let os_file = fs::File::open(path).map_err(E::Open)?;
        let size = os_file.metadata().map_or(0, |m| m.len());
        let mime_type = mime_guess::from_path(path)
            .first()
            .unwrap_or(mime::APPLICATION_OCTET_STREAM);
        let drive_id = ids.next().await.map_err(E::GenerateId)?;

        let file = File {
            name,
            path: path.to_path_buf(),
            size,
            mime_type,
            parent: Arc::clone(&parent.info),
            drive_id,
        };

        Ok(file)
    }

    #[must_use]
    pub(crate) fn relative_path(&self) -> &Path {
        get_relative_path(&self.path, &self.parent)
    }

    #[must_use]
    pub(crate) fn info(&self, parents: Option<Vec<String>>) -> FileInfo<'_> {
        FileInfo {
            name: Cow::Borrowed(&self.name),
            size: self.size,
            mime_type: Cow::Borrowed(&self.mime_type),
            source_mime_type: None,
            parents,
        }
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

fn get_relative_path<'a>(path: &'a Path, folder_info: &FolderInfo) -> &'a Path {
    folder_info
        .root()
        .path
        .parent()
        .and_then(|root_path| path.strip_prefix(root_path).ok())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use super::{errors, File, FileTree, Folder, Node};
    use crate::common::{
        drive_file::MIME_TYPE_CSV_MIME, file_tree::FolderInfo, id_gen::IdGen, FileTreeLike,
        FolderLike,
    };

    struct Walked {
        folders: BTreeSet<PathBuf>,
        files: BTreeMap<PathBuf, u64>,
        unreadable: Vec<super::Unreadable>,
    }

    async fn walk(root: &Path) -> Result<Walked, errors::FileTree> {
        let tree = FileTree::from_path(root, &mut IdGen::sequential()).await?;
        let mut folders = BTreeSet::new();
        let mut files = BTreeMap::new();
        for folder in tree.folders() {
            folders.insert(folder.relative_path().to_path_buf());
            for file in folder.files() {
                files.insert(file.relative_path().to_path_buf(), file.size);
            }
        }
        Ok(Walked {
            folders,
            files,
            unreadable: tree.unreadable,
        })
    }

    impl Walked {
        /// The relative paths of the unreadable entries, with whether they are directories.
        fn unreadable(&self) -> BTreeMap<PathBuf, bool> {
            self.unreadable
                .iter()
                .map(|entry| (entry.relative_path.clone(), entry.is_dir))
                .collect()
        }
    }

    fn paths<const N: usize>(paths: [&str; N]) -> BTreeSet<PathBuf> {
        paths.into_iter().map(PathBuf::from).collect()
    }

    fn sizes<const N: usize>(entries: [(&str, u64); N]) -> BTreeMap<PathBuf, u64> {
        entries
            .into_iter()
            .map(|(path, size)| (PathBuf::from(path), size))
            .collect()
    }

    #[tokio::test]
    async fn walks_plain_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"abc").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"b").unwrap();

        let walked = walk(&root).await.unwrap();
        assert_eq!(walked.folders, paths(["root", "root/sub"]));
        assert_eq!(
            walked.files,
            sizes([("root/a.txt", 3), ("root/sub/b.txt", 1)])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn follows_symlinks_to_files_and_directories() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        std::fs::write(root.join("real.txt"), b"hello").unwrap();
        std::fs::write(root.join("dir/inner.txt"), b"inner").unwrap();
        std::fs::write(tmp.path().join("outside/out.txt"), b"out!").unwrap();
        symlink("real.txt", root.join("link.txt")).unwrap();
        symlink("dir", root.join("linkdir")).unwrap();
        symlink("../outside", root.join("outlink")).unwrap();

        let walked = walk(&root).await.unwrap();
        assert_eq!(
            walked.folders,
            paths(["root", "root/dir", "root/linkdir", "root/outlink"])
        );
        assert_eq!(
            walked.files,
            sizes([
                ("root/dir/inner.txt", 5),
                ("root/link.txt", 5),
                ("root/linkdir/inner.txt", 5),
                ("root/outlink/out.txt", 4),
                ("root/real.txt", 5),
            ])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_symlink_cycles() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/file.txt"), b"x").unwrap();
        symlink(".", root.join("self")).unwrap();
        symlink(".", root.join("a/again")).unwrap();
        symlink("../..", root.join("a/b/up")).unwrap();
        symlink(&root, root.join("a/b/absolute")).unwrap();

        let walked = walk(&root).await.unwrap();
        assert_eq!(walked.folders, paths(["root", "root/a", "root/a/b"]));
        assert_eq!(walked.files, sizes([("root/a/file.txt", 1)]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directory_linked_twice_is_walked_twice() {
        use std::os::unix::fs::symlink;

        // Not a cycle: like `cp -rL`, each link gets its own copy.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("x")).unwrap();
        std::fs::write(root.join("x/f.txt"), b"f").unwrap();
        symlink("x", root.join("y")).unwrap();
        symlink("x", root.join("z")).unwrap();

        let walked = walk(&root).await.unwrap();
        assert_eq!(
            walked.folders,
            paths(["root", "root/x", "root/y", "root/z"])
        );
        assert_eq!(
            walked.files,
            sizes([
                ("root/x/f.txt", 1),
                ("root/y/f.txt", 1),
                ("root/z/f.txt", 1)
            ])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_broken_symlink_is_left_out_and_reported() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("ok.txt"), b"ok").unwrap();
        symlink("missing", root.join("dangling")).unwrap();

        let walked = walk(&root).await.unwrap();
        assert_eq!(walked.files, sizes([("root/ok.txt", 2)]));
        assert_eq!(
            walked.unreadable(),
            BTreeMap::from([(PathBuf::from("root/dangling"), false)])
        );
        assert!(
            matches!(
                &walked.unreadable[0].error,
                errors::Folder::BrokenSymlink { path, .. } if path.ends_with("dangling")
            ),
            "{:?}",
            walked.unreadable
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_loop_is_left_out_and_reported() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/f.txt"), b"f").unwrap();
        symlink("l2", root.join("sub/l1")).unwrap();
        symlink("l1", root.join("sub/l2")).unwrap();

        let walked = walk(&root).await.unwrap();
        assert_eq!(walked.folders, paths(["root", "root/sub"]));
        assert_eq!(walked.files, sizes([("root/sub/f.txt", 1)]));
        assert_eq!(
            walked.unreadable(),
            BTreeMap::from([
                (PathBuf::from("root/sub/l1"), false),
                (PathBuf::from("root/sub/l2"), false)
            ])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_entries_are_left_out_and_reported() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(root.join("ok.txt"), b"ok").unwrap();
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"s").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Permissions do not apply to root; nothing to check then.
        if std::fs::read_dir(&locked).is_ok() {
            return;
        }

        let walked = walk(&root).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let walked = walked.unwrap();

        assert_eq!(walked.folders, paths(["root"]));
        assert_eq!(walked.files, sizes([("root/ok.txt", 2)]));
        assert_eq!(
            walked.unreadable(),
            BTreeMap::from([
                (PathBuf::from("root/locked"), true),
                (PathBuf::from("root/secret.txt"), false)
            ])
        );
        for entry in &walked.unreadable {
            assert!(
                matches!(
                    &entry.error,
                    errors::Folder::ReadDir(_)
                        | errors::Folder::File {
                            source: errors::File::Open(_),
                            ..
                        }
                ),
                "{entry:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_root_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&root).is_ok() {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let walked = walk(&root).await;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(
                walked.err(),
                Some(errors::FileTree::Folder(errors::Folder::ReadDir(_)))
            ),
            "the root cannot be left out"
        );
    }

    #[test]
    fn folder_folders_recursive() {
        let folder_a = Arc::new(super::FolderInfo {
            name: "a".to_string(),
            path: PathBuf::from("a"),
            parent: None,
            drive_id: "a".to_string(),
        });
        let folder_b = Arc::new(FolderInfo {
            name: "b".to_string(),
            path: PathBuf::from("a/b"),
            parent: Some(Arc::clone(&folder_a)),
            drive_id: "b".to_string(),
        });
        let folder_c = Arc::new(FolderInfo {
            name: "c".to_string(),
            path: PathBuf::from("a/c"),
            parent: Some(Arc::clone(&folder_a)),
            drive_id: "c".to_string(),
        });
        let folder_e = Arc::new(FolderInfo {
            name: "e".to_string(),
            path: PathBuf::from("a/b/e"),
            parent: Some(Arc::clone(&folder_b)),
            drive_id: "e".to_string(),
        });

        let folder = Folder {
            info: Arc::clone(&folder_a),
            children: vec![
                Node::Folder(Folder {
                    info: folder_b,
                    children: vec![Node::Folder(Folder {
                        info: folder_e,
                        children: vec![],
                    })],
                }),
                Node::Folder(Folder {
                    info: Arc::clone(&folder_c),
                    children: vec![Node::File(File {
                        name: "f".to_string(),
                        path: PathBuf::from("a/c/f"),
                        size: 12,
                        mime_type: MIME_TYPE_CSV_MIME.clone(),
                        parent: Arc::clone(&folder_c),
                        drive_id: "f".to_string(),
                    })],
                }),
                Node::File(File {
                    name: "d".to_string(),
                    path: PathBuf::from("a/d"),
                    size: 12,
                    mime_type: MIME_TYPE_CSV_MIME.clone(),
                    parent: folder_a,
                    drive_id: "d".to_string(),
                }),
            ],
        };

        folder
            .folders_recursive()
            .iter()
            .map(|folder| folder.info.path.as_path())
            .eq([
                Path::new("a"),
                Path::new("a/b"),
                Path::new("a/b/e"),
                Path::new("a/c"),
                Path::new("a/c/f"),
                Path::new("a/d"),
            ]);
    }

    #[test]
    fn check_folder_not_leaking() {
        let folder_a = Arc::new(super::FolderInfo {
            name: "a".to_string(),
            path: PathBuf::from("a"),
            parent: None,
            drive_id: "a".to_string(),
        });
        let folder_b = Arc::new(FolderInfo {
            name: "b".to_string(),
            path: PathBuf::from("a/b"),
            parent: Some(Arc::clone(&folder_a)),
            drive_id: "b".to_string(),
        });

        let weak_folder_a = Arc::downgrade(&folder_a);
        let weak_folder_b = Arc::downgrade(&folder_b);

        let folder = Folder {
            info: folder_a,
            children: vec![Node::Folder(Folder {
                info: folder_b,
                children: vec![],
            })],
        };
        drop(folder);

        assert_eq!(weak_folder_a.strong_count(), 0);
        assert_eq!(weak_folder_b.strong_count(), 0);
    }
}
