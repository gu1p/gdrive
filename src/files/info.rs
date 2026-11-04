use std::{
    error,
    fmt::{self, Display, Formatter, Write as _},
};

use chrono::{
    self,
    format::{DelayedFormat, StrftimeItems},
    DateTime,
};

use crate::{
    common::{
        hub_helper::{get_hub, GetHubError},
        human_size::human_size,
        retry::retry_read,
    },
    files::FileResult,
    hub::Hub,
};

pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) size_in_bytes: bool,
}

pub(crate) async fn info(config: Config) -> Result<(), Box<Error>> {
    let hub = get_hub().await.map_err(Error::Hub)?;

    let file = get_file(&hub, &config.file_id)
        .await
        .map_err(Error::GetFile)?;

    print!(
        "{}",
        info_text(
            &file,
            &DisplayConfig {
                size_in_bytes: config.size_in_bytes,
            },
        )
    );

    Ok(())
}

/// What `files info` prints: the fields of [`print_file_info`], then, for a file in the trash
/// only, a `Trashed` line. The output for a file that is not in the trash is that of gdrive 3.x.
fn info_text(file: &google_drive3::api::File, display_config: &DisplayConfig) -> String {
    let mut text = file_info_text(file, display_config);
    push_field(
        &mut text,
        "Trashed",
        (file.trashed == Some(true)).then(|| trashed_status(file.explicitly_trashed)),
    );
    text
}

/// `explicitly`: the file itself was trashed, not (only) a folder it is in.
fn trashed_status(explicitly: Option<bool>) -> &'static str {
    if explicitly == Some(true) {
        "yes (explicitly)"
    } else {
        "yes"
    }
}

#[expect(
    clippy::result_large_err,
    reason = "Ok variant is bigger, see test next to FileResult"
)]
pub(crate) async fn get_file(hub: &Hub, file_id: &str) -> FileResult {
    let (_, file) = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
        hub.files()
            .get(file_id)
            .param(
                "fields",
                "id,name,size,createdTime,modifiedTime,version,md5Checksum,mimeType,parents,\
                driveId,shared,description,webContentLink,webViewLink,\
                shortcutDetails(targetId,targetMimeType),trashed,explicitlyTrashed",
            )
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await?;

    Ok(file)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DisplayConfig {
    pub(crate) size_in_bytes: bool,
}

pub(crate) fn print_file_info(file: &google_drive3::api::File, display_config: &DisplayConfig) {
    print!("{}", file_info_text(file, display_config));
}

/// The lines [`print_file_info`] prints, one per field that `file` has.
fn file_info_text(file: &google_drive3::api::File, display_config: &DisplayConfig) -> String {
    let google_drive3::api::File {
        created_time,
        id,
        md5_checksum,
        mime_type,
        modified_time,
        name,
        parents,
        shared,
        size,
        web_view_link,
        ..
    } = file;

    let mut text = String::new();
    push_field(&mut text, "Id", id.as_ref());
    push_field(&mut text, "Name", name.as_ref());
    push_field(&mut text, "Mime", mime_type.as_ref());
    push_field(
        &mut text,
        "Size",
        size.map(|bytes| DisplayBytes {
            bytes: u64::try_from(bytes).unwrap_or(0),
            config: display_config,
        }),
    );
    push_field(&mut text, "Created", created_time.map(format_date_time));
    push_field(&mut text, "Modified", modified_time.map(format_date_time));
    push_field(&mut text, "MD5", md5_checksum.as_ref());
    push_field(&mut text, "Shared", shared.map(format_bool));
    push_field(
        &mut text,
        "Parents",
        parents.as_deref().map(DisplayJoinedSlice),
    );
    push_field(&mut text, "ViewUrl", web_view_link.as_ref());
    text
}

/// Adds the line `name: value` to `text`, if there is a value.
fn push_field(text: &mut String, name: &str, value: Option<impl Display>) {
    if let Some(value) = value {
        // Writing to a String does not fail.
        let _ = writeln!(text, "{name}: {value}");
    }
}

// TODO: move to common
#[must_use]
pub(crate) fn format_bool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DisplayJoinedSlice<'a, T>(pub(crate) &'a [T]);

