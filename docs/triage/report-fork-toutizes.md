# Fork analysis: toutizes/gdrive (parallel transfers + sync)

- Source: `gdrive-data/forks/toutizes` (HEAD `ce70446`, 2025-04-12). 40 commits on top of upstream `8551556`, all by Matthieu Devin, written 2025-03-31 to 2025-04-12.
- Diff vs upstream: 17 files, +1674/-1569. New: `src/files/tasks.rs` (69 LOC), `src/common/drive_item.rs` (455), `src/common/disk_item.rs` (271), `src/common/drive_names.rs` (92). Rewritten: `src/files/download.rs`, `src/files/upload.rs`, `src/files/import.rs`, `src/files/export.rs`, `src/main.rs`. Removed: `file_tree.rs`, `file_tree_drive.rs`, `id_gen.rs` (`3b43ce0`). Dependencies are unchanged except `anyhow` and `async-trait` (still hyper 0.14 and the prasmussen google-drive3 git fork).
- Unless stated otherwise, line numbers refer to files at fork HEAD.
- Work dir: `gdrive-data/analysis/tmp-fork-tout/` holds the copy (`src-copy/`), `target/`, `build_log.txt`, `clippy_log.txt`, and `probe/`: offline probes with a fake account under `fakehome/`, plus two small Rust programs.

---

## 1. Build on rustc 1.99.1

| Item | Result |
|---|---|
| Command | `CARGO_TARGET_DIR=tmp-fork-tout/target cargo build --release` in `tmp-fork-tout/src-copy` |
| Result | **Success**, exit 0, 1m03s wall time (146 crates; the registry and git caches were already warm from the upstream build). |
| Warnings | 0 rustc warnings. `cargo clippy --release` reports 110 warnings, mostly style (34 large `Err` variants, 25 needless borrows, 14 needless `return`, and others). |
| Binary | `target/release/gdrive`, 11.4 MB. `gdrive version` reports `3.9.1 / Commit: ce70446... / Rust: 1.99.1`. |
| Lockfile | Upgraded to lock format v4. Bumps `syn` 2.0.100, `proc-macro2`, `quote` and `anyhow` for async-trait. Nothing else changed. |

The fork still sits on the old stack (hyper 0.14, google-drive3 5.0.2-beta from the git fork, yup-oauth2 8), so it builds for the same reason upstream does: everything is pinned.

---

## 2. Concurrency architecture

### 2.1 Components
- **`DriveTask` trait** (`src/files/tasks.rs:11-15`) has two methods: `async fn process(&self)` (returns nothing) and `fn get_status() -> DriveTaskStatus{Pending|Completed(usize)|Failed(String)}`. `get_status` is never called anywhere (grep: only the trait and its two impls).
- **`TaskManager<T>`** (`tasks.rs:17-69`) holds `Mutex<Vec<Arc<Box<T>>>>` (every task ever added), `Mutex<Vec<JoinHandle>>`, and one `tokio::Semaphore(num_workers)`.
  - `add_task` (`:32-51`) calls **`tokio::spawn` immediately for every item**. The spawned future first `acquire()`s a permit, then runs `process()`. So the "queue" is the semaphore's FIFO wait list. It is **unbounded**: every discovered item becomes a parked tokio task right away, and every task is kept in `tasks` until the run ends (O(total items) memory).
  - `wait` (`:53-68`) loops: drain the handle vec, then `join_all`, until no new handles appear. This terminates correctly because a parent calls `add_task` for its children *inside* `process()`, before its own handle completes. `JoinError`s (panics) are discarded.
- **Worker count**: a global `--workers N` (default 1, `main.rs:32-34`) sets **both** the tokio runtime `worker_threads(N)` (`main.rs:481-485`, from `b514372`) and the semaphore permits = **`2 * N`** (`main.rs:644`, `:686`). The default is therefore **2 concurrent tasks**, and directory listings count against those same permits. `--workers 0` panics: "Worker threads cannot be set to 0" (verified).

