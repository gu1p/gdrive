//! What the audit log says about the items, counted while it is read ([`Tally`]), and the three
//! reports made of it.
//!
//! An event counts when it is about an item of the report (its `doc_id`), is the primary event
//! of its user action (`primary_event` true or absent: one action can log several events), and
//! its actor is not excluded. It counts once however many requests return it: a record is
//! identified by its time and Google's `uniqueQualifier`, an event by its position in the record,
//! its name and its item.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    io,
};

use chrono::{DateTime, Local, NaiveDate, Utc};

use super::{sharing::Access, Output, Who};
use crate::{
    common::{
        audit_log::{Activity, DOC_ID, PRIMARY_EVENT},
        local_name::printable,
        table::{self, escape_tsv, Table},
    },
    files::info::format_date_time,
};

/// The columns an event counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Category {
    View,
    Download,
    Edit,
    Other,
}

impl Category {
    /// The column of the event `name`; every name that is not one of the known ones is
    /// [`Category::Other`].
    pub(super) fn of(name: &str) -> Category {
        match name {
            "view" | "preview" => Category::View,
            "download" | "print" | "source_copy" | "email_as_attachment" => Category::Download,
            "edit" | "create" | "upload" | "copy" | "rename" | "move" | "trash" | "untrash"
            | "delete" | "add_to_folder" | "remove_from_folder" => Category::Edit,
            _ => Category::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) views: u64,
    pub(super) downloads: u64,
    pub(super) edits: u64,
    pub(super) other: u64,
}

impl Counts {
    fn add(&mut self, category: Category) {
        let count = match category {
            Category::View => &mut self.views,
            Category::Download => &mut self.downloads,
            Category::Edit => &mut self.edits,
            Category::Other => &mut self.other,
        };
        *count += 1;
    }

    fn cells(self) -> [String; 4] {
        [self.views, self.downloads, self.edits, self.other].map(|count| count.to_string())
    }
}

/// What one person did.
#[derive(Debug, Clone)]
pub(super) struct UserActivity {
    pub(super) last: DateTime<Utc>,
    /// Calendar days of the local time zone.
    days: HashSet<NaiveDate>,
    counts: Counts,
    /// The items touched.
    items: HashSet<String>,
}

/// What one person did with one item.
#[derive(Debug, Clone)]
struct ItemActivity {
    last: DateTime<Utc>,
    counts: Counts,
}

/// One counted event, for `--by event`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counted {
    time: DateTime<Utc>,
    who: Who,
    name: String,
    item: String,
}

/// Which events count.
pub(super) struct Selection<'a> {
    /// The items of the report, by id.
    pub(super) items: &'a HashMap<String, String>,
    /// Lower-case email addresses whose events do not count.
    pub(super) exclude: &'a HashSet<String>,
}

/// An event as it is recognized when a second request returns it: the record's time and
/// `uniqueQualifier`, the event's position in the record, its name and its item.
type EventKey = (String, String, usize, String, String);

/// The counted events, kept as the report needs them: per person, per item and person, and
/// every event for `--by event` only. Events about other items are counted as read and dropped.
#[derive(Debug)]
pub(super) struct Tally {
    /// Events the audit log returned.
    pub(super) read: u64,
    /// Events that count, each once.
    pub(super) counted: u64,
    /// Events about the items whose record has no readable time; they are not counted.
    pub(super) without_time: u64,
    seen: HashSet<EventKey>,
    pub(super) users: HashMap<Who, UserActivity>,
    items: HashMap<(String, Who), ItemActivity>,
    events: Option<Vec<Counted>>,
    /// The calendar day of an event, for `Days`: [`local_day`]; tests take another time zone.
    day_of: fn(DateTime<Utc>) -> NaiveDate,
}

/// The day of `time` in the local time zone, the one times are printed in.
fn local_day(time: DateTime<Utc>) -> NaiveDate {
    time.with_timezone(&Local).date_naive()
}

impl Tally {
    /// A tally that keeps every counted event if `keep_events`.
    pub(super) fn new(keep_events: bool) -> Self {
        Tally {
            read: 0,
            counted: 0,
            without_time: 0,
            seen: HashSet::new(),
            users: HashMap::new(),
            items: HashMap::new(),
            events: keep_events.then(Vec::new),
            day_of: local_day,
        }
    }