impl<T> Display for DisplayJoinedSlice<'_, T>
where
    T: Display,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut iter = self.0.iter();
        if let Some(first) = iter.next() {
            write!(f, "{first}")?;
            for element in iter {
                write!(f, ", {element}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DisplayBytes<'a> {
    pub(crate) bytes: u64,
    pub(crate) config: &'a DisplayConfig,
}

impl Display for DisplayBytes<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let &Self { bytes, config } = self;

        if config.size_in_bytes {
            write!(f, "{bytes}")
        } else {
            f.write_str(&human_size(bytes))
        }
    }
}

#[must_use]
pub(crate) fn format_date_time(
    utc_time: DateTime<chrono::Utc>,
) -> DelayedFormat<StrftimeItems<'static>> {
    let local_time = DateTime::<chrono::Local>::from(utc_time);
    local_time.format("%Y-%m-%d %H:%M:%S")
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    GetFile(google_drive3::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::GetFile(_) => f.write_str("unable to get file"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::GetFile(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{file_info_text, format_date_time, get_file, info_text, DisplayConfig};
    use crate::common::{
        human_size::human_size,
        mock_server::{MockServer, Response},
    };

    async fn info_of(file: &serde_json::Value) -> (String, String, Vec<String>) {
        let server = MockServer::start(vec![Response::json(200, file)]).await;
        let file = get_file(&server.hub(), "file-id").await.unwrap();
        let fields = server.requests()[0]
            .query_param("fields")
            .unwrap()
            .replace("%2C", ",")
            .split(',')
            .map(str::to_owned)
            .collect();
        let config = DisplayConfig::default();
        (
            info_text(&file, &config),
            file_info_text(&file, &config),
            fields,
        )
    }

    #[tokio::test]
    async fn info_prints_the_trashed_status_after_the_other_fields() {
        let (text, other_commands, fields) = info_of(&json!({
            "id": "file-id",
            "name": "a.txt",
            "mimeType": "text/plain",
            "size": "2048",
            "createdTime": "2024-01-02T03:04:05Z",
            "modifiedTime": "2024-02-03T04:05:06Z",
            "md5Checksum": "0cc175b9c0f1b6a831c399e269772661",
            "shared": false,
            "parents": ["parent-a", "parent-b"],
            "webViewLink": "https://drive.example/view",
            "trashed": true,
            "explicitlyTrashed": true,
        }))
        .await;

        let time = |rfc3339: &str| {
            format_date_time(
                chrono::DateTime::parse_from_rfc3339(rfc3339)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            )
            .to_string()
        };
        let fields_before = format!(
            "Id: file-id\n\
             Name: a.txt\n\
             Mime: text/plain\n\
             Size: {}\n\
             Created: {}\n\
             Modified: {}\n\
             MD5: 0cc175b9c0f1b6a831c399e269772661\n\
             Shared: False\n\
             Parents: parent-a, parent-b\n\
             ViewUrl: https://drive.example/view\n",
            human_size(2048),
            time("2024-01-02T03:04:05Z"),
            time("2024-02-03T04:05:06Z"),
        );
        assert_eq!(text, format!("{fields_before}Trashed: yes (explicitly)\n"));
        // The other commands print the file as before.
        assert_eq!(other_commands, fields_before);
        assert!(
            fields.iter().any(|field| field == "trashed")
                && fields.iter().any(|field| field == "explicitlyTrashed"),
            "{fields:?}"
        );
    }

    #[tokio::test]
    async fn a_file_that_is_not_in_the_trash_prints_as_before() {
        let file = json!({
            "id": "file-id",
            "name": "a.txt",
            "mimeType": "text/plain",
            "shared": true,
            "parents": ["parent-a"],
            "trashed": false,
            "explicitlyTrashed": false,
        });
        let (text, other_commands, _) = info_of(&file).await;
        assert_eq!(
            text,
            "Id: file-id\n\
             Name: a.txt\n\
             Mime: text/plain\n\
             Shared: True\n\
             Parents: parent-a\n"
        );
        assert_eq!(text, other_commands);

        // Drive did not say.
        let (text, ..) = info_of(&json!({"id": "file-id"})).await;
        assert_eq!(text, "Id: file-id\n");
    }

    #[tokio::test]
    async fn trashed_status_says_whether_the_file_itself_was_trashed() {
        // Trashed with a folder it is in.
        let (text, ..) = info_of(&json!({
            "id": "file-id",
            "trashed": true,
            "explicitlyTrashed": false,
        }))
        .await;
        assert_eq!(text, "Id: file-id\nTrashed: yes\n");

        let (text, ..) = info_of(&json!({"id": "file-id", "trashed": true})).await;
        assert_eq!(text, "Id: file-id\nTrashed: yes\n");
    }
}
