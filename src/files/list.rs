use std::{
    borrow::Cow,
    cmp::min,
    error,
    fmt::{self, Display, Formatter},
    io,
    str::FromStr,
};

use crate::{
    common::{
        drive_file,
        hub_helper::{get_hub, GetHubError},
        table::{self, Table},
    },
    files::{self, info::DisplayConfig},
    hub::Hub,
};

const MAX_PAGE_SIZE: usize = 1000;

pub(crate) struct Config {
    pub(crate) query: ListQuery,
    pub(crate) order_by: ListSortOrder,
    pub(crate) max_files: usize,
    pub(crate) skip_header: bool,
    pub(crate) truncate_name: bool,
    pub(crate) field_separator: String,
}

pub(crate) async fn list(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    let files = list_files(
        &hub,
        ListFilesConfig {
            query: &config.query,
            order_by: &config.order_by,
            max_files: config.max_files,
        },
    )
    .await?;

    let mut values: Vec<[String; 5]> = vec![];

    for file in files {
        let file_type = simplified_file_type(&file);
        let file_name = format_file_name(&config, &file).into_owned();

        values.push([
            file.id.unwrap_or_default(),
            file_name,
            file_type.to_owned(),
            file.size
                .map(|bytes| {
                    files::info::DisplayBytes {
                        bytes: bytes.try_into().unwrap_or(0),
                        config: &DisplayConfig::default(),
                    }
                    .to_string()
                })
                .unwrap_or_default(),
            file.created_time
                .map(|created_time| files::info::format_date_time(created_time).to_string())
                .unwrap_or_default(),
        ]);
    }

    let table = Table {
        header: ["Id", "Name", "Type", "Size", "Created"],
        values,
    };

    let _ = table::write(
        io::stdout(),
        table,
        &table::DisplayConfig {
            skip_header: config.skip_header,
            separator: &config.field_separator,
        },
    );

    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListFilesConfig<'a> {
    pub(crate) query: &'a ListQuery,
    pub(crate) order_by: &'a ListSortOrder,
    pub(crate) max_files: usize,
}

pub(crate) async fn list_files(
    hub: &Hub,
    config: ListFilesConfig<'_>,
) -> Result<Vec<google_drive3::api::File>, Error> {
    let mut collected_files: Vec<google_drive3::api::File> = vec![];
    let mut next_page_token: Option<String> = None;
    let q = config.query.q();

    loop {
        let max_files = config.max_files - collected_files.len();
        let page_size = min(MAX_PAGE_SIZE, max_files);

        let mut req = hub.files().list();

        if let Some(token) = next_page_token {
            req = req.page_token(&token);
        }

        if !q.is_empty() {
            req = req.q(&q);
        }

        if let Some(drive_id) = config.query.drive_id() {
            req = req.corpora("drive").drive_id(drive_id);
        }

        let (_, file_list) = req
            .page_size(page_size.try_into().unwrap_or(i32::MAX))
            .order_by(&config.order_by.to_string())
            .add_scope(google_drive3::api::Scope::Full)
            .supports_all_drives(true)
            .include_items_from_all_drives(true)
            .param(
                "fields",
                "files(id,name,md5Checksum,mimeType,size,createdTime,parents),nextPageToken",
            )
            .doit()
            .await
            .map_err(|err| Error::ListFiles(Box::new(err)))?;

        if let Some(mut files) = file_list.files {
            collected_files.append(&mut files);
        }

        next_page_token = file_list.next_page_token;

        if collected_files.len() >= config.max_files || next_page_token.is_none() {
            break;
        }
    }

    collected_files.truncate(config.max_files);
    Ok(collected_files)
}

/// What `files list` searches: the `q` expression and, for `--drive`, the shared drive to search in.
///
/// Composition rules:
/// - no `--query`: the non-trashed files directly in `--parent`, else at the top level of `--drive`,
///   else in My Drive's root (the 3.9 defaults);
/// - `--query` alone: the query as given;
/// - `--query` with `--parent`: `'<parent>' in parents and (<query>)`;
/// - `--drive` searches the shared drive (`corpora=drive`, `driveId`) with the query above, so
///   `--drive` with `--query` searches the whole drive rather than only its top level.
///
/// An empty `--query` adds no condition. The user query is passed through unchanged; only the ids
/// are quoted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ListQuery {
    /// `--query`; `None` selects the default listed above.
    pub(crate) query: Option<String>,
    /// `--parent`: only list direct children of this folder.
    pub(crate) parent: Option<String>,
    /// `--drive`: search this shared drive.
    pub(crate) drive: Option<String>,
}