### 2.2 Download flow ("full parallel download, without a materialized tree", `fb7edc4`)
`DownloadTask::download` (`src/files/download.rs:142-151`) dispatches on `DriveItemDetails`:
- **Directory** (`:153-195`): holds a permit, creates the local dir, `files.list` of that one folder (`drive_item.rs:119-154`, pages of 1000, serial pages), then `add_task` for every child. Children are spawned and wait for permits. The task then optionally runs the sync deletion and **releases its permit without waiting for its children**. No tree is ever built: the listing of a subfolder is just another task in the same FIFO.
- **File** (`:197-236`): optional local md5 check, then `files.get alt=media` (`drive_item.rs:233-264`), streamed to `path.with_extension("incomplete")`, stream md5 checked, then rename (`disk_item.rs:173-210`).
- **Shortcut** (`:238-246`): `files.get` on the target, then `add_task(target)` with the same local path.

So listing does overlap with transfers. Because the tokio semaphore is fair (FIFO), the order is roughly breadth-first in discovery order. A folder listing queued behind large file downloads waits for a free permit. There is no separate listing pool and no priority.

### 2.3 Upload flow
`UploadTask` (`src/files/upload.rs:86-337`) mirrors the download flow. A directory task either creates the Drive folder (`drive_item.rs:342-375`) or lists the existing one once, builds `name -> Vec<DriveItem>` (`upload.rs:193-202`), and spawns one child per local entry, **handing each child its pre-matched existing Drive items**. Children never re-list. Each file task then does upload, skip, or update (`:234-294`). Sync deletions run sequentially inside the directory task (`:219-228`).

### 2.4 Diagram
```
main(): tokio multi-thread runtime, worker_threads = W (--workers, default 1)
  |
  | drive_names::resolve("/a/b" | id)       serial files.list per path component
  v
TaskManager  [Semaphore permits = 2W, FIFO]  [tasks: Vec<Arc<Task>> (all, kept)]  [handles: Vec<JoinHandle>]
  |  add_task(root) -> tokio::spawn{ acquire permit ; task.process() }
  v
+---------------------- one spawned tokio task per Drive/local item (spawned eagerly) ---------------------+
| Dir task   [permit]  mkdir  ->  files.list(folder)  ->  add_task(child) x N  -> (--sync: delete extras) |
|                      ... returns, releases permit; children are already queued on the semaphore        |
| File task  [permit]  md5(local)? -> files.get?alt=media -> write NAME.incomplete -> md5(stream) -> rename |
|            (upload)  md5 match? skip : files.update | files.create  via upload_resumable(chunk 32 MiB)   |
| Shortcut   [permit]  files.get(target) -> add_task(target)                                              |
+---------------------------------------------------------------------------------------------------------+
  ^  wait(): loop { drain handles; join_all } until empty           all HTTP: ONE Arc<Hub> -> one hyper 0.14
  |                                                                 client, ALPN h2 -> requests multiplexed
report_stats(): sum per-task stats (errors never counted)           on one TCP conn (5 MiB conn window,
-> stdout ; return Ok(())  => exit code 0 even if tasks failed      hyper-0.14 proto/h2/client.rs:40-41)
```

### 2.5 Errors, abort and cancellation
- A failed task prints `Err: <msg>` to **stdout** and stores `Failed(String)` (`download.rs:255-264`, `upload.rs:327-336`). Nothing reads it. `DownloadStats.num_errors` is initialized to 0 and never incremented (`download.rs:137,191,232`). The summary therefore always says `errors: 0`. Upload keeps no stats (`UploadStats` is commented out, `upload.rs:94,119-125`).
- `download()` and `upload()` return `Ok(())` regardless (`download.rs:61-64`, `upload.rs:72-75`), so the **exit code is 0 when files failed**. Only setup errors (resolve, hub) produce exit 1.
- A failure never aborts the other tasks. There is no cancellation and no Ctrl-C handling, so `.incomplete` files are left behind. If a directory listing fails, that whole subtree is silently skipped apart from the one `Err:` line.
- Regression within the fork: the first parallel version (`b002a28`, `tasks.rs` `CopyTask`) did count successes and failures in `report_stats`. That was lost in the `DriveTask` refactor (`b37eb96`/`73777a8`).

