//! `files upload --recursive` through the scheduler (milestone 2, item 2 of
//! docs/triage/README.md).
//!
//! The local tree is walked first ([`FileTree::from_path`], with its symlink and cycle policy),
//! which also takes a pre-generated Drive id for every folder and file (`files.generateIds`).
//! Then one job creates the top folder; a folder's job, once Drive created the folder, releases
//! the jobs of its files and subfolders. So every folder exists before anything is uploaded
//! into it, folders are created breadth-first, and files of different folders upload side by
//! side, `--jobs` at a time; folder creations take the metadata slots.
//!
//! A folder that cannot be created fails, and everything below it counts as cancelled. A file
//! is opened only when its job starts, so the open files are bounded by `--jobs` as well.

#[cfg(test)]
mod tests;

use std::{fs, future::Future, path::PathBuf, sync::Arc};

use futures::stream;

use super::{upload_file, Error, UploadDirectoryConfig};
use crate::{
    common::{
        delegate::UploadDelegateConfig,
        file_tree::{self, FileTree},
        file_tree_like::Node,
        human_size::human_size,
        id_gen::IdGen,
        FileTreeLike,
    },
    files::mkdir,
    hub::Hub,
    transfer::scheduler::{
        Job, JobContext, JobKind, JobOutput, Outcome, Printer, Scheduler, Subtree,
    },
};

/// Uploads the local folder of `config`, printing through `printer`, until every file is done
/// or `cancel` resolves.
pub(super) async fn upload_directory(
    hub: &Hub,
    config: UploadDirectoryConfig,
    delegate_config: &UploadDelegateConfig,
    printer: &Printer,
    cancel: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    let UploadDirectoryConfig { file_path, common } = config;
    let mut ids = IdGen::new(hub);
    let tree = FileTree::from_path(&file_path, &mut ids)
        .await
        .map_err(Error::CreateFileTree)?;
    let tree_info = tree.info();
    let print_only_id = common.print_only_id;

    if !print_only_id {
        printer
            .out(format!(
                "Found {} files in {} directories with a total size of {}",
                tree_info.file_count,
                tree_info.folder_count,
                human_size(tree_info.total_file_size),
            ))
            .await;
    }

    let uploads = Arc::new(Uploads {
        hub: hub.clone(),
        delegate_config: delegate_config.clone(),
        print_only_id,
    });
    let root = uploads.folder_job(tree.root, common.parents);
    let report = Scheduler::new(common.transfer, printer.clone())
        .run(stream::iter([root]), cancel)
        .await;

    let result = report.check().map_err(Error::Incomplete);
    if result.is_ok() && !print_only_id {
        printer
            .out(format!(
                "Uploaded {} files in {} directories with a total size of {}",
                tree_info.file_count,
                tree_info.folder_count,
                human_size(tree_info.total_file_size),
            ))
            .await;
    }
    printer.err(report.summary()).await;
    printer.flush().await;
    result
}

/// What every job of a recursive upload shares.
struct Uploads {
    hub: Hub,
    delegate_config: UploadDelegateConfig,
    print_only_id: bool,
}

impl Uploads {
    /// The job that creates `folder` in `parents` (the user's `--parent`, or none for My Drive)
    /// and then releases its contents.
    fn folder_job(
        self: &Arc<Self>,
        folder: file_tree::Folder,
        parents: Option<Vec<String>>,
    ) -> Job {
        let label = folder.relative_path().display().to_string();
        let subtree = subtree(&folder);
        let uploads = Arc::clone(self);
        Job::new(JobKind::Metadata, label, move |context| async move {
            uploads.create_folder(folder, parents, &context).await
        })
        .with_subtree(subtree)
    }

