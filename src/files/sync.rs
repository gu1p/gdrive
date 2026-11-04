//! `files sync up` and `files sync down`: one-way sync between a local directory and a Drive
//! folder (milestone 2, item 6 of docs/triage/README.md; the user's guide is docs/sync.md).
//!
//! The contents of `LOCAL_DIR` and of the Drive folder are synced, not the folders themselves:
//! `LOCAL_DIR/a/b.txt` and `<folder>/a/b.txt` are the same item.
//!
//! # Phases
//!
//! 1. **Inventory** ([`inventory`], [`plan`]): both trees are walked completely, then matched by
//!    relative path with the local names of a recursive download, and every pair classified:
//!    unchanged, changed (update in place), new (create), extra (only on the destination),
//!    skipped (ambiguous names, documents without an export, shortcuts, symlinks for `down`,
//!    unreadable entries, ...). The md5 of a local file is computed only where the size matches
//!    and nothing cheaper decides, `--jobs` files at a time.
//! 2. **Transfer** ([`transfer`]): creates and updates through the scheduler, with `--jobs` and
//!    `--continue-on-error`, folders before their contents.
//! 3. **Deletion** ([`delete`]), only with `--delete`, and only when the inventory is complete
//!    (no folder that could not be listed, no incomplete Drive search, no unreadable local
//!    entry) and phase 2 ended with no failure and no cancellation: Drive extras go to the
//!    Drive trash, local extras to `LOCAL_DIR/.gdrive-trash/<time>/` (or are removed with
//!    `--delete-permanently`).
//!
//! `--dry-run` runs phase 1, which reads but never writes (no request but `files.get` of the
//! folder and `files.list`), and prints the lines the real run prints as it acts: one per item,
//! `create`, `update`, `unchanged`, `extra`, `extra->trash`, `extra->delete` or
//! `skip:<reason>`, then the path. A folder that exists on both sides has no line of its own.
//!
//! Exit status: 0 when everything planned was done; 1 when something failed or was cancelled,
//! the inventory was incomplete, or `--delete` was given and nothing could be deleted. Skipped
//! items are not failures. The summary on stderr counts every class.

mod delete;
pub(crate) mod errors;
mod inventory;
mod plan;
#[cfg(test)]
mod tests;
mod transfer;

use std::{
    fmt::Write as _,
    future::Future,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::{future::Shared, stream, FutureExt, StreamExt};

pub(crate) use delete::Mode as DeleteMode;
pub(crate) use plan::Direction;

use crate::{
    common::{drive_file, hub_helper::get_hub, id_gen::IdGen},
    files::{self, export::ExportFormats},
    hub::Hub,
    transfer::{
        scheduler::{self, Options, Printer, Report, Scheduler},
        upload::UploadOptions,
    },
};
use inventory::{Inventory, LocalTree};
use plan::{Plan, Tally};

/// A sync to run.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) direction: Direction,
    pub(crate) local_dir: PathBuf,
    pub(crate) folder_id: String,
    /// Print the plan and change nothing.
    pub(crate) dry_run: bool,
    /// `None`: extras stay (no `--delete`).
    pub(crate) delete: Option<DeleteMode>,
    /// `down`: the export formats of Google Docs files (`--export`); `None` skips them.
    pub(crate) export: Option<ExportFormats>,
    /// `down`: download files Google flagged as abuse.
    pub(crate) acknowledge_abuse: bool,
    pub(crate) transfer: Options,
    /// `up`: how files are uploaded.
    pub(crate) upload: UploadOptions,
}

pub(crate) async fn sync(config: Config) -> Result<(), errors::Sync> {
    let hub = get_hub().await.map_err(errors::Sync::Hub)?;
    let printer = Printer::stdio();
    sync_with_hub(&hub, &config, &printer, scheduler::ctrl_c()).await
}

type Cancel = Shared<futures::future::BoxFuture<'static, ()>>;

