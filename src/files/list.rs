use std::{
    borrow::Cow,
    cmp::min,
    error,
    fmt::{self, Display, Formatter},
    io::{self, BufWriter, Write},
    str::FromStr,
};

use chrono::SecondsFormat;
use serde::Serialize;

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
    pub(crate) output: Output,
}

/// How `files list` prints the files.
#[derive(Debug, Clone)]
pub(crate) enum Output {
    /// Aligned columns, the 3.9 format.
    Table {
        skip_header: bool,
        truncate_name: bool,
        field_separator: String,
    },
    /// The table's columns as tab-separated values with full, escaped names.
    Tsv { skip_header: bool },
    /// A JSON array with one object per file.
    Json,
}

const HEADER: [&str; 5] = ["Id", "Name", "Type", "Size", "Created"];

pub(crate) async fn list(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    list_to(&hub, &config, io::stdout()).await
}

async fn list_to(hub: &Hub, config: &Config, writer: impl Write) -> Result<(), Error> {
    let files = list_files(
        hub,
        ListFilesConfig {
            query: &config.query,
            order_by: &config.order_by,
            max_files: config.max_files,
        },
    )
    .await?;

    write_files(writer, &files, &config.output).map_err(Error::WriteOutput)
}

fn write_files(
    writer: impl Write,
    files: &[google_drive3::api::File],
    output: &Output,
) -> Result<(), io::Error> {
    match output {
        Output::Table {
            skip_header,
            truncate_name,
            field_separator,
        } => table::write(
            writer,
            Table {
                header: HEADER,
                values: table_rows(files, *truncate_name),
            },
            &table::DisplayConfig {
                skip_header: *skip_header,
                separator: field_separator,
            },
        ),

        Output::Tsv { skip_header } => table::write_tsv(
            writer,
            &Table {
                header: HEADER,
                values: table_rows(files, false),
            },
            *skip_header,
        ),

        Output::Json => write_json(writer, files),
    }
}

fn table_rows(files: &[google_drive3::api::File], truncate_name: bool) -> Vec<[String; 5]> {
    files
        .iter()
        .map(|file| {
            [
                file.id.clone().unwrap_or_default(),
                format_file_name(truncate_name, file).into_owned(),
                simplified_file_type(file).to_owned(),
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
            ]
        })
        .collect()
}

/// One element of the `--json` output. Every key is always present; missing values are `null`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonFile<'a> {
    id: Option<&'a str>,
    name: Option<&'a str>,
    /// The table's Type column: folder, regular, shortcut or document.
    #[serde(rename = "type")]
    file_type: &'static str,
    mime_type: Option<&'a str>,
    /// Size in bytes.
    size: Option<i64>,
    /// RFC 3339 in UTC with milliseconds, as Drive reports it.
    created_time: Option<String>,
}

impl<'a> From<&'a google_drive3::api::File> for JsonFile<'a> {
    fn from(file: &'a google_drive3::api::File) -> Self {
        JsonFile {
            id: file.id.as_deref(),
            name: file.name.as_deref(),
            file_type: simplified_file_type(file),
            mime_type: file.mime_type.as_deref(),
            size: file.size,
            created_time: file
                .created_time
                .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true)),
        }
    }
}

