//! Who has access to the given items: the sharing lists (`permissions.list`) of the `FILE_ID`s,
//! not of the items below them. Each person, group, domain or link sharing keeps its strongest
//! role.

use std::collections::HashMap;

use google_drive3::api::PermissionList;

use super::{tree::Root, Error, Who};
use crate::{
    common::{permission::Role, retry::retry_read},
    hub::Hub,
};

/// The largest page of `permissions.list`.
const PAGE_SIZE: i32 = 100;

/// What a page of the sharing list returns of each permission.
const FIELDS: &str = "nextPageToken,permissions(type,role,emailAddress,domain)";

/// A role on a sharing list, as Drive names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Access {
    pub(super) role: String,
    /// 0 for the strongest role (owner); unknown roles are the weakest.
    rank: usize,
}

impl Access {
    fn of(role: &str) -> Self {
        let rank = match role.parse::<Role>() {
            Ok(Role::Owner) => 0,
            Ok(Role::Organizer) => 1,
            Ok(Role::FileOrganizer) => 2,
            Ok(Role::Writer) => 3,
            Ok(Role::Commenter) => 4,
            Ok(Role::Reader) => 5,
            Err(_) => 6,
        };
        Access {
            role: role.to_owned(),
            rank,
        }
    }
}

/// Everyone on the sharing lists of `roots`, with the strongest role among them.
pub(super) async fn read(hub: &Hub, roots: &[Root]) -> Result<HashMap<Who, Access>, Error> {
    let mut access = HashMap::<Who, Access>::new();
    for root in roots {
        let mut page_token = None::<String>;
        loop {
            let list = page(hub, &root.id, page_token.as_deref())
                .await
                .map_err(|source| Error::Sharing {
                    path: root.name.clone(),
                    source,
                })?;
            for permission in list.permissions.into_iter().flatten() {
                let lower = |value: Option<String>| value.map(|value| value.to_lowercase());
                let who = match permission.type_.as_deref() {
                    Some("user") => lower(permission.email_address).map(Who::Person),
                    Some("group") => lower(permission.email_address).map(Who::Group),
                    Some("domain") => lower(permission.domain).map(Who::Domain),
                    Some("anyone") => Some(Who::Anyone),
                    _ => None,
                };
                let (Some(who), Some(role)) = (who, permission.role) else {
                    continue;
                };
                let role = Access::of(&role);
                match access.get_mut(&who) {
                    Some(known) if known.rank <= role.rank => {}
                    Some(known) => *known = role,
                    None => {
                        access.insert(who, role);
                    }
                }
            }
            page_token = list.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
    }
    Ok(access)
}

async fn page(
    hub: &Hub,
    file_id: &str,
    page_token: Option<&str>,
) -> Result<PermissionList, Box<google_drive3::Error>> {
    let (_, list) = retry_read(
        hub.retrier(),
        "permissions.list",
        |mut delegate| async move {
            let mut call = hub
                .permissions()
                .list(file_id)
                .page_size(PAGE_SIZE)
                .supports_all_drives(true)
                .param("fields", FIELDS)
                .add_scope(google_drive3::api::Scope::Full);
            if let Some(token) = page_token {
                call = call.page_token(token);
            }
            call.delegate(&mut delegate).doit().await
        },
    )
    .await
    .map_err(Box::new)?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::Access;

    #[test]
    fn roles_rank_from_owner_to_reader_and_unknown_ones_last() {
        let ranks = [
            "owner",
            "organizer",
            "fileOrganizer",
            "writer",
            "commenter",
            "reader",
            "somethingNew",
        ]
        .map(|role| Access::of(role).rank);
        assert!(ranks.windows(2).all(|pair| pair[0] < pair[1]), "{ranks:?}");
        assert_eq!(Access::of("somethingNew").role, "somethingNew");
    }
}
