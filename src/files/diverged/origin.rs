//! The origin of a migration: each `FILE_ID` and, for a folder, everything in it that is not in
//! the trash, listed by the walk ([`crate::transfer::walk`]) with who last modified and who owns
//! each item; then, for every listed folder, its children in the trash ([`trash`]). Paths are the
//! Drive names from the given item down, joined with `/`, as Drive has them (escaped only when
//! printed). A listing that fails or is incomplete fails the command, as does a `FILE_ID` that
//! does not exist or is in the trash.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use google_drive3::api::{File, User};

use super::{
    listing::{self, Listed},
    Error, Side, Who,
};
use crate::{
    common::{drive_file, retry::retry_read},
    hub::Hub,
    transfer::{
        relay::{self, Unsupported},
        walk::{Children, EntryKind, FolderKey, Walk, WalkItem, WalkOptions, WalkRoot},
    },
};

/// What the report needs of a `FILE_ID`.
const ROOT_FIELDS: &str = "id,name,mimeType,size,md5Checksum,modifiedTime,createdTime,driveId,\
    resourceKey,trashed,explicitlyTrashed,lastModifyingUser(displayName,emailAddress),\
    owners(displayName,emailAddress)";

/// The walk's fields ([`crate::transfer::walk::FIELDS`]) and who last modified and who owns
/// each file.
pub(super) const WALK_FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,size,\
    md5Checksum,modifiedTime,createdTime,parents,driveId,trashed,resourceKey,\
    shortcutDetails(targetId,targetMimeType,targetResourceKey),\
    lastModifyingUser(displayName,emailAddress),owners(displayName,emailAddress))";

/// What a listing of the trash returns of each child: who moved it there and when, which
/// Drive tells only in shared drives, and its owner, who alone can move it there elsewhere.
pub(super) const TRASH_FIELDS: &str = "nextPageToken,incompleteSearch,files(id,name,mimeType,\
    parents,trashed,explicitlyTrashed,trashedTime,trashingUser(displayName,emailAddress),\
    owners(displayName,emailAddress))";

/// The prefix of the Google Workspace MIME types.
const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// What the report knows about an item of the origin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) mime_type: String,
    pub(super) size: Option<i64>,
    pub(super) md5: Option<String>,
    pub(super) modified: Option<DateTime<Utc>>,
    pub(super) created: Option<DateTime<Utc>>,
    /// `lastModifyingUser`.
    pub(super) modified_by: Option<Who>,
    /// The first of `owners`.
    pub(super) owner: Option<Who>,
}

impl Item {
    pub(super) fn of(file: &File) -> Self {
        Item {
            id: file.id.clone().unwrap_or_default(),
            name: file.name.clone().unwrap_or_default(),
            mime_type: file.mime_type.clone().unwrap_or_default(),
            size: file.size,
            md5: file.md5_checksum.clone(),
            modified: file.modified_time,
            created: file.created_time,
            modified_by: Who::of(file.last_modifying_user.as_ref()),
            owner: first_owner(file),
        }
    }

    pub(super) fn is_folder(&self) -> bool {
        self.mime_type == drive_file::MIME_TYPE_DRIVE_FOLDER
    }

    /// A Google Docs, Sheets, ... file: no md5, and a migration converts its export back.
    pub(super) fn is_native(&self) -> bool {
        self.mime_type.starts_with(GOOGLE_APPS_PREFIX)
            && self.mime_type != drive_file::MIME_TYPE_DRIVE_SHORTCUT
            && !self.is_folder()
    }

    /// Why a migration does not copy this file (a shortcut, a Drawing, a Form, ...); `None` for a
    /// folder and for what it copies.
    pub(super) fn unsupported(&self) -> Option<Unsupported> {
        if self.is_folder() {
            return None;
        }
        let file = File {
            mime_type: Some(self.mime_type.clone()),
            ..File::default()
        };
        relay::check_supported(&file).err()
    }

    /// Who changed it last: `lastModifyingUser`, else its owner.
    pub(super) fn editor(&self) -> Who {
        self.modified_by
            .clone()
            .or_else(|| self.owner.clone())
            .unwrap_or(Who::Unknown)
    }

    /// Who put it there: its owner, who created or uploaded it, else `lastModifyingUser`.
    pub(super) fn creator(&self) -> Who {
        self.owner
            .clone()
            .or_else(|| self.modified_by.clone())
            .unwrap_or(Who::Unknown)
    }
}

