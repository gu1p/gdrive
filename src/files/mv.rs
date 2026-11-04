use std::{
    error,
    fmt::{Display, Formatter},
    ops::Not,
};

use crate::{
    common::{
        drive_file,
        hub_helper::{get_hub, GetHubError},
        retry::retry_api,
    },
    files::{self, FileResult},
    hub::Hub,
};

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) file_id: String,
    pub(crate) to_folder_id: String,
}

pub(crate) async fn mv(config: Config) -> Result<(), Error> {
    let hub = get_hub().await.map_err(Error::Hub)?;

    let old_file = files::info::get_file(&hub, &config.file_id)
        .await
        .map_err(|err| Error::GetFile(Box::new(err)))?;

    let old_parent_id = get_old_parent_id(&old_file)?;

    let old_parent = files::info::get_file(&hub, old_parent_id)
        .await
        .map_err(|err| Error::GetOldParent(old_parent_id.to_owned(), Box::new(err)))?;

    let new_parent = files::info::get_file(&hub, &config.to_folder_id)
        .await
        .map_err(|err| Error::GetNewParent(Box::new(err)))?;

    if drive_file::is_directory(&new_parent).not() {
        return Err(Error::NotADirectory);
    }

    println!(
        "Moving '{}' from '{}' to '{}'",
        old_file.name.as_deref().unwrap_or_default(),
        old_parent.name.unwrap_or_default(),
        new_parent.name.unwrap_or_default()
    );

    let change_parent_ids = ChangeParentIds {
        file: &config.file_id,
        old_parent: old_parent_id,
        new_parent: &config.to_folder_id,
    };

    change_parent(&hub, change_parent_ids)
        .await
        .map_err(|err| Error::Move(Box::new(err)))?;

    Ok(())
}

struct ChangeParentIds<'a> {
    pub(crate) file: &'a str,
    pub(crate) old_parent: &'a str,
    pub(crate) new_parent: &'a str,
}

#[expect(
    clippy::result_large_err,
    reason = "Ok variant is bigger, see test next to FileResult"
)]
async fn change_parent(hub: &Hub, ids: ChangeParentIds<'_>) -> FileResult {
    let ids = &ids;
    let (_, file) = retry_api(hub.retrier(), "files.update", |mut delegate| async move {
        hub.files()
            .update(google_drive3::api::File::default(), ids.file)
            .remove_parents(ids.old_parent)
            .add_parents(ids.new_parent)
            .param("fields", "id,name,size,createdTime,modifiedTime,md5Checksum,mimeType,parents,shared,description,webContentLink,webViewLink")
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .supports_all_drives(true)
            .doit_without_upload()
            .await
    })
    .await?;

    Ok(file)
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    GetFile(Box<google_drive3::Error>),
    GetOldParent(String, Box<google_drive3::Error>),
    GetNewParent(Box<google_drive3::Error>),
    NoParents,
    MultipleParents,
    NotADirectory,
    Move(Box<google_drive3::Error>),
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::GetFile(source)
            | Error::GetOldParent(_, source)
            | Error::GetNewParent(source)
            | Error::Move(source) => Some(source),
            Error::NoParents | Error::MultipleParents | Error::NotADirectory => None,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::GetFile(_) => f.write_str("unable to get file"),
            Error::GetNewParent(_) => f.write_str("unable to get new parent"),
            Error::GetOldParent(id, _) => {
                write!(f, "unable to get old parent '{id}'")
            }
            Error::NoParents => f.write_str("file has no parents"),
            Error::MultipleParents => f.write_str("can't move file with multiple parents"),
            Error::NotADirectory => f.write_str("new parent is not a directory"),
            Error::Move(_) => f.write_str("unable to move file"),
        }
    }
}

fn get_old_parent_id(file: &google_drive3::api::File) -> Result<&'_ str, Error> {
    match &file.parents {
        None => Err(Error::NoParents),

        Some(parents) => match &parents[..] {
            [] => Err(Error::NoParents),
            [parent_id] => Ok(parent_id.as_str()),
            _ => Err(Error::MultipleParents),
        },
    }
}