    /// Counts the events of `record` that `selection` selects. `read` tells whether an event of
    /// this time counts as read by the request that returned it.
    pub(super) fn add(
        &mut self,
        record: &Activity,
        selection: &Selection,
        read: impl Fn(Option<DateTime<Utc>>) -> bool,
    ) {
        let time = DateTime::parse_from_rfc3339(&record.id.time)
            .ok()
            .map(|time| time.with_timezone(&Utc));
        let who = match record.actor.email.as_deref().map(str::trim) {
            Some(email) if !email.is_empty() => Who::Person(email.to_lowercase()),
            _ => Who::Anonymous,
        };
        if read(time) {
            self.read += u64::try_from(record.events.len()).unwrap_or(u64::MAX);
        }
        if who.is_excluded(selection.exclude) {
            return;
        }
        for (position, event) in record.events.iter().enumerate() {
            let Some(item) = event.value(DOC_ID) else {
                continue;
            };
            if !selection.items.contains_key(item) || event.bool_value(PRIMARY_EVENT) == Some(false)
            {
                continue;
            }
            let Some(time) = time else {
                self.without_time += 1;
                continue;
            };
            let key = (
                record.id.time.clone(),
                record.id.unique_qualifier.clone(),
                position,
                event.name.clone(),
                item.to_owned(),
            );
            if !self.seen.insert(key) {
                continue;
            }
            self.count(time, &who, &event.name, item);
        }
    }

    fn count(&mut self, time: DateTime<Utc>, who: &Who, name: &str, item: &str) {
        self.counted += 1;
        let category = Category::of(name);
        let user = self
            .users
            .entry(who.clone())
            .or_insert_with(|| UserActivity {
                last: time,
                days: HashSet::new(),
                counts: Counts::default(),
                items: HashSet::new(),
            });
        user.last = user.last.max(time);
        user.days.insert((self.day_of)(time));
        user.counts.add(category);
        user.items.insert(item.to_owned());
        let per_item = self
            .items
            .entry((item.to_owned(), who.clone()))
            .or_insert(ItemActivity {
                last: time,
                counts: Counts::default(),
            });
        per_item.last = per_item.last.max(time);
        per_item.counts.add(category);
        if let Some(events) = &mut self.events {
            events.push(Counted {
                time,
                who: who.clone(),
                name: name.to_owned(),
                item: item.to_owned(),
            });
        }
    }
}

pub(super) const USER_HEADER: [&str; 9] = [
    "User",
    "Access",
    "Last activity",
    "Days",
    "Views",
    "Downloads",
    "Edits",
    "Other",
    "Files",
];

pub(super) const FILE_HEADER: [&str; 7] = [
    "Path",
    "User",
    "Last activity",
    "Views",
    "Downloads",
    "Edits",
    "Other",
];

pub(super) const EVENT_HEADER: [&str; 4] = ["Time", "User", "Event", "Path"];

/// What a cell shows for a missing value.
const NONE: &str = "-";

fn time(time: DateTime<Utc>) -> String {
    format_date_time(time).to_string()
}

/// One row per person with activity, most recent first, then, with `access` (the sharing lists),
/// one per entry of the sharing lists without activity, by name.
pub(super) fn by_user(tally: &Tally, access: Option<&HashMap<Who, Access>>) -> Vec<[String; 9]> {
    let role = |who: &Who| {
        access
            .and_then(|access| access.get(who))
            .map_or_else(|| NONE.to_owned(), |access| access.role.clone())
    };
    let mut active = tally.users.iter().collect::<Vec<_>>();
    active.sort_by_key(|(who, user)| (Reverse(user.last), who.to_string()));
    let mut rows = active
        .into_iter()
        .map(|(who, user)| {
            let [views, downloads, edits, other] = user.counts.cells();
            [
                who.to_string(),
                role(who),
                time(user.last),
                user.days.len().to_string(),
                views,
                downloads,
                edits,
                other,
                user.items.len().to_string(),
            ]
        })
        .collect::<Vec<_>>();
    let mut idle = idle(tally, access);
    idle.sort_by_key(ToString::to_string);
    rows.extend(idle.into_iter().map(|who| {
        let zero = || String::from("0");
        [
            who.to_string(),
            role(who),
            NONE.to_owned(),
            zero(),
            zero(),
            zero(),
            zero(),
            zero(),
            zero(),
        ]
    }));
    rows
}

/// The entries of the sharing lists without activity.
pub(super) fn idle<'a>(tally: &Tally, access: Option<&'a HashMap<Who, Access>>) -> Vec<&'a Who> {
    access
        .into_iter()
        .flat_map(HashMap::keys)
        .filter(|who| !tally.users.contains_key(*who))
        .collect()
}

