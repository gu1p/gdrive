use std::{
    error,
    fmt::{Display, Formatter},
    io,
};

use crate::{
    common::{
        delegate::{UploadDelegate, UploadDelegateConfig},
        hub_helper::{get_hub, GetHubError},
        table::{self, Table},
    },
    files,
    hub::Hub,
};

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) skip_header: bool,
    pub(crate) field_separator: String,
}

pub(crate) async fn list(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    let delegate_config = UploadDelegateConfig::default();

    files::info::get_file(&hub, &config.file_id)
        .await
        .map_err(|err| Error::GetFile(Box::new(err)))?;

    let permissions = list_permissions(&hub, &delegate_config, &config.file_id)
        .await
        .map_err(Error::ListPermissions)?;

    write_permissions_table(io::stdout(), &config, &permissions).map_err(Error::WriteOutput)?;

    Ok(())
}

fn write_permissions_table(
    writer: impl io::Write,
    config: &Config,
    permissions: &[google_drive3::api::Permission],
) -> Result<(), io::Error> {
    let values = permissions
        .iter()
        .map(|permission| {
            [
                permission.id.as_deref().unwrap_or_default(),
                permission.type_.as_deref().unwrap_or_default(),
                permission.role.as_deref().unwrap_or_default(),
                permission.email_address.as_deref().unwrap_or_default(),
                permission.domain.as_deref().unwrap_or_default(),
                files::info::format_bool(permission.allow_file_discovery.unwrap_or_default()),
            ]
        })
        .collect();

    let table = Table {
        header: ["Id", "Type", "Role", "Email", "Domain", "Discoverable"],
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

pub(crate) async fn list_permissions(
    hub: &Hub,
    delegate_config: &UploadDelegateConfig,
    file_id: &str,
) -> Result<Vec<google_drive3::api::Permission>, Box<google_drive3::Error>> {
    let mut delegate = UploadDelegate::new(delegate_config);

    let (_, permission_list) = hub
        .permissions()
        .list(file_id)
        .param(
            "fields",
            "permissions(id,role,type,domain,emailAddress,allowFileDiscovery)",
        )
        .add_scope(google_drive3::api::Scope::Full)
        .delegate(&mut delegate)
        .supports_all_drives(true)
        .doit()
        .await?;

    Ok(permission_list.permissions.unwrap_or_default())
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    GetFile(Box<google_drive3::Error>),
    ListPermissions(Box<google_drive3::Error>),
    WriteOutput(io::Error),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::GetFile(source) | Error::ListPermissions(source) => Some(source),
            Error::WriteOutput(source) => Some(source),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::GetFile(_) => f.write_str("unable to get file"),
            Error::ListPermissions(_) => f.write_str("unable to list permissions"),
            Error::WriteOutput(_) => f.write_str("unable to write the permission list to stdout"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{write_permissions_table, Config};
    use crate::common::table::FailingWriter;

    fn sample() -> (Config, Vec<google_drive3::api::Permission>) {
        let config = Config {
            file_id: String::from("f"),
            skip_header: false,
            field_separator: String::from("\t"),
        };
        let permissions = vec![google_drive3::api::Permission {
            id: Some(String::from("anyoneWithLink")),
            type_: Some(String::from("anyone")),
            role: Some(String::from("reader")),
            ..Default::default()
        }];
        (config, permissions)
    }

    #[test]
    fn closed_stdout_ends_the_listing_successfully() {
        let (config, permissions) = sample();

        let writer = FailingWriter(io::ErrorKind::BrokenPipe);
        assert!(write_permissions_table(writer, &config, &permissions).is_ok());

        let writer = FailingWriter(io::ErrorKind::StorageFull);
        let err = write_permissions_table(writer, &config, &permissions)
            .expect_err("other write errors are reported");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);

        let mut out = Vec::new();
        write_permissions_table(&mut out, &config, &permissions).expect("write to vec");
        assert!(String::from_utf8(out)
            .expect("utf-8")
            .starts_with("Id               Type     Role     Email   Domain   Discoverable\n"));
    }
}
