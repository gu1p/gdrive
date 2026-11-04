use std::{
    error,
    fmt::{Display, Formatter},
};

use crate::{
    common::{
        drive_file,
        file_tree_drive::errors::FileIdentifier,
        hub_helper::{get_hub, GetHubError},
    },
    files::{
        self,
        batch::{Batch, BatchError},
    },
    hub::Hub,
};

pub(crate) struct Config {
    /// Deleted one after another; see [`Batch`] for how failures are reported.
    pub(crate) file_ids: Vec<String>,
    pub(crate) delete_directories: bool,
}

pub(crate) async fn delete(config: Config) -> Result<(), BatchError<Error>> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    delete_files(&hub, &config).await
}

async fn delete_files(hub: &Hub, config: &Config) -> Result<(), BatchError<Error>> {
    let mut batch = Batch::new("delete", config.file_ids.len());
    for file_id in &config.file_ids {
        let result = delete_file(hub, file_id, config.delete_directories).await;
        batch.record(file_id, result);
    }
    batch.finish()
}

async fn delete_file(hub: &Hub, file_id: &str, delete_directories: bool) -> Result<(), Error> {
    let file = files::info::get_file(hub, file_id)
        .await
        .map_err(|err| Error::GetFile(Box::new(err)))?;

    if drive_file::is_directory(&file) && !delete_directories {
        return Err(Error::IsDirectory(FileIdentifier::from(file)));
    }

    hub.files()
        .delete(file_id)
        .supports_all_drives(true)
        .add_scope(google_drive3::api::Scope::Full)
        .doit()
        .await
        .map_err(|err| Error::DeleteFile(Box::new(err)))?;

    println!("Deleted '{}'", file.name.unwrap_or_default());

    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    GetFile(Box<google_drive3::Error>),
    DeleteFile(Box<google_drive3::Error>),
    IsDirectory(FileIdentifier),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::GetFile(_) => f.write_str("unable to get file to delete"),
            Error::DeleteFile(_) => f.write_str("unable to delete file"),
            Error::IsDirectory(identifier) => write!(
                f,
                "file{} is a directory, use --recursive to delete directories",
                identifier.display(),
            ),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::GetFile(source) | Error::DeleteFile(source) => Some(source),
            Error::IsDirectory(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{delete_files, Config, Error};
    use crate::{
        common::{
            drive_file::MIME_TYPE_DRIVE_FOLDER,
            mock_server::{MockServer, Response},
        },
        files::batch::BatchError,
    };

    fn file(id: &str) -> Response {
        Response::json(
            200,
            &json!({"id": id, "name": format!("{id}.txt"), "mimeType": "text/plain"}),
        )
    }

    fn deleted() -> Response {
        Response::json(200, &json!({}))
    }

    fn methods_and_paths(server: &MockServer) -> Vec<(String, String)> {
        server
            .requests()
            .into_iter()
            .map(|request| (request.method, request.path))
            .collect()
    }

    fn config(ids: &[&str]) -> Config {
        Config {
            file_ids: ids.iter().map(|&id| id.to_owned()).collect(),
            delete_directories: false,
        }
    }

    #[tokio::test]
    async fn deletes_every_id_in_order() {
        let server = MockServer::start(vec![file("a"), deleted(), file("b"), deleted()]).await;

        delete_files(&server.hub(), &config(&["a", "b"]))
            .await
            .unwrap();

        assert_eq!(
            methods_and_paths(&server),
            [
                ("GET", "/drive/v3/files/a"),
                ("DELETE", "/drive/v3/files/a"),
                ("GET", "/drive/v3/files/b"),
                ("DELETE", "/drive/v3/files/b"),
            ]
            .map(|(method, path)| (method.to_owned(), path.to_owned()))
        );
    }

    #[tokio::test]
    async fn a_failed_id_does_not_stop_the_others() {
        let directory = Response::json(
            200,
            &json!({"id": "dir", "name": "dir", "mimeType": MIME_TYPE_DRIVE_FOLDER}),
        );
        let server = MockServer::start(vec![directory, file("b"), deleted()]).await;

        let err = delete_files(&server.hub(), &config(&["dir", "b"]))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            BatchError::Failed {
                verb: "delete",
                failed: 1,
                total: 2
            }
        ));
        assert_eq!(
            methods_and_paths(&server),
            [
                ("GET", "/drive/v3/files/dir"),
                ("GET", "/drive/v3/files/b"),
                ("DELETE", "/drive/v3/files/b"),
            ]
            .map(|(method, path)| (method.to_owned(), path.to_owned()))
        );
    }

    #[tokio::test]
    async fn single_id_returns_its_own_error() {
        let directory = Response::json(
            200,
            &json!({"id": "dir", "name": "dir", "mimeType": MIME_TYPE_DRIVE_FOLDER}),
        );
        let server = MockServer::start(vec![directory]).await;

        let err = delete_files(&server.hub(), &config(&["dir"]))
            .await
            .unwrap_err();
        assert!(matches!(err, BatchError::Single(Error::IsDirectory(_))));
        assert_eq!(
            err.to_string(),
            "file with name 'dir' is a directory, use --recursive to delete directories"
        );
    }
}
