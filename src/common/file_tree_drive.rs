pub(crate) mod errors;

use std::{iter, ops::Not, path::PathBuf, sync::Arc};

use async_recursion::async_recursion;

use super::{file_tree_like, parse_md5_digest, FileLike, FileTreeLike, FolderInfoLike, FolderLike};
use crate::{
    common::{
        drive_file,
        file_tree_drive::errors::FileIdentifier,
        local_name::{self, NameChange, NameEntry, UnusableName},
    },
    files::list::{self, ListQuery, ListSortOrder},
    hub::Hub,
};

#[derive(Debug, Clone)]
pub(crate) struct FileTreeDrive {
    pub(crate) root: Folder,
}

impl FileTreeDrive {
    pub(crate) async fn from_file(
        hub: &Hub,
        file: google_drive3::api::File,
    ) -> Result<FileTreeDrive, errors::FileTreeDrive> {
        let root = Folder::from_file(hub, file, None, None)
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
    /// Builds the folder and its subtree. `local_name` is the name assigned by the parent folder;
    /// the root (`None`) maps its own name.
    #[async_recursion]
    async fn from_file(
        hub: &Hub,
        file: google_drive3::api::File,
        parent: Option<&'async_recursion Arc<FolderInfo>>,
        local_name: Option<String>,
    ) -> Result<Folder, errors::Folder> {
        if drive_file::is_directory(&file).not() {
            return Err(errors::Folder::NotDirectory);
        }

        let name = file.name.ok_or(errors::Folder::MissingFileName)?;
        let file_id = file.id.ok_or(errors::Folder::MissingFileId)?;

        let local_name = match local_name {
            Some(local_name) => local_name,
            None => root_local_name(&name, &file_id)?,
        };

        let mut folder = Folder {
            info: Arc::new(FolderInfo {
                name,
                local_name,
                parent: parent.map(Arc::clone),
            }),
            children: Vec::new(),
        };

        let files = list::list_files(
            hub,
            list::ListFilesConfig {
                query: &ListQuery::files_in_folder(file_id),
                order_by: &ListSortOrder::default(),
                max_files: usize::MAX,
            },
        )
        .await
        .map_err(errors::Folder::ListFiles)?;

        // Skip documents
        let files = files
            .into_iter()
            .filter(|file| drive_file::is_directory(file) || drive_file::is_binary(file))
            .collect::<Vec<_>>();
        let local_names = children_local_names(&files, &folder.info)?;

        let mut children = Vec::new();

        for (file, local_name) in files.into_iter().zip(local_names) {
            if drive_file::is_directory(&file) {
                let folder =
                    Folder::from_file(hub, file, Some(&folder.info), Some(local_name)).await?;
                let node = Node::Folder(folder);
                children.push(node);
            } else {
                let f = File::from_file(file, &folder, local_name)
                    .map_err(|(source, identifier)| errors::Folder::File { identifier, source })?;
                let node = Node::File(f);
                children.push(node);
            }
        }

        folder.children = children;

        Ok(folder)
    }
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

    use super::{children_local_names, errors, File, FolderInfo};
    use crate::common::{drive_file::MIME_TYPE_DRIVE_FOLDER, local_name::InvalidName};

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
}
