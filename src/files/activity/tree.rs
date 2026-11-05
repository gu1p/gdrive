//! The items a report is about: each `FILE_ID` and, for a folder, everything in it that is not
//! in the trash, listed by the walk ([`crate::transfer::walk`]) before the audit log is read.
//! Every item gets its path, the names from the given item down. A listing that fails or is
//! incomplete fails the command: a report over part of a tree would tell wrongly that the people
//! who use the rest are gone. A `FILE_ID` in the trash (itself or through a folder above it,
//! which Drive reports the same way) is refused, as the items in the trash below a folder are
//! left out.

use std::{collections::HashMap, sync::Arc};

use futures::StreamExt;
use google_drive3::api::File;

use super::Error;
use crate::{
    common::{drive_file, retry::retry_read},
    hub::Hub,
    transfer::walk::{EntryKind, Walk, WalkItem, WalkOptions, WalkRoot},
};

/// What the report needs of a `FILE_ID`.
const ROOT_FIELDS: &str = "id,name,mimeType,driveId,resourceKey,trashed,explicitlyTrashed";

/// A `FILE_ID`, as Drive resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Root {
    /// Drive's id: for `root`, the id of the top folder of My Drive.
    pub(super) id: String,
    /// The item's name, which starts the paths below it.
    pub(super) name: String,
    /// The shared drive it is in, if any.
    pub(super) drive_id: Option<String>,
}

/// Every item of the report.
#[derive(Debug, Default)]
pub(super) struct Items {
    /// Each item's path: the Drive names from its `FILE_ID` down, joined with `/`, as Drive has
    /// them (not escaped). An item found on several paths keeps the shortest, then the first in
    /// order.
    pub(super) paths: HashMap<String, String>,
    pub(super) files: u64,
    pub(super) folders: u64,
    /// The `FILE_ID`s, each once, in the order given.
    pub(super) roots: Vec<Root>,
}

impl Items {
    /// Adds the item `id` at `path`; a new item is counted as a folder or a file.
    fn add(&mut self, id: String, path: String, folder: bool) {
        if let Some(known) = self.paths.get_mut(&id) {
            let shorter =
                (path.matches('/').count(), &path) < (known.matches('/').count(), &*known);
            if shorter {
                *known = path;
            }
        } else {
            self.paths.insert(id, path);
            if folder {
                self.folders += 1;
            } else {
                self.files += 1;
            }
        }
    }
}

/// Gets every `FILE_ID` of `ids` and lists the folders among them, `listing_jobs` requests at
/// once.
pub(super) async fn resolve(
    hub: &Hub,
    ids: &[String],
    listing_jobs: usize,
) -> Result<Items, Error> {
    let mut items = Items::default();
    for id in ids {
        let file = get(hub, id).await.map_err(|source| Error::GetItem {
            id: id.clone(),
            source,
        })?;
        let root = Root {
            id: file.id.clone().unwrap_or_else(|| id.clone()),
            name: file.name.clone().unwrap_or_default(),
            drive_id: file.drive_id.clone(),
        };
        if file.trashed == Some(true) {
            return Err(Error::Trashed {
                id: id.clone(),
                name: root.name,
                // Drive reports an item in a trashed folder as trashed, not explicitly.
                by_folder: file.explicitly_trashed == Some(false),
            });
        }
        if items.roots.iter().any(|known| known.id == root.id) {
            continue;
        }
        let folder = drive_file::is_directory(&file);
        items.add(root.id.clone(), root.name.clone(), folder);
        if folder {
            walk(hub, &root, file.resource_key, listing_jobs, &mut items).await?;
        }
        items.roots.push(root);
    }
    Ok(items)
}

async fn get(hub: &Hub, id: &str) -> Result<File, Box<google_drive3::Error>> {
    let (_, file) = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
        hub.files()
            .get(id)
            .param("fields", ROOT_FIELDS)
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(Box::new)?;
    Ok(file)
}

/// The path of a file the walk below `root` found: the root's name, then `components`.
fn path(root: &str, components: &[String]) -> String {
    std::iter::once(root)
        .chain(components.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("/")
}

/// Adds everything below the folder `root` to `items`. Shortcuts are items of their own; their
/// targets are not followed.
async fn walk(
    hub: &Hub,
    root: &Root,
    resource_key: Option<String>,
    listing_jobs: usize,
    items: &mut Items,
) -> Result<(), Error> {
    let walk_root = WalkRoot {
        id: root.id.clone(),
        name: root.name.clone(),
        drive_id: root.drive_id.clone(),
        resource_key,
    };
    let options = WalkOptions {
        listing_jobs,
        ..WalkOptions::default()
    };
    let mut walk = Walk::start(Arc::new(hub.clone()), walk_root, options);
    while let Some(item) = walk.next().await {
        match item {
            WalkItem::Entry(entry) => {
                let Some(id) = entry.file.id.clone() else {
                    continue;
                };
                let folder = entry.kind == EntryKind::Folder;
                items.add(id, path(&root.name, &entry.components), folder);
            }
            WalkItem::FolderDone(_) => {}
            WalkItem::FolderError(error) => {
                return Err(Error::ListFolder {
                    path: path(&root.name, &error.folder.components),
                    source: error.error,
                });
            }
            WalkItem::IncompleteSearch(incomplete) => {
                let components = incomplete
                    .folders
                    .first()
                    .map(|folder| folder.components.clone())
                    .unwrap_or_default();
                return Err(Error::IncompleteListing {
                    path: path(&root.name, &components),
                });
            }
        }
    }
    Ok(())
}