fn write_json(writer: impl Write, files: &[google_drive3::api::File]) -> Result<(), io::Error> {
    let files: Vec<JsonFile> = files.iter().map(JsonFile::from).collect();
    let mut writer = BufWriter::new(writer);
    serde_json::to_writer_pretty(&mut writer, &files)?;
    writeln!(writer)?;
    writer.flush()
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
    WriteOutput(io::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::ListFiles(_) => f.write_str("unable to list files"),
            Error::WriteOutput(_) => f.write_str("unable to write the file list to stdout"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::ListFiles(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
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

fn format_file_name(truncate_name: bool, file: &google_drive3::api::File) -> Cow<'_, str> {
    let file_name = file.name.as_ref();

    if truncate_name {
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
    use std::io::{self, Write};

    use chrono::{TimeZone, Utc};

    use super::{
        list_files, list_to, quote, truncate_middle, write_files, Config, ListFilesConfig,
        ListQuery, ListSortOrder, Output,
    };
    use crate::{
        common::{
            drive_file::{MIME_TYPE_DRIVE_DOCUMENT, MIME_TYPE_DRIVE_FOLDER},
            stub_server::{Request, StubServer, TIMEOUT},
        },
        files::info::{format_date_time, DisplayBytes, DisplayConfig},
    };

    /// A `files.list` response with files `f<first>..f<last>` and an optional next page token.
    fn file_page(range: std::ops::Range<usize>, next_page_token: Option<&str>) -> String {
        let files: Vec<_> = range
            .map(|i| serde_json::json!({ "id": format!("f{i}"), "name": format!("file {i}") }))
            .collect();
        let mut page = serde_json::json!({ "files": files });
        if let Some(token) = next_page_token {
            page["nextPageToken"] = token.into();
        }
        page.to_string()
    }

    /// Lists files against a stub serving `pages`; returns the file ids and the requests.
    async fn list_from_stub(
        query: ListQuery,
        max_files: usize,
        pages: Vec<String>,
    ) -> (Vec<String>, Vec<Request>) {
        let server = StubServer::start(pages).await.expect("start stub server");
        let hub = server.hub().expect("create hub");
        let config = ListFilesConfig {
            query: &query,
            order_by: &ListSortOrder::default(),
            max_files,
        };

        let files = tokio::time::timeout(TIMEOUT, list_files(&hub, config))
            .await
            .expect("listing should finish")
            .expect("listing should succeed");

        let ids = files.into_iter().filter_map(|file| file.id).collect();
        (ids, server.requests().await)
    }

    /// `files list --max 2500`: regression test for the 1000 cap that upstream PR #104 removed.
    #[tokio::test]
    async fn max_above_1000_pages_through_results() {
        let pages = vec![
            file_page(0..1000, Some("t1")),
            file_page(1000..2000, Some("t2")),
            file_page(2000..2500, Some("t3")),
        ];
        let server = StubServer::start(pages).await.expect("start stub server");
        let hub = server.hub().expect("create hub");
        let config = Config {
            query: ListQuery::default(),
            order_by: ListSortOrder::default(),
            max_files: 2500,
            output: Output::Tsv { skip_header: true },
        };

        let mut out = Vec::new();
        tokio::time::timeout(TIMEOUT, list_to(&hub, &config, &mut out))
            .await
            .expect("listing should finish")
            .expect("listing should succeed");

        let out = String::from_utf8(out).expect("utf-8");
        assert_eq!(out.lines().count(), 2500);
        assert!(out.lines().last().expect("rows").starts_with("f2499\t"));

        let requests = server.requests().await;
        let page_params: Vec<_> = requests
            .iter()
            .map(|r| (r.param("pageSize"), r.param("pageToken")))
            .collect();
        assert_eq!(
            page_params,
            [
                (Some("1000"), None),
                (Some("1000"), Some("t1")),
                (Some("500"), Some("t2")),
            ]
        );
    }

    #[tokio::test]
    async fn stops_when_no_next_page_token() {
        let pages = vec![file_page(0..1000, Some("t1")), file_page(1000..1200, None)];

        let (ids, requests) = list_from_stub(ListQuery::default(), 5000, pages).await;

        assert_eq!(ids.len(), 1200);
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn request_parameters_follow_the_query() {
        let cases = [
            (
                list_query(None, None, None),
                Some("'root' in parents and trashed = false"),
                None,
            ),
            (
                list_query(Some("name contains 'x'"), Some("P"), None),
                Some("'P' in parents and (name contains 'x')"),
                None,
            ),
            (
                list_query(Some("name contains 'x'"), None, Some("D")),
                Some("name contains 'x'"),
                Some("D"),
            ),
            (list_query(Some(""), None, Some("D")), None, Some("D")),
        ];

        for (query, expected_q, expected_drive) in cases {
            let (_, requests) =
                list_from_stub(query.clone(), 30, vec![file_page(0..1, None)]).await;
            let request = &requests[0];

            assert_eq!(request.path, "/drive/v3/files", "{query:?}");
            assert_eq!(request.param("q"), expected_q, "{query:?}");
            assert_eq!(request.param("driveId"), expected_drive, "{query:?}");
            assert_eq!(
                request.param("corpora"),
                expected_drive.map(|_| "drive"),
                "{query:?}"
            );
            assert_eq!(request.param("supportsAllDrives"), Some("true"));
            assert_eq!(request.param("includeItemsFromAllDrives"), Some("true"));
        }
    }

    const LONG_NAME: &str = "a name that is much longer than forty-one characters.pdf";

    fn sample_files() -> Vec<google_drive3::api::File> {
        let created = Utc
            .with_ymd_and_hms(2024, 5, 6, 7, 8, 9)
            .single()
            .expect("valid date")
            + chrono::Duration::milliseconds(123);

        vec![
            google_drive3::api::File {
                id: Some(String::from("folder1")),
                name: Some(String::from("Projects")),
                mime_type: Some(String::from(MIME_TYPE_DRIVE_FOLDER)),
                created_time: Some(created),
                ..Default::default()
            },
            google_drive3::api::File {
                id: Some(String::from("file2")),
                name: Some(String::from(LONG_NAME)),
                mime_type: Some(String::from("application/pdf")),
                md5_checksum: Some(String::from("d41d8cd98f00b204e9800998ecf8427e")),
                size: Some(2_000_000),
                created_time: Some(created),
                ..Default::default()
            },
            google_drive3::api::File {
                id: Some(String::from("doc3")),
                name: Some(String::from("tab\there\nnew\rline")),
                mime_type: Some(String::from(MIME_TYPE_DRIVE_DOCUMENT)),
                ..Default::default()
            },
        ]
    }

    fn render(output: &Output) -> String {
        let mut out = Vec::new();
        write_files(&mut out, &sample_files(), output).expect("write to vec");
        String::from_utf8(out).expect("utf-8")
    }

    fn table(skip_header: bool, truncate_name: bool) -> Output {
        Output::Table {
            skip_header,
            truncate_name,
            field_separator: String::from("\t"),
        }
    }

    #[test]
    fn table_output_keeps_3_9_layout() {
        let created = format_date_time(sample_files()[0].created_time.expect("set")).to_string();
        let size = DisplayBytes {
            bytes: 2_000_000,
            config: &DisplayConfig::default(),
        }
        .to_string();
        let rows = [
            ["Id", "Name", "Type", "Size", "Created"],
            ["folder1", "Projects", "folder", "", &created],
            [
                "file2",
                "a name that is much …y-one characters.pdf",
                "regular",
                &size,
                &created,
            ],
            ["doc3", "tab\there\nnew\rline", "document", "", ""],
        ];
        // TabWriter pads every column to its widest cell plus 3 spaces. The name with a tab and
        // a newline breaks the alignment, as it did in 3.9.
        let mut expected = Vec::new();
        let mut tw = tabwriter::TabWriter::new(&mut expected).padding(3);
        for row in rows {
            writeln!(tw, "{}", row.join("\t")).expect("write to vec");
        }
        tw.flush().expect("flush");
        drop(tw);

        assert_eq!(
            render(&table(false, true)),
            String::from_utf8(expected).expect("utf-8")
        );
        assert!(render(&table(true, true)).starts_with("folder1 "));
        assert!(render(&table(false, false)).contains(LONG_NAME));
    }

    #[test]
    fn table_output_with_custom_separator_is_not_aligned() {
        let output = Output::Table {
            skip_header: false,
            truncate_name: true,
            field_separator: String::from(","),
        };
        let rendered = render(&output);
        let mut lines = rendered.lines();
        assert_eq!(lines.next(), Some("Id,Name,Type,Size,Created"));
        assert!(lines
            .next()
            .expect("first row")
            .starts_with("folder1,Projects,folder,,"));
    }

    #[test]
    fn tsv_output_has_full_escaped_names_and_one_line_per_file() {
        let rendered = render(&Output::Tsv { skip_header: false });
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines.len(), 4, "{rendered:?}");
        assert_eq!(lines[0], "Id\tName\tType\tSize\tCreated");
        for line in &lines {
            assert_eq!(line.split('\t').count(), 5, "{line:?}");
        }
        assert!(lines[2].starts_with(&format!("file2\t{LONG_NAME}\tregular\t")));
        assert_eq!(lines[3], "doc3\ttab\\there\\nnew\\rline\tdocument\t\t");

        let without_header = render(&Output::Tsv { skip_header: true });
        assert!(without_header.starts_with("folder1\tProjects\tfolder\t\t"));
    }

    #[test]
    fn json_output_shape() {
        let rendered = render(&Output::Json);
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");

        assert_eq!(
            value,
            serde_json::json!([
                {
                    "id": "folder1",
                    "name": "Projects",
                    "type": "folder",
                    "mimeType": MIME_TYPE_DRIVE_FOLDER,
                    "size": null,
                    "createdTime": "2024-05-06T07:08:09.123Z",
                },
                {
                    "id": "file2",
                    "name": LONG_NAME,
                    "type": "regular",
                    "mimeType": "application/pdf",
                    "size": 2_000_000,
                    "createdTime": "2024-05-06T07:08:09.123Z",
                },
                {
                    "id": "doc3",
                    "name": "tab\there\nnew\rline",
                    "type": "document",
                    "mimeType": MIME_TYPE_DRIVE_DOCUMENT,
                    "size": null,
                    "createdTime": null,
                },
            ])
        );
        assert!(value[1]["size"].is_u64());
        assert!(rendered.ends_with("]\n"));
    }

    #[test]
    fn json_output_of_no_files_is_an_empty_array() {
        let mut out = Vec::new();
        write_files(&mut out, &[], &Output::Json).expect("write to vec");
        assert_eq!(out, b"[]\n");
    }

    /// A stdout whose reader has gone away.
    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn write_errors_are_returned() {
        for output in [
            table(false, true),
            Output::Tsv { skip_header: false },
            Output::Json,
        ] {
            let err = write_files(BrokenPipe, &sample_files(), &output)
                .expect_err("writing to a broken pipe should fail");
            assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "{output:?}");
        }
    }

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