impl ListQuery {
    /// The non-trashed files directly in a folder.
    #[must_use]
    pub(crate) fn files_in_folder(folder_id: String) -> Self {
        Self {
            parent: Some(folder_id),
            ..Self::default()
        }
    }

    /// The `q` parameter of `files.list`. Empty means no filter.
    #[must_use]
    pub(crate) fn q(&self) -> String {
        match (&self.query, &self.parent, &self.drive) {
            (None, None, None) => String::from("'root' in parents and trashed = false"),
            (None, Some(folder_id), _) | (None, None, Some(folder_id)) => {
                format!("{} in parents and trashed = false", quote(folder_id))
            }
            (Some(query), Some(parent), _) if query.trim().is_empty() => {
                format!("{} in parents", quote(parent))
            }
            (Some(query), Some(parent), _) => format!("{} in parents and ({query})", quote(parent)),
            (Some(query), None, _) => query.clone(),
        }
    }

    /// The shared drive to search (`corpora=drive` and `driveId`), if any.
    #[must_use]
    pub(crate) fn drive_id(&self) -> Option<&str> {
        self.drive.as_deref()
    }
}

/// Quotes a value as a string literal of the Drive query language: `'` and `\` are escaped
/// with a backslash.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if matches!(c, '\'' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

#[derive(Debug, Clone, Default)]
pub(crate) enum ListSortOrder {
    #[default]
    FolderModifiedName,
    Custom(String),
}

impl FromStr for ListSortOrder {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            Err("Sort by can't be an empty string")
        } else {
            Ok(ListSortOrder::Custom(s.to_string()))
        }
    }
}

impl fmt::Display for ListSortOrder {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            ListSortOrder::FolderModifiedName => {
                write!(f, "folder,modifiedTime desc,name")
            }

            ListSortOrder::Custom(query) => {
                write!(f, "{query}")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    ListFiles(Box<google_drive3::Error>),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::ListFiles(_) => f.write_str("unable to list files"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::ListFiles(source) => Some(source),
        }
    }
}

fn simplified_file_type(file: &google_drive3::api::File) -> &'static str {
    if drive_file::is_directory(file) {
        "folder"
    } else if drive_file::is_binary(file) {
        "regular"
    } else if drive_file::is_shortcut(file) {
        "shortcut"
    } else {
        "document"
    }
}

fn format_file_name<'a>(config: &Config, file: &'a google_drive3::api::File) -> Cow<'a, str> {
    let file_name = file.name.as_ref();

    if config.truncate_name {
        file_name.map_or(Cow::Borrowed(""), |s| truncate_middle(s, 41))
    } else {
        Cow::Borrowed(file_name.map(String::as_str).unwrap_or_default())
    }
}

// Truncates string to given max length, and inserts ellipsis into
// the middle of the string to signify that the string has been truncated
fn truncate_middle(s: &str, max_length: usize) -> Cow<'_, str> {
    let tail_count = max_length / 2;
    let head_count = max_length - tail_count - 1;

    let mut chars_iter = s.char_indices();
    // Advance the iterator by head_count.
    // TODO: replace with `advance_by` when stabilized
    for _ in 0..head_count {
        if chars_iter.next().is_none() {
            break;
        }
    }
    let mut tail_end_iter = chars_iter.clone().skip(tail_count);
    if tail_end_iter.next().is_none() {
        return Cow::Borrowed(s);
    }
    let head_end_index = chars_iter.next().unwrap().0;

    let mut tail_windows = 1;
    let mut tail;
    loop {
        tail = chars_iter.as_str();
        chars_iter.next().unwrap();
        tail_windows += 1;
        if tail_end_iter.next().is_none() {
            break;
        }
    }
    if tail_windows <= 2 {
        return Cow::Borrowed(s);
    }

    let truncated_len = head_end_index + '…'.len_utf8() + tail.len();
    let mut truncated = String::with_capacity(truncated_len);
    truncated.push_str(&s[..head_end_index]);
    truncated.push('…');
    truncated.push_str(tail);
    debug_assert_eq!(truncated.len(), truncated_len);
    Cow::Owned(truncated)
}

#[cfg(test)]
mod tests {
    use super::{quote, truncate_middle, ListQuery};

    fn list_query(query: Option<&str>, parent: Option<&str>, drive: Option<&str>) -> ListQuery {
        ListQuery {
            query: query.map(String::from),
            parent: parent.map(String::from),
            drive: drive.map(String::from),
        }
    }

