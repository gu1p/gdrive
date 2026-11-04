use std::{error, fmt, io};

use crate::{
    common::{
        delegate::{UploadDelegate, UploadDelegateConfig},
        hub_helper::{get_hub, GetHubError},
        paginate::{self, Page, PageRequest},
        table::{self, Table},
    },
    hub::Hub,
};

/// The API maximum for `drives.list`.
const MAX_PAGE_SIZE: usize = 100;

/// Only what the table prints.
const FIELDS: &str = "drives(id,name),nextPageToken";

pub(crate) struct Config {
    pub(crate) skip_header: bool,
    pub(crate) field_separator: String,
    /// List at most this many drives; `None` lists all of them.
    pub(crate) max_drives: Option<usize>,
}

pub(crate) async fn list(config: Config) -> Result<(), Box<Error>> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    let delegate_config = UploadDelegateConfig::default();

    let drives = list_drives(
        &hub,
        &delegate_config,
        config.max_drives.unwrap_or(usize::MAX),
    )
    .await
    .map_err(Error::ListDrives)?;

    write_drives_table(io::stdout(), &config, drives).map_err(Error::WriteOutput)?;

    Ok(())
}

fn write_drives_table(
    writer: impl io::Write,
    config: &Config,
    drives: Vec<google_drive3::api::Drive>,
) -> Result<(), io::Error> {
    let mut values: Vec<[String; 2]> = vec![];

    for drive in drives {
        values.push([drive.id.unwrap_or_default(), drive.name.unwrap_or_default()]);
    }

    let table = Table {
        header: ["Id", "Name"],
        values,
    };

    table::ignore_broken_pipe(table::write(
        writer,
        table,
        &table::DisplayConfig {
            skip_header: config.skip_header,
            separator: &config.field_separator,
        },
    ))
}

async fn list_drives(
    hub: &Hub,
    delegate_config: &UploadDelegateConfig,
    max_drives: usize,
) -> Result<Vec<google_drive3::api::Drive>, Box<google_drive3::Error>> {
    paginate::collect_pages(max_drives, MAX_PAGE_SIZE, async |page: PageRequest| {
        let mut delegate = UploadDelegate::new(delegate_config);

        let mut req = hub
            .drives()
            .list()
            .page_size(page.page_size)
            .param("fields", FIELDS)
            .add_scope(google_drive3::api::Scope::Full);

        if let Some(token) = &page.page_token {
            req = req.page_token(token);
        }

        let (_, drive_list) = req.delegate(&mut delegate).doit().await?;

        Ok(Page {
            items: drive_list.drives.unwrap_or_default(),
            next_page_token: drive_list.next_page_token,
        })
    })
    .await
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    ListDrives(Box<google_drive3::Error>),
    WriteOutput(io::Error),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::ListDrives(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            Error::Hub(_) => "unable to get drive hub",
            Error::ListDrives(_) => "failed to list drives",
            Error::WriteOutput(_) => "unable to write the drive list to stdout",
        };

        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{list_drives, write_drives_table, Config, FIELDS};
    use crate::common::{
        delegate::UploadDelegateConfig,
        stub_server::{Request, StubServer, TIMEOUT},
        table::FailingWriter,
    };

    /// A `drives.list` response with drives `d<first>..d<last>` and an optional next page token.
    fn drive_page(range: std::ops::Range<usize>, next_page_token: Option<&str>) -> String {
        let drives: Vec<_> = range
            .map(|i| serde_json::json!({ "id": format!("d{i}"), "name": format!("Drive {i}") }))
            .collect();
        let mut page = serde_json::json!({ "drives": drives });
        if let Some(token) = next_page_token {
            page["nextPageToken"] = token.into();
        }
        page.to_string()
    }

    /// Lists drives against a stub serving `pages`; returns the drive ids and the requests.
    async fn list_from_stub(max_drives: usize, pages: Vec<String>) -> (Vec<String>, Vec<Request>) {
        let server = StubServer::start(pages).await.expect("start stub server");
        let hub = server.hub().expect("create hub");

        let drives = tokio::time::timeout(
            TIMEOUT,
            list_drives(&hub, &UploadDelegateConfig::default(), max_drives),
        )
        .await
        .expect("listing should finish")
        .expect("listing should succeed");

        let ids = drives.into_iter().filter_map(|drive| drive.id).collect();
        (ids, server.requests().await)
    }

    fn page_params(requests: &[Request]) -> Vec<(Option<&str>, Option<&str>)> {
        requests
            .iter()
            .map(|r| (r.param("pageSize"), r.param("pageToken")))
            .collect()
    }

    #[tokio::test]
    async fn lists_all_pages_by_default() {
        let pages = vec![
            drive_page(0..100, Some("t1")),
            drive_page(100..200, Some("t2")),
            drive_page(200..205, None),
        ];

        let (ids, requests) = list_from_stub(usize::MAX, pages).await;

        assert_eq!(ids.len(), 205);
        assert_eq!(ids.first().map(String::as_str), Some("d0"));
        assert_eq!(ids.last().map(String::as_str), Some("d204"));
        assert_eq!(
            page_params(&requests),
            [
                (Some("100"), None),
                (Some("100"), Some("t1")),
                (Some("100"), Some("t2")),
            ]
        );
        for request in &requests {
            assert_eq!(request.path, "/drive/v3/drives");
            assert_eq!(request.param("fields"), Some(FIELDS));
        }
    }

    #[tokio::test]
    async fn max_caps_the_listing() {
        let pages = vec![
            drive_page(0..100, Some("t1")),
            drive_page(100..120, Some("t2")),
        ];

        let (ids, requests) = list_from_stub(120, pages).await;

        assert_eq!(ids.len(), 120);
        assert_eq!(
            page_params(&requests),
            [(Some("100"), None), (Some("20"), Some("t1"))]
        );
    }

    fn sample_drives() -> Vec<google_drive3::api::Drive> {
        vec![google_drive3::api::Drive {
            id: Some(String::from("d0")),
            name: Some(String::from("Team")),
            ..Default::default()
        }]
    }

    fn config() -> Config {
        Config {
            skip_header: false,
            field_separator: String::from("\t"),
            max_drives: None,
        }
    }

    #[test]
    fn table_lists_id_and_name() {
        let mut out = Vec::new();
        write_drives_table(&mut out, &config(), sample_drives()).expect("write to vec");
        assert_eq!(out, b"Id   Name\nd0   Team\n");
    }

    #[test]
    fn closed_stdout_ends_the_listing_successfully() {
        let writer = FailingWriter(io::ErrorKind::BrokenPipe);
        assert!(write_drives_table(writer, &config(), sample_drives()).is_ok());

        let writer = FailingWriter(io::ErrorKind::StorageFull);
        let err = write_drives_table(writer, &config(), sample_drives())
            .expect_err("other write errors are reported");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    }
}