/// The three phases, printing through `printer`, until done or `cancel` resolves.
async fn sync_with_hub(
    hub: &Hub,
    config: &Config,
    printer: &Printer,
    cancel: impl Future<Output = ()> + Send + 'static,
) -> Result<(), errors::Sync> {
    let cancel: Cancel = cancel.boxed().shared();

    let (plan, local_root, problems) = phase_one(hub, config, printer, &cancel).await?;
    let tally = Tally::of(&plan.items);
    let deleting = match config.delete {
        Some(mode) if problems.is_empty() => Some(mode),
        Some(_) => {
            printer
                .err(format!(
                    "Deletion skipped: the inventory is incomplete ({}); nothing is deleted \
                    until a sync sees everything",
                    scheduler::plural(problems.len() as u64, "problem", "problems")
                ))
                .await;
            None
        }
        None => None,
    };
    if config.dry_run {
        return dry_run(&plan, &tally, deleting, config.delete, printer).await;
    }

    let acted = act(hub, config, plan, &local_root, deleting, printer, cancel).await?;
    let summary = Summary {
        created: transfer::Done::get(&acted.transfers.done.created),
        updated: transfer::Done::get(&acted.transfers.done.updated),
        unchanged: transfer::Done::get(&acted.transfers.done.unchanged),
        removed: acted.removed,
        mode: config.delete,
        kept: acted.kept,
        skipped: &tally,
        failed: acted.failed(),
        cancelled: acted.cancelled(),
    };
    printer.err(summary.line()).await;
    printer.flush().await.map_err(errors::Sync::WriteOutput)?;

    let reports = acted.reports();
    let incomplete = errors::Incomplete {
        failed: acted.failed(),
        cancelled: acted.cancelled(),
        interrupted: reports.iter().any(|report| report.interrupted),
        inventory_problems: problems.len() as u64,
        deletion_skipped: config.delete.is_some() && acted.deleted.is_none(),
        stopped: reports.iter().any(|report| report.stopped),
        output_failed: reports.iter().any(|report| report.output_failed),
    };
    if incomplete == errors::Incomplete::default() {
        Ok(())
    } else {
        Err(errors::Sync::Incomplete(incomplete))
    }
}

/// Phase 1: the inventory, the plan, and the md5 of the local files whose size matches. Returns
/// the plan, the canonical local root and the inventory's problems, which are printed already.
async fn phase_one(
    hub: &Hub,
    config: &Config,
    printer: &Printer,
    cancel: &Cancel,
) -> Result<(Plan, PathBuf, Vec<String>), errors::Sync> {
    let inventory = tokio::select! {
        biased;
        () = cancel.clone() => return Err(errors::Sync::Interrupted),
        inventory = take_inventory(hub, config, printer) => inventory?,
    };
    let mut plan = plan::build(&inventory, config.direction, config.export);
    let Inventory {
        local,
        mut problems,
        ..
    } = inventory;
    for warning in &plan.warnings {
        printer.err(warning.clone()).await;
    }
    let digests = tokio::select! {
        biased;
        () = cancel.clone() => return Err(errors::Sync::Interrupted),
        digests = hash_all(&plan.compares, config.transfer.jobs.get()) => digests,
    };
    for (label, path, error) in plan.resolve(digests) {
        let error = errors::Item {
            label,
            source: errors::ItemError::Hash(path, error),
        };
        let message = crate::failure_message(&error);
        printer.err(message.clone()).await;
        problems.push(message);
    }
    Ok((plan, local.root, problems))
}

/// What phases 2 and 3 did.
struct Acted {
    transfers: Arc<transfer::Transfers>,
    transferred: Report,
    /// The report of phase 3, if it ran.
    deleted: Option<Report>,
    removed: u64,
    /// Extras left in place.
    kept: u64,
}

impl Acted {
    fn reports(&self) -> Vec<&Report> {
        [Some(&self.transferred), self.deleted.as_ref()]
            .into_iter()
            .flatten()
            .collect()
    }

    fn failed(&self) -> u64 {
        self.reports()
            .iter()
            .map(|report| report.files.failed + report.folders.failed)
            .sum()
    }