/// One row per item and person with activity, by path, then most recent first.
pub(super) fn by_file(tally: &Tally, paths: &HashMap<String, String>) -> Vec<[String; 7]> {
    let path = |item: &String| paths.get(item).cloned().unwrap_or_else(|| item.clone());
    let mut rows = tally
        .items
        .iter()
        .map(|((item, who), activity)| {
            (path(item), activity.last, who.to_string(), activity.counts)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| (&a.0, Reverse(a.1), &a.2).cmp(&(&b.0, Reverse(b.1), &b.2)));
    rows.into_iter()
        .map(|(path, last, who, counts)| {
            let [views, downloads, edits, other] = counts.cells();
            [path, who, time(last), views, downloads, edits, other]
        })
        .collect()
}

/// One row per counted event, oldest first.
pub(super) fn by_event(tally: &Tally, paths: &HashMap<String, String>) -> Vec<[String; 4]> {
    let mut rows = tally
        .events
        .iter()
        .flatten()
        .map(|event| {
            let path = paths
                .get(&event.item)
                .cloned()
                .unwrap_or_else(|| event.item.clone());
            (event.time, event.who.to_string(), event.name.clone(), path)
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows.into_iter()
        .map(|(at, who, name, path)| [time(at), who, name, path])
        .collect()
}

/// The report as text: aligned columns as `files list` prints them, or tab-separated values.
/// Names, paths, addresses and event names come from Drive and the audit log: control
/// characters are escaped (`\n`, `\u{1b}`), and in tab-separated values a tab, newline,
/// carriage return or backslash is `\t`, `\n`, `\r` or `\\` first. `files diverged` prints
/// its reports with it too.
pub(crate) fn render<const N: usize>(
    header: [&str; N],
    rows: Vec<[String; N]>,
    output: &Output,
) -> io::Result<String> {
    match output {
        Output::Table {
            skip_header,
            field_separator,
        } => {
            let values = rows
                .into_iter()
                .map(|row| row.map(|cell| printable(&cell).into_owned()))
                .collect();
            let mut text = Vec::new();
            table::write(
                &mut text,
                Table { header, values },
                &table::DisplayConfig {
                    skip_header: *skip_header,
                    separator: field_separator,
                },
            )?;
            Ok(String::from_utf8_lossy(&text).into_owned())
        }
        Output::Tsv { skip_header } => {
            let mut text = String::new();
            let mut line = |cells: &mut dyn Iterator<Item = String>| {
                let cells = cells
                    .map(|cell| printable(&escape_tsv(&cell)).into_owned())
                    .collect::<Vec<_>>();
                text.push_str(&cells.join("\t"));
                text.push('\n');
            };
            if !skip_header {
                line(&mut header.iter().map(|cell| (*cell).to_owned()));
            }
            for row in rows {
                line(&mut row.into_iter());
            }
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use chrono::{DateTime, NaiveDate, Utc};
    use serde_json::json;

    use super::{by_user, Category, Selection, Tally};
    use crate::{common::audit_log::Activity, files::activity::test_zone::Berlin};

    fn berlin_day(time: DateTime<Utc>) -> NaiveDate {
        time.with_timezone(&Berlin).date_naive()
    }

    #[test]
    fn days_are_calendar_days_of_the_time_zone_not_of_utc() {
        let mut tally = Tally::new(false);
        tally.day_of = berlin_day;
        let items = HashMap::from([(String::from("a"), String::from("a.txt"))]);
        let exclude = HashSet::new();
        let selection = Selection {
            items: &items,
            exclude: &exclude,
        };
        let view = |qualifier: &str, time: &str, actor: &str| -> Activity {
            serde_json::from_value(json!({
                "id": {"time": time, "uniqueQualifier": qualifier},
                "actor": {"email": actor},
                "events": [{"name": "view", "parameters": [{"name": "doc_id", "value": "a"}]}],
            }))
            .unwrap()
        };
        for record in [
            // One UTC day, two days in Berlin: a second before and after its midnight (CET).
            view("1", "2026-11-01T22:59:59Z", "ana@example.com"),
            view("2", "2026-11-01T23:00:01Z", "ana@example.com"),
            // Two UTC days, one day in Berlin: 00:30 and 23:30 on 2026-11-03.
            view("3", "2026-11-02T23:30:00Z", "bob@example.com"),
            view("4", "2026-11-03T22:30:00Z", "bob@example.com"),
        ] {
            tally.add(&record, &selection, |_| true);
        }

        let days = by_user(&tally, None)
            .into_iter()
            .map(|row| (row[0].clone(), row[3].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            days,
            [
                (String::from("bob@example.com"), String::from("1")),
                (String::from("ana@example.com"), String::from("2")),
            ]
        );
        assert_eq!(tally.counted, 4);
    }

    #[test]
    fn event_names_map_to_their_columns() {
        for (names, category) in [
            (&["view", "preview"][..], Category::View),
            (
                &["download", "print", "source_copy", "email_as_attachment"],
                Category::Download,
            ),
            (
                &[
                    "edit",
                    "create",
                    "upload",
                    "copy",
                    "rename",
                    "move",
                    "trash",
                    "untrash",
                    "delete",
                    "add_to_folder",
                    "remove_from_folder",
                ],
                Category::Edit,
            ),
            (
                &[
                    "access_item_content",
                    "sync_item_content",
                    "request_access",
                    "change_user_access",
                    "change_document_visibility",
                    "something_new",
                    "",
                    "VIEW",
                ],
                Category::Other,
            ),
        ] {
            for name in names {
                assert_eq!(Category::of(name), category, "{name}");
            }
        }
    }
}
