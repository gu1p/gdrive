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
//!
//! Local entries that cannot be read (a broken symlink, a file or directory without permission)
//! are failed items too, before the top folder's job: by default the first one stops the upload
//! before anything is created, as before; with `--continue-on-error` every one is reported and
//! the rest is uploaded.

#[cfg(test)]
mod tests;

use std::{
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use futures::stream;

use super::{Error, UploadDirectoryConfig};
use crate::{
    common::{
        file_tree::{self, FileTree},
        file_tree_like::Node,
        human_size::human_size,
        id_gen::IdGen,
        local_name, FileTreeLike,
    },
    files::mkdir,
    hub::Hub,
    transfer::{
        progress::JobProgress,
        scheduler::{Job, JobContext, JobKind, JobOutput, Outcome, Printer, Scheduler, Subtree},
        upload::{upload_path, UploadOptions, UploadTarget},
    },
};

/// Uploads the local folder of `config`, printing through `printer`, until every file is done
/// or `cancel` resolves.
pub(super) async fn upload_directory(
    hub: &Hub,
    config: UploadDirectoryConfig,
    options: &UploadOptions,
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
        options: options.clone(),
        print_only_id,
    });
    let unreadable = tree.unreadable.into_iter().map(|entry| {
        let kind = if entry.is_dir {
            JobKind::Metadata
        } else {
            JobKind::Media
        };
        let label = local_name::tree_path(&entry.relative_path);
        let error = Error::ReadLocal(entry.error);
        Job::done(kind, label, failed(entry.relative_path, error))
    });
    let root = uploads.folder_job(tree.root, common.parents);
    let jobs = unreadable.chain([root]).collect::<Vec<_>>();
    let report = Scheduler::new(common.transfer, printer.clone())
        .run(stream::iter(jobs), cancel)
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
    // A lost output (the ids of --print-only-id) is the first thing to fix.
    printer.flush().await.map_err(Error::WriteOutput)?;
    result
}

/// What every job of a recursive upload shares.
struct Uploads {
    hub: Hub,
    options: UploadOptions,
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
        let label = local_name::tree_path(folder.relative_path());
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

        // Two iterators: the subfolders are created while the files upload.
        let parents = vec![folder_id];
        let uploads = Arc::clone(&self);
        let folder_parents = parents.clone();
        let folders = folders
            .into_iter()
            .map(move |folder| uploads.folder_job(folder, Some(folder_parents.clone())));
        let files = files
            .into_iter()
            .map(move |file| self.file_job(file, parents.clone()));
        JobOutput::from(Outcome::Transferred { bytes: 0 })
            .follow_up(folders)
            .follow_up(files)
    }

    fn file_job(self: &Arc<Self>, file: file_tree::File, parents: Vec<String>) -> Job {
        let label = local_name::tree_path(file.relative_path());
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
        let bytes = match self.send(&file, parents, &context.progress).await {
            Ok(bytes) => bytes,
            Err(source) => return failed(path, source),
        };
        if self.print_only_id {
            context
                .printer
                .out(format!("{}: {}", path.display(), file.drive_id))
                .await;
        }

        Outcome::Transferred { bytes }
    }

    /// The one call that sends a file of a recursive upload to Drive, under its pre-generated
    /// id, into the folder `parents`: gdrive's uploader, which opens the file once the job has
    /// its slot (so at most `--jobs` files are open), measures it then, and reports the bytes
    /// Drive confirmed to `progress` after every chunk. Returns the size of the uploaded
    /// content.
    async fn send(
        &self,
        file: &file_tree::File,
        parents: Vec<String>,
        progress: &JobProgress,
    ) -> Result<u64, Error> {
        let target = UploadTarget {
            parents,
            name: file.name.clone(),
            mime: Some(file.mime_type.clone()),
            existing_file_id: None,
            convert_to: None,
            new_file_id: Some(file.drive_id.clone()),
        };
        // The uploader tells the bytes Drive holds so far; progress takes what is new.
        let confirmed = AtomicU64::new(0);
        let report = |held: u64| {
            let before = confirmed.fetch_max(held, Ordering::SeqCst);
            progress.bytes(held.saturating_sub(before));
        };
        let uploaded = upload_path(&self.hub, &file.path, &target, &self.options, Some(&report))
            .await
            .map_err(|err| Error::Transfer(Box::new(err)))?;
        let size = uploaded
            .size
            .and_then(|size| u64::try_from(size).ok())
            .unwrap_or_else(|| confirmed.load(Ordering::SeqCst));
        Ok(size)
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
