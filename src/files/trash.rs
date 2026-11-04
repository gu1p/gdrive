//! `files trash` and `files untrash`: move a file to or from the trash by patching its
//! `trashed` field. Both commands share this implementation; they only differ in [`Action`].

use std::{
    error,
    fmt::{self, Display, Formatter},
};

use crate::{
    common::{
        drive_file,
        file_tree_drive::errors::FileIdentifier,
        hub_helper::{get_hub, GetHubError},
        retry::retry_read,
    },
    files::{
        self,
        batch::{Batch, BatchError},
        update::PatchFile,
    },
    hub::Hub,
};

/// Metadata needed to decide whether a file may be (un)trashed.
const FILE_FIELDS: &str = "id,name,mimeType,trashed,explicitlyTrashed,driveId,ownedByMe,\
    capabilities(canTrash,canUntrash)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Trash,
    Untrash,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Trash => "trash",
            Action::Untrash => "untrash",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            Action::Trash => "Trashed",
            Action::Untrash => "Untrashed",
        }
    }
}

pub(crate) struct Config {
    /// Processed one after another; see [`Batch`] for how failures are reported.
    pub(crate) file_ids: Vec<String>,
    pub(crate) action: Action,
    /// Allow trashing a directory together with its content. Untrash ignores it.
    pub(crate) trash_directories: bool,
}

pub(crate) async fn trash(config: Config) -> Result<(), BatchError<Error>> {
    let hub = get_hub().await.map_err(Error::Hub)?;
    trash_files(&hub, &config).await
}

async fn trash_files(hub: &Hub, config: &Config) -> Result<(), BatchError<Error>> {
    let mut batch = Batch::new(config.action.verb(), config.file_ids.len());
    for file_id in &config.file_ids {
        let result = trash_file(hub, file_id, config.action, config.trash_directories).await;
        batch.record(file_id, result);
    }
    batch.finish()
}

async fn trash_file(
    hub: &Hub,
    file_id: &str,
    action: Action,
    trash_directories: bool,
) -> Result<(), Error> {
    let (_, file) = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
        hub.files()
            .get(file_id)
            .param("fields", FILE_FIELDS)
            .supports_all_drives(true)
            .add_scope(google_drive3::api::Scope::Full)
            .delegate(&mut delegate)
            .doit()
            .await
    })
    .await
    .map_err(|err| Error::GetFile(Box::new(err)))?;

    let name = file.name.clone().unwrap_or_default();

    if check(&file, action, trash_directories)? == Check::AlreadyDone {
        match action {
            Action::Trash => println!("'{name}' is already in the trash"),
            Action::Untrash => println!("'{name}' is not in the trash"),
        }
        return Ok(());
    }

    let patch_file = PatchFile::new(file_id.to_owned()).with_trashed(action == Action::Trash);
    files::update::update_metadata(hub, patch_file)
        .await
        .map_err(|err| Error::Update(action, Box::new(err)))?;

    println!("{} '{name}'", action.past_tense());

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Proceed,
    AlreadyDone,
}

/// Decides from the file metadata whether `action` can go ahead. Refusals that Drive would
/// reject anyway get an explanation instead of a raw API error.
fn check(
    file: &google_drive3::api::File,
    action: Action,
    trash_directories: bool,
) -> Result<Check, Error> {
    if is_shared_drive_root(file) {
        return Err(Error::SharedDriveRoot(FileIdentifier::from(file)));
    }

    // `trashed` is also true for everything inside a trashed folder. Such a file is not in the
    // trash on its own: skipping the patch would let a later restore of the folder bring it
    // back. Only an explicitly trashed file needs nothing more.
    let done = match action {
        Action::Trash => file.explicitly_trashed == Some(true),
        Action::Untrash => !file.trashed.unwrap_or(false),
    };
    if done {
        return Ok(Check::AlreadyDone);
    }

    if action == Action::Trash && drive_file::is_directory(file) && !trash_directories {
        return Err(Error::IsDirectory(FileIdentifier::from(file)));
    }

    let capabilities = file.capabilities.as_ref();
    let allowed = match action {
        Action::Trash => capabilities.and_then(|caps| caps.can_trash),
        Action::Untrash => capabilities.and_then(|caps| caps.can_untrash),
    };
    if allowed == Some(false) {
        // Outside shared drives only the owner may trash a file: a file shared with the
        // user, or the target of a shortcut in their drive, is someone else's.
        return if file.drive_id.is_none() && file.owned_by_me == Some(false) {
            Err(Error::NotOwner(action, FileIdentifier::from(file)))
        } else {
            Err(Error::NotPermitted(action, FileIdentifier::from(file)))
        };
    }

    Ok(Check::Proceed)
}