    #[test]
    fn query_defaults_without_user_query() {
        let cases = [
            (None, None, "'root' in parents and trashed = false", None),
            (Some("P"), None, "'P' in parents and trashed = false", None),
            (
                None,
                Some("D"),
                "'D' in parents and trashed = false",
                Some("D"),
            ),
            (
                Some("P"),
                Some("D"),
                "'P' in parents and trashed = false",
                Some("D"),
            ),
        ];

        for (parent, drive, expected_q, expected_drive) in cases {
            let query = list_query(None, parent, drive);
            assert_eq!(query.q(), expected_q, "parent {parent:?}, drive {drive:?}");
            assert_eq!(query.drive_id(), expected_drive);
        }
    }

    #[test]
    fn query_composes_with_parent_and_drive() {
        let user = "name contains 'report'";
        let cases = [
            (None, None, "name contains 'report'", None),
            (
                Some("P"),
                None,
                "'P' in parents and (name contains 'report')",
                None,
            ),
            (None, Some("D"), "name contains 'report'", Some("D")),
            (
                Some("P"),
                Some("D"),
                "'P' in parents and (name contains 'report')",
                Some("D"),
            ),
        ];

        for (parent, drive, expected_q, expected_drive) in cases {
            let query = list_query(Some(user), parent, drive);
            assert_eq!(query.q(), expected_q, "parent {parent:?}, drive {drive:?}");
            assert_eq!(query.drive_id(), expected_drive);
        }
    }

    #[test]
    fn query_with_drive_does_not_add_root_default() {
        let query = list_query(Some("trashed = false"), None, Some("D"));
        assert_eq!(query.q(), "trashed = false");
        assert!(!query.q().contains("'root'"));
    }

    #[test]
    fn query_parenthesizes_user_query() {
        let query = list_query(Some("name = 'a' or name = 'b'"), Some("P"), None);
        assert_eq!(query.q(), "'P' in parents and (name = 'a' or name = 'b')");
    }

    #[test]
    fn query_keeps_quotes_in_user_query() {
        let user = r"name = 'O\'Brien' and fullText contains '\'quoted\''";
        assert_eq!(list_query(Some(user), None, None).q(), user);
        assert_eq!(
            list_query(Some(user), Some("P"), Some("D")).q(),
            format!("'P' in parents and ({user})")
        );
    }

    #[test]
    fn empty_query_adds_no_condition() {
        for user in ["", "  "] {
            assert_eq!(list_query(Some(user), None, None).q().trim(), "");
            assert_eq!(
                list_query(Some(user), Some("P"), None).q(),
                "'P' in parents"
            );
            let on_drive = list_query(Some(user), None, Some("D"));
            assert_eq!(on_drive.q().trim(), "");
            assert_eq!(on_drive.drive_id(), Some("D"));
        }
    }

    #[test]
    fn ids_are_quoted_and_escaped() {
        assert_eq!(quote("abc-_123"), "'abc-_123'");
        assert_eq!(quote(r"it's"), r"'it\'s'");
        assert_eq!(quote(r"a\b"), r"'a\\b'");
        assert_eq!(
            list_query(Some("starred"), Some("x'y"), None).q(),
            r"'x\'y' in parents and (starred)"
        );
    }

    #[test]
    fn files_in_folder_lists_non_trashed_children() {
        let query = ListQuery::files_in_folder(String::from("F"));
        assert_eq!(query.q(), "'F' in parents and trashed = false");
        assert_eq!(query.drive_id(), None);
    }

    #[test]
    fn truncate_middle_ascii() {
        assert_eq!(truncate_middle("hell", 5), "hell");
        assert_eq!(truncate_middle("hello", 5), "hello");
        assert_eq!(truncate_middle("hellow", 5), "he…ow");
        assert_eq!(truncate_middle("hello world", 5), "he…ld");
        assert_eq!(truncate_middle("hello world", 6), "he…rld");
    }

    #[test]
    fn truncate_middle_multibyte() {
        assert_eq!(truncate_middle("🤡🧑🌾😞", 5), "🤡🧑🌾😞");
        assert_eq!(truncate_middle("🤡🧑🌾😞🧐", 5), "🤡🧑🌾😞🧐");
        assert_eq!(truncate_middle("🤡🧑🌾😞🧐✅", 5), "🤡🧑…🧐✅");
        assert_eq!(truncate_middle("🤡🧑🌾😞🧐✅❌😃", 5), "🤡🧑…❌😃");
        assert_eq!(truncate_middle("🤡🧑🌾😞🧐✅❌😃", 6), "🤡🧑…✅❌😃");
    }
}