    fn cancelled(&self) -> u64 {
        self.reports()
            .iter()
            .map(|report| report.files.cancelled + report.folders.cancelled)
            .sum()
    }
}

/// Phase 2, and phase 3 when `deleting` and phase 2 ended without a failure.
async fn act(
    hub: &Hub,
    config: &Config,
    mut plan: Plan,
    local_root: &Path,
    deleting: Option<DeleteMode>,
    printer: &Printer,
    cancel: Cancel,
) -> Result<Acted, errors::Sync> {
    let ids = match config.direction {
        Direction::Up => new_ids(hub, transfer::new_ids(&plan.items)).await?,
        Direction::Down => Vec::new(),
    };
    let transfers = transfer::Transfers::new(
        hub,
        config.direction,
        local_root.to_path_buf(),
        config.upload.clone(),
        config.acknowledge_abuse,
        ids,
    );
    let items = std::mem::take(&mut plan.items);
    let source = transfers.source(items, plan.remote_root.clone(), printer.clone());
    let transferred = Scheduler::new(config.transfer, printer.clone())
        .run(source, cancel.clone())
        .await;

    let mut acted = Acted {
        transfers,
        transferred,
        deleted: None,
        removed: 0,
        kept: 0,
    };
    match deleting {
        Some(mode) if acted.transferred.check().is_ok() => {
            let total = plan.extras.len() as u64;
            let (report, removed) = delete::run(
                hub,
                plan.extras,
                mode,
                local_root,
                config.transfer,
                printer,
                cancel,
            )
            .await;
            acted.removed = removed;
            acted.kept = total
                - removed
                - (report.files.failed + report.folders.failed)
                - (report.files.cancelled + report.folders.cancelled);
            acted.deleted = Some(report);
        }
        _ => {
            if deleting.is_some() {
                printer
                    .err(
                        "Deletion skipped: not everything was transferred; nothing is deleted \
                        until a sync completes",
                    )
                    .await;
            }
            for extra in &plan.extras {
                printer.out(format!("extra {}", extra.label)).await;
            }
            acted.kept = plan.extras.len() as u64;
        }
    }
    Ok(acted)
}

/// Prints the plan as the real run would, and changes nothing.
async fn dry_run(
    plan: &Plan,
    tally: &Tally,
    deleting: Option<DeleteMode>,
    requested: Option<DeleteMode>,
    printer: &Printer,
) -> Result<(), errors::Sync> {
    let mut lines = Vec::new();
    plan::lines(&plan.items, &mut lines);
    let action = deleting.map_or("extra", DeleteMode::action);
    lines.extend(
        plan.extras
            .iter()
            .map(|extra| format!("{action} {}", extra.label)),
    );
    for line in lines {
        printer.out(line).await;
    }
    let extras = plan.extras.len() as u64;
    let (removed, kept) = if deleting.is_some() {
        (extras, 0)
    } else {
        (0, extras)
    };
    let summary = Summary {
        created: tally.created,
        updated: tally.updated,
        unchanged: tally.unchanged,
        removed,
        mode: requested,
        kept,
        skipped: tally,
        failed: 0,
        cancelled: 0,
    };
    printer
        .err(format!("Dry run, nothing was changed. {}", summary.line()))
        .await;
    printer.flush().await.map_err(errors::Sync::WriteOutput)
}

/// The counts of a sync for its last line on stderr.
struct Summary<'a> {
    created: u64,
    updated: u64,
    unchanged: u64,
    removed: u64,
    mode: Option<DeleteMode>,
    kept: u64,
    skipped: &'a Tally,
    failed: u64,
    cancelled: u64,
}