/// The first of the file's `owners` that Drive names.
fn first_owner(file: &File) -> Option<Who> {
    file.owners
        .as_ref()?
        .first()
        .and_then(|user| Who::of(Some(user)))
}

impl Who {
    /// `user`'s email address in lower case, else its display name; `None` without either.
    pub(super) fn of(user: Option<&User>) -> Option<Who> {
        let user = user?;
        let text = |value: Option<&String>| {
            value
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        text(user.email_address.as_ref())
            .map(|email| Who::Email(email.to_lowercase()))
            .or_else(|| text(user.display_name.as_ref()).map(Who::Name))
    }
}

/// An item of the origin and, for a folder, what the walk found in it.
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub(super) item: Item,
    /// The names from the `FILE_ID` down, joined with `/`.
    pub(super) path: String,
    pub(super) children: Vec<Node>,
    /// For a folder: whether the walk listed it. A folder that is its own ancestor is not.
    pub(super) listed: bool,
    /// The shared drive of its `FILE_ID`, whose corpus its listings search.
    pub(super) drive_id: Option<String>,
    pub(super) resource_key: Option<String>,
}

impl Node {
    /// This node and every node below it, depth first.
    pub(super) fn all(&self) -> Vec<&Node> {
        let mut nodes = Vec::new();
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            nodes.push(node);
            pending.extend(node.children.iter().rev());
        }
        nodes
    }
}

/// The path of `name` in the folder at `parent`.
pub(super) fn child_path(parent: &str, name: &str) -> String {
    format!("{parent}/{name}")
}

/// Every item of the origin.
#[derive(Debug, Default)]
pub(super) struct Origin {
    /// The `FILE_ID`s, each once, in the order given.
    pub(super) roots: Vec<Node>,
    /// The ids of every item.
    pub(super) ids: HashSet<String>,
    pub(super) files: u64,
    pub(super) folders: u64,
}

impl Origin {
    /// Every listed folder, the given ones included, each id once.
    pub(super) fn folders(&self) -> Vec<&Node> {
        let mut seen = HashSet::new();
        self.roots
            .iter()
            .flat_map(Node::all)
            .filter(|node| node.item.is_folder() && node.listed && seen.insert(&node.item.id))
            .collect()
    }
}