### 2.6 Retries, backoff and rate limits
- **Only resumable uploads (and mkdir) retry.** Each upload creates its own `UploadDelegate` (`drive_item.rs:206`, `:304`). `delegate.rs` is unchanged from upstream: it retries on `5xx` and `429` only (`delegate.rs:119-121`), using exponential-backoff 1.1 (1s to 60s, jitter 0.3), with `max_retries: 100000` for uploads (`upload.rs:45-49`), which is effectively infinite.
- **403 `userRateLimitExceeded`/`rateLimitExceeded` is not retried** (it goes to `abort()`). Downloads (`files.get alt=media`), listings, metadata gets, updates of metadata and deletes use the default delegate, so they have **no retries at all**. A transient error mid-stream fails that file, with no Range resume.
- **There is no global rate limiter, no shared or coordinated backoff and no adaptive concurrency.** Each task backs off independently. The fork avoids rate limits only by having a low default (2 permits). With a large `--workers`, a burst of small-file creates hits per-user write throttling and fails the affected files permanently, behind exit code 0.
- **Blocking I/O on runtime threads.** Downloads write with `std::fs::File` inside the async loop (`disk_item.rs:183-191`). The md5 pre-check reads the whole local file synchronously (`disk_item.rs:225-258`). The resumable upload loop in google-apis-common does `seek` + `read_to_end` of a whole chunk synchronously (`~/.cargo/git/checkouts/google-apis-rs-*/1f75444/google-apis-common/src/lib.rs:677-687`; google-apis-common 8.0.0 still does the same at `lib.rs:687`). Tying `worker_threads` to `--workers` partly hides this. At the default of 1 thread, disk I/O, md5 and TLS all serialize on one thread.

### 2.7 Upload chunking per task
- Each file gets a fresh delegate. The resumable session URL is held in memory only (`store_upload_url`/`upload_url`). There is no cross-task state and no persistence, so a restart re-uploads from byte 0. Chunks are sequential per file (Drive protocol). A failed chunk is retried from its start offset.
- Chunk size comes from `--chunk-size` (default 32 MiB). The library buffers each chunk fully in a `Vec`, so peak memory is about `2W x chunk_size` (for example `--workers 8` gives 16 x 32 MiB = 512 MiB).
- **Change vs upstream:** the fork always calls `upload_resumable` (`drive_item.rs:216`, `:313`). Upstream used a single-request multipart `upload()` for files no larger than the chunk size (`gdrive-src/src/files/upload.rs:242-245`). The fork therefore needs at least 2 requests per small file, which hurts exactly the many-small-files case that parallelism is meant to speed up.
- **Probable 0-byte file regression:** with `content_length = 0` the library computes `last = start + 0 - 1` (u64 wrap in release, `google-apis-common/src/lib.rs:689-692`). Upstream sent such files via multipart. This was not verified against the live API.

---

## 3. Sync semantics and data-loss risks

### 3.1 `files download --sync` (`ExistingFileAction::SyncLocal`, first sketched in `61f368d`)
- Per Drive folder: list the children, spawn their tasks, then **delete every local regular file or symlink in that folder whose name is not among the listed children**. Directories are kept and print `keep (not a file or symlink)` (`download.rs:163-184`, `disk_item.rs:122-155`). Symlink deletion was added in `4e64fcc` "Also delete symlinks".
- Comparison is **md5 only**: the local file is hashed fully on every run (`disk_item.rs:225`). There is no size or mtime fast path, and local mtimes are not set from Drive.
- **Google Docs/Sheets/Slides are skipped** with `ignored. Unknown file type` (`drive_item.rs:94-108`, `:147-149`). They are not exported.
- **Dry run** (`-n`): prints `create directory`, `download to` and `delete` without acting. For a folder that does not exist locally yet, `delete_extra_local_files` then fails on `read_dir` and prints a spurious `Err:`.
- `--overwrite` and `--sync` are mutually exclusive (`main.rs:613-617`).