/// A shared drive's root folder has the drive's id as its own id.
fn is_shared_drive_root(file: &google_drive3::api::File) -> bool {
    matches!((&file.id, &file.drive_id), (Some(id), Some(drive_id)) if id == drive_id)
}

#[derive(Debug)]
pub(crate) enum Error {
    Hub(GetHubError),
    GetFile(Box<google_drive3::Error>),
    Update(Action, Box<google_drive3::Error>),
    SharedDriveRoot(FileIdentifier),
    IsDirectory(FileIdentifier),
    NotOwner(Action, FileIdentifier),
    NotPermitted(Action, FileIdentifier),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hub(_) => f.write_str("unable to get drive hub"),
            Error::GetFile(_) => f.write_str("unable to get file"),
            Error::Update(action, _) => write!(f, "unable to {} file", action.verb()),
            Error::SharedDriveRoot(identifier) => write!(
                f,
                "file{} is the root of a shared drive, which cannot be trashed or untrashed",
                identifier.display(),
            ),
            Error::IsDirectory(identifier) => write!(
                f,
                "file{} is a directory, use --recursive to trash directories",
                identifier.display(),
            ),
            Error::NotOwner(action, identifier) => write!(
                f,
                "file{} is owned by someone else and only its owner can {} it; if it is the \
                target of a shortcut, trash the shortcut's id instead",
                identifier.display(),
                action.verb(),
            ),
            Error::NotPermitted(action, identifier) => write!(
                f,
                "you are not allowed to {} file{}; on a shared drive this needs the Manager or \
                Content manager role",
                action.verb(),
                identifier.display(),
            ),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hub(source) => Some(source),
            Error::GetFile(source) | Error::Update(_, source) => Some(source),
            Error::SharedDriveRoot(_)
            | Error::IsDirectory(_)
            | Error::NotOwner(..)
            | Error::NotPermitted(..) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use google_drive3::api::{File, FileCapabilities};
    use serde_json::json;

    use super::{check, trash_file, trash_files, Action, Check, Config, Error};
    use crate::{
        common::{
            drive_file::MIME_TYPE_DRIVE_FOLDER,
            mock_server::{MockServer, Response},
        },
        files::batch::BatchError,
    };

    fn my_file() -> File {
        File {
            id: Some("file-id".to_owned()),
            name: Some("report.txt".to_owned()),
            mime_type: Some("text/plain".to_owned()),
            trashed: Some(false),
            owned_by_me: Some(true),
            capabilities: Some(FileCapabilities {
                can_trash: Some(true),
                can_untrash: Some(true),
                ..FileCapabilities::default()
            }),
            ..File::default()
        }
    }

    fn not_allowed(file: File) -> File {
        File {
            capabilities: Some(FileCapabilities {
                can_trash: Some(false),
                can_untrash: Some(false),
                ..FileCapabilities::default()
            }),
            ..file
        }
    }

    #[test]
    fn own_file_can_be_trashed() {
        assert_eq!(
            check(&my_file(), Action::Trash, false).unwrap(),
            Check::Proceed
        );
    }

    #[test]
    fn missing_capabilities_leave_the_decision_to_drive() {
        let file = File {
            capabilities: None,
            owned_by_me: None,
            ..my_file()
        };
        assert_eq!(check(&file, Action::Trash, false).unwrap(), Check::Proceed);
    }

