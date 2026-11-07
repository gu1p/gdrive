//! `--archive --max-part-size SIZE`: the archive of a `FILE_ID` as several tar files, so that a
//! failure costs only the part it fell in, and a later run sends only what no part holds.
//!
//! ```text
//!   photos.part00001.tar   a tar of its own, of whole entries: at most SIZE
//!   photos.part00002.tar   an entry larger than SIZE is alone in a part larger than SIZE
//!   photos.parts.json      the index: which source the parts are of, what each one holds
//! ```
//!
//! The walk goes through the entries of the plan in their order. An entry that a part holds is
//! left alone, whatever maximum made it; the others fill the next part, which
//! goes up when the next entry would make it larger than the maximum. A Workspace file is exported
//! when the walk reaches it, since its size decides the part: only the exports of the part being
//! made are held.
//!
//! An entry is told by its digest (one entry's [`Manifest`]): its path and, for a file, its
//! content as the listing tells it. So a file added or edited in the source later is what the next
//! run sends. It goes into a new part, the old part stays, and extracting the parts in the order
//! of their names gives the newest content. Two rules keep that true: a new part takes a number
//! above every part that counts, also when a lower one is free, and a part holds a path only when
//! no later part holds that path with other content, so a file edited and then edited back goes
//! up again.
//!
//! What a part holds counts only when the destination proves it: the index names the part, and
//! the file of that name has the mark of exactly those entries (the digest of their digests) on
//! the bytes it was checked with, as for a whole archive ([`Existing::archives`]). A part that
//! was deleted, replaced or never finished proves nothing, and what it held goes up again. So an
//! edit of the index can only cause more uploads, never fewer.
//!
//! The order of the writes: the index names a part before it goes up, the mark comes once it is
//! checked. A run that dies at any point leaves a part that the next run recognizes, or one that
//! counts for nothing. An unfinished part gets its session continued: the next run makes the same
//! part (the same entries, and the same exports through [`ExportsCache`]) under the same number.
//!
//! Nothing in the destination is overwritten but the run's own index: a part's number skips the
//! names that files have. That is why this mode takes neither `--replace-smaller` nor
//! `--ask`: it has no conflicts to settle. Two `FILE_ID`s of one name share the names of their
//! parts, as they do with whole archives, and each finds its own by the marks.
//!
//! The parts of one `FILE_ID` go up one after the other. With `--continue-on-error`, a part or an
//! export that fails is printed and the walk goes on, so that one unreadable file does not hold up
//! everything after it; what failed is tried again by the next run.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    io::Cursor,
    mem,
    sync::{atomic::Ordering, Arc},
};

use serde::{Deserialize, Serialize};

use super::{failed, label, Kind, Migration, Place, Plan, Planned, Sending};
use crate::{
    common::{drive_file, human_size::human_size},
    files::migrate::{Error, PartsIndexError},
    transfer::{
        archive::{self, CachedExport, Entry, ExportsCache, Layout},
        relay::{self, RelayError, EXPORT_LIMIT},
        scheduler::{plural, JobContext, JobOutput, Outcome, SkipReason},
        upload::{ReaderSource, Upload, UploadTarget},
    },
};

/// What ends the name of an index: `photos.parts.json`.
const INDEX_SUFFIX: &str = ".parts.json";

/// The name a `FILE_ID` named `stem` goes by, in output, when its archive comes in parts.
pub(super) fn parts_label(stem: &str) -> String {
    label(None, &format!("{stem}.part*.tar"))
}