### 3.2 `files upload --sync` (`ExistingDriveFileAction::Sync`, `fab99d2`)
- Per existing Drive folder: Drive children whose names do not match a local entry are removed with **`files.delete`, which is permanent and bypasses the trash** (`upload.rs:219-228` calling `drive_item.rs:329-340`). This includes **folders, and with them their whole subtree**, although `--help` says "gdrive never deletes a Google Drive folder, only Google Drive files" (`main.rs` Upload `sync` doc). Docs and shortcuts survive only because the listing filters them out.
- Files with the same md5 are skipped. Otherwise `files.update` replaces the content and keeps the file id, sharing and revisions (`upload.rs:279-290`, `drive_item.rs:282-327`). That is the useful "replace" semantics (`e6ed6df`). Duplicate names on Drive cause a skip.

### 3.3 Data-loss and integrity risks, most severe first
1. **Silent file corruption under parallelism (temp-name collision).** The temp file name is `path.with_extension("incomplete")` (`disk_item.rs:181`), so siblings `report.pdf` and `report.docx` share `report.incomplete`, and `archive.tar.gz` becomes `archive.tar.incomplete`. The md5 is computed over the *stream*, not the file on disk. A simulation of the exact `overwrite()` logic (`tmp-fork-tout/probe/race.rs`) produced this:
   `report.pdf task: Ok`, but `report.pdf` holds **100% of report.docx's bytes**, and `report.docx task: Err(rename ... No such file)`. The scheme is inherited from upstream, where downloads were serial and harmless. PR #68 has the same issue.
2. **Duplicate names in a Drive folder** (common on Drive) lead to two concurrent tasks writing the same path and the same temp file, with the same corruption. `--help` claims "It is an error if a folder contains multiple files with the same name", but nothing checks for it (`download.rs:168-176`).
3. **Path traversal plus sync deletion.** Drive names are joined unsanitized (`disk_item.rs:75`). `PathBuf::join` with an absolute name replaces the base path: a probe shows `./dl/MyFolder`.join(`"/home/gp"`) = `/home/gp`, and `"../.."` escapes upward. A shared folder containing a subfolder named `/home/<user>` or `../..` makes `--sync` **delete every regular file in that outside directory** that is not in the Drive folder. Upstream joins unsanitized too (`gdrive-src/src/common/file_tree_drive.rs:161`), but there it only causes stray writes. Sync turns it into deletion.
4. **`upload --sync` permanently deletes Drive folders and files** (see 3.2). There is no trash and no confirmation, and the documentation says folders are never deleted.
5. **Default download mode (no flags) overwrites locally modified files.** The `Abort` branch prints `file exists, skipped.` but does not return, so a differing md5 falls through to the download (`download.rs:207-212`). Upstream refused with "File already exists, use --overwrite" (`gdrive-src/src/files/download.rs:345-346`).
6. **The keep-set is built from a filtered listing.** Anything `for_drive_file` rejects counts as "not on Drive", and a local file with the same name is deleted. That covers Google Docs, items with more than one parent (`drive_item.rs:401-413`), and **every shortcut inside folders**, because `list_files` does not request `shortcutDetails` (`src/files/list.rs:105-108`), so `get_file_shortcut_details` fails (`drive_item.rs:440-448`).
7. **Deletion races with the children's temp files.** Deletion runs right after spawning the children (`download.rs:178`). With W ≥ 2, an in-flight `X.incomplete` can be unlinked, and that child then fails at rename. This causes spurious failures rather than data loss.
8. **Symlink deletion** (`4e64fcc`): user symlinks inside a synced folder are unlinked (targets are untouched).
9. **Name mismatches.** `to_string_lossy` mangles non-UTF-8 names, and macOS NFD vs Drive NFC normalization differs. Either way the name does not match, so the file is deleted (download) or deleted and re-uploaded (upload).