    #[test]
    fn trashing_a_trashed_file_is_a_no_op() {
        let file = File {
            trashed: Some(true),
            explicitly_trashed: Some(true),
            ..my_file()
        };
        assert_eq!(
            check(&file, Action::Trash, false).unwrap(),
            Check::AlreadyDone
        );
        assert_eq!(
            check(&file, Action::Untrash, false).unwrap(),
            Check::Proceed
        );
        assert_eq!(
            check(&my_file(), Action::Untrash, false).unwrap(),
            Check::AlreadyDone
        );
    }

    #[test]
    fn file_trashed_only_through_its_folder_is_still_trashed() {
        for explicitly_trashed in [Some(false), None] {
            let child = File {
                trashed: Some(true),
                explicitly_trashed,
                ..my_file()
            };
            assert_eq!(
                check(&child, Action::Trash, false).unwrap(),
                Check::Proceed,
                "explicitly_trashed: {explicitly_trashed:?}"
            );
        }
    }

    #[test]
    fn shared_drive_root_is_refused() {
        let root = File {
            id: Some("drive-id".to_owned()),
            drive_id: Some("drive-id".to_owned()),
            mime_type: Some(MIME_TYPE_DRIVE_FOLDER.to_owned()),
            ..my_file()
        };
        for action in [Action::Trash, Action::Untrash] {
            assert!(matches!(
                check(&root, action, true),
                Err(Error::SharedDriveRoot(_))
            ));
        }

        let inside = File {
            drive_id: Some("drive-id".to_owned()),
            ..my_file()
        };
        assert_eq!(
            check(&inside, Action::Trash, false).unwrap(),
            Check::Proceed
        );
    }

    #[test]
    fn directory_needs_recursive_to_be_trashed() {
        let dir = File {
            mime_type: Some(MIME_TYPE_DRIVE_FOLDER.to_owned()),
            ..my_file()
        };
        assert!(matches!(
            check(&dir, Action::Trash, false),
            Err(Error::IsDirectory(_))
        ));
        assert_eq!(check(&dir, Action::Trash, true).unwrap(), Check::Proceed);

        let trashed_dir = File {
            trashed: Some(true),
            ..dir
        };
        assert_eq!(
            check(&trashed_dir, Action::Untrash, false).unwrap(),
            Check::Proceed
        );
    }

    #[test]
    fn file_owned_by_someone_else_is_refused() {
        let shared_with_me = not_allowed(File {
            owned_by_me: Some(false),
            ..my_file()
        });
        let err = check(&shared_with_me, Action::Trash, false).unwrap_err();
        assert!(matches!(err, Error::NotOwner(Action::Trash, _)));
        assert!(err.to_string().contains("shortcut"), "{err}");
    }

    #[test]
    fn shared_drive_file_without_permission_is_refused() {
        let file = not_allowed(File {
            drive_id: Some("drive-id".to_owned()),
            owned_by_me: None,
            ..my_file()
        });
        assert!(matches!(
            check(&file, Action::Trash, false),
            Err(Error::NotPermitted(Action::Trash, _))
        ));
    }

    fn metadata(trashed: bool) -> Response {
        Response::json(
            200,
            &json!({
                "id": "file-id",
                "name": "report.txt",
                "mimeType": "text/plain",
                "trashed": trashed,
                "explicitlyTrashed": trashed,
                "ownedByMe": true,
                "capabilities": {"canTrash": true, "canUntrash": true},
            }),
        )
    }

    #[tokio::test]
    async fn trash_patches_trashed_field() {
        let server = MockServer::start(vec![
            metadata(false),
            Response::json(200, &json!({"id": "file-id", "trashed": true})),
        ])
        .await;

        trash_file(&server.hub(), "file-id", Action::Trash, false)
            .await
            .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 2);

        let get = &requests[0];
        assert_eq!(get.method, "GET");
        assert_eq!(get.path, "/drive/v3/files/file-id");
        assert!(get.query_param("fields").unwrap().contains("capabilities"));