    async fn create_folder(
        self: Arc<Self>,
        folder: file_tree::Folder,
        parents: Option<Vec<String>>,
        context: &JobContext,
    ) -> JobOutput {
        let path = folder.relative_path().to_path_buf();
        let id = folder.info.drive_id.clone();

        if !self.print_only_id {
            context
                .printer
                .out(format!(
                    "Creating directory '{}' with id: {id}",
                    path.display()
                ))
                .await;
        }
        let mkdir = mkdir::Config {
            id: Some(id.clone()),
            name: folder.info.name.clone(),
            parents,
            print_only_id: false,
        };
        let created = match mkdir::create_directory(&self.hub, &mkdir).await {
            Ok(created) => created.id.ok_or(Error::DriveFolderMissingId),
            Err(err) => Err(Error::Mkdir(Box::new(err))),
        };
        let folder_id = match created {
            Ok(folder_id) => folder_id,
            Err(source) => return failed(path, source).into(),
        };
        if self.print_only_id {
            context
                .printer
                .out(format!("{}: {id}", path.display()))
                .await;
        }

        let mut files = Vec::new();
        let mut folders = Vec::new();
        for child in folder.children {
            match child {
                Node::File(file) => files.push(file),
                Node::Folder(folder) => folders.push(folder),
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        folders.sort_by(|a, b| a.info.name.cmp(&b.info.name));

        let parents = vec![folder_id];
        let follow_ups = files
            .into_iter()
            .map(|file| self.file_job(file, parents.clone()))
            .chain(
                folders
                    .into_iter()
                    .map(|folder| self.folder_job(folder, Some(parents.clone()))),
            )
            .collect();
        JobOutput {
            outcome: Outcome::Transferred { bytes: 0 },
            follow_ups,
        }
    }

    fn file_job(self: &Arc<Self>, file: file_tree::File, parents: Vec<String>) -> Job {
        let label = file.relative_path().display().to_string();
        let size = file.size;
        let uploads = Arc::clone(self);
        Job::new(JobKind::Media, label, move |context| async move {
            uploads.upload(file, parents, &context).await.into()
        })
        .with_size(size)
    }

    async fn upload(
        &self,
        file: file_tree::File,
        parents: Vec<String>,
        context: &JobContext,
    ) -> Outcome {
        let path = file.relative_path().to_path_buf();

        // Opened once the job has its slot: at most `--jobs` files are open.
        let os_file = match fs::File::open(&file.path) {
            Ok(os_file) => os_file,
            Err(err) => return failed(path, Error::OpenFile(file.path.clone(), err)),
        };
        if !self.print_only_id {
            context
                .printer
                .out(format!(
                    "Uploading file '{}' with id: {}",
                    path.display(),
                    file.drive_id
                ))
                .await;
        }
        if let Err(source) = self.upload_file(os_file, &file, parents).await {
            return failed(path, source);
        }
        if self.print_only_id {
            context
                .printer
                .out(format!("{}: {}", path.display(), file.drive_id))
                .await;
        }
        context.progress.bytes(file.size);

        Outcome::Transferred { bytes: file.size }
    }

    /// The one call that sends a file of a recursive upload to Drive, under its pre-generated
    /// id, into the folder `parents`.
    async fn upload_file(
        &self,
        os_file: fs::File,
        file: &file_tree::File,
        parents: Vec<String>,
    ) -> Result<(), Error> {
        upload_file(
            &self.hub,
            os_file,
            Some(file.drive_id.clone()),
            file.info(Some(parents)),
            &self.delegate_config,
        )
        .await
        .map_err(|err| Error::Upload(Box::new(err)))?;
        Ok(())
    }
}

fn failed(path: PathBuf, source: Error) -> Outcome {
    Outcome::failed(Error::RecursiveItem {
        path,
        source: Box::new(source),
    })
}

/// The files and folders below `folder`.
fn subtree(folder: &file_tree::Folder) -> Subtree {
    let mut subtree = Subtree::default();
    let mut folders = vec![folder];
    while let Some(folder) = folders.pop() {
        for child in &folder.children {
            match child {
                Node::File(_) => subtree.files += 1,
                Node::Folder(folder) => {
                    subtree.folders += 1;
                    folders.push(folder);
                }
            }
        }
    }
    subtree
}