/// The content of the index: JSON, readable by people too (which part holds this file?).
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Index {
    /// The id of the source file or folder the parts are an archive of.
    source: String,
    parts: Vec<IndexPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexPart {
    /// The file's name in the destination folder.
    name: String,
    entries: Vec<IndexEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexEntry {
    /// The path in the part.
    path: String,
    digest: String,
}

impl IndexPart {
    /// The mark of the file that holds these entries.
    fn mark(&self) -> String {
        mark_of(&self.entries)
    }
}

/// The number in the name of a part; 0 for a name that has none.
fn number_of(name: &str) -> u64 {
    name.strip_suffix(".tar")
        .and_then(|name| name.rsplit_once(".part"))
        .and_then(|(_, number)| number.parse().ok())
        .unwrap_or_default()
}

fn mark_of(entries: &[IndexEntry]) -> String {
    archive::part_digest(entries.iter().map(|entry| entry.digest.as_str()))
}

/// The part being made.
#[derive(Default)]
struct Part {
    entries: Vec<Entry>,
    listed: Vec<IndexEntry>,
    /// The bytes of its entries.
    size: u64,
    /// How many of them are exports.
    exports: u64,
}

impl Part {
    /// Whether an entry of `size` bytes makes it larger than `max`; an empty part takes any.
    fn overflows(&self, size: u64, max: u64) -> bool {
        !self.entries.is_empty()
            && self
                .size
                .saturating_add(size)
                .saturating_add(archive::END_SIZE)
                > max
    }
}

/// The run of one archive in parts.
struct Parts<'a> {
    migration: &'a Migration,
    plan: &'a Plan,
    place: &'a Place,
    context: &'a JobContext,
    /// The largest part.
    max: u64,
    index: Index,
    index_name: String,
    /// The destination's file of the index, once there is one.
    index_id: Option<String>,
    /// A dry run said that it would create the index.
    index_told: bool,
    /// The number of the last part named.
    number: u64,
    cache: Option<ExportsCache>,
    /// What an interrupted run kept of the exports, by file id.
    kept: BTreeMap<String, CachedExport>,
    /// The exports of the part being made and of the entry after it, by file id.
    exports: BTreeMap<String, CachedExport>,
    /// Parts already there.
    there: u64,
    /// Parts this run made.
    made: u64,
    /// Exports over Drive's limit, left out.
    left_out: u64,
    /// The bytes of the parts this run made.
    bytes: u64,
    /// The last failure of a run that continues on errors: the job's own.
    failure: Option<Error>,
}

impl Migration {
    /// Makes what is missing of the archive of `plan` in the folder of `place`, in parts of at
    /// most `max` bytes.
    pub(super) async fn archive_in_parts(
        self: Arc<Self>,
        plan: Plan,
        max: u64,
        place: Place,
        context: &JobContext,
    ) -> JobOutput {
        let parts = match Parts::start(&self, &plan, &place, max, context).await {
            Ok(parts) => parts,
            Err(error) => return failed(&parts_label(&plan.stem), error).into(),
        };
        for (label, reason) in &plan.skipped {
            self.left_out(label, reason, context).await;
        }
        parts.make().await.into()
    }
}

impl<'a> Parts<'a> {
    /// Reads the index and tells which of its parts are there.
    async fn start(
        migration: &'a Migration,
        plan: &'a Plan,
        place: &'a Place,
        max: u64,
        context: &'a JobContext,
    ) -> Result<Parts<'a>, Error> {
        let index_name = format!("{}{INDEX_SUFFIX}", plan.stem);
        let (index_id, mut index) = read_index(migration, plan, place, &index_name).await?;
        let listed = mem::take(&mut index.parts);
        let mut there = 0;
        for part in listed {
            let shown = label(None, &part.name);
            let mark = part.mark();
            let proof = place.existing.archives(&part.name, &mark).next();
            if let Some(file) = proof {
                context.printer.out(format!("already-there {shown}")).await;
                let size = file.size.and_then(|size| u64::try_from(size).ok());
                context.progress.bytes(size.unwrap_or_default());
                there += 1;
                index.parts.push(part);
            } else if place.existing.by_name.contains_key(&part.name) {
                context
                    .printer
                    .err(format!(
                        "Warning: {shown} has no mark of a checked part: what it was to hold \
                         goes into a new part, and it can be deleted"
                    ))
                    .await;
            }
        }
        migration
            .tally
            .parts_there
            .fetch_add(there, Ordering::SeqCst);

        let cache = migration
            .options
            .session_dir
            .as_deref()
            .filter(|_| plan.exports() > 0 && !migration.dry_run)
            .map(|dir| ExportsCache::new(dir, &plan.manifest));
        let kept = match &cache {
            Some(cache) => cache.load().await.unwrap_or_default(),
            None => BTreeMap::new(),
        };
        // Above every part that counts: the order of the names is the order of the uploads.
        let number = index
            .parts
            .iter()
            .map(|part| number_of(&part.name))
            .max()
            .unwrap_or_default();
        Ok(Parts {
            migration,
            plan,
            place,
            context,
            max,
            index,
            index_name,
            index_id,
            index_told: false,
            number,
            cache,
            kept,
            exports: BTreeMap::new(),
            there,
            made: 0,
            left_out: 0,
            bytes: 0,
            failure: None,
        })
    }

    /// Makes the parts, and tells how the job ended.
    async fn make(mut self) -> Outcome {
        let stopped = self.walk().await;
        let failure = stopped.err().or_else(|| self.failure.take());
        // A FILE_ID that is a Workspace file whose export is too large is the job's own skip.
        let alone = self.left_out > 0 && self.made == 0 && self.there == 0;
        if !alone {
            let tally = &self.migration.tally;
            tally.skipped.fetch_add(self.left_out, Ordering::SeqCst);
        }
        if let Some(error) = failure {
            return Outcome::failed(error);
        }
        if let Some(cache) = &self.cache {
            cache.remove().await;
        }
        if self.made > 0 {
            Outcome::Transferred { bytes: self.bytes }
        } else if alone {
            Outcome::Skipped {
                reason: SkipReason::NotExportable,
            }
        } else {
            Outcome::Unchanged
        }
    }

    /// Goes through the entries that no part holds, and makes the parts of them.
    async fn walk(&mut self) -> Result<(), Error> {
        let plan = self.plan;
        // What each path extracts to: its entry in the last part that holds it.
        let held = self
            .index
            .parts
            .iter()
            .flat_map(|part| &part.entries)
            .map(|entry| (entry.path.clone(), entry.digest.clone()))
            .collect::<HashMap<_, _>>();
        let mut part = Part::default();
        for planned in &plan.entries {
            let digest = planned.digest();
            if held.get(&planned.path) == Some(&digest) {
                continue;
            }
            let entry = match self.entry(planned).await {
                Ok(Some(entry)) => entry,
                // Over Drive's limit, and left out.
                Ok(None) => continue,
                Err(error) => {
                    self.fail(&parts_label(&plan.stem), error).await?;
                    continue;
                }
            };
            let size = entry.archived_size();
            if part.overflows(size, self.max) {
                let full = mem::take(&mut part);
                self.finish(full).await?;
                self.exports.retain(|id, _| planned.is_export_of(id));
            }
            part.size += size;
            part.exports += u64::from(matches!(planned.kind, Kind::Export(_)));
            part.entries.push(entry);
            part.listed.push(IndexEntry {
                path: planned.path.clone(),
                digest,
            });
        }
        if !part.entries.is_empty() {
            self.finish(part).await?;
        }
        Ok(())
    }

    /// The entry of `planned` with its export, unless that is over Drive's limit. A dry run makes
    /// no exports: they count as empty.
    async fn entry(&mut self, planned: &Planned) -> Result<Option<Entry>, Error> {
        let Kind::Export(export) = &planned.kind else {
            return Ok(Some(planned.entry(None)));
        };
        if self.migration.dry_run {
            return Ok(Some(planned.entry(None)));
        }
        let made = match self.kept.remove(&export.id) {
            Some(made) => made,
            None => self.migration.export_of(&planned.label, export).await?,
        };
        match &made {
            CachedExport::Content(bytes) => {
                let entry = planned.entry(Some(bytes.clone()));
                self.exports.insert(export.id.clone(), made);
                Ok(Some(entry))
            }
            CachedExport::TooLarge => {
                let too_large = RelayError::ExportTooLarge {
                    limit: EXPORT_LIMIT,
                };
                self.migration
                    .note_skipped(&planned.label, &too_large, self.context)
                    .await;
                self.left_out += 1;
                Ok(None)
            }
        }
    }

    /// Makes `part` under the next free name. A failure ends the walk unless the run goes on
    /// after failures.
    async fn finish(&mut self, part: Part) -> Result<(), Error> {
        let (name, shown) = self.next_name();
        match self.send_part(&name, &shown, part).await {
            Ok(()) => Ok(()),
            Err(error) => self.fail(&shown, error).await,
        }
    }

    /// What `error` of `label` means for the walk: its end, unless the run goes on after
    /// failures. The job ends with one failure, so an earlier one is printed now.
    async fn fail(&mut self, label: &str, error: Error) -> Result<(), Error> {
        let error = Error::Item {
            label: label.to_owned(),
            source: Box::new(error),
        };
        if !self.migration.continue_on_error {
            return Err(error);
        }
        if let Some(earlier) = self.failure.replace(error) {
            let message = crate::failure_message(&earlier);
            self.context.printer.err(message).await;
            let tally = &self.migration.tally;
            tally.more_failures.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    /// The next part's name, which no file of the destination folder has, and how output shows
    /// it.
    fn next_name(&mut self) -> (String, String) {
        loop {
            self.number += 1;
            let name = format!("{}.part{:05}.tar", self.plan.stem, self.number);
            if !self.place.existing.by_name.contains_key(&name) {
                let shown = label(None, &name);
                return (name, shown);
            }
        }
    }

    async fn send_part(&mut self, name: &str, shown: &str, part: Part) -> Result<(), Error> {
        let Part {
            entries,
            listed,
            exports,
            ..
        } = part;
        let dry_run = self.migration.dry_run;
        let layout = Layout::new(entries);
        let size = layout.size();
        let mut line = format!("create {shown} ({}", human_size(size));
        if dry_run && exports > 0 {
            let documents = plural(exports, "Google Workspace file", "Google Workspace files");
            let _ = write!(line, " without the exports of {documents}");
        }
        if size > self.max {
            line.push_str(", larger than the maximum: a file is never split");
        }
        line.push(')');
        let mark = mark_of(&listed);
        if dry_run {
            self.tell_new_index().await;
            self.context.printer.out(line).await;
            self.count(size, exports);
            return Ok(());
        }

        // Named before it goes up, so that the next run recognizes it wherever this one stops.
        self.index.parts.push(IndexPart {
            name: name.to_owned(),
            entries: listed,
        });
        if let Err(error) = self.save_index().await {
            self.index.parts.pop();
            return Err(error);
        }
        self.context.printer.out(line).await;
        let sending = Sending {
            root_id: &self.plan.root_id,
            name,
            label: shown,
            layout: &layout,
            cache: self.cache.as_ref(),
            exports: (!self.exports.is_empty()).then_some(&self.exports),
            replaced: None,
        };
        let sent = self.migration.send(sending, self.place, self.context).await;
        let file = match sent {
            Ok(file) => file,
            Err(error) => {
                // Not there: a later index does not name it. After this point it may be.
                self.index.parts.pop();
                return Err(error);
            }
        };
        let id = file.id.unwrap_or_default();
        let md5 = file.md5_checksum.as_deref();
        self.migration.mark(&mark, shown, id, md5).await?;
        self.count(size, exports);
        Ok(())
    }

    fn count(&mut self, size: u64, exports: u64) {
        self.made += 1;
        self.bytes += size;
        let tally = &self.migration.tally;
        tally.parts_made.fetch_add(1, Ordering::SeqCst);
        tally.parts_bytes.fetch_add(size, Ordering::SeqCst);
        tally.converted.fetch_add(exports, Ordering::SeqCst);
    }

    /// Prints that the index is created, the first time.
    async fn tell_new_index(&mut self) {
        if self.index_id.is_none() && !mem::replace(&mut self.index_told, true) {
            let shown = label(None, &self.index_name);
            self.context.printer.out(format!("create {shown}")).await;
        }
    }

    /// Writes the index to the destination.
    async fn save_index(&mut self) -> Result<(), Error> {
        let write_error = |source: Box<dyn std::error::Error + Send + Sync>| {
            Error::PartsIndex(PartsIndexError::Access {
                doing: "write",
                name: label(None, &self.index_name),
                source,
            })
        };
        let content =
            serde_json::to_vec_pretty(&self.index).map_err(|error| write_error(Box::new(error)))?;
        let dest = &self.migration.dest;
        let target = UploadTarget {
            parents: self.place.folder.iter().cloned().collect(),
            name: Some(self.index_name.clone()),
            mime: Some(mime::APPLICATION_JSON),
            existing_file_id: self.index_id.clone(),
            ..UploadTarget::default()
        };
        let upload = Upload::new(
            dest,
            dest.retrier().state(),
            &target,
            &self.migration.options,
            None,
            content.len() as u64,
            mime::APPLICATION_JSON,
        );
        let mut source = ReaderSource::new(Cursor::new(content));
        let file = Box::pin(upload.run(&mut source))
            .await
            .map_err(|error| write_error(Box::new(error)))?;
        self.tell_new_index().await;
        if file.id.is_some() {
            self.index_id = file.id;
        }
        Ok(())
    }
}

/// The index of the archive of `plan` in the destination folder, and its file, if there is one.
/// Files of its name that are indexes of other sources stay out of it, and any other file of its
/// name fails the archive: gdrive would be sending everything again next to what it may be.
async fn read_index(
    migration: &Migration,
    plan: &Plan,
    place: &Place,
    name: &str,
) -> Result<(Option<String>, Index), Error> {
    let shown = label(None, name);
    let files = place
        .existing
        .by_name
        .get(name)
        .into_iter()
        .flatten()
        .filter(|file| file.mime_type != drive_file::MIME_TYPE_DRIVE_FOLDER);
    for file in files {
        if migration.source_ids.contains(&file.id) {
            return Err(Error::InsideSource { folder: false });
        }
        let content = relay::read_file(&migration.dest, &file.id, &shown)
            .await
            .map_err(|error| {
                Error::PartsIndex(PartsIndexError::Access {
                    doing: "read",
                    name: shown.clone(),
                    source: Box::new(error),
                })
            })?;
        let index = serde_json::from_slice::<Index>(&content)
            .map_err(|_| Error::PartsIndex(PartsIndexError::Other(shown.clone())))?;
        if index.source == plan.root_id {
            return Ok((Some(file.id.clone()), index));
        }
    }
    let index = Index {
        source: plan.root_id.clone(),
        parts: Vec::new(),
    };
    Ok((None, index))
}