        let patch = &requests[1];
        assert_eq!(patch.method, "PATCH");
        assert_eq!(patch.path, "/drive/v3/files/file-id");
        assert_eq!(patch.query_param("supportsAllDrives"), Some("true"));
        assert_eq!(patch.body_json(), json!({"trashed": true}));
    }

    #[tokio::test]
    async fn untrash_patches_trashed_field() {
        let server = MockServer::start(vec![
            metadata(true),
            Response::json(200, &json!({"id": "file-id", "trashed": false})),
        ])
        .await;

        trash_file(&server.hub(), "file-id", Action::Untrash, false)
            .await
            .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(requests[1].body_json(), json!({"trashed": false}));
    }

    #[tokio::test]
    async fn refused_or_no_op_trash_sends_no_patch() {
        let server = MockServer::start(vec![metadata(true)]).await;
        trash_file(&server.hub(), "file-id", Action::Trash, false)
            .await
            .unwrap();
        assert_eq!(server.requests().len(), 1);

        let server = MockServer::start(vec![Response::json(
            200,
            &json!({"id": "drive-id", "driveId": "drive-id", "name": "Team"}),
        )])
        .await;
        let err = trash_file(&server.hub(), "drive-id", Action::Trash, true)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::SharedDriveRoot(_)));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn several_ids_continue_after_a_failure() {
        let not_found = Response::json(
            404,
            &json!({"error": {"code": 404, "message": "File not found"}}),
        );
        let server = MockServer::start(vec![
            metadata(false),
            Response::json(200, &json!({"id": "file-id"})),
            not_found,
            metadata(false),
            Response::json(200, &json!({"id": "file-id"})),
        ])
        .await;

        let config = Config {
            file_ids: vec!["a".to_owned(), "missing".to_owned(), "c".to_owned()],
            action: Action::Trash,
            trash_directories: false,
        };
        let err = trash_files(&server.hub(), &config).await.unwrap_err();
        assert!(matches!(
            err,
            BatchError::Failed {
                verb: "trash",
                failed: 1,
                total: 3
            }
        ));

        let requests: Vec<_> = server
            .requests()
            .into_iter()
            .map(|request| (request.method, request.path))
            .collect();
        assert_eq!(
            requests,
            [
                ("GET", "/drive/v3/files/a"),
                ("PATCH", "/drive/v3/files/a"),
                ("GET", "/drive/v3/files/missing"),
                ("GET", "/drive/v3/files/c"),
                ("PATCH", "/drive/v3/files/c"),
            ]
            .map(|(method, path)| (method.to_owned(), path.to_owned()))
        );
    }

    #[tokio::test]
    async fn child_of_a_trashed_folder_is_still_patched() {
        // Regression: trashing folder P and then its child C must patch C, otherwise C has no
        // trash entry of its own and restoring P restores C as well.
        let capabilities = json!({"canTrash": true, "canUntrash": true});
        let server = MockServer::start(vec![
            Response::json(
                200,
                &json!({
                    "id": "parent", "name": "P", "mimeType": MIME_TYPE_DRIVE_FOLDER,
                    "trashed": false, "explicitlyTrashed": false,
                    "ownedByMe": true, "capabilities": capabilities,
                }),
            ),
            Response::json(200, &json!({"id": "parent", "trashed": true})),
            Response::json(
                200,
                &json!({
                    "id": "child", "name": "c.txt", "mimeType": "text/plain",
                    "trashed": true, "explicitlyTrashed": false,
                    "ownedByMe": true, "capabilities": capabilities,
                }),
            ),
            Response::json(200, &json!({"id": "child", "trashed": true})),
        ])
        .await;

        let config = Config {
            file_ids: vec!["parent".to_owned(), "child".to_owned()],
            action: Action::Trash,
            trash_directories: true,
        };
        trash_files(&server.hub(), &config).await.unwrap();

        let requests = server.requests();
        let methods_and_paths: Vec<_> = requests
            .iter()
            .map(|request| (request.method.as_str(), request.path.as_str()))
            .collect();
        assert_eq!(
            methods_and_paths,
            [
                ("GET", "/drive/v3/files/parent"),
                ("PATCH", "/drive/v3/files/parent"),
                ("GET", "/drive/v3/files/child"),
                ("PATCH", "/drive/v3/files/child"),
            ]
        );
        assert!(requests[2]
            .query_param("fields")
            .unwrap()
            .contains("explicitlyTrashed"));
        assert_eq!(requests[3].body_json(), json!({"trashed": true}));
    }
}