/// Gets every `FILE_ID` of `ids` and walks the folders among them, `listing_jobs` requests at
/// once.
pub(super) async fn resolve(
    hub: &Hub,
    ids: &[String],
    listing_jobs: usize,
) -> Result<Origin, Error> {
    let mut origin = Origin::default();
    for id in ids {
        let file = get(hub, id).await.map_err(|source| Error::GetItem {
            id: id.clone(),
            source,
        })?;
        let item = Item::of(&file);
        if file.trashed == Some(true) {
            return Err(Error::Trashed {
                id: id.clone(),
                name: item.name,
                // Drive reports an item in a trashed folder as trashed, not explicitly.
                by_folder: file.explicitly_trashed == Some(false),
            });
        }
        let id = if item.id.is_empty() {
            id.clone()
        } else {
            item.id.clone()
        };
        if origin.roots.iter().any(|root| root.item.id == id) {
            continue;
        }
        let root = Node {
            path: item.name.clone(),
            listed: item.is_folder(),
            item: Item { id, ..item },
            children: Vec::new(),
            drive_id: file.drive_id.clone(),
            resource_key: file.resource_key.clone(),
        };
        let root = if root.item.is_folder() {
            walk(hub, root, listing_jobs).await?
        } else {
            root
        };
        for node in root.all() {
            origin.ids.insert(node.item.id.clone());
            if node.item.is_folder() {
                origin.folders += 1;
            } else {
                origin.files += 1;
            }
        }
        origin.roots.push(root);
    }
    Ok(origin)
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

/// A child found by the walk; a folder's children are placed after the walk.
#[derive(Debug)]
struct Found {
    item: Item,
    resource_key: Option<String>,
    /// For a folder: the key of its listing, `None` when it is not listed (a cycle).
    key: Option<FolderKey>,
}

/// The folder `root` with everything below it. Shortcuts are items of their own; their targets
/// are not followed.
async fn walk(hub: &Hub, root: Node, listing_jobs: usize) -> Result<Node, Error> {
    let walk_root = WalkRoot {
        id: root.item.id.clone(),
        name: root.item.name.clone(),
        drive_id: root.drive_id.clone(),
        resource_key: root.resource_key.clone(),
    };
    let options = WalkOptions {
        listing_jobs,
        ..WalkOptions::default()
    };
    let mut walk = Walk::start_with_fields(Arc::new(hub.clone()), walk_root, options, WALK_FIELDS);
    let path_of = |components: &[String]| {
        components
            .iter()
            .fold(root.path.clone(), |parent, name| child_path(&parent, name))
    };
    let mut found: HashMap<FolderKey, Vec<Found>> = HashMap::new();
    while let Some(item) = walk.next().await {
        match item {
            WalkItem::Entry(entry) => {
                let key = match (entry.kind, entry.children) {
                    (EntryKind::Folder, Children::Listed(key)) => Some(key),
                    _ => None,
                };
                found.entry(entry.parent).or_default().push(Found {
                    item: Item::of(&entry.file),
                    resource_key: entry.file.resource_key.clone(),
                    key,
                });
            }
            WalkItem::FolderDone(_) => {}
            WalkItem::FolderError(error) => {
                return Err(Error::ListFolder {
                    side: Side::Origin,
                    path: path_of(&error.folder.components),
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
                    side: Side::Origin,
                    path: path_of(&components),
                });
            }
        }
    }
    Ok(assemble(root, FolderKey::ROOT, &mut found))
}

/// `folder` with the children the walk found under `key`, and theirs.
fn assemble(mut folder: Node, key: FolderKey, found: &mut HashMap<FolderKey, Vec<Found>>) -> Node {
    let children = found.remove(&key).unwrap_or_default();
    folder.children = children
        .into_iter()
        .map(|child| {
            let node = Node {
                path: child_path(&folder.path, &child.item.name),
                listed: child.key.is_some(),
                item: child.item,
                children: Vec::new(),
                drive_id: folder.drive_id.clone(),
                resource_key: child.resource_key,
            };
            match child.key {
                Some(key) => assemble(node, key, found),
                None => node,
            }
        })
        .collect();
    folder
}

/// An item that someone moved to the trash from a folder of the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Trashed {
    pub(super) id: String,
    pub(super) path: String,
    /// `trashedTime`: Drive tells it only in shared drives.
    pub(super) when: Option<DateTime<Utc>>,
    /// `trashingUser`, which Drive tells only in shared drives; elsewhere the owner, the only one
    /// who can move a file of My Drive to the trash.
    pub(super) who: Who,
}

/// The children of every listed folder of `origin` that were moved to the trash themselves
/// (`explicitlyTrashed`; not what is in a trashed folder), `listing_jobs` requests at once.
pub(super) async fn trash(
    hub: &Hub,
    origin: &Origin,
    listing_jobs: usize,
) -> Result<Vec<Trashed>, Error> {
    let folders = origin.folders();
    let listed = listing::list_children(
        hub,
        &folders
            .iter()
            .map(|node| listing::Folder {
                id: &node.item.id,
                path: &node.path,
                drive_id: node.drive_id.as_deref(),
                resource_key: node.resource_key.as_deref(),
            })
            .collect::<Vec<_>>(),
        listing::Trash::Only,
        TRASH_FIELDS,
        listing_jobs,
    )
    .await
    .map_err(|error| error.into_error(Side::Origin))?;
    Ok(trashed(&folders, &listed))
}

/// The trashed children of `folders`, from their listings `listed`, each item once.
fn trashed(folders: &[&Node], listed: &Listed) -> Vec<Trashed> {
    let mut seen = HashSet::new();
    let mut trashed = Vec::new();
    for folder in folders {
        for file in listed.get(&folder.item.id).into_iter().flatten() {
            let (Some(id), Some(name)) = (&file.id, &file.name) else {
                continue;
            };
            if file.explicitly_trashed != Some(true) || !seen.insert(id.clone()) {
                continue;
            }
            let who = Who::of(file.trashing_user.as_ref())
                .or_else(|| first_owner(file))
                .unwrap_or(Who::Unknown);
            trashed.push(Trashed {
                id: id.clone(),
                path: child_path(&folder.path, name),
                when: file.trashed_time,
                who,
            });
        }
    }
    trashed
}
