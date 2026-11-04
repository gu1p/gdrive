# Pull request triage (all 31 upstream PRs)

Source: glotlabs/gdrive pull requests, snapshot 2026-11-04. Each diff was read in full and patch-checked against upstream main and the dodomorandi fork by three Opus 5.5 review agents; cross-checked against an independent codex (gpt-6-astra) review.

| # | State | Author | Category | Area | Speed | Quality | Applies upstream | Applies dodomorandi | Recommendation | Effort | Reason |
|---|---|---|---|---|---|---|---|---|---|---|---|
| [#32](https://github.com/glotlabs/gdrive/pull/32) how to get past an error | merged | holtzermann17 | docs | docs |  | ok | not-tested | not-tested | **skip** | S | Already merged (c577e36) and present in upstream and dodomorandi. Nothing to port; revisit the advice when the credentials guide is rewritten. |
| [#38](https://github.com/glotlabs/gdrive/pull/38) Fix typo in README | merged | nick-cd | docs | docs |  | good | not-tested | not-tested | **skip** | S | Already merged (bc66bd8) and present in upstream and dodomorandi. |
| [#41](https://github.com/glotlabs/gdrive/pull/41) Upload file from stdin | merged | nick-cd | feature | upload |  | ok | not-tested | not-tested | **rework** | S | Already merged and present. Keep the feature but schedule follow-ups: a --name flag (warn or require it for stdin), keep the existing name on `update` from stdin, keep the Temp alive (dodomorandi already does this via FilePath::Temp and FileKind::Temp), and optionally stream stdin through a resumable session one chunk at a time instead of spooling to disk. |
| [#54](https://github.com/glotlabs/gdrive/pull/54) Make description a bit more explicit/complete | merged | DirkHoffmann | docs | docs |  | good | not-tested | not-tested | **skip** | S | Already merged (ff83747) and present in upstream and dodomorandi. |
| [#66](https://github.com/glotlabs/gdrive/pull/66) Add basic usage doc | merged | anlif | docs | docs |  | ok | not-tested | not-tested | **skip** | S | Already merged (ca941ca) and present in upstream and dodomorandi. Apply #138 on top and expand later. |
| [#67](https://github.com/glotlabs/gdrive/pull/67) Add ability to trash files for when deleting is not an optio | open | CrmsnDragoon | feature | other |  | ok | clean | conflicts | **rework** | S | Wanted feature (a safer alternative to the permanent files.delete) that no fork has. Re-implement as a ~40-line module, or as `files delete --trash`, on top of update_metadata/PatchFile. Drop --skip-trashed or add `trashed` to the list fields, and request trashingUser(displayName) if it is shown. |
| [#68](https://github.com/glotlabs/gdrive/pull/68) Concurrent directory download | open | Defman | feature | download | yes | poor | clean | conflicts | **cherry-pick-idea** | M | Keep the pipeline shape (lazy per-folder listing, stream flatten, bounded buffer_unordered) and the async-write md5 path, but re-implement on the dodomorandi base. Give --parallel a sane default (about 4-8), reject 0, keep download_regular, shortcuts, md5 checks and skip-identical, use unique temp names (append '.incomplete' or use a tempfile in the same dir), keep the binary-only filter or export Docs (#162), aggregate per-file errors into a summary and a non-zero exit, and add retry with backoff for 429/403-rateLimit/5xx and dropped HTTP/2 connections. |
| [#75](https://github.com/glotlabs/gdrive/pull/75) some random change | closed | fireattack | docs | docs |  | poor | conflicts | conflicts | **skip** | S | Accidental, closed by the author; it would delete the README. |
| [#82](https://github.com/glotlabs/gdrive/pull/82) 🏎️ Speed up uploading by parallelizing uploads | open | Jasper-Bekkers | feature | upload | yes | poor | clean | conflicts | **cherry-pick-idea** | M | Right idea: IdGen pre-generates the IDs, so the file uploads are independent once their folder exists. Wrong implementation. On the dodomorandi base: create all folders first, parent-first (optionally in parallel per depth level, since the IDs are known), then push all files through one global bounded pool (`--parallel N`, default about 4, stream::buffer_unordered or JoinSet plus a Semaphore with tokio::spawn and Arc<Hub>). Open files lazily inside the task, aggregate per-file results into a summary and a non-zero exit, budget memory as N x chunk_size, and fix the delegate retry policy (see notes). |
| [#96](https://github.com/glotlabs/gdrive/pull/96) [DOC] Update create_google_api_credentials.md to directly sh | open | fseasy | docs | docs |  | ok | clean | clean | **merge-as-is** | S | Zero risk and applies cleanly to both trees. It is only a stopgap: the credentials guide needs a rewrite with current screenshots. |
| [#104](https://github.com/glotlabs/gdrive/pull/104) FIX --max parameter when listing files | merged | pacomix | bugfix | list |  | good | not-tested | not-tested | **skip** | S | Already merged (1662db6) and present in upstream main and in both forks. Nothing to do except close the stale issues #57 and #102 in the fork. |
| [#113](https://github.com/glotlabs/gdrive/pull/113) Add the --tsv option to allow tab as a separator. | open | kenchou | feature | cli-ux |  | ok | clean | conflicts | **rework** | S | It fixes a real scripting bug (#52), but should be redone on the fork. Options: a --raw/--no-align flag, or skip TabWriter automatically when --field-separator is given or stdout is not a TTY. Add escaping, or use a real delimited writer (csv crate with delimiter b'\t'), and maybe a --format json\|tsv\|csv later. |
| [#126](https://github.com/glotlabs/gdrive/pull/126) Add support for determining folder size | closed | dachrisch | feature | list |  | ok | clean | conflicts | **cherry-pick-idea** | M | A 'du'-style feature that no issue asks for. If wanted, write a dedicated concurrent size walker that requests only fields=files(id,mimeType,size),nextPageToken with bounded buffer_unordered, exposed as `files info --size` or `files du`. The same walker could speed up building the tree for recursive download. |
| [#127](https://github.com/glotlabs/gdrive/pull/127) ixed warning: & without an explicit lifetime name cannot be  | closed | dachrisch | bugfix | build-ci |  | good | clean | conflicts | **skip** | S | Not needed on a dodomorandi base, which already rewrote the const. If the fork stays on upstream, apply the one-line hunk, which #130 also contains word for word. |
| [#130](https://github.com/glotlabs/gdrive/pull/130) Release builds for ARM architectures | open | mike-kfed | ci | build-ci |  | ok | clean | conflicts | **cherry-pick-idea** | M | Keep the target list and the cross recipe, but rewrite as one matrix release workflow. Use native ubuntu-24.04-arm runners (as #154 does) or cross / houseabsolute/actions-rust-cross (the author's own suggestion), publish with gh release upload or softprops/action-gh-release, build with --locked, add sha256 sums, and split macOS correctly into x86_64 and arm64 (or a universal binary). |
| [#132](https://github.com/glotlabs/gdrive/pull/132) Add ssh tunnel guide for setup | open | TombLL | docs | auth |  | ok | clean | clean | **merge-as-is** | S | Useful, accurate docs at no cost. Fix the typo when merging, and optionally add the curl-the-redirect alternative. |
| [#137](https://github.com/glotlabs/gdrive/pull/137) Fix typo in create_google_api_credentials.md | open | umaumax | docs | docs |  | good | clean | clean | **merge-as-is** | S | A free docs fix; the typo is still in both forks. |
| [#138](https://github.com/glotlabs/gdrive/pull/138) docs: add missing argument in basic_usage.md | open | hyoklee | docs | docs |  | good | clean | clean | **merge-as-is** | S | A free docs fix; the wrong command is still in both forks' docs/basic_usage.md. |
| [#140](https://github.com/glotlabs/gdrive/pull/140) support HTTP/HTTPS proxy | closed | GikLaang | feature | other |  | poor | clean | conflicts | **skip** | S | A duplicate of #141 (identical diff); closed with its branch deleted. |
| [#141](https://github.com/glotlabs/gdrive/pull/141) automatically detect and use system HTTP/HTTPS proxy | open | GikLaang | feature | other |  | poor | clean | conflicts | **cherry-pick-idea** | M | Proxy support is wanted (#26 and #74; users behind the GFW in #21 and #59), but this code is unsafe to ship and hyper-proxy 0.9 supports only hyper 0.14. Reimplement it on dodomorandi with hyper-util's built-in proxy support. client::proxy::matcher::Matcher::from_system() covers HTTP(S)_PROXY, ALL_PROXY and NO_PROXY, plus macOS/Windows system settings via the client-proxy-system feature. client::legacy::connect::proxy::{Tunnel, SocksV5} can be wrapped with hyper_rustls HttpsConnectorBuilder::wrap_connector, which keeps rustls and h2 end to end. One create_connector() change in hub.rs covers both DriveHub and yup-oauth2 (CustomHyperClientBuilder). |
| [#144](https://github.com/glotlabs/gdrive/pull/144) feat: add max parameter for drive list | closed | iamjoeker | feature | drives |  | poor | clean | conflicts | **cherry-pick-idea** | S | Rewrite the roughly 30-line pagination in dodomorandi's drives/list.rs (list all drives by default, --max optional). Discard the dependency swap, the .idea files and the formatting hunks. |
| [#146](https://github.com/glotlabs/gdrive/pull/146) Markdown file export | open | Loh1917 | feature | export |  | ok | clean | conflicts | **skip** | S | #159 contains the same export change and also fixes import, by sending the source mime as the media Content-Type. Keep only #146's help-text tweak (adding md to the import example list) when porting #159. |
| [#153](https://github.com/glotlabs/gdrive/pull/153) Arm build | closed | gueraf | ci | build-ci |  | poor | clean | clean | **skip** | S | Broken runner label; the same author's #154 replaces it. |
| [#154](https://github.com/glotlabs/gdrive/pull/154) Add linux_arm workflow. | open | gueraf | ci | build-ci |  | ok | clean | clean | **cherry-pick-idea** | S | Native arm64 runners are the right way to ship aarch64 Linux. Fold the idea into one matrix release workflow (native x86_64 and arm64 Linux, macOS arm64 plus an x86_64 cross target, Windows, and armv7 via cargo-zigbuild or cross) with current actions, instead of adding another copy-pasted YAML file. |
| [#156](https://github.com/glotlabs/gdrive/pull/156) Add configurable drive list max with pagination | open | somsak | feature | drives |  | ok | clean | conflicts | **rework** | S | The pagination is needed and absent from both forks. Re-implement it on dodo using the same pattern as files list (usize max, MAX_PAGE_SIZE=100, page_size=min(100, remaining)), preferably defaulting to all drives (or to a high cap), and validate max>=1. |
| [#157](https://github.com/glotlabs/gdrive/pull/157) Add TUI navigate mode | open | stulluk | feature | cli-ux |  | poor | clean | conflicts | **skip** | L | Out of scope for a scriptable CLI. It adds about 1.8k lines plus TUI dependencies, has no tests, and would block the dependency migration and refactors. If anyone ever wants a TUI, it belongs in a separate binary or crate (or behind a non-default cargo feature) built on a stable library API. Only the ProgressReader idea is worth keeping. |
| [#158](https://github.com/glotlabs/gdrive/pull/158) Add Docker scripts for ARM static builds | open | stulluk | ci | build-ci |  | ok | clean | clean | **cherry-pick-idea** | S | Ship armv7-unknown-linux-musleabihf (and maybe arm-unknown-linux-musleabi for Pi Zero/1) from CI using cargo-zigbuild or cross. Document `cross build --target ...` (or a single docker run line with --user and a registry cache) for local builds rather than committing three script files. |
| [#159](https://github.com/glotlabs/gdrive/pull/159) Add support for Markdown file type and MIME type | open | lev-n | feature | export |  | good | clean | conflicts | **rework** | S | It is the better of the two Markdown PRs and would be merge-as-is on upstream. On the dodo base it needs a mechanical port (pub(crate) consts, LazyLock mime statics, a reshaped file_info::Config). Add #146's help-text line when porting. |
| [#162](https://github.com/glotlabs/gdrive/pull/162) Recursive download: add export | open | serebrov | feature | recursive |  | ok | clean | conflicts | **rework** | M | The feature is wanted: whole-drive backup with Docs (issues #64 and #123, and the documented 'Docs silently ignored' limitation). Port the improved version from #164 (same author: exports warn instead of abort, shortcuts, filename sanitizing, mtime) onto dodo's download module. Add a --export-format or --skip-docs option and delete the dead FileTreeDrive code. |
| [#163](https://github.com/glotlabs/gdrive/pull/163) Add short flag for common CLI arguments | open | antoine2711 | feature | cli-ux |  | good | clean | conflicts | **rework** | S | Re-apply on dodo's restructured main.rs, where the patch conflicts. Settle the short-flag namespace first: toutizes already uses -r recursive, -f overwrite and -s sync on download/upload, so if sync or overwrite semantics are adopted, -s should not mean --chunk-size. A one-key -r on delete may also deserve a second thought. |
| [#164](https://github.com/glotlabs/gdrive/pull/164) Fix dependency update problem | closed | serebrov | feature | recursive | yes | ok | clean | conflicts | **cherry-pick-idea** | M | Drop the Cargo.toml/Cargo.lock part (dodo's migration makes the pin unnecessary) and the README. Port the download.rs logic (export with warnings, Drawings, shortcuts, sanitize_filename with the underflow and Windows fixes, filetime mtime, skip-unchanged) onto dodo's download module. It supersedes #162. |

## Summaries and review notes

### #32 how to get past an error

Docs only. Fixes the 'meny' typo in docs/create_google_api_credentials.md and adds to step 9 that the Project ID must be put into the OAuth consent screen App name, the author's workaround for 'An error saving your app has occurred'.

Quality: Two-line doc tweak. The 'put the Project ID into the app name' advice is anecdotal and not explained; issue #160 shows users still hit app-name validation errors. The whole guide predates the current Google Cloud console layout.

Related issues: #160. Superseded by: none.

Already merged upstream: forward `git apply --check` fails only because the change is already applied; `git apply --check --reverse` is clean on upstream main and on dodomorandi, which confirms the code is present.

### #38 Fix typo in README

README typo fix: 'via you favorite search engine' becomes 'via your favorite search engine'.

Quality: Trivial and correct.

Related issues: none. Superseded by: none.

Already merged upstream: forward `git apply --check` fails only because the change is already applied; `git apply --check --reverse` is clean on upstream main and on dodomorandi, which confirms the code is present.

### #41 Upload file from stdin

Makes FILE_PATH optional for `files upload` and `files update`; when it is omitted, stdin is spooled to a mktemp temporary file (new src/common/file_helper.rs, new mktemp dependency) and that file is uploaded. As the maintainer asked, it uses an omitted argument rather than '-'.

Quality: Works for the basic case, but: (1) the Drive file gets the random temp-file name (a UUID) and, because the temp file has no extension, the mime type is guessed as application/octet-stream; there is no --name flag; (2) `files update <id>` from stdin renames the Drive file to that UUID, because update_file() always sets name from file_info; (3) file_helper::open_file() drops the mktemp::Temp before returning, so the temp file is unlinked while still open. That works on Unix; on Windows the delete fails silently and the file leaks; (4) all of stdin is spooled to $TMPDIR before the upload starts: no streaming, needs free disk equal to the input size, no progress; (5) adds config.file_path.as_ref().unwrap() calls in upload_regular/upload_directory.

Related issues: #23. Superseded by: none.

Presence verified: upstream src/common/file_helper.rs (stdin_to_file, open_file), src/files/upload.rs lines 51-70 (None branch spools stdin and calls upload_regular), src/files/update.rs line 44 (file_helper::open_file), main.rs Upload/Update file_path: Option<PathBuf>, Cargo.toml mktemp = 0.5.0. `git apply --check --reverse` is clean on upstream main. On dodomorandi the reverse check fails because the code was refactored, but the feature is present (src/files/upload.rs:105, src/common/file_helper.rs). toutizes made upload's file_path required, dropping stdin upload (it keeps it for update). Issue #23 is still open upstream even though this PR was merged.

### #54 Make description a bit more explicit/complete

Docs: expands the last steps of the credentials guide (copy or download the client id and secret now, click OK, open the 'OAuth consent screen' menu item, publish the app; renumbered to 28 steps) and notes that `gdrive account add` then points the user to a consent URL.

Quality: Clear, correct incremental doc improvement. Keeps the existing typo 'Cliend Id', which open PR #137 fixes.

Related issues: #22, #17. Superseded by: none.

Already merged upstream: forward `git apply --check` fails only because the change is already applied; `git apply --check --reverse` is clean on upstream main and on dodomorandi, which confirms the code is present. The 'Publish app' step is the fix for 7-day refresh-token expiry (#22).

### #66 Add basic usage doc

Adds docs/basic_usage.md: account add, files list, files info, files download, download --recursive, and a note that Google Docs files are skipped by recursive download.

Quality: Useful but thin. It gives the wrong command `gdrive export` (should be `gdrive files export`; open PR #138 fixes it), uses hedging wording ('id/hash(?)') and does not cover upload, --parent, update, sharing or account export/import.

Related issues: #48, #62, #106. Superseded by: none.

Already merged upstream: forward `git apply --check` fails only because the change is already applied; `git apply --check --reverse` is clean on upstream main and on dodomorandi, which confirms the code is present. If #162 (export during recursive download) is adopted, the 'Google Docs silently ignored' note must change.

### #67 Add ability to trash files for when deleting is not an option

Adds `files trash <id> [--recursive]` and `files untrash <id> [--recursive]` (files.update with trashed=true/false, supportsAllDrives), adds Trashed/TrashedTime/TrashingUser to `files info` (requests trashed,trashedTime in info::get_file), adds `files list --skip-trashed`, bumps proc-macro2 in Cargo.lock and rustfmt-reorders file_helper.rs and common/mod.rs.

Quality: The core API usage is right: Drive v3 has no files.trash, so update with {trashed:true} is the right call, and supportsAllDrives is set. Valid use case: shared-drive content managers can trash but not permanently delete. Sloppy elsewhere: trash.rs and untrash.rs are 125-line near-duplicates, each with an unused copy of the PatchFile struct (update.rs already has PatchFile plus update_metadata(), which a with_trashed() could reuse). `--skip-trashed` does nothing: list_files() requests fields files(id,name,md5Checksum,mimeType,size,createdTime,parents) without `trashed`, so file.trashed is always None, and the default queries already contain `trashed = false`. TrashingUser is never filled because get_file does not request trashingUser. The messages are copy-pasted: 'File successfully updated', and untrash says 'use --recursive to trash directories'. The diff has trailing whitespace, and `trashed.is_some() && trashed.unwrap()` should be `== Some(true)`.

Related issues: #134. Superseded by: none.

Based on current upstream main (8551556), so it applies cleanly there. On dodomorandi every touched Rust file conflicts (pub(crate) visibility, Box'ed error types, restructured main.rs/list.rs/info.rs, google-drive3 7 API). It also conflicts textually with #82, because both carry the same rustfmt hunks for src/common/file_helper.rs and src/common/mod.rs; #68 still applies after #67. The proc-macro2 1.0.49 to 1.0.86 lock bump is not needed today: upstream builds on rustc 1.99 with 1.0.49 (build_log.txt). Adding trashed,trashedTime to info::get_file's fields slightly enlarges every caller's response, which is harmless. Loosely related: #134 (permanently deleted files linger in the web UI), #14 (bulk delete).

### #68 Concurrent directory download

Draft PR. Replaces recursive download with a lazy streaming traversal: stream::unfold over a DFS stack of folders (one files.list per folder) yields each folder's non-folder files, .flatten() feeds them into .buffer_unordered(N), with N from a new `--concurent` flag. save_body_to_file is rewritten to async tokio::fs, with md5 computed through tokio_util InspectReader and create_dir_all per file. Adds an ext.rs FileExtension trait, derives Default on ListFilesConfig, and adds the tokio-stream and tokio-util deps. It also comments out single-file download and shortcut following.

Quality: The concurrency design is sound: in-flight downloads are capped at N, the folder walk is lazy and memory-bounded, and listing overlaps with downloads. Folder listing is serial, so deep trees of tiny folders become listing-bound. The PR is an unfinished draft that breaks existing behaviour: `gdrive files download <file-id>` does nothing (the download_regular call is commented out); --follow-shortcuts does nothing; `--concurent` is a required usize with no default, so every download call must pass it; `--concurent 0` hangs forever (futures-util 0.3.25 BufferUnordered with max=0 returns Pending without a waker). All results are collected into a Vec and dropped, and the function always returns Ok, so download, write and listing errors are swallowed; listing errors are even remapped to MissingFileName ('TODO: fix error'). MD5 verification is removed (expected_md5=None) and so is the skip-identical-local-file check. Google Docs are no longer filtered (the old FileTreeDrive kept only is_binary files), so they fail silently on alt=media. Introduced race: tmp path file_path.with_extension("incomplete") replaces the extension, so 'a.pdf' and 'a.docx' (or Drive's allowed duplicate names) in one folder write the same 'a.incomplete' concurrently, giving a corrupt file plus a failed rename, all silent.

Related issues: #64, #145, #148, #19, #78. Superseded by: toutizes fork: parallel download via TaskManager + tokio Semaphore (src/files/tasks.rs, src/files/download.rs, global --workers, download uses 2*workers).

Concurrency details: only the downloads run concurrently, on a single task (buffer_unordered polls the futures in the caller's task, not across worker threads). That is fine for IO-bound downloads, but tokio::spawn would scale writes and md5 across cores. hub.rs enables h1+h2 and googleapis negotiates h2, so all N downloads multiplex over one TCP connection. Bandwidth gains come from overlapping per-request latency, and a GOAWAY (issue #78, 'max_age') hits all in-flight streams at once, with no retry because downloads use the default delegate. Smaller bugs: `file.id.as_ref()?` inside unfold ends the whole traversal if a folder lacks an id; empty folders are never created; the hyper error text is replaced by 'Error!'; files without a name are silently dropped; Drive names containing '/' or '..' are joined into the path unsanitized (pre-existing, issue #19). This is now concurrent and still unsanitized, a path-traversal risk. Dead code left: download_directory, download_regular, the Md5Writer import, the async_recursion removal, and ext.rs duplicating drive_file::is_directory/is_binary/is_shortcut. git apply: clean on upstream main and still clean after #67 or #82. Conflicts on dodomorandi (download.rs split into errors.rs, google_apis_common::Body instead of hyper::Body, Digest md5). The toutizes equivalent has bounded tokio::spawn plus a Semaphore and keeps md5. It has the same with_extension("incomplete") collision (src/common/disk_item.rs:181) and returns Ok after printing error stats.

### #75 some random change

Accidental PR: replaces the whole README.md with the line 'make some random change'. The author closed it the same day ('I misclick it in VSCode').

Quality: Not a real contribution: it deletes the README.

Related issues: none. Superseded by: none.

The forward apply fails on both trees because README.md changed after the PR base (da821bf 'Add new invite link'); the reverse apply also fails, as expected.

### #82 🏎️ Speed up uploading by parallelizing uploads

In `files upload --recursive`, replaces the per-folder sequential file loop with futures::future::join_all over all files of that folder, so each folder's files upload concurrently. Folders are still created one by one, parent first. The rest of the diff is rustfmt of #41's code (file_helper.rs, common/mod.rs, update.rs, upload.rs). The author calls it 'one-off throw-away code' and reports reaching 1 Gbps.

Quality: Proof of concept. Fan-out is unbounded: one in-flight upload per file in the folder. join_all collects the iterator eagerly, so every file is fs::File::open'ed before any upload starts, and the open error is .unwrap()'ed. A folder with more files than `ulimit -n` (1024 on Linux, 256 on macOS) panics mid-upload and leaves a partial tree on Drive. Memory: google-drive3/google-apis-common read the whole body (simple upload, files up to the chunk size) or one full chunk (resumable, default 32 MiB) into a Vec per in-flight upload, using blocking read_to_end. That costs N x up to 32 MiB of RAM (1000 x 20 MB photos is about 20 GB), and each blocking disk read stalls every other upload sharing the single join_all task. The Vec<Result> returned by join_all is dropped, so failed uploads are ignored silently, the success summary still prints and the exit code is 0. --print-only-id lines are printed before the upload instead of after success. The per-folder barrier means trees of many small folders gain little.

Related issues: #145, #148, #36. Superseded by: toutizes fork: parallel upload via TaskManager + tokio Semaphore (src/files/tasks.rs, src/files/upload.rs, global --workers).

Interaction with src/common/delegate.rs and src/files/upload.rs: every upload_file() builds its own UploadDelegate, so each file has an independent Backoff (min 1 s, max 60 s, max_retries 100000) and its own in-memory resumable session URL. Nothing is shared or global: when Drive throttles, all N uploads back off and retry independently (thundering herd), and one file stuck on persistent 5xx keeps join_all, and the whole command, waiting almost indefinitely. should_retry() retries only 5xx and 429. Drive reports most rate limiting as 403 rateLimitExceeded/userRateLimitExceeded, and also 408 (issue #36); those abort at once, and under #82 they turn into silent data loss. The JSON error body passed to http_failure is ignored, though it carries the reason. Backoff.attempts is never reset after a successful chunk, so late retries in long uploads sleep close to 60 s. Ordering is safe: folder IDs are pre-generated and folders are created before their files. HTTP: hub.rs enables h1+h2 and googleapis negotiates h2, so the parallel uploads share one TCP connection. The gains come from overlapping per-request latency (large for many small files, smaller for a few big files), and an h1 connection pool is worth benchmarking. Drive quotas (per-minute query quotas, the 750 GB/day per-user upload cap, undocumented sustained create rate) are neither respected nor surfaced. git apply: clean on upstream main. Conflicts on dodomorandi (upload.rs/update.rs/file_helper.rs refactored; UploadDelegate borrows &UploadDelegateConfig). Also conflicts with #67, which carries the same rustfmt hunks. dodomorandi uploads sequentially; google-drive3 7 / google-apis-common 8 there still buffer a chunk with blocking read_to_end, so the memory analysis also holds on that base.

### #96 [DOC] Update create_google_api_credentials.md to directly show screenshot

Docs: turns the 14 '[[screenshot]](url)' links in the credentials guide into inline images ('![[screenshot]](url)').

Quality: Mechanical, correct markdown change; the screenshots render inline. The screenshots date from January 2023 and no longer match Google's reorganized console (the OAuth consent screen now lives under 'Google Auth Platform'), so inlining them makes the staleness more visible.

Related issues: #160, #106. Superseded by: none.

Conflicts textually with open PR #137 (typo 'Cliend' to 'Client'), since both edit the step-24 line: after applying #96, #137 no longer applies. Fold the typo fix in by hand.

### #104 FIX --max parameter when listing files

Removes the min(max, 1000) cap applied before list_files, so `files list --max N` above 1000 works. list_files already pages with page_size <= MAX_PAGE_SIZE (1000) and cuts the result to max_files.

Quality: A correct one-line change: the pagination loop in list_files already limits page size and truncates the result.

Related issues: #57, #102, #103. Superseded by: none.

Verified present: upstream src/files/list.rs:35 `max_files: config.max_files` (git apply -R --check succeeds on the copy of upstream main). A forward apply fails on both trees only because the change is already there. Same line in dodomorandi src/files/list.rs:38 (which also uses truncate()) and toutizes src/files/list.rs:34.

### #113 Add the --tsv option to allow tab as a separator.

Adds --tsv to `files list`, `drives list` and `permissions list`. With the flag, rows are written straight to stdout, joined with --field-separator, instead of through TabWriter, which otherwise turns tabs into aligned spaces so a real tab separator can never come out (#52).

Quality: Small and consistent: all three table::DisplayConfig construction sites are updated. But the flag is misnamed: it does not force a tab, it only turns off alignment, so `--tsv --field-separator ,` gives raw comma output. The help text 'Use tab separator' is therefore wrong. Values are not escaped: Drive names can contain tabs or newlines, which produce broken TSV. There is also a stray blank line before the closing brace.

Related issues: #52. Superseded by: none.

Neither fork has an equivalent: dodomorandi and toutizes common/table.rs still always use TabWriter. On dodomorandi it conflicts in every file, because DisplayConfig.separator is now &str and main.rs and the list modules were rewritten.

### #126 Add support for determining folder size

Adds `files list --folder-size`. For each folder row it builds a full FileTreeDrive (a recursive listing of the whole subtree) and shows the summed size of the binary files, clamped to i64::MAX.

Quality: Works for small trees, but it is slow and drops errors. Each folder gets a sequential walk: one paginated files.list per subfolder, folders processed one after another, no concurrency. It builds the whole FileTreeDrive, which clones the parent chain into every node, only to add up sizes. Error::FolderSize discards the source error and prints 'Failed to determine folder size: ' with nothing after the colon. A binary file without a size (MissingFileSize) aborts the whole listing. The flag has no help text. Formatting is worse than rustfmt (.await indentation, missing trailing commas, import order). Google Docs, which have no size, are silently left out.

Related issues: none. Superseded by: none.

Closed unmerged by the author. No fork has an equivalent. It conflicts with dodomorandi in list.rs and main.rs.

### #127 ixed warning: & without an explicit lifetime name cannot be used here

Adds an explicit 'static lifetime to DocType::IMPORT_EXTENSION_MAP (an elided lifetime in an associated const) and adds .idea to .gitignore.

Quality: A correct one-token fix. On older rustc (about 1.62) the elided form is a hard error E0106 (#79, also hit on NetBSD). On current rustc it is only the future-incompat lint elided_lifetimes_in_associated_constant. The .gitignore hunk is unrelated IDE noise, and the title has a typo ('ixed').

Related issues: #79. Superseded by: dodomorandi fork (IMPORT_EXTENSION_MAP rewritten as a fixed-size array [(FileExtension, DocType); 18]).

The same drive_file.rs hunk appears in #130. toutizes still has the elided form at src/common/drive_file.rs:64.

### #130 Release builds for ARM architectures

Adds release workflows for linux aarch64-musl, linux armv7-musleabihf and macOS aarch64-apple-darwin. The Linux targets are cross-compiled on ubuntu-latest with TARGET_CC=clang / TARGET_AR=llvm-ar and rust-lld as linker (new .cargo/config.toml). It renames the existing linux and macos workflows to say 'x86_64' and also includes the #127 'static fix.

Quality: The cross-compile recipe is sound: ring builds with clang, and the musl targets link with rust-lld using Rust's bundled crt. But it copies the old one-file-per-target pattern (now 6 near-identical files) with deprecated actions/checkout@v3 and actions/upload-release-asset@v1, and no --locked, strip or checksums. Real bug: macos-latest has been an arm64 image since 2024, so the job renamed 'macos x86_64' builds an arm64 binary and both macOS assets end up arm64. It needs --target x86_64-apple-darwin or a lipo universal binary. Asset names are inconsistent (linux-x64, linux-aarch64, macos-arm64). There is no armv6 job (arm-unknown-linux-musleabi), although config.toml sets a linker for it.

Related issues: #63, #79, #99, #101, #131, #135. Superseded by: none.

On dodomorandi only the drive_file.rs hunk fails, and that hunk is obsolete there. The workflows and .cargo/config.toml apply cleanly with --exclude=src/common/drive_file.rs (dodomorandi's .github is identical to upstream's). Overlap: #153 and #154 add .github/workflows/release_linux_arm.yaml, the same path as this PR's armv7 file, so merging more than one conflicts. #158 adds Docker ARM build scripts. #131 is not specific to ARM: the reporter built without the lockfile and ended up with two hyper-rustls versions, which --locked release binaries avoid. dodomorandi uses ring 0.17.14 and no aws-lc-sys, so the clang cross recipe should still apply.

### #132 Add ssh tunnel guide for setup

README: documents SSH local port forwarding (ssh -L 8085:localhost:8085) so `gdrive account add` can run on a remote server while the OAuth consent happens in a local browser.

Quality: Technically correct: yup-oauth2 (8.1 and 12.1) binds 127.0.0.1:8085 and redirects to http://localhost:8085, so the forward works. Small problems: the typo 'use SSH an ssh tunnel'; it could suggest -N and 127.0.0.1 instead of localhost, since localhost on the remote may resolve to ::1 first; step 5 should say to close the tunnel after the account is added. It does not mention the simpler trick of curling the redirect URL on the remote (#30, #161).

Related issues: #30, #125, #139, #161. Superseded by: none.

The real fix is a paste-the-code or device flow for headless machines (#125, #147). This section stays useful until then. Neither fork README mentions SSH.

### #137 Fix typo in create_google_api_credentials.md

Docs typo: 'Cliend Id' becomes 'Client Id' in step 24 of docs/create_google_api_credentials.md.

Quality: Trivial and correct. The hunk header says line 25, the text is at line 28, and git applies it with an offset.

Related issues: none. Superseded by: none.

The same line also has 'dont'; fix it in the same commit.

### #138 docs: add missing argument in basic_usage.md

docs/basic_usage.md: `gdrive export <file id> <output file>` becomes `gdrive files export ...`, because export is a subcommand of files.

Quality: Correct; it matches the CLI in src/main.rs (FileCommand::Export).

Related issues: none. Superseded by: none.

The same doc still says 'id/hash(?)'; worth cleaning up when the docs are refreshed.

### #140 support HTTP/HTTPS proxy

First version of the env-proxy PR; its diff is byte-identical to #141. It wraps the rustls HttpsConnector in a hyper-proxy 0.9 ProxyConnector (Intercept::All), reads ALL_PROXY/HTTPS_PROXY/HTTP_PROXY (both cases), takes basic auth from the URL userinfo, and gives the same client to yup-oauth2 via InstalledFlowAuthenticator::with_client.

Quality: Same code in #140 and #141. Panics: todo!() for any scheme other than http/https, and ALL_PROXY is read first and is often socks5:// (the exact setup quoted in #26), so those users get a panic on every command; .unwrap() on Uri/Url parsing panics on malformed values; a value without a scheme (proxy:3128) passes the check, but Url::parse then reads 'proxy' as the scheme and the proxy URI is wrong. Prints 'using system proxy <url>' to STDOUT, user:password included, twice per command (Auth::new and Hub::new each call create_client): that leaks the password and corrupts `files download --stdout` and list output read by scripts. Intercept::All, so NO_PROXY is ignored. hyper-proxy 0.9.1's default 'tls' feature brings in native-tls/OpenSSL: through the tunnel, the TLS session to googleapis.com is made by native-tls, not by the configured rustls connector, which only carries the plain hop to the proxy. That adds an OpenSSL dependency to a rustls static build and drops HTTP/2. proxy.rs is copied from ureq with about 10 unused Error variants and doc comments about ureq. It adds http 1.x and url only for parsing, the files lack trailing newlines, and the code is not rustfmt'd. hyper-proxy supports only hyper 0.14 and is unmaintained, so this approach does not survive the hyper 1.x / hyper-util migration.

Related issues: #21, #26, #59, #74. Superseded by: #141.

See #141 for the hyper 1.x reimplementation path.

### #141 automatically detect and use system HTTP/HTTPS proxy

Reopened #140 (identical diff), with screenshots and prebuilt Windows/Linux binaries attached (untrusted; ignore them). Builds a hyper 0.14 client from ProxyConnector<HttpsConnector<HttpConnector>> using hyper-proxy 0.9.1, with the proxy taken from the ALL_PROXY/HTTPS_PROXY/HTTP_PROXY env vars. It is used for both DriveHub and the yup-oauth2 8 authenticator (with_client), so token refresh and API calls both go through the proxy.

Quality: Same code in #140 and #141. Panics: todo!() for any scheme other than http/https, and ALL_PROXY is read first and is often socks5:// (the exact setup quoted in #26), so those users get a panic on every command; .unwrap() on Uri/Url parsing panics on malformed values; a value without a scheme (proxy:3128) passes the check, but Url::parse then reads 'proxy' as the scheme and the proxy URI is wrong. Prints 'using system proxy <url>' to STDOUT, user:password included, twice per command (Auth::new and Hub::new each call create_client): that leaks the password and corrupts `files download --stdout` and list output read by scripts. Intercept::All, so NO_PROXY is ignored. hyper-proxy 0.9.1's default 'tls' feature brings in native-tls/OpenSSL: through the tunnel, the TLS session to googleapis.com is made by native-tls, not by the configured rustls connector, which only carries the plain hop to the proxy. That adds an OpenSSL dependency to a rustls static build and drops HTTP/2. proxy.rs is copied from ureq with about 10 unused Error variants and doc comments about ureq. It adds http 1.x and url only for parsing, the files lack trailing newlines, and the code is not rustfmt'd. hyper-proxy supports only hyper 0.14 and is unmaintained, so this approach does not survive the hyper 1.x / hyper-util migration.

Related issues: #21, #26, #59, #74. Superseded by: none.

dodomorandi's lock already has hyper-util 0.1.20, which ships the client-proxy and client-proxy-system features (not yet enabled). Tunnel<C> and SocksV5<C> both return C::Response, so a small enum connector (Direct \| Tunnel \| Socks over HttpConnector) implementing Service<Uri> yields one stream type. Hub and Auth then become DriveHub/Authenticator<HttpsConnector<ProxyConnector>>. Also add an explicit --proxy / config key, print diagnostics only to stderr with credentials redacted, and handle https:// proxies, which need TLS to the proxy itself. Neither fork has proxy support. Issue #151 sends users to this PR's binaries.

### #144 feat: add max parameter for drive list

Adds `drives list --max` (default 10) with pageToken pagination at pageSize <= 100. It also quietly replaces the prasmussen resumable-fix git fork with google-drive3 5.0.5+20240618 from crates.io, which brings google-apis-common 6.0.4, hyper-rustls 0.25, rustls 0.22, ring 0.17.8 and yup-oauth2 9.0.0. Other changes: the Hub connector becomes https_only with with_native_roots().unwrap(), the unused bytes dependency is removed, upload.rs/update.rs/file_helper.rs/mod.rs are reformatted with rustfmt, and six .idea/ IDE files are committed.

Quality: The pagination loop is a correct copy of files::list_files: MAX_PAGE_SIZE 100 matches the API cap, and usize cannot underflow because the loop stops once collected >= max. But the default stays 10, so #119 and #142 are only fixed when --max is passed; it should default to all drives. --max 0 sends pageSize=0. list_drives now takes the CLI print Config instead of a max argument. The Cargo.lock churn (+348/-172) is NOT just noise: it comes from dropping the git fork for google-drive3 5.0.5, a dependency migration hidden in a feature PR with an empty description. That migration silently loses the fork's resumable-upload retry fixes. .idea/ files (including a plugin userId) must not be committed. The rustfmt-only hunks are noise.

Related issues: #119, #142. Superseded by: #156 (smaller alternative for the same feature, reviewed separately).

Applies cleanly to upstream as a whole, Cargo.lock included. On dodomorandi only the .idea files apply. The fork's two resumable-fix commits (9d336ab 'Retry previous chunk' and 1f75444 'Retry if reading body fails') reset the chunk offset when a chunk upload is retried. google-apis-common 8.0.0, which dodomorandi uses, still runs `start += request_size` before the http_failure retry, and dodomorandi's delegate returns Retry::After on 5xx/429. So in dodomorandi a retried chunk may be skipped. This needs a fault-injection test. Neither fork pages drives.list; both return only the first page of 10.

### #146 Markdown file export

Adds FileExtension::Md / text/markdown, lists md among Google Docs export types and in the import extension map, and mentions md in the `files import` help text. 9-line diff in drive_file.rs + main.rs.

Quality: Correct and minimal for export (Drive has supported text/markdown export of Docs since 2024). Import is only half-done: md is added to IMPORT_EXTENSION_MAP but the upload still sends media Content-Type application/vnd.google-apps.document, so Drive has to sniff the content and probably imports it as plain text instead of converting Markdown (not verified against the live API). No tests.

Related issues: #143. Superseded by: #159.

It conflicts with dodomorandi only because dodo made the consts pub(crate) and wraps mimes in LazyLock statics, so a port is mechanical. Neither fork has Markdown support yet.

### #153 Arm build

Adds release_linux_arm.yaml, a copy of the linux release job with runs-on: ubuntu-latest-arm and output read from target/aarch64-unknown-linux-musl, and adds workflow_dispatch to the linux and macos release workflows.

Quality: The runner label ubuntu-latest-arm does not exist, so the job never starts; the author closed the PR and fixed this in #154. The workflow_dispatch triggers are broken: a manual run has no .release.upload_url in the event payload, so UPLOAD_URL becomes 'null' and the upload step fails. Still uses checkout@v3 and the deprecated actions/upload-release-asset@v1.

Related issues: #63, #79, #99, #101, #135. Superseded by: #154.

It applies cleanly to dodo because dodo kept the upstream workflow files unchanged. For how it overlaps with #130, #154 and #158, see the ARM section of pulls_3.md.

### #154 Add linux_arm workflow.

Adds release_linux_arm.yaml, which builds a static aarch64-unknown-linux-musl binary on the free native GitHub runner ubuntu-24.04-arm using the multi-arch clux/muslrust:stable docker image, then uploads gdrive_linux-arm64.tar.gz to the release.

Quality: Works. The author's fork ran it successfully for release 3.9.2 (gueraf/gdrive, 42 downloads of the arm64 asset). I downloaded that asset: it is an aarch64 ELF with no PT_INTERP or PT_DYNAMIC, so it is fully static. clux/muslrust:stable has amd64 and arm64 images (both pushed 2026-11-03). Downsides: it is the fourth copy-pasted single-target workflow, it keeps checkout@v3 and the deprecated upload-release-asset@v1, it has no cargo cache, and it covers neither armv7 nor macOS arm64.

Related issues: #63, #79, #99, #101, #135. Superseded by: none.

It overlaps with #130 (another agent's batch): #130 cross-compiles aarch64 and armv7 on x86_64 with clang and rust-lld, and adds macOS arm64. Upstream's release_macos.yaml runs on macos-latest, which is arm64 now, yet names the archive macos-x64. gueraf's gdrive_macos-x64.tar.gz is in fact an arm64 Mach-O (checked), so the fork must fix the labels.

### #156 Add configurable drive list max with pagination

Adds `drives list --max <n>` (Option<i32>). list_drives now pages through drives.list with page_size = min(max, 100) and next_page_token until it has max drives or runs out of pages, then truncates.

Quality: Small and readable, and it compiles. Problems: (1) the default is still 10 (max.unwrap_or(10)), so without --max the issue #119/#142 behaviour stays and you must pass a large number to see everything; (2) i32 accepts 0 or negative values, which give pageSize<=0 (an API error) or `as usize` wrap-around in truncate; (3) truncation only runs when max is Some, so with no flag and short pages it can return more than 10; (4) page_size is not reduced for the last page; (5) trailing whitespace (git apply warns about 6 lines).

Related issues: #119, #142. Superseded by: none.

Compared with #144 (another agent's batch): the pagination loop is nearly identical. #144 mirrors files::list (usize, remaining-aware page_size, default 10) but drags in .idea/ files, a Cargo.lock v4 bump and unrelated edits to upload.rs, update.rs and hub.rs. #156 is the cleaner diff and #144 has the better types; combine them. Neither dodo nor toutizes paginates drives.list.

### #157 Add TUI navigate mode

Adds a `gdrive navigate` ratatui/crossterm TUI (src/navigate.rs, 1503 lines): browse folders, download a single file with progress, upload a file or folder with progress through a picker, delete with y/n confirmation, and confirm before quitting during transfers. The PR also includes #158's Docker ARM scripts, a .cursorignore file, a README section and a Cargo.lock rewritten to lockfile v4 (ratatui 0.26.3, crossterm 0.27, proc-macro2/quote bumps).

Quality: The code is readable but has design and correctness problems. The whole UI runs in spawn_blocking and calls Handle::block_on for listing and deleting, so it freezes during network calls; transfers run on ad-hoc std::threads that also block_on. Any error from reload() (refresh, opening a folder, going back) propagates out and quits the TUI. files::delete prints 'Deleted ...' to stdout inside the alternate screen and corrupts it. Delete is permanent (not trash) and recursive for folders. Listing is silently capped at 1000 items. Folder download is refused, although the PR body says directories work, and Google Docs and shortcuts are unsupported. It duplicates download.rs's save/MD5 logic. There is no panic hook to restore the terminal, and retries are hard-coded to max_retries 100000. It bundles unrelated files.

Related issues: none. Superseded by: none.

Worth keeping: ProgressReader<R: Read+Seek>, which wraps the upload source to report bytes read, and the streaming progress counter in the download loop. That is a cheap way to add indicatif progress bars to the plain CLI (issues #12 and #44, the most-requested UX items). The lockfile churn conflicts with dodo's dependency migration.

### #158 Add Docker scripts for ARM static builds

Adds a trivial Dockerfile (FROM ${BASE_IMAGE}, WORKDIR /work), dockerbuild.sh (picks messense/rust-musl-cross:aarch64-musl or :armv7-musleabihf and builds a local image) and indockerbuild.sh (runs cargo build --release --target <t> in that image and copies the binary to dist/<t>/), plus README instructions. Local builds only, no CI.

Quality: It does what it says, and it is the only PR in this batch that covers armv7 (32-bit Raspberry Pi). The Dockerfile/image step adds nothing: you can `docker run` the messense image directly. The container runs as root, so target/ and dist/ end up root-owned on Linux hosts. There is no cargo registry cache volume, so every run downloads all crates again. Scripts sit in the repo root. Not exercised here: I did not build the images.

Related issues: #63, #79, #101, #131, #135. Superseded by: none.

The same files are also included in #157. #130 (another agent) does armv7 and aarch64 in CI by cross-compiling with clang and rust-lld. dodo uses rustls 0.23 with ring 0.17 and no aws-lc-rs, so zig/cross cross-compiles need only a C cross compiler.

### #159 Add support for Markdown file type and MIME type

Adds Md / text/markdown (export of Docs to .md, md in the import map) and fixes import mime handling: FileInfo gets source_mime_type, import.rs fills it from the file extension, and upload_file sends it as the media Content-Type while the metadata mimeType stays the Google Docs target type.

Quality: Complete and correct. Separating source and target mime is what the Drive API expects for conversions, and it also makes csv, docx, jpg and other imports send accurate media types instead of relying on content sniffing. Small diff, compiles cleanly, tested manually by the author for export and import. Nits: adding an Option field to FileInfo is a bit ad hoc (an explicit upload_mime parameter would be cleaner); the `files import` help text was not updated (#146 did that); ext.get_export_mime() is reused for an import-side lookup, so the name reads oddly.

Related issues: #143. Superseded by: none.

It conflicts with dodo in drive_file.rs, file_info.rs, file_tree.rs, import.rs and upload.rs, but 3-way conflicts are only context changes. dodo's import.rs and upload_file have the same single-mime logic as upstream, so the fix still applies conceptually. Neither fork has Markdown.

### #162 Recursive download: add export

`files download --recursive` now exports Google Docs, Sheets and Slides to docx, xlsx and pptx (new DocType::default_office_export_type) instead of silently skipping them. It also replaces 'build the whole FileTreeDrive, then download' with a streaming recursive walk (list a folder, download or export its files, recurse) that prints a summary at the end.

Quality: Works and compiles. Problems: (1) the file_tree_drive.rs changes (from_google_doc, is_google_doc, the mime_type field) are dead code, because the second commit stops using FileTreeDrive at all; the whole module is now unused, and no warning appears only because pub items in bin crates are not linted by default; (2) one failed export (for example Drive's 10 MB export limit) aborts the entire recursive download with `?`; (3) export is always on, with no flag to choose a format or opt out; (4) exports are redone on every run with no change detection; (5) a missing folder id reports Error::MissingFileName; (6) Drawings, Forms and shortcuts are still silently skipped; (7) '/' in names (#19) and long names (#53) are not handled; (8) the md5-mismatch warning was lost.

Related issues: #64, #123. Superseded by: #164.

Speed: the streaming walk starts the first download sooner and prints progress, but the total API calls and transfer time are unchanged (still sequential). Parallel recursive download is in PR #68 and toutizes' TaskManager. Upstream already has default_export_type (pdf/csv/pdf); office formats are better for backups because CSV keeps only the first sheet (#98). Pre-existing bug, also present in dodo: the temp file is file_path.with_extension("incomplete"), so a.pdf and a.txt share a.incomplete.

### #163 Add short flag for common CLI arguments

Adds clap short aliases: -m/--max, -q/--query, -p/--parent (list, upload, mkdir, import), -r/--recursive (download, upload, delete), -s/--chunk-size (upload, update).

Quality: Tiny and correct. I built a debug binary, where clap's debug assertions are active, ran --help on all seven touched subcommands, and found no short-flag collisions. -p matches gdrive v2 habits (issues #1 and #5). The PR body omits that delete also gets -r.

Related issues: #1, #5. Superseded by: none.

dodo has no short flags. toutizes has -r, -f and -s (see its main.rs lines 205-291).

### #164 Fix dependency update problem

Accidental PR of serebrov's fork main; the author says it was meant for his own repo. It is NOT a migration to google-drive3 7.x. It contains: (a) a Cargo.lock regenerated with `cargo update` inside the OLD semver ranges (lockfile v4; hyper 0.14.23 to 0.14.32, tokio 1.24 to 1.54, clap 4.0 to 4.6, 51 crates added, 30 removed, 133 bumped), keeping google-drive3 5.0.2-beta from the prasmussen git fork, hyper-rustls 0.23.2 and rustls 0.20, plus a direct `yup-oauth2 = "=8.1.0"` pin, so that `cargo update` no longer pulls in a second hyper-rustls (0.24) and breaks the build with the enable_http2 error from issue #131; (b) everything in #162, plus Google Drawings export (png/svg/pdf), shortcut handling (symlink when the target is in the tree, otherwise download or export it), '/' to '_' and 255-byte filename sanitizing, mtime preservation via the new filetime dependency, mtime-based skipping of unchanged exports, warnings instead of aborts for failed exports, a warning for unsupported google-apps types, and modifiedTime/shortcutDetails added to the files.list fields; (c) README replaced with the fork's own README.

Quality: The most complete recursive backup implementation among the PRs, and it compiles. Bugs: sanitize_filename computes MAX_FILENAME_BYTES - ext.len(), which underflows and panics when the 'extension' after the last '.' exceeds 255 bytes; exported names are sanitized and then get '.docx' appended, so they can reach 260 bytes and fail with ENAMETOOLONG; only '/' is replaced, not the characters Windows forbids. Shortcuts: downloaded_files maps only files, so a shortcut to a folder inside the tree downloads that folder a second time; shortcuts found while processing shortcuts are pushed after std::mem::take and silently dropped; shortcut targets are downloaded with no md5 check or skip, so they are re-downloaded every run; non-unix platforms only get a warning. Same-name collisions are not handled. The yup-oauth2 pin is a workaround, not a fix. The README is hijacked, with personal paths and OAuth client names.

Related issues: #19, #53, #64, #95, #123, #131. Superseded by: dodomorandi fork (real dependency migration: google-drive3 7.0, google-apis-common 8, hyper 1, hyper-rustls 0.27, yup-oauth2 12).

Speed: it skips re-exporting unchanged Docs (mtime match), which speeds up repeated backups, but it still MD5-hashes every existing local binary file on every run. Now that mtime is preserved, a size+mtime fast path before MD5 would make incremental runs much faster. It does nothing for parallelism. The pin explains issue #131: building on a host without the pinned lock, or after `cargo update`, mixes hyper-rustls 0.23 and 0.24.

