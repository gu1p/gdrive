//! One level of folders at a time: the children of the copy's folders, and the children in the
//! trash of the origin's folders. Up to [`MAX_PARENTS_PER_QUERY`] folders of the same corpus go
//! into one query, as the walk batches them ([`crate::transfer::walk`]), and each result is
//! routed to its folder by `parents`. A batch whose results cannot be routed, that fails, or that
//! Drive did not search completely is listed again one folder per query, so that the folder at
//! fault is named; a single folder that fails or is listed incompletely fails the listing, since a
//! comparison over a partial listing would call things added or unchanged wrongly.

use std::collections::{HashMap, HashSet};

use futures::{stream, StreamExt, TryStreamExt};
use google_drive3::api::File;

use super::{Error, Side};
use crate::{
    hub::Hub,
    transfer::walk::{children_query, request_page, PageQuery, MAX_PARENTS_PER_QUERY},
};

/// A folder to list.
#[derive(Debug, Clone, Copy)]
pub(super) struct Folder<'a> {
    pub(super) id: &'a str,
    /// For errors: its path from the name of its `FILE_ID`.
    pub(super) path: &'a str,
    /// The shared drive whose corpus is searched; the user's corpus without one.
    pub(super) drive_id: Option<&'a str>,
    /// Needed to list a folder that is shared by link only.
    pub(super) resource_key: Option<&'a str>,
}

/// Which children are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trash {
    /// Those that are not in the trash.
    Without,
    /// Only those in the trash.
    Only,
}

/// The children of each folder, by folder id.
pub(super) type Listed = HashMap<String, Vec<File>>;

/// A folder that could not be listed completely.
#[derive(Debug)]
pub(super) struct ListError {
    path: String,
    /// `None`: Drive did not search everything (`incompleteSearch`).
    source: Option<Box<google_drive3::Error>>,
}

impl ListError {
    pub(super) fn into_error(self, side: Side) -> Error {
        match self.source {
            Some(source) => Error::ListFolder {
                side,
                path: self.path,
                source,
            },
            None => Error::IncompleteListing {
                side,
                path: self.path,
            },
        }
    }
}

/// Lists the children of `folders` per `trash` with the `fields` mask `fields` (which must name
/// `parents` and `trashed`), up to `jobs` requests at once. A folder given twice is listed once.
pub(super) async fn list_children(
    hub: &Hub,
    folders: &[Folder<'_>],
    trash: Trash,
    fields: &str,
    jobs: usize,
) -> Result<Listed, ListError> {
    let mut seen = HashSet::new();
    let mut batches: Vec<Vec<Folder>> = Vec::new();
    for folder in folders.iter().filter(|folder| seen.insert(folder.id)) {
        match batches.last_mut() {
            Some(batch)
                if batch.len() < MAX_PARENTS_PER_QUERY && batch[0].drive_id == folder.drive_id =>
            {
                batch.push(*folder);
            }
            _ => batches.push(vec![*folder]),
        }
    }
    let lists = stream::iter(batches)
        .map(|batch| list_batch(hub, batch, trash, fields))
        .buffer_unordered(jobs.max(1))
        .try_collect::<Vec<_>>()
        .await?;
    Ok(lists.into_iter().flatten().collect())
}

/// The children of the folders of `batch`, with one query or, when that query cannot tell them
/// apart, one per folder.
async fn list_batch(
    hub: &Hub,
    batch: Vec<Folder<'_>>,
    trash: Trash,
    fields: &str,
) -> Result<Vec<(String, Vec<File>)>, ListError> {
    if let Some(listed) = list_query(hub, &batch, trash, fields).await? {
        return Ok(listed);
    }
    let mut listed = Vec::new();
    for folder in batch {
        // A single folder is never split.
        listed.extend(
            list_query(hub, &[folder], trash, fields)
                .await?
                .unwrap_or_default(),
        );
    }
    Ok(listed)
}

/// Every page of one query for the children of `folders`; `None` when a query of several folders
/// must be split: it failed, Drive did not search everything, or a result names no parent.
async fn list_query(
    hub: &Hub,
    folders: &[Folder<'_>],
    trash: Trash,
    fields: &str,
) -> Result<Option<Vec<(String, Vec<File>)>>, ListError> {
    let several = folders.len() > 1;
    let q = children_query(folders.iter().map(|folder| folder.id), trash == Trash::Only);
    let keys = folders
        .iter()
        .filter_map(|folder| Some(format!("{}/{}", folder.id, folder.resource_key?)))
        .collect::<Vec<_>>()
        .join(",");
    let query = PageQuery {
        q: &q,
        fields,
        drive_id: folders.first().and_then(|folder| folder.drive_id),
        resource_keys: (!keys.is_empty()).then_some(keys.as_str()),
    };
    let first_path = || {
        folders
            .first()
            .map(|folder| folder.path.to_owned())
            .unwrap_or_default()
    };

    let mut children = vec![Vec::new(); folders.len()];
    let mut seen = vec![HashSet::new(); folders.len()];
    let mut page_token = None;
    loop {
        let list = match request_page(hub, &query, page_token.as_deref()).await {
            Ok(list) => list,
            Err(_) if several => return Ok(None),
            Err(source) => {
                return Err(ListError {
                    path: first_path(),
                    source: Some(source),
                })
            }
        };
        if list.incomplete_search == Some(true) {
            if several {
                return Ok(None);
            }
            return Err(ListError {
                path: first_path(),
                source: None,
            });
        }
        for file in list.files.unwrap_or_default() {
            // The query selects by `trashed`: only a result that contradicts it is dropped, never
            // one whose field is missing.
            let contradicts = match trash {
                Trash::Without => file.trashed == Some(true),
                Trash::Only => file.trashed == Some(false),
            };
            if contradicts {
                continue;
            }
            let targets = if several {
                let parents = file.parents.as_deref().unwrap_or_default();
                let targets = folders
                    .iter()
                    .enumerate()
                    .filter(|(_, folder)| parents.iter().any(|parent| parent == folder.id))
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                if targets.is_empty() {
                    return Ok(None);
                }
                targets
            } else {
                vec![0]
            };
            for index in targets {
                let new = file
                    .id
                    .as_ref()
                    .is_none_or(|id| seen[index].insert(id.clone()));
                if new {
                    children[index].push(file.clone());
                }
            }
        }
        page_token = list.next_page_token;
        if page_token.is_none() {
            break;
        }
    }
    Ok(Some(
        folders
            .iter()
            .map(|folder| folder.id.to_owned())
            .zip(children)
            .collect(),
    ))
}