impl Summary<'_> {
    /// `Summary: 2 created, 1 updated, 10 unchanged, 1 moved to the trash, 0 extra kept,
    /// 2 skipped (1 document, 1 shortcut), 0 failed, 0 cancelled`.
    fn line(&self) -> String {
        let removed = match self.mode {
            Some(DeleteMode::Permanently) => "deleted",
            Some(DeleteMode::Trash) | None => "moved to the trash",
        };
        let mut skipped = format!("{} skipped", self.skipped.skipped_total());
        if !self.skipped.skipped.is_empty() {
            let reasons = self
                .skipped
                .skipped
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect::<Vec<_>>();
            let _ = write!(skipped, " ({})", reasons.join(", "));
        }
        format!(
            "Summary: {} created, {} updated, {} unchanged, {} {removed}, {} extra kept, \
            {skipped}, {} failed, {} cancelled",
            self.created,
            self.updated,
            self.unchanged,
            self.removed,
            self.kept,
            self.failed,
            self.cancelled,
        )
    }
}

/// The Drive folder and the local directory, both complete.
async fn take_inventory(
    hub: &Hub,
    config: &Config,
    printer: &Printer,
) -> Result<Inventory, errors::Sync> {
    use errors::Sync as E;

    let root = files::info::get_file(hub, &config.folder_id)
        .await
        .map_err(|err| E::GetFolder(Box::new(err)))?;
    if !drive_file::is_directory(&root) {
        return Err(E::NotAFolder(config.folder_id.clone()));
    }
    if root.trashed == Some(true) {
        return Err(E::FolderTrashed(config.folder_id.clone()));
    }
    let (local_root, exists) = local_root(config).await?;

    let mut problems = Vec::new();
    let listing_jobs = config.transfer.metadata_jobs.get();
    let remote = inventory::remote(hub, root, listing_jobs, printer, &mut problems).await;
    let children = if exists {
        Some(inventory::local(local_root.clone(), printer, &mut problems).await?)
    } else {
        None
    };
    Ok(Inventory {
        remote,
        local: LocalTree {
            root: local_root,
            children,
        },
        problems,
    })
}

/// The canonical local root, and whether it exists. `up` needs it; a real `down` creates it,
/// a dry run only pretends it is empty.
async fn local_root(config: &Config) -> Result<(PathBuf, bool), errors::Sync> {
    use errors::Sync as E;

    let path = &config.local_dir;
    let exists = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => return Err(E::LocalNotADirectory(path.clone())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => return Err(E::CanonicalizeLocalDir(path.clone(), err)),
    };
    if !exists {
        match config.direction {
            Direction::Up => return Err(E::LocalDirMissing(path.clone())),
            Direction::Down if config.dry_run => {
                let absolute = std::path::absolute(path)
                    .map_err(|err| E::CanonicalizeLocalDir(path.clone(), err))?;
                return Ok((absolute, false));
            }
            Direction::Down => tokio::fs::create_dir_all(path)
                .await
                .map_err(|err| E::CreateLocalDir(path.clone(), err))?,
        }
    }
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|err| E::CanonicalizeLocalDir(path.clone(), err))?;
    Ok((canonical, true))
}

/// `count` ids from `files.generateIds`, for the new files and folders of `up`.
async fn new_ids(hub: &Hub, count: usize) -> Result<Vec<String>, errors::Sync> {
    let mut ids = IdGen::new(hub);
    let mut new = Vec::with_capacity(count);
    for _ in 0..count {
        new.push(ids.next().await.map_err(errors::Sync::GenerateIds)?);
    }
    Ok(new)
}

/// The md5 of each local file of `compares`, `jobs` files at a time, in order.
async fn hash_all(
    compares: &[(PathBuf, md5::Digest)],
    jobs: usize,
) -> Vec<Option<io::Result<md5::Digest>>> {
    let hashes = compares.iter().map(|(path, _)| {
        let path = path.clone();
        async move {
            tokio::task::spawn_blocking(move || md5_of(&path))
                .await
                .unwrap_or_else(|err| Err(io::Error::other(err)))
        }
    });
    stream::iter(hashes)
        .buffered(jobs.max(1))
        .map(Some)
        .collect()
        .await
}

/// The md5 of the file at `path`, read in 1 MiB pieces.
fn md5_of(path: &Path) -> io::Result<md5::Digest> {
    let mut file = std::fs::File::open(path)?;
    let mut context = md5::Context::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(context.compute()),
            Ok(read) => context.consume(&buffer[..read]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}