---

## 4. User-visible CLI changes and 3.x compatibility

| Area | Upstream 3.9.1 | Fork | Breaks 3.x scripts? |
|---|---|---|---|
| Global flags | none | `--workers N` (default 1, giving 2 concurrent tasks) and `-n/--dry-run` (the commit says `--dry_run`, but clap renders `--dry-run`). Not `global = true`, so they must come **before** the subcommand: `gdrive files download --workers 4 x` gives "Found argument '--workers' which wasn't expected" (verified). | No (additive), but awkward |
| `download <file_id>` | id only | `<DRIVE FILE OR FOLDER>`: an id, or a path if it starts with `/` (`drive_names.rs:282-326`). Only `'root' in parents` is searched, so no shared drives by path. With duplicates the first match wins silently. | No for ids |
| `download --stdout` | boolean flag, clean output | `--stdout <true\|false>` (`main.rs:229`). A bare `--stdout` is a parse error (verified). `--stdout false` *also* streams to stdout (`main.rs:627`). Progress lines (`drive_item.rs:250`) and the "Download finished..." summary (`download.rs:79`) are printed **into the same stdout stream**. | **Yes** |
| `download` existing file | error and exit 1 unless `--overwrite` | prints "skipped", then overwrites if the content differs (bug, 3.3 item 5), exit 0 | **Yes** (semantics) |
| `download` new flags | `--overwrite`, `--recursive`, `--follow-shortcuts`, `--destination` | adds `-r`, `-f/--overwrite`, `-s/--sync`. `--follow-shortcuts` is parsed but never read (`DownloadOptions.follow_shortcuts` unused): top-level shortcuts are always followed and nested ones always ignored. | Partly |
| `download` output | "Downloading X" / "Successfully downloaded X" | `<drive path> (<id>): download to <local>: 0.42s`, plus a multi-line summary with "duratin" and "bandwidth: x Mb/s". Errors go to stdout as `Err: ...`. Exit 0 on per-file failures. | **Yes** (parsers, exit codes) |
| `upload [file_path]` | optional; stdin when omitted (PR #41, merged) | **required** (`main.rs:254`). Stdin upload removed (verified: "required arguments were not provided"). | **Yes** |
| `upload --parent <ID>...` | repeatable Vec | `--destination <ID or /path>` with alias `--parent`, single value. **The default `/` is broken**: `gdrive files upload hello.txt` gives `Error: /: Empty path`, exit 1 (verified offline with a fake account). `resolve("/")` has no components and falls through to the error (`drive_names.rs:306-325`). Works: `--destination root` or `""`. | **Yes** (any upload without `--parent` fails) |
| `upload` existing name | always creates a new file | default **skip** (prints, exit 0). `-f` updates in place; `-s` updates and deletes extras. | **Yes** (silent no-op) |
| `upload -r` | always creates a new folder | merges into an existing same-name folder | Yes (semantics; arguably a fix for #18/#69) |
| `upload --print-only-id` | prints only the id | accepted but ignored (`upload.rs:38` `_print_only_id`). Output is `<local>: upload to <drive path> (<id>): 1.2s`. | **Yes** (common `id=$(gdrive files upload --print-only-id ...)` pattern) |
| `files import` | converts to a Google Doc type; `--parent` optional | `--parent` **required** (`main.rs:400`). **No conversion any more**: it uploads with the guessed mime (`import.rs:15-29`). `--print-only-id` ignored. Output has no trailing newline. | **Yes** |
| `files export` | works | **broken** for Google Docs: `for_drive_file` rejects files without md5 as "Unknown file type" (`drive_item.rs:94-108` via `export.rs:65`). It also passes the *source* mime to `files.export` (`drive_item.rs:271`). | **Yes** |
| Others (account, list, info, update, delete, mkdir, rename, mv, copy, permissions, drives) | unchanged | unchanged | No |

---

## 5. Code quality and design comparison

### 5.1 Quality verdict: a working personal prototype, not mergeable
- **Tests: none** (0 `#[test]`). No README or docs updates. Two `// TODO: review` markers sit on CLI parameters (`main.rs:231`, `:294`).
- **Dead code:** `DriveTask::get_status`, `DriveTaskStatus::Completed(n)` payloads (written, never read), `UploadStats`, `DownloadOptions.follow_shortcuts`, `_print_only_id`, `DriveItem._parent`, and `handle_error` is still used next to `anyhow` (mixed error styles).
- **Regressions:** upload with the default destination, export, import conversion, stdin upload, `--print-only-id`, and `--stdout` all broke (section 4). The fork also lost upstream's error on an existing file, its multipart small-file path, and its tree summary ("Found N files in M directories").
- **Positive:** the shape of the code is clear and readable. `DriveItem`/`DiskItem` give one vocabulary for both directions. The commit history is small-step and easy to follow. md5 skip-if-identical, in-place update, merge-into-existing-folder and dry-run are the features users ask for (#16, #18, #56, #65, #69, #84, #116).

### 5.2 Is `DriveTask`/`TaskManager` worth adopting as the core engine?
**No. Take the ideas, not the code.** The 69-line manager does three things: spawn eagerly, gate with a semaphore, and join in a loop. Its weaknesses are structural: unbounded parked tasks and a retained task list, `process()` returning `()` so results go through `Mutex` side channels, ignored `JoinError`s, no cancellation, no listing/transfer separation, and no retry or rate-limit hooks. In modern tokio, `JoinSet` plus a `Semaphore` (or an `mpsc` work queue feeding `buffer_unordered`) gives the same structure with typed results, `abort_all()` and backpressure in a similar amount of code. What is worth keeping is the **decomposition**: one task per directory that lists once and spawns children with their pre-matched remote state, plus one task per file. Also worth keeping: the DriveItem/DiskItem model, md5 skip, update-in-place and dry-run threading.

### 5.3 Comparison with PR #68 and PR #82

| | toutizes | PR #68 (Defman, concurrent dir download) | PR #82 (Jasper-Bekkers, parallel upload) |
|---|---|---|---|
| Mechanism | eager `tokio::spawn` per item, `Semaphore(2W)` | `stream::unfold` DFS over folders, then `flatten`, then `buffer_unordered(N)` | `futures::join_all` over all files of a folder |
| Concurrency bound | yes (2W, default 2) | yes (`--concurent N`, required flag, no default) | **none** within a folder; folders run serially |
| Listing overlaps transfers | yes (listings share the permit pool) | yes (lazy serial listing pulled when the buffer has room) | no (FileTree is materialized first) |
| Backpressure / memory | unbounded parked tasks + retained Vec | **bounded**: pending-dir stack + one folder's files | unbounded fds and N x 32 MiB chunk buffers |
| Errors | printed, swallowed, exit 0 | collected into a Vec and dropped, returns Ok | dropped; `.unwrap()` panics on open errors |
| Retries | uploads only (5xx/429) | none | uploads only (5xx/429) |
| Integrity | stream md5 checked; temp-name collision | **md5 not checked** (`None`); same `with_extension` temp collision; async `tokio::fs` I/O | n/a |
| Scope | download + upload + sync + dry-run | directory download only (single-file and shortcut paths commented out) | directory upload only |
| Status | prototype with regressions | self-described "very rough" draft | self-described "one-off throw-away code" |

**Best for throughput and safety:** none of them as-is. The right design combines:
- #68's bounded, backpressured pipeline and async file I/O;
- toutizes' per-directory task decomposition (listing overlapped with transfers, children receive pre-matched remote state) and its symmetric upload/download/sync semantics;
- what all three lack:
  - separate limits for listing (for example 4) and transfers (for example 8-16, with a sane default above 2);
  - a shared adaptive limiter that also treats 403 `userRateLimitExceeded`/`rateLimitExceeded` and 429 as backoff signals across all workers (AIMD on the permit count);
  - retries for downloads and listings, with Range resume;
  - multipart upload for small files;
  - unique temp names (`.<name>.<rand>.gdrive-part`), detection of duplicate names and Drive ids, and name sanitization (reject `/`, `..`, absolute);
  - a typed per-item result, a summary on stderr and a non-zero exit;
  - trash by default for sync deletions, with deletion done only after the folder's children finish;
  - `spawn_blocking` for hashing, and HTTP/1.1 multi-connection or tuned h2 windows for multi-Gbit links (currently all traffic is multiplexed on one h2 connection).

---

## 6. Porting effort onto dodomorandi's tree (hyper 1.x / google-drive3 7 / google-apis-common 8)

Context: dodomorandi has **no concurrency at all** (no `spawn`, `JoinSet`, `Semaphore` or `buffer_unordered` in `src/`). It keeps the upstream `file_tree*` approach, uses typed per-module error enums with `error-trace` (no anyhow), async `tokio::fs` in download, `ExitCode` from main, and has 13 tests. Its `UploadDelegate` already has the same structure on the new `google_apis_common::Retry`/`Response` types (`dodomorandi/src/common/delegate.rs:87-118`). The engine therefore has to be added, not merged, and toutizes' code would be rewritten rather than cherry-picked. Its commits will not apply: the files it rewrites were themselves refactored heavily in dodomorandi.

| Work item | Notes | Estimate |
|---|---|---|
| Transfer engine (JoinSet + listing/transfer semaphores + bounded queue + result aggregation + cancellation + exit code) | new code of about 200-300 LOC, inspired by tasks.rs | 1-1.5 d |
| DriveItem / DiskItem model | port about 730 LOC. Body type changes from `hyper::Body` to `google_apis_common::Body` (`BodyExt::frame`/`into_data_stream`). Convert anyhow to typed errors. Add `shortcutDetails` and `modifiedTime` to list fields. Add an explicit GoogleDoc variant. | 1.5-2 d |
| Download/upload tasks with the fixes from section 3.3 | temp names, duplicate detection, sanitization, download/list retries with 403/429 handling, small-file multipart, `spawn_blocking` md5 | 2-3 d |
| Sync (download and upload) | keep-set from the unfiltered listing, deletion after children finish, trash by default, `--delete-permanently` opt-in, never-delete-folders option, dry-run | 1-1.5 d |
| Path resolution (`drive_names.rs`) | fix `/`, ambiguity errors, shared drives | 0.5 d |
| CLI integration that does **not** break 3.x | keep `--parent` multi, `--print-only-id`, stdin upload, `--stdout` flag and error-on-existing. New behavior goes behind `--skip-existing`/`--overwrite`/`--sync` or a `sync` subcommand. `--workers`/`--dry-run` with `global = true`. | 0.5-1 d |
| Tests | fake Drive (trait-based hub or wiremock) for sync and deletion semantics, concurrency races, rate-limit backoff | 2-3 d |
| **Total** | production quality | **about 8-12 developer-days** |

A literal port (only making toutizes' code compile on the new stack) is about 2-3 days, but it would import every bug in sections 3 and 4. Not recommended.

---

## Adoptable items

```json
[
  {"item": "Streaming tree walk: one task per Drive/local directory lists once and spawns child tasks; transfers start while listing continues (no materialized tree)", "files": ["src/files/download.rs", "src/files/upload.rs", "src/files/tasks.rs"], "value": "high", "risk": "medium", "note": "Re-implement on tokio JoinSet + separate listing/transfer semaphores + bounded queue; do not copy TaskManager (unbounded eager spawn, swallowed JoinErrors, results via Mutex side channel)."},
  {"item": "Children receive pre-matched remote state from the parent's single listing (upload existing_items by name; download uses md5/size from files.list, no per-file files.get)", "files": ["src/files/upload.rs", "src/common/drive_item.rs"], "value": "high", "risk": "low", "note": "About 1 files.list per folder; add shortcutDetails and modifiedTime to the list fields (src/files/list.rs:105-108)."},
  {"item": "Skip identical files by md5 on download/upload (--overwrite)", "files": ["src/common/disk_item.rs", "src/files/download.rs", "src/files/upload.rs"], "value": "high", "risk": "low", "note": "Add a size pre-check before hashing; hash in spawn_blocking; keep upstream's error-on-existing as the default."},
  {"item": "Replace semantics: update existing Drive file content in place via files.update (keeps id, sharing, revisions)", "files": ["src/common/drive_item.rs", "src/files/upload.rs"], "value": "high", "risk": "low", "note": "Addresses issues #18/#65/#69; opt-in flag; use multipart for small files."},
  {"item": "Recursive upload merges into an existing same-name Drive folder", "files": ["src/files/upload.rs"], "value": "high", "risk": "medium", "note": "Behavior change vs 3.x (which always creates a new folder); gate behind --overwrite/--sync or a sync subcommand; error on duplicate folder names."},
  {"item": "download --sync / upload --sync mirroring with deletion", "files": ["src/files/download.rs", "src/common/disk_item.rs", "src/files/upload.rs"], "value": "high", "risk": "high", "note": "Take the semantics only. Must fix: trash not files.delete; folders deleted only on explicit opt-in; keep-set from unfiltered listing; delete after children finish; sanitize names (absolute and .. traversal); unique temp names; dry-run on new dirs."},
  {"item": "Dry-run threading through every mutating operation", "files": ["src/main.rs", "src/common/drive_item.rs", "src/common/disk_item.rs", "src/files/upload.rs", "src/files/download.rs"], "value": "medium", "risk": "low", "note": "Expose as --dry-run with global=true; print the plan to stderr."},
  {"item": "Drive path resolution (/A/B -> id) for download source and upload/import destination", "files": ["src/common/drive_names.rs"], "value": "medium", "risk": "medium", "note": "Fix '/' (currently 'Empty path'), error on ambiguous names instead of first match, support shared drives; keep ids working."},
  {"item": "DriveItem/DiskItem domain model (Root/Directory/File/Shortcut vs Stdout/Root/FileOrDir)", "files": ["src/common/drive_item.rs", "src/common/disk_item.rs"], "value": "medium", "risk": "low", "note": "Good basis for a transfer engine; port with dodomorandi-style typed errors; add an explicit GoogleDoc variant (export) instead of 'Unknown file type'."},
  {"item": "End-of-run statistics with bandwidth", "files": ["src/files/download.rs"], "value": "low", "risk": "low", "note": "Count errors (always 0 today), print to stderr, set a non-zero exit on failures, fix the 'duratin' typo."},
  {"item": "TaskManager / DriveTask trait verbatim", "files": ["src/files/tasks.rs"], "value": "low", "risk": "medium", "note": "Do not adopt; replace with JoinSet + Semaphore and typed per-item results."},
  {"item": "Runtime worker_threads tied to --workers and permits = 2x workers", "files": ["src/main.rs"], "value": "low", "risk": "medium", "note": "Do not adopt; keep the default multi-thread runtime, use an independent --jobs default (for example 8) and move blocking I/O off the runtime threads."}
]
```
