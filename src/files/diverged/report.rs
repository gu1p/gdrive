//! The two reports of the changes: one row per item (`--by item`), or one row per person
//! (`--by user`). They are printed as `files activity` prints its reports
//! ([`crate::files::activity::report::render`]): aligned columns or tab-separated values, with
//! names, paths and addresses escaped.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use chrono::{DateTime, Utc};

use super::{Change, Kind, Who};
use crate::files::info::format_date_time;

pub(super) const ITEM_HEADER: [&str; 4] = ["Change", "When", "Who", "Path"];

pub(super) const USER_HEADER: [&str; 7] = [
    "User",
    "Last change",
    "Added",
    "Edited",
    "Trashed",
    "Other",
    "Items",
];

/// What a cell shows for a missing value.
const NONE: &str = "-";

fn time(time: Option<DateTime<Utc>>) -> String {
    time.map_or_else(
        || NONE.to_owned(),
        |time| format_date_time(time).to_string(),
    )
}

/// Newest first; without a time last.
fn newest_first(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// One row per change, newest first, those without a time last, then by path.
pub(super) fn by_item(changes: &[Change]) -> Vec<[String; 4]> {
    let mut changes = changes.iter().collect::<Vec<_>>();
    changes.sort_by(|a, b| {
        newest_first(a.when, b.when)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.kind.name().cmp(b.kind.name()))
    });
    changes
        .into_iter()
        .map(|change| {
            let path = match &change.kind {
                Kind::Moved { was } => format!("{} (was {was})", change.path),
                _ => change.path.clone(),
            };
            [
                change.kind.name().to_owned(),
                time(change.when),
                change.who.to_string(),
                path,
            ]
        })
        .collect()
}

/// What one person did.
#[derive(Debug, Default)]
struct PerUser<'a> {
    last: Option<DateTime<Utc>>,
    added: u64,
    edited: u64,
    trashed: u64,
    other: u64,
    items: HashSet<&'a str>,
}

/// One row per person, the most recent change first, those without a time after them and the
/// changes of nobody Drive names (`-`) last.
pub(super) fn by_user(changes: &[Change]) -> Vec<[String; 7]> {
    let mut users: HashMap<&Who, PerUser> = HashMap::new();
    for change in changes {
        let user = users.entry(&change.who).or_default();
        user.last = match (user.last, change.when) {
            (Some(last), Some(when)) => Some(last.max(when)),
            (last, when) => last.or(when),
        };
        let count = match change.kind {
            Kind::Added => &mut user.added,
            Kind::Edited => &mut user.edited,
            Kind::Trashed => &mut user.trashed,
            Kind::Differs | Kind::Moved { .. } => &mut user.other,
        };
        *count += 1;
        user.items.insert(&change.id);
    }
    let mut users = users.into_iter().collect::<Vec<_>>();
    users.sort_by(|(a, one), (b, other)| {
        (**a == Who::Unknown)
            .cmp(&(**b == Who::Unknown))
            .then_with(|| newest_first(one.last, other.last))
            .then_with(|| a.to_string().cmp(&b.to_string()))
    });
    users
        .into_iter()
        .map(|(who, user)| {
            [
                who.to_string(),
                time(user.last),
                user.added.to_string(),
                user.edited.to_string(),
                user.trashed.to_string(),
                user.other.to_string(),
                user.items.len().to_string(),
            ]
        })
        .collect()
}

/// The changes per kind, for the summary, in the order of the report's columns.
pub(super) fn counts(changes: &[Change]) -> Vec<(&'static str, u64)> {
    let mut counts = [
        ("added", 0),
        ("edited", 0),
        ("trashed", 0),
        ("differs", 0),
        ("moved", 0),
    ];
    for change in changes {
        if let Some((_, count)) = counts
            .iter_mut()
            .find(|(name, _)| *name == change.kind.name())
        {
            *count += 1;
        }
    }
    counts.into_iter().filter(|(_, count)| *count > 0).collect()
}

/// The people Drive names among the changes.
pub(super) fn people(changes: &[Change]) -> u64 {
    let people = changes
        .iter()
        .filter(|change| change.who != Who::Unknown)
        .map(|change| &change.who)
        .collect::<HashSet<_>>();
    u64::try_from(people.len()).unwrap_or(u64::MAX)
}

/// How many changes name nobody.
pub(super) fn unknown(changes: &[Change]) -> u64 {
    let unknown = changes
        .iter()
        .filter(|change| change.who == Who::Unknown)
        .count();
    u64::try_from(unknown).unwrap_or(u64::MAX)
}

/// Keeps one change per item: the one with the shortest path, then the first by path. An item
/// given as a `FILE_ID` and found in a given folder too is compared twice.
pub(super) fn one_per_item(changes: &mut Vec<Change>) {
    changes.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| {
                a.path
                    .matches('/')
                    .count()
                    .cmp(&b.path.matches('/').count())
            })
            .then_with(|| a.path.cmp(&b.path))
    });
    changes.dedup_by(|later, first| later.id == first.id);
}
