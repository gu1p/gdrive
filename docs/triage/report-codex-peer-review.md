# Independent peer review: glotlabs/gdrive modernization

Review date: 2026-11-04. This is an independent assessment of the supplied snapshots, not a reconciliation with another team's classifications.

I read `gdrive-data/CONTEXT.md` in full, all 31 `pulls/N.md` files including their complete diffs and discussion, and all 133 `issues/N.md` files including comments, in ascending numerical order. Sections D and E give a disposition for every supplied PR and issue. I inspected upstream and both forks, their manifests/lockfiles, and relevant cached dependency source. I did not run Cargo, change a repository, access private credentials, or benchmark transfers. The upstream successful build is supplied evidence (`gdrive-data/CONTEXT.md:247–248`), not a build I performed. Findings below distinguish source-confirmed defects from diagnoses that still need reproduction.

Citation shorthand, relative to the workspace:

| Prefix | Exact path |
|---|---|
| `U/` | `gdrive-src/` at `855155649a5911565c9134eac6a1f9b9bf235494` |
| `D/` | `gdrive-data/forks/dodomorandi/` at `ec37b03ab671474c037ca877120732ff0e5cfa75` |
| `T/` | `gdrive-data/forks/toutizes/` at `ce70446` |
| `I/` | `gdrive-data/issues/` |
| `P/` | `gdrive-data/pulls/` — citations are lines of the saved Markdown/full diff |
| `R/` | `/home/gp/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` |
| `G/` | `/home/gp/.cargo/git/checkouts/google-apis-rs-e87ae0754339ff1f/1f75444/` |

Google and platform constraints were checked against the primary sources linked below. The requested Chrome connection listed its blank tab but rejected opening a page under the session's approval policy; public documentation was therefore read through the web research tool. No browser page was opened or left behind.

## A. Modernization strategy

**Base the development fork on dodomorandi's pinned tree, with explicit release blockers. Do not ship that tree unchanged, and do not merge toutizes wholesale.** Keep upstream's locked build as the behavioral reference. The reason to choose dodomorandi is the coherent migration and useful ownership/error-handling work, not its 265-commit count or an assumption that newer dependencies fix transfers.

### Dependency evidence

These are resolved lockfile versions, rather than the minimum versions written in `Cargo.toml`:

| Component | Upstream | dodomorandi | toutizes |
|---|---|---|---|
| Drive bindings | `5.0.2-beta-1+20220225`, git `resumable-fix` | registry `7.0.0+20251218` | same git version as upstream |
| Common API support | git `5.0.2` | registry `8.0.0` | git `5.0.2` |
| Hyper / hyper-rustls | `0.14.23` / `0.23.2` | `1.11.1` / `0.27.9` | `0.14.23` / `0.23.2` |
| Tokio / yup-oauth2 | `1.24.1` / `8.1.0` | `1.53.1` / `12.1.2` | `1.24.1` / `8.1.0` |
| Backoff | `1.1.0` | `2.1.1` | `1.1.0` |

Evidence: `U/Cargo.lock:273,455,475,589,613,1415,1775`; `D/Cargo.lock:387,568,590,707,729,1658,2108`; `T/Cargo.lock:273,457,477,591,615,1428,1788`. dodomorandi explicitly declares Rust 1.88 and the modern HTTP adapter dependencies (`D/Cargo.toml:5–35`). Its lock is not identical to the latest ecosystem versions listed in CONTEXT; a second broad dependency update is unnecessary before establishing a baseline.

#131 is a dependency-resolution incompatibility involving two distinct `HttpsConnector` types, not proof that Rust or ARM intrinsically cannot compile gdrive (`I/131.md:175–216`). Conversely, upstream's supplied locked build succeeds. Preserve the lock, declare the supported compiler, build with `--locked`, and test dependency updates deliberately. PR #164 pins old yup-oauth2 and bundles unrelated behavior; it is not the modernization solution (`P/164.md:2465–2486`).

### A critical regression in the migrated dependency

**Dropping the old git fork loses an important retry correction.** The published common library reads an entire chunk, advances `start` on any received response, and then retries HTTP failures without restoring that offset (`R/google-apis-common-8.0.0/src/lib.rs:677–737`). The old fork explicitly subtracts `request_size` before retrying (`G/google-apis-common/src/lib.rs:734–744`). For example, a 503 after the first 32 MiB chunk makes the new helper construct its next request at 32 MiB, even if Google accepted zero bytes. This can skip data in the attempted request sequence; the exact server outcome still needs a fault-injection test. Do not claim an observed corrupted Drive file.

The helper also uses POST for status/chunk requests, assumes full acceptance of a 308 chunk, returns the last acknowledged offset without adding one in its status path, and unwraps local seeks/reads (`R/google-apis-common-8.0.0/src/lib.rs:600–644,677–716`). Its Range parser expects a `bytes ` prefix, whereas the documented resume response uses `bytes=...` (`:562–575`). These merit protocol tests against Google's documented PUT/Range behavior. The generated Drive client actually invokes this helper (`R/google-drive3-7.0.0+20251218/src/api.rs:13239–13281`), so this is not dead dependency code.

**Release gate:** repair and pin the common library with regression coverage, or implement a small Drive media transport using the modern shared client. Keep generated bindings for metadata. Test before releasing either option; merely copying the old rewind is insufficient for ambiguous partial acceptance.

### Benefits and behavior changes in dodomorandi

- Parent metadata is shared with `Arc<FolderInfo>` instead of copying ancestor `Folder` structures; traversal returns references. This reduces allocations while retaining the full-tree design (`D/src/common/file_tree.rs:44–57,76–81,165–172`; compare `U/src/common/file_tree.rs:84–110,141–148`). Tests cover traversal, ancestor handling, MD5 parsing, truncation and permission matching; they do not establish network-transfer reliability (`D/src/common/file_tree.rs:252,325`; `D/src/common/mod.rs:46–64`; `D/src/permissions/revoke.rs:302–413`).
- Native certificate loading now returns an error instead of unconditionally panicking in connector construction (`D/src/hub.rs:22–29`). Downloads use Tokio I/O, and stdin helper ownership keeps the temporary file alive (`D/src/files/download.rs:233–271`; `D/src/common/file_helper.rs:49–78`). Archive creation uses `create_new`, removing upstream's check-then-create race (`D/src/common/account_archive.rs:23`; `U/src/common/account_archive.rs:9–13`).
- **The async conversion needs a fix:** download consumes/drops `Md5Writer` to obtain its digest before any explicit flush, then renames the file (`D/src/files/download.rs:249–271`; `D/src/common/md5_writer.rs:26–28`). Tokio can still have pending file I/O when dropped. Await `flush`, expose delayed write failures, then close/finalize; add optional durable sync semantics. This is supported by [Tokio's File lifecycle documentation](https://docs.rs/tokio/latest/tokio/fs/struct.File.html), not an assumption that `write_all().await` implies durable completion.
- Symlink behavior changes: dodomorandi rejects file symlinks, but checks `is_dir()` first and still follows directory symlinks. Upstream has a duplicated, unreachable `is_file()` branch where its symlink check should be (`D/src/common/file_tree.rs:89–108`; `U/src/common/file_tree.rs:121–132`). Define and test a consistent policy before calling this fixed.
- Both trees still serialize transfers, materialize directory trees, silently omit native documents in recursive download, use colliding `.incomplete` paths, retain port 8085 and the same broad scopes, and ignore `--query` when a parent/drive option is present (`D/src/files/upload.rs:233–299`; `D/src/files/download.rs:163–205,241`; `D/src/common/file_tree_drive.rs:88–100`; `D/src/hub.rs:60–83`; `D/src/main.rs:512`). This is a useful base, not a completed product.

The diff from upstream spans 56 files, approximately 6,056 insertions/3,684 deletions including the lockfile. Preserve the original commit history and review behavior separately from formatting/error-enum churn. Reusing this migration is less work than repeating it; the release gates prevent treating its size as evidence of maturity. If the team cannot budget those gates, remain temporarily on the known upstream lock rather than releasing an unqualified dependency bump.

### Why toutizes is a source of design ideas only

Its task abstraction enables directory discovery and file transfers to overlap (`T/src/files/tasks.rs:17–50`; `T/src/files/download.rs:153–175`). Its name-based upload checks and dry-run concepts are useful. However:

- Every job is spawned and retained before acquiring its semaphore; only active execution is bounded. `JoinError`s are discarded (`T/src/files/tasks.rs:32–67`). Tasks retain a context containing an `Arc` to the manager which retains the tasks, forming a reference cycle (`T/src/files/upload.rs:78–93`; `T/src/files/download.rs:91–103`).
- Upload task failures are printed, but the command returns `Ok(())`; success never changes upload status to Completed (`T/src/files/upload.rs:72–75,327–335`). Download also returns success after failures; its error path does not increment the reported `num_errors` (`T/src/files/download.rs:61–64,75–88,255–263`).
- The default download Abort branch prints “skipped” but does not return; a differing existing file proceeds to overwrite (`T/src/files/download.rs:197–225`). Shortcuts are followed without consulting `follow_shortcuts`, with no cycle detection (`:142–149,238–245`).
- Sync deletes local files immediately after scheduling children; remote sync permanently deletes extras before child uploads succeed (`T/src/files/download.rs:168–183`; `T/src/files/upload.rs:204–225`; `T/src/common/drive_item.rs:329–336`). A child may create `foo.incomplete` before that deletion scan; it is not in `keep_names`. More seriously, native documents are filtered out of the remote listing, so a local counterpart can be mistaken for an extra and deleted (`T/src/common/drive_item.rs:90–107,139–153`).
- Native-document export is broken by a representation requiring binary MD5 metadata; import now calls ordinary upload without setting a Google document target MIME (`T/src/files/export.rs:35–59`; `T/src/common/drive_item.rs:90–107,266–276`; `T/src/files/import.rs:19–25`). Upload also removes stdin and ignores `print_only_id` (`T/src/main.rs:251–254`; `T/src/files/upload.rs:38`).

None of those tradeoffs is acceptable merely to obtain more parallelism.

## B. Ranked top 25

Ranks combine data safety, ability to run unattended, observed demand, and transfer benefit. Reactions are those in the supplied snapshot, not current voting totals or unique-user counts. S = localized change; M = several coordinated changes/tests; L = substantial subsystem/integration work. “Faster” distinguishes bandwidth/latency from avoiding repeated work; no speedup is claimed as measured.

| Rank | Issue/PR IDs | What to deliver | Why here; concrete evidence | Effort | Faster transfers? |
|---:|---|---|---|:---:|---|
| 1 | #131; PR #164; modern fork | Coherent locked dependency migration **with resumable-protocol repair** | Foundation for all work; new helper loses the old retry rewind. See A and `I/131.md:175–216`. Do not merge #164. | L | Indirect: reliable modern transport; no automatic throughput gain |
| 2 | #19, #53 | Safe, deterministic local names and unique atomic partial files | Existing remote names enter paths verbatim; `a.jpg` and `a.png` share `a.incomplete`. Parallelism would amplify corruption/overwrite hazards (`U/src/files/download.rs:121–126,296–318`). | M | No direct gain; prerequisite for safe concurrency |
| 3 | #129, #115, #121, #160; PR #96 | Current OAuth setup and scope/verification guidance | Repeated authentication blockers; stale app-name instructions and screenshots (`I/160.md:9`; `U/docs/create_google_api_credentials.md:13–31`). Distinguish policy failures from networking. | M | No |
| 4 | #147, #33, #94, #149, #125; PR #132 | Noninteractive auth failure, bounded login, configurable loopback port, headless guide | Cron hangs and stale port listeners defeat backups; killing processes is not a fix (`I/147.md:9–16`; `U/src/hub.rs:64–69,95–110`). | M | Indirect: avoids stalled jobs |
| 5 | #36, #87 | Upload recovery and persisted resumable sessions | Retryable 408/transport errors and restart requests; current URL exists only in memory, with 100,000 configured retries (`U/src/common/delegate.rs:82–121`; `U/src/files/upload.rs:40–49`). | L | Yes: avoids retransmitting completed chunks |
| 6 | #25, #78 | Download body retry and validated Range resume | A day-long HTTP/2 connection can die mid-body; existing code simply fails (`I/78.md:10–21`; `U/src/files/download.rs:189–200,308–311`). | L | Yes: preserves partial work |
| 7 | PR #82 | Bounded concurrent uploads across files | Clear sequential bottleneck; proposed `join_all` is unbounded and discards errors (`U/src/files/upload.rs:171–193`; `P/82.md:167–198`). | L | Yes: particularly many small files |
| 8 | PR #68 | Bounded concurrent downloads plus incremental discovery | Same bottleneck; PR disables single-file paths, drops checksums and ignores results (`P/68.md:157–170,228–263`). | L | Yes: multiple files and earlier first byte |
| 9 | #12, #44; PR #157 only as requirements | Accurate aggregate/file progress on stderr | #44 has 19 reactions and closed without an upstream implementation; #12 has five. Distinguish bytes sent from accepted, skipped and verified (`I/44.md:2,36`; `U/src/common/delegate.rs:75–79`). | M | No direct gain |
| 10 | #49, #37 | Auth-provider interface: service accounts, then scoped ADC | Largest explicit demand: 23 reactions for service accounts, four for ADC (`I/49.md:2`; `I/37.md:2,14–52`). Must respect ownership and quota constraints; see F. | L | No direct gain |
| 11 | #123, #64; PR #162 | Recursive native-document exports with explicit coverage | Current “recursive download” silently excludes documents (`U/src/common/file_tree_drive.rs:121–132`). Avoid false backup-complete claims. | L | No direct gain; pipeline shares concurrency |
| 12 | #26, #21, #59, #74, #151; PR #141 | Modern proxy support for both OAuth and Drive | Four reactions plus repeated token exchange hangs. Existing PR leaks raw proxy credentials and panics on schemes (`P/141.md:136–146,213–224`). | M | Indirect: connectivity; not a generic speed feature |
| 13 | #155, #117 | Compose parent/drive constraints with custom query | Source-confirmed silent query loss; expensive and potentially wrong downstream selection (`U/src/main.rs:475–480`; `I/117.md:11`). | S | Yes for filtered scans, not media bandwidth |
| 14 | #119, #142; PR #156 | Complete shared-drive pagination with positive limits / `--all` | Five combined reactions; source fetches just one default-sized page (`U/src/drives/list.rs:55–72`; `P/156.md:49–91`). Distinct from fixed file pagination. | S | No direct media gain |
| 15 | #63, #101, #135, #79; PRs #130, #154, #158 | Supported ARM release matrix and reproducible CI | Repeated installation blockers; five reactions on #63, four on #101. Fix compiler/version messaging too (`I/79.md:32`). | M | No network gain; native execution avoids emulation |
| 16 | #150, #51, #22 | Preserve/validate refresh tokens and explicit reauthorization | A null refresh token causes hourly failure, unlike the seven-day Testing rule (`I/150.md:12–14`). Do not overwrite a usable existing token with a response lacking one. | M | Indirect: unattended completion |
| 17 | #80, #112, #58 | Certificate-store diagnostics and supported custom CA configuration | Windows UnknownIssuer, missing Linux roots and corrupt macOS trust records are different failures (`I/80.md:23`; `I/112.md:11–18`). | M | No |
| 18 | #128, #122 | Windows runtime/OS support policy and clean-machine artifacts | Missing CRT DLLs are a packaging issue, not fixed by changing Rust HTTP crates (`I/128.md:9–25`; `U/.github/workflows/release_windows.yaml:25–35`). | M | No |
| 19 | #52; PR #113 | Stable machine output: real TSV plus JSON, propagated write errors | Demonstrated TabWriter transformation breaks scripting; also escape embedded tabs/newlines and avoid name truncation (`P/113.md:43–76,107–119`). | M | No |
| 20 | #152, #56 | Explicit skip/checksum modes and correct overwrite help | Real 1.2 TB recovery workload; recursive MD5 skip already exists, partial resume does not (`I/152.md:11–20`; `U/src/files/download.rs:166–168,406–422`). | S | Yes: avoids unnecessary transfers |
| 21 | #84, #16, #65, #55, #69, #116 | Safe one-way update/sync, with a separate deletion phase | #84 has eight reactions; duplicate names make naive rsync behavior unsafe. Reuse requirements, rewrite toutizes execution (`T/src/files/upload.rs:204–225`). | L | Yes: transfers only changed content |
| 22 | #143; PRs #159, #146 | Markdown conversion and separate source/target MIME types | Current API feature plus a broader import correctness flaw (`P/159.md:152–174,188–201`; `U/src/files/import.rs:27–50`). | M | No |
| 23 | #136, #15 | Explicit abuse acknowledgment and per-file failure policy | One flagged file stopped a large recursive job (#152); current API permits a restricted opt-in, not blanket bypass (`I/136.md:9–16`). | S | Indirect: prevents wasted reruns |
| 24 | #42, #86 | Local traversal policy for symlinks/special files and ignore rules | Broken symlink branch and inconsistent fork behavior; unknown `.sh` file is not evidence that shell MIME is unsupported (`U/src/common/file_tree.rs:121–132`). | M | Yes when ignored trees are pruned |
| 25 | #34, #13; #50/#111 follow-on | Resource-key-aware URLs and safe recursive shortcut handling | IDs alone can be insufficient; current field masks omit resource keys and recursive shortcut details (`U/src/files/list.rs:105–107`). Anonymous access is a separate feature. | M | No direct gain |

Also retain #43 (account discovery/export diagnosis), #72 (per-command account selection), #76 (secret-store integration), #73 (MIME output), #77 (completions), #14 (bulk deletion), #46 (explicit TLS diagnostics) and #93 (verified move-after-upload) as real backlog items. Account migration tests and release identity/checksums are mandatory release work even though they do not each consume a separate top-25 slot. Fold #106's documentation demand into the relevant deliverables, rather than leaving a vague documentation epic indefinitely open.

## C. Faster transfers

### Execution model

Use one Tokio runtime, one shared reusable HTTP client/auth provider, and a scheduler with separate bounded queues for metadata discovery and media jobs. Start with **four media transfers and two listing requests**, configurable through positive bounded CLI values. These are benchmark starting points, not Drive-prescribed limits. Tune upload/download separately. Acquire capacity before opening files or spawning jobs; bound pending work as well as active requests. A coordinator owns `JoinSet`/`FuturesUnordered` results and cancellation, avoiding the task/manager reference cycle in toutizes. Directory workers must not hold all permits while blocking to enqueue their children.

Create each upload parent before releasing its child jobs, but permit unrelated subtrees and files to overlap. Yield download jobs page by page instead of building the whole tree first. Keep a per-directory collision map, spilling inventory to a manifest when necessary. Listing, hashing and disk I/O need their own limits; do not equate runtime threads with transfer count as toutizes does (`T/src/main.rs:32–34,482–485,644,686`). Prefer a bounded blocking pool for CPU-heavy hashes; ordinary async file APIs still use blocking facilities underneath.

Each job returns a typed outcome: transferred, unchanged, explicitly excluded, unsupported, failed, or canceled. Await every handle. Any unhandled transfer failure yields a nonzero exit; `--continue-on-error` can finish independent jobs but still reports failure. Cancellation stops scheduling and preserves valid recovery state. Never print an uploaded ID before completion, as #82 does. Do not allow parallel binary streams to share stdout.

**Reuse:** dodomorandi's HTTP integration, typed errors, MD5 writer after its flush fix, and metadata/tree ownership improvements; toutizes' task/outcome and dry-run concepts; #68's bounded-stream idea. **Rewrite:** #82's execution loop, #68's download implementation, toutizes' TaskManager, sync deletion and error aggregation. Their unsafe semantics are documented in A/D. No supplied concurrency patch is ready to merge.

### Upload transport, chunks and recovery

Retain 32 MiB as the initial chunk default; benchmark 8/32/64 MiB with 1/2/4/8 concurrent files. Label units MiB. Limit aggregate buffered bytes, not just job count: the current common helper buffers each entire chunk synchronously (`R/google-apis-common-8.0.0/src/lib.rs:677–707`). Four 32 MiB jobs already imply roughly 128 MiB of payload buffers before other overhead; the existing 8 GiB option is unsuitable as an unconstrained concurrent default. A replacement transport should stream bounded replayable segments from disk and surface seek/read errors.

Separate the multipart-versus-resumable threshold from chunk size; upstream couples them (`U/src/files/upload.rs:242–245`). Use multipart for small files on reliable connections and resumable for larger or interruption-prone work. Within one resumable session, send chunks sequentially, using the server's accepted offset. File-level concurrency provides the main parallelism. Drive has no S3-style independently uploaded part-composition API to exploit here.

Persist a restricted-permission session record containing account/client identity, target ID/parent, source identity/size/mtime, session URL and last confirmed offset. Validate the source has not changed on restart. Probe the session after ambiguous failures, handle absent Range as zero accepted bytes, and treat the reported inclusive endpoint correctly. Refresh authorization when necessary without launching an interactive login during transfer. Remove state only after final metadata and available checksum/size checks succeed. Avoid duplicate creates after an uncertain final response: use pre-generated IDs where supported and reconcile completion before creating anew.

Google's upload protocol recommends multipart for files at most 5 MB, requires nonfinal resumable chunks to be multiples of 256 KiB, specifies PUT for subsequent data/status requests, and makes session URLs expire after a week. A 308 response's Range determines the accepted offset; a status-probe 404 requires restarting the expired session. Pre-generated IDs are not available for ordinary native-document conversion. These are protocol constraints, not performance settings. [Google upload guide](https://developers.google.com/workspace/drive/api/guides/manage-uploads)

Replace the status-only retry predicate and 100,000-retry configuration with bounded elapsed-time/attempt budgets, jitter, Retry-After support and reason-aware classification. Retry transient transport/408/429/5xx failures and appropriate 403 rate-limit reasons; stop on permission, policy, storage or daily-limit errors. Reset consecutive retry state after successful progress and slow all workers on shared throttling. A resumable protocol error must reconcile accepted bytes, not merely replay a blindly advanced offset. [Google error handling](https://developers.google.com/workspace/drive/api/guides/handle-errors)

Stdin currently spools the entire input to a temporary file (`U/src/files/upload.rs:62–68`); call that out. Preserve the spool lifetime and place it on a configurable filesystem with enough space. A future true streaming mode needs a replay strategy and a clearly documented restart limitation, rather than pretending arbitrary stdin is seekable.

### Download transport and integrity

First implement file-level concurrency and robust sequential Range resume. Use a unique, exclusively created partial file in the destination directory, keyed by Drive ID/version rather than replacing the extension. Maintain a sidecar with source identity/version/checksum and completed byte count. Resume only the same remote content; validate 206 and Content-Range. If a server returns 200 to a Range request, safely restart instead of appending. Handle 416 by rechecking size/version and possible completion. Flush and close before committing; use platform-appropriate atomic replacement or no-clobber behavior. Check expected byte count even when MD5 is unavailable.

Only then trial **two to four nonoverlapping ranges for a large, unchanged binary file**, sharing the global request/buffer budget. Use positional writes or independent bounded part files, validate each range and hash the complete file in order before commit. Reject changed versions; do not mix fragments from successive revisions. More ranges increase request cost and disk contention, so leave this opt-in until benchmarks show benefit. A revision-pinned request may require retention changes; do not silently mark revisions Keep Forever.

Blob downloads support Range; Workspace document exports do not. `files.export` has a 10 MB export limit. Native documents require explicit supported export formats and completion accounting; unsupported formats cannot disappear from a successful backup summary. [Google download/export guide](https://developers.google.com/workspace/drive/api/guides/manage-downloads) Evaluate the newer `files.download` long-running operation separately for supported exports and Vids; verify its limits and returned download capabilities instead of promising a blanket size-limit bypass or assuming #162 implements it. [Long-running operations](https://developers.google.com/workspace/drive/api/guides/long-running-operations)

Build a reversible name mapping: reject absolute/parent components, account for slash/backslash, Windows reserved names and trailing dots/spaces, component byte limits, case folding, Unicode and collisions introduced by export extensions. Resolve duplicate Drive names with stable ID suffixes or explicit conflict errors. Constrain destination traversal against existing symlinks, including partial files; canonicalizing the root alone is insufficient. Keep the mapping in the manifest so sync does not guess identity from a lossy name. #164's slash replacement and truncation are not adequate (`P/164.md:3289–3309`).

### Listing, incremental work and progress

Use purpose-specific field masks including `nextPageToken`, `incompleteSearch`, ID, name, MIME, size, checksum, version/modified time, shortcut target and resource keys. Do not sort an internal transfer listing unless order matters. Keep API page size separate from a user's total limit: files pages cap at 1,000; shared-drive pages cap at 100 (default 10). Continue while a next token exists, including short/empty pages. For shared-drive-wide searches use the appropriate `driveId`/corpus, with parent restrictions only when actually requested. Treat `incompleteSearch` as incomplete inventory, never permission to delete extras. [files.list](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/list), [drives.list](https://developers.google.com/workspace/drive/api/reference/rest/v3/drives/list)

`--drive` currently means a root-parent query rather than a whole-drive corpus, and `.or(...)` discards the custom filter (`U/src/files/list.rs:161–166`; `U/src/main.rs:475–480`). Specify the intended semantics, combine predicates with parentheses and correct escaping, and keep defaults from accidentally adding `'root' in parents` to a different parent selection. #117 also contains `useDomainAdminAccess` inside `q`; preserving that string should expose its invalid syntax, not magically make it valid.

For repeated sync, persist an ID-based inventory and eventually use the Changes API with durable cursors; handle expired cursors with a full rescan. Initial sync should first be copy/update-only. Deletion is a separate explicit phase after a complete successful inventory and all required transfers. `--dry-run` must show the same planned actions. Incomplete listings, unsupported files, name ambiguity, inaccessible subtrees or transfer failures suppress deletion. Quarantine/trash is preferable to immediate permanent removal.

Progress should be event-driven and throttled: listing count, queued/active/completed files, confirmed upload bytes, written download bytes, skipped bytes, retry state and failures. Totals may be unknown during discovery. Send terminal progress to stderr, disable animation off-TTY, and offer structured events. A reader's seek position is not upload progress: size-probing can seek to EOF before sending anything, as #157's tracker permits (`P/157.md:2022–2031`).

### HTTP stack and quotas

Share pooled Hyper 1 clients; use the same proxy/CA policy for OAuth and Drive, with redaction and NO_PROXY support. Set connect, header, idle-body and whole-operation timeouts separately. Recover dropped HTTP/2 streams/connections. Upstream already enables HTTP/2 (`U/src/hub.rs:33–40`), so the dependency bump alone neither introduces multiplexing nor guarantees speed. The #78 workaround of disabling HTTP/2 is a diagnostic fallback, not the default fix. Keep generated metadata APIs; replace only deficient media behavior instead of rewriting the entire CLI around another client without evidence.

**Current quota model matters.** Google's page says projects created from May 1, 2026 use quota units, while projects active in November 2025–April 2026 retain prior quotas: 1,000,000 units/minute/project and 325,000/minute/user/project, with list calls costing 100 units and downloads 200. It also lists a 1 TB/day/project egress threshold and 400 million daily quota units, with billing details still forthcoming. Workspace upload/copy limits remain 750 GB/user/day and a 5 TB upload maximum, with the documented first-oversized-file exception. Inspect the actual project's quotas; do not hard-code the older 12,000 requests/minute rule or promise unlimited free egress. [Drive usage limits](https://developers.google.com/workspace/drive/api/guides/limits)

Batching can help independent metadata/delete operations, at most 100 calls per batch, but does not support media upload/download/export and does not turn multiple operations into one quota charge. It is not the solution to #68/#82. [Drive performance and batching](https://developers.google.com/workspace/drive/api/guides/performance)

Before selecting defaults, measure time to first byte and completion, useful throughput, peak RSS, open files, request/quota counts and retries on: many tiny files, mixed/deep folders, large binaries, a slow disk, high RTT, throttling, mid-body resets and unchanged reruns. Run each against the same account/dataset and repeat enough to distinguish variance. #82's reported 1 Gbps is an author anecdote (`P/82.md:15`), not a controlled comparison or a target every Drive account can attain.

## D. Every PR: merge, rework or reject

“Merge as-is” means accept the change's substance without redesign, after a mechanical rebase and appropriate checks on the chosen base; none was actually merged or compiled in this review. Rework includes a full rewrite where stated. All **18 open PRs** are marked **open** below; six are already merged and seven closed unmerged.

| PR | Snapshot | Decision and reason |
|---:|---|---|
| 32 | Merged | **Retain history, replace advice:** putting the project ID into the app name is not a general OAuth requirement; current setup needs a rewrite (`P/32.md:39`; `U/docs/create_google_api_credentials.md:13`). |
| 38 | Merged | **Already accepted:** README typo; no new work (`P/38.md:1`). |
| 41 | Merged | **Retain and repair:** stdin is implemented by spooling; preserve temporary-file ownership in update helpers, as dodomorandi does (`P/41.md:145–164`; `U/src/common/file_helper.rs:20–24`; `D/src/common/file_helper.rs:57–78`). |
| 54 | Merged | **Retain intent, refresh wording:** production versus Testing token lifetime matters; it does not remove all verification requirements (`P/54.md:37–38`). |
| 66 | Merged | **Retain and correct:** useful usage documentation, but wrong export syntax, limited default listing and silent native-document omissions need explanation (`P/66.md:52,61`; PR #138). |
| 67 | **Open** | **Rework:** useful trash/untrash; filter trash server-side, request fields actually displayed, remove duplicated/dead PatchFile code and test role constraints; current list mask omits `trashed` (`P/67.md:122–140,161–165,213–228,345–360`; `U/src/files/list.rs:107`). |
| 68 | **Open** | **Rewrite:** error results discarded, MD5 disabled, single-file and shortcut paths commented out; retain bounded concurrency concept only (`P/68.md:157–170,228–263`). |
| 75 | Closed | **Reject:** accidental README deletion, acknowledged by author (`P/75.md:16`). |
| 82 | **Open** | **Rewrite:** unbounded open files/futures, panic on open error, ignored transfer results and premature IDs (`P/82.md:167–198`). |
| 96 | **Open** | **Rework into current auth guide:** embedding old screenshots does not fix stale policy/UI instructions (`P/96.md:45–72`). |
| 104 | Merged | **Already accepted:** preserves requested total above 1,000; do not re-solve #57/#102/#103 (`P/104.md:39–40`). |
| 113 | **Open** | **Rework, small:** bypassing TabWriter is right; define escaping/output-format semantics, preserve full names and propagate write errors (`P/113.md:43–76,107–119`). |
| 126 | Closed | **Defer/rework:** optional folder-size command could help, but recursive work for each list row is expensive and errors become blank values; do not make normal listing slower (`P/126.md:54,81–89`). |
| 127 | Closed | **Reject as superseded:** old compiler lifetime workaround and unrelated ignore entry; selected base uses a different representation and an explicit MSRV (`P/127.md:37`; `D/Cargo.toml:5`). |
| 130 | **Open** | **Rework:** best combined ARM target proposal; validate actual cross/native toolchains, use locked builds and supported/pinned actions; consolidate #154/#158 (`P/130.md:42–49,83–106,152,209`). |
| 132 | **Open** | **Merge as-is:** useful loopback SSH tunnel instructions; preserve an explicit 8085 option if the default port becomes dynamic (`P/132.md:33–38`). |
| 137 | **Open** | **Merge as-is:** Client ID typo only (`P/137.md:26–27`). |
| 138 | **Open** | **Merge as-is:** fixes `gdrive files export` syntax (`P/138.md:26–27`). |
| 140 | Closed | **Reject as superseded by #141's proposal:** do not revive its old proxy stack; same redesign applies. (`P/140.md:1`; `P/141.md:125–154`). |
| 141 | **Open** | **Rewrite:** good requirement and coverage of both clients, but raw credential-bearing URI logging, unwrap/todo failures, old Hyper and no bypass rules (`P/141.md:110–154,213–224,255–261`). |
| 144 | Closed | **Reject whole diff:** pagination is useful but #156 is narrower; this one mixes IDE files and a Drive 5.x migration (`P/144.md:1305,1381–1429`). |
| 146 | **Open** | **Rework/consolidate into #159:** Markdown export mapping is useful; mapping alone does not correctly separate import media and target types (`P/146.md:43`; `P/159.md:152–174`). |
| 153 | Closed | **Reject as superseded:** invalid runner name `ubuntu-latest-arm`; #154 corrects the direction (`P/153.md:51`). |
| 154 | **Open** | **Rework/consolidate with #130:** native `ubuntu-24.04-arm` is sensible, but mutable image, release action and locked-build issues remain (`P/154.md:36,49,56`). |
| 156 | **Open** | **Rework, small and high value:** validate nonnegative/positive semantics, offer all-results behavior and size remaining pages correctly; current `i32` permits invalid limits (`P/156.md:49–91,108–110`). |
| 157 | **Open** | **Reject current large PR; defer feature:** duplicates transfer paths, has misleading seek-based progress, incomplete folder handling and terminal-lifecycle hazards; rebuild optional TUI on shared events later (`P/157.md:855–859,1556,1657–1659,2022–2031`). |
| 158 | **Open** | **Rework/consolidate:** useful static ARM build tooling; pin image/toolchain, build locked and validate shell input in the run wrapper too (`P/158.md:70–73,98–109`). |
| 159 | **Open** | **Rework, preferred Markdown patch:** preserve source/target MIME separation, split it from extension support, test other imports/update and both upload paths (`P/159.md:119,152–174,188–201`). |
| 162 | **Open** | **Rework:** useful recursive export and earlier discovery; add formats, collision mapping, size/error policy and explicit unsupported outcomes; remove unused alternate tree machinery (`P/162.md:42–47,92–115,189–218,248–258`). |
| 163 | **Open** | **Merge as-is:** conventional short flags with no collision found in supplied CLI; run argument-parsing/help checks after rebasing (`P/163.md:1,94`). |
| 164 | Closed | **Reject whole diff:** author says accidental; old dependency pin plus mixed export/shortcut/mtime/name changes, not a modern migration; mine individual requirements only (`P/164.md:22,2486,3002–3009,3289–3309`). |

## E. Tracker cleanup and complete issue ledger

Close duplicates **only after transferring their distinct reproductions and demand to the named canonical issue**. “Support” means answer with the precise current command/policy and then close; it does not mean ignore documentation defects. Preserve valid feature requests even when low priority. For already closed issues, “history” means do not import a new open ticket, not that every closure proved a fix.

The following currently open tickets can be closed as already implemented: **#23, #57, #102**. In #23 explicitly document that stdin is spooled, not streamed directly.

Currently open support/nonactionable tickets to answer and close: **#6, #28, #30, #39, #40, #45, #48, #61, #62, #70, #88, #89, #90, #95, #98, #109, #134, #145, #148**. #134 is an external UI/consistency report without evidence of an incorrect CLI request; retain it as a reference and reopen if API-level evidence emerges.

Currently open duplicate/consolidation tickets to close after migration: **#15, #16, #21, #24, #33, #35, #55, #56, #59, #60, #64, #69, #71, #74, #79, #101, #111, #114, #115, #116, #117, #121, #135, #142, #151, #160, #161**. The destinations and caveats are in the ledger. This is **49 closures among the 92 currently open issues**, leaving 43 canonical/actionable backlog tickets; the closed #44 and #53 findings are carried into #12/#19 rather than lost. No tracker was actually changed.

Codes: **Keep** = actionable canonical item; **Backlog** = valid lower-priority work; **Merge →** = consolidate then close; **Support** = answer/close; **Done** = implementation exists; **History** = already closed, retain context. Every supplied issue is listed below in numerical order, with its own evidence anchor.

| Issue | Disposition | Independent assessment / preserved detail |
|---:|---|---|
| 1 | History | Parent-ID upload added in alpha8; command migration question (`I/1.md:17`). |
| 2 | History | Old whole-body/OOM large-download problem resolved by streaming; different from interrupted streams in #25 (`I/2.md:17`). |
| 3 | History | Document export added/fixed in 3.4 (`I/3.md:25`). |
| 4 | History | Delete help typo fixed (`I/4.md:1`). |
| 5 | History | Old short `-p` usage; preserve ergonomics through PR #163 (`I/5.md:1`). |
| 6 | Support → #106/#63 | Windows installation solved in comments; move ARM demand into release work (`I/6.md:42–50`). |
| 7 | History | `mkdir` implemented in 3.2 (`I/7.md:1`). |
| 8 | History | Recursive operations introduced in 3.3; later correctness gaps remain separate (`I/8.md:1`). |
| 9 | History + regression case | Reported size is `i64::MAX`, not proof of a real >5 TB file; keep seek/size detection regression (`I/9.md:1`). |
| 10 | History | Requested move/copy behavior implemented; use current subcommand docs (`I/10.md:32`). |
| 11 | History | `mkdir --print-only-id` implemented (`I/11.md:33`). |
| 12 | Keep | Canonical transfer progress, absorbing #44's demand (`I/12.md:2`). |
| 13 | History + #34 work | Top-level shortcut/shared-drive improvements landed; recursive coverage is still incomplete (`I/13.md:27`). |
| 14 | Backlog | Multiple-ID deletion with bounded concurrency/batching and per-ID results; not media acceleration (`I/14.md:9`). |
| 15 | Merge → #136 | Same abuse acknowledgment need (`I/15.md:1`). |
| 16 | Merge → #84 | Preserve upload as well as download sync requirements and nonunique-name discussion (`I/16.md:15,41–60`). |
| 17 | History | Verification support; old advice that verification does not matter must not become current policy (`I/17.md:23,32`). |
| 18 | History + #65/#42 | Upload creates new directories; updating a directory also needs early validation rather than a library panic (`I/18.md:22`). |
| 19 | Keep | Canonical filename/path/collision handling; absorb closed #53 (`I/19.md:13–25`). |
| 20 | History | Antivirus interaction without a reproducible CLI defect (`I/20.md:1`). |
| 21 | Merge → #26 | Browser success followed by token-exchange timeout; preserve networking diagnosis, do not call it verification (`I/21.md:9–19`). |
| 22 | History + #129 | Seven-day Testing grants; obtain a fresh grant after policy/setup changes (`I/22.md:55`). |
| 23 | Done | PR #41 supports omitted-path stdin; mention disk spool and no required `-` operand (`I/23.md:1`). |
| 24 | Merge → #129 | Stale consent/warning documentation report; insufficient detail for a separate code bug (`I/24.md:1`). |
| 25 | Keep | Canonical interrupted download/retry/resume (`I/25.md:9–15`). |
| 26 | Keep | Proxy plus network diagnostics; SOCKS demand in comments must get an explicit support/error decision (`I/26.md:9,15`). |
| 27 | History | Full-name/separator options exist; actual tab transformation remains #52 (`I/27.md:1`). |
| 28 | Support | Append rows/preserve Sheet formatting is Sheets API work, not ordinary Drive media replacement (`I/28.md:9–11`). |
| 29 | History | Language-choice discussion (`I/29.md:1`). |
| 30 | Support → #125 | Resolved remote setup plus old PATH/command confusion; publish supported headless instructions (`I/30.md:27,72`). |
| 31 | History | Large CSV conversion hits document/service limits; do not promise larger upload chunks solve conversion (`I/31.md:26–49`). |
| 33 | Merge → #147 | Explicit unattended reauth hang followed by port conflict; not merely local misconfiguration (`I/33.md:9–17`). |
| 34 | Keep | Resource keys and link/ID parsing; distinguish anonymous download requirement (`I/34.md:18,30`). |
| 35 | Merge → #129 | Verification/personal-use explanation, not a separate transport defect (`I/35.md:17`). |
| 36 | Keep | Upload 408 needs recovery policy and protocol testing (`I/36.md:10–15`). |
| 37 | Keep | Scoped ADC/external credential providers; preserve explicit Drive scopes requirement (`I/37.md:14–52`). |
| 39 | Support | Thanks only; acknowledge and close (`I/39.md:1`). |
| 40 | Support → #106/#84 | Explain file update versus directory sync; preserve early directory validation requirement (`I/40.md:1`). |
| 42 | Keep | Unknown local file type; investigate symlinks/special files, not extension guesses (`I/42.md:14`; `U/src/common/file_tree.rs:121–132`). |
| 43 | Keep, needs repro | Reporter says `account current` matches; inspect missing-token discovery/config mismatch before blaming literal placeholder input (`I/43.md:16`; `U/src/app_config.rs:42–46`). |
| 44 | History → #12 | Closed because reporter moved to another tool, not because progress shipped; preserve 19 reactions (`I/44.md:2,36`). |
| 45 | Support | Delete by ID, not name; discussion confirms it works (`I/45.md:9,34`). |
| 46 | Backlog | Explicit TLS key logging is a valid diagnostic feature; sensitive and off by default (`I/46.md:9`). |
| 47 | History | PATH/install issue solved; remote-auth follow-ups belong in headless docs (`I/47.md:27`). |
| 48 | Support → #106 | Explain move/rename and shell expansion; do not repeat unsafe unquoted delete-after-upload recipes (`I/48.md:1`). |
| 49 | Keep | Service accounts, highest recorded feature demand; no assumption of free personal Drive storage (`I/49.md:2`). |
| 50 | Backlog | Public downloads without OAuth are a distinct auth/API-key mode, not a missing `copy` flag (`I/50.md:2,15`). |
| 51 | History + #150 | Tokens disappeared/replaced; preserve safe persistence/reauth regression (`I/51.md:20`). |
| 52 | Keep | Genuine TabWriter output bug, demonstrated after shell quoting was corrected (`I/52.md:13–23,71`). |
| 53 | History → #19 | Reporter fixed their own fork; upstream long-name failure remains (`I/53.md:51–53`). |
| 55 | Merge → #65 | Name-based overwrite/upsert, not “Drive must forbid duplicate names” (`I/55.md:9`). |
| 56 | Merge → #152 | Existing recursive checksum skip needs docs; partial resume remains #25 (`I/56.md:29`). |
| 57 | Done | File-list 1,000-total cap fixed by #104 (`I/57.md:1`; `P/104.md:39–40`). |
| 58 | History → #112 | Corrupt macOS trust settings; retain the diagnostic signature (`I/58.md:13`). |
| 59 | Merge → #26 | Successful browser auth, terminal network failure and missing proxy support (`I/59.md:12,18`). |
| 60 | Merge → #129 | OOB-blocked flow/possibly stale binary; do not reinstate OOB (`I/60.md:15–18`). |
| 61 | Support | `files info` exposes MIME; separate list-column enhancement is #73 (`I/61.md:25`). |
| 62 | Support → #106 | Write accurate v3 examples; copying v2 documentation would introduce wrong syntax (`I/62.md:9`). |
| 63 | Keep | Canonical ARM artifact/build work (`I/63.md:2,17–24`). |
| 64 | Merge → #123 | Whole-Drive evacuation needs documented native-file coverage; recursive blob download is not a full backup (`I/64.md:1`). |
| 65 | Keep | Parent-scoped upsert by name, refusing ambiguity; update by ID already exists (`I/65.md:17,27`). |
| 69 | Merge → #84 | Recursive update/sync; preserve upload-specific requirement (`I/69.md:1`). |
| 70 | Support → #72/#106 | Windows system-profile config explained; a global world-readable credential directory is not the fix (`I/70.md:15`). |
| 71 | Merge → #125 | Headless loopback mismatch; local export workaround confirmed (`I/71.md:13,56`). |
| 72 | Backlog | Accounts already add/switch; a per-command account option avoids global-switch races (`I/72.md:1`). |
| 73 | Backlog | Distinguish native document MIME types in listing (`I/73.md:16`). |
| 74 | Merge → #26 | Same proxy requirement (`I/74.md:1`). |
| 76 | Backlog | Credential-store integration useful; prefer explicit provider interface over arbitrary shell commands by default (`I/76.md:9–13`). |
| 77 | Backlog | Generate completions from Clap; small legitimate usability work (`I/77.md:9`). |
| 78 | Keep, linked #25 | Preserve HTTP/2 `max_age` long-running failure as a distinct regression case (`I/78.md:10–21`). |
| 79 | Merge → #63/#131 | Lifetime error also occurs on NetBSD/x86 with old Rust; document MSRV, not ARM-specific patch folklore (`I/79.md:32`). |
| 80 | Keep | Windows UnknownIssuer on OAuth and API calls, distinct from missing CRT (`I/80.md:23,30`). |
| 81 | Keep | Concrete wrong help URL; localized documentation fix (`I/81.md:9–14`). |
| 83 | Backlog | Explain scopes versus ACLs; a folder CLI argument does not enforce a folder-only OAuth boundary (`I/83.md:13–15`). |
| 84 | Keep | Canonical one-way sync/unchanged-file semantics; retain eight reactions (`I/84.md:2,9–11`). |
| 85 | History | Upload to shared-drive root uses parent ID; improve wording (`I/85.md:18–22`). |
| 86 | Keep | Ignore rules prune discovery and avoid unwanted backup data (`I/86.md:9`). |
| 87 | Keep, linked #36 | Cross-process upload resume is missing, separate from retries within one invocation (`I/87.md:11`). |
| 88 | Support | Go/v2 command syntax and web-server credential ownership; not a Rust v3 regression (`I/88.md:12–21`). |
| 89 | Support | Document `root` alias and ID-based move examples (`I/89.md:11`). |
| 90 | Support | Thanks only (`I/90.md:9–10`). |
| 91 | History | Invite link updated in discussion (`I/91.md:16–17`). |
| 92 | History | Personal Drive works; Cloud project is API/OAuth configuration (`I/92.md:15`). |
| 93 | Backlog | Opt-in local removal only after verified successful upload, never directory-wide cleanup on partial failure (`I/93.md:10`). |
| 94 | Keep, linked #147 | Configurable listener port and useful bind diagnostics; port conflict need not mean expired credentials (`I/94.md:14,20–23`). |
| 95 | Support | “Shared with me” folders differ from shared drives; use folder ID/parent query and direct download; comments confuse them (`I/95.md:11,17–32`). |
| 97 | History, linked #43 | Account export/name confusion without a demonstrated archive-format defect (`I/97.md:9`). |
| 98 | Support | CSV/TSV export is first-sheet only; use workbook export or Sheets API for another tab (`I/98.md:12–15`). |
| 99 | History → #63 | Reporter abandoned tool; closure does not mean ARM artifacts exist (`I/99.md:9–18`). |
| 100 | History | Shared folder downloads by ID confirmed working (`I/100.md:13–17`). |
| 101 | Merge → #63 | Preserve four reactions and native-build demand; do not recommend emulation as final support (`I/101.md:2,23–30,55–63`). |
| 102 | Done | Explicitly fixed by #104 (`I/102.md:17`). |
| 103 | History | Same resolved file-list total cap (`I/103.md:14`). |
| 105 | History | Upload `--parent` question answered (`I/105.md:20–22`). |
| 106 | Keep | Documentation worklist: installation, IDs/root/shared folders, auth, format limits, stdout and skip semantics (`I/106.md:9–15`). |
| 107 | History + release gate | Maintainer confirms retagging; future tags/artifacts must be immutable (`I/107.md:9–13`). |
| 108 | History → #63 | ELF execution failure, solved by native build; title misleadingly suggests archive format (`I/108.md:16–25`). |
| 109 | Support → #125/#129 | Web-client redirect mismatch and remote localhost are separate cases; choose Desktop credentials (`I/109.md:9–10,21,27–43`). |
| 110 | History | `drives list` lists shared drives, not personal root files (`I/110.md:12–16`). |
| 111 | Merge → #50 | Real anonymous-download feature despite incorrect `files copy URL` example (`I/111.md:9`). |
| 112 | Keep | Canonical native-root loading errors; do not bypass certificate verification (`I/112.md:11–18`). |
| 114 | Merge → #125 | Do not recommend public DNAT of the loopback OAuth listener; use SSH tunnel/import (`I/114.md:25–34`). |
| 115 | Merge → #129 | Preserve exact `invalid_request` examples; cause not established merely by “Google changed policy” (`I/115.md:12–18,38–41`). |
| 116 | Merge → #84 | Same missing sync request (`I/116.md:9,39`). |
| 117 | Merge → #155 | Real ignored-filter bug plus an invalid query term; both need accurate diagnostics (`I/117.md:11–13`). |
| 118 | History | Empty/misspelled shell variable consumed the positional path as parent; quote examples and validate inputs (`I/118.md:36–38`). |
| 119 | Keep | Canonical shared-drive pagination, PR #156 (`I/119.md:9`). |
| 120 | History | DNS failure; comment's test-user procedure cannot establish causation for DNS resolution (`I/120.md:29,35–40`). |
| 121 | Merge → #129 | Blocked consent, preserve error rather than assume bypass possible (`I/121.md:9–10`). |
| 122 | History → #128 | Unsupported old Windows target/support-matrix question (`I/122.md:9–12`). |
| 123 | Keep | Canonical recursive native-document export (`I/123.md:10–14`). |
| 124 | History | Wrong binary/PATH: reported Go 2.1.1, not Rust 3.9.x (`I/124.md:9–19`). |
| 125 | Keep | Canonical supported headless auth; do not implement deprecated manual OOB just because requested (`I/125.md:9–10`). |
| 128 | Keep | CRT deployment and architecture diagnostics; reproduced on fresh Windows machines (`I/128.md:9–25`). |
| 129 | Keep | Canonical current OAuth policy/setup guide and diagnostic matrix (`I/129.md:9–31`). |
| 131 | Keep | Locked dependency migration/build matrix; actual incompatible connector versions (`I/131.md:175–216`). |
| 133 | History | Reporter isolates PDF/pageless export behavior on Google's side; preserve format-specific diagnostic, not broad workaround (`I/133.md:33–35`). |
| 134 | Support / external | UI retains ghost of permanently deleted file; no CLI request defect shown, preserve repro for escalation (`I/134.md:9–19,35–41`). |
| 135 | Merge → #63 | ARM artifact/loader issue; do not distribute an unverified binary from a comment as official release (`I/135.md:18,22–26`). |
| 136 | Keep | Explicit `acknowledgeAbuse` support and continued-error accounting (`I/136.md:9–16`). |
| 139 | History → #125/#150 | Export after successful consent; imported token may still need different scopes/refresh handling (`I/139.md:9–13`). |
| 142 | Merge → #119 | Same ten-shared-drive default truncation (`I/142.md:9`). |
| 143 | Keep | Markdown import/export now supported by API; PR #159 preferred starting point (`I/143.md:9`). |
| 145 | Support / discussion | Maintenance/governance discussion; answer with fork ownership and contribution process (`I/145.md:9–23`). |
| 147 | Keep | Canonical fail-fast unattended operation; never overwrite another process's listener (`I/147.md:9–16`). |
| 148 | Support → #106 | Publish factual v2/v3 capability matrix; avoid “surpassed” claim (`I/148.md:9`). |
| 149 | History → #147/#94 | Killing gdrive cleared its stale listener; underlying unattended-auth behavior remains (`I/149.md:21–26`). |
| 150 | Keep | Null refresh token; investigate prior grants/persistence before labeling Windows-specific library bug (`I/150.md:12–14`). |
| 151 | Merge → #26/#147 | Browser callback succeeded but token exchange stalled; keep timeout/proxy diagnostics, cause unproven (`I/151.md:9,15–20`). |
| 152 | Keep | Explain existing checksum skip accurately and expose explicit modes (`I/152.md:11–20`). |
| 155 | Keep | Canonical custom-query composition defect (`I/155.md:9–18`). |
| 160 | Merge → #129 | Current app-name/setup failure; remove unsupported project-ID naming folklore (`I/160.md:9`). |
| 161 | Merge → #125 | Callback forwarding over SSH can be documented carefully; protect authorization-code URLs from shell history/logs (`I/161.md:11–17`). |

Do not close #43 as proven user error, #50 as merely bad syntax, #80/#112 as “upgrade fixes it,” or #150 as the seven-day Testing problem. Do not import closed #44/#53 as resolved functionality. For #98, current [Google export formats](https://developers.google.com/workspace/drive/api/guides/ref-export-formats) explicitly documents first-sheet CSV/TSV and Markdown export; #143 is an implementation gap, whereas #98 is primarily a service-format limitation.

## F. Risks an issue-title or dependency-only review will miss

### OAuth is a product requirement, not just a crate version

**Publishing status and verification are different.** External apps in Testing receive seven-day refresh tokens for Drive scopes. A short-lived access token, a null refresh token and an expired Testing grant are three different states. Production refresh tokens can still fail through revocation, inactivity, token limits or administrative restrictions. The CLI must preserve valid grants and report an actionable reauthorization requirement. [Google refresh-token rules](https://developers.google.com/identity/protocols/oauth2)

Full `drive`, `drive.readonly` and `drive.metadata.readonly` are restricted scopes. Removing the redundant metadata scope is sensible but does not remove restricted-scope status. Public distribution of a shared OAuth client requires planning for verification; transmitting/storing restricted data on a server can trigger a security assessment. Do not universally label a local CLI as requiring an assessment, or universally exempt it. [Drive scope classifications](https://developers.google.com/workspace/drive/api/guides/api-specific-auth) Google documents personal-use and internal-use exceptions; personal use with fewer than 100 users can remain unverified with a warning, subject to applicable policies. That is not a promise that every blocked screen offers an override, particularly under Workspace administration. [Verification exceptions](https://support.google.com/cloud/answer/13464323?hl=en)

Offer clearly described scope profiles if supported end to end. `drive.file` is per-file/app-authorized access, not a drop-in way to recursively enumerate every existing file or enforce an arbitrary folder boundary. A `--parent` restriction is application behavior, not an OAuth security boundary (#83). Keep bring-your-own Desktop credentials as an explicit deployment choice; do not automatically solve onboarding by distributing a privileged common client ID.

**Do not infer active OOB from the stale string alone.** Both app trees contain `urn:ietf:wg:oauth:2.0:oob`, but configure `HTTPPortRedirect`. Cached yup-oauth2 12 constructs the loopback URI from its listener (`D/src/hub.rs:60–83`; `R/yup-oauth2-12.1.2/src/installed.rs:183–207`). Remove stale configuration and inspect the generated request. Google's OOB flow is blocked for production clients; supported Desktop loopback flow, SSH tunneling and credential import are the appropriate paths here. [OOB migration](https://developers.google.com/identity/protocols/oauth2/resources/oob-migration) #114's exposed HTTP listener is an unsuitable workaround. #161's manually forwarded callback is not itself OOB, but authorization-code URLs must not enter logs/history.

**Even yup-oauth2 12 does not automatically supply the desired callback protections.** Its inspected installed-flow URL builder adds scopes/offline/redirect/client ID, with no PKCE challenge or state; its callback parser accepts a `code` query value (`R/yup-oauth2-12.1.2/src/installed.rs:28–64,356–408`). Audit and implement PKCE/state-bound callback handling through a suitable library/adapter before a public release. Google's current Desktop guidance calls for PKCE. [Desktop OAuth guidance](https://developers.google.com/identity/protocols/oauth2/native-app) This is a source-level gap, not a claim of an exploited vulnerability.

For #147, simply returning an error from `present_user_url` is insufficient in that library: its HTTP flow discards the delegate's result and then waits for a callback (`R/yup-oauth2-12.1.2/src/installed.rs:200–205`). Normal file commands should use a refresh-only/noninteractive credential path; only explicit login commands should create an authorization listener. Apply deadlines and clean shutdown. Permit a selected loopback port for SSH tunnels while allowing an ephemeral default for local use. Do not seize, unlink or “overwrite” another process's port.

For #150, the library already requests offline access (`R/yup-oauth2-12.1.2/src/installed.rs:51–59`). A repeated authorization may omit a refresh token; preserve an existing one and offer deliberate consent renewal. Do not reset the credential store reflexively or state that this proves a Windows defect. [Google token issuance/persistence guidance](https://developers.google.com/identity/protocols/oauth2/web-server)

### Service accounts, shared drives and public links

#49's 23 reactions justify implementation, but service accounts do not have storage quota and cannot own files. Their upload path is a shared drive or acting for a human user through authorized OAuth/delegation. They do not automatically obtain access to a user's personal Drive. ADC must request suitable Drive scopes and resolve the intended identity; domain-wide delegation needs explicit administrator authorization. Never present service accounts or account rotation as ways to evade quota or OAuth policy. [Google storage-quota error guidance](https://developers.google.com/workspace/drive/api/guides/handle-errors)

Shared-drive support flags already exist across many calls, but they do not grant permissions or fix corpus/parent mistakes. Respect capabilities and administrator download restrictions. `acknowledgeAbuse` applies to media downloads only and requires the file owner or shared-drive organizer; expose it as an explicit opt-in, not a universal 403 bypass. [files.get parameters](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/get)

Resource keys are additional access material: preserve `resourceKey` and `shortcutDetails.targetResourceKey`, and transmit ID/key pairs using `X-Goog-Drive-Resource-Keys`. A share URL cannot always be reduced to its ID. [Google resource-key guide](https://developers.google.com/workspace/drive/api/guides/resource-keys) Treat #50's anonymous mode as a separately tested public-access feature, including whether API keys are needed for the operation. Avoid fragile HTML scraping as an implicit authentication fallback.

### Account archives and credential migration

Keep the existing `~/.config/gdrive3/` discovery path and `<account>/secret.json`, `<account>/tokens.json`, plus root `account.json` readable during migration (`U/src/app_config.rs:10–14,143–164`; `D/src/app_config.rs:235–282`). Export is a **tar**, despite upstream CLI help calling it zip (`U/src/main.rs:83`; `U/src/account/export.rs:22`). The archive's outer filename normalizes punctuation; the inner account directory retains the account name. Do not conflate those names when diagnosing #43/#97.

The serialized token entry still contains `scopes` and `token` in both inspected yup versions (`R/yup-oauth2-8.1.0/src/storage.rs:195–205`; `R/yup-oauth2-12.1.2/src/storage.rs:197–207`). That supports an expectation of compatibility, **not proof of a successful v8-to-v12 refresh**. Test real-format redacted fixtures with expired access tokens, valid/missing refresh tokens and multiple scope sets; verify refresh through a controlled server. Preserve backups and avoid silently rewriting the only usable credentials. #108 does not prove incompatible archives: its ELF error occurs before gdrive starts.

Current import infers an account from directory entries, then unpacks the whole archive directly into the config root (`D/src/common/account_archive.rs:42–82`; `D/src/account/import.rs:18–27`). Validating one directory is not an allowlist for every other archive entry: an extra root-level `account.json` is still possible. Stage extraction, reject unexpected paths/types/links, validate required files and schemas, and atomically install only the new account. Do not claim the tar library itself has a demonstrated traversal exploit from this inspection.

Create credentials and export archives with restrictive permissions from the start, rather than chmod only after writing; protect directories too. Unix mode enforcement is currently a no-op on Windows (`D/src/app_config.rs:285–292`). Provide Windows ACL behavior explicitly. The library's token writer truncates in place and its in-memory lock does not cover the whole filesystem write (`R/yup-oauth2-12.1.2/src/storage.rs:383–416`): use atomic writes and cross-process coordination. Per-command account selection must avoid racing `account switch`. Exported refresh tokens and resumable session URLs are sensitive; redact logs, including OAuth dependency debug logs (`R/yup-oauth2-12.1.2/src/installed.rs:219–222,341`).

### Platform and release pitfalls

Windows missing CRT DLLs (#128), unsupported old Windows (#122), and certificate failures (#80/#112) need separate diagnoses. Document/test the chosen target's runtime dependency, or deliberately build/test a static CRT configuration. Do not direct users to random DLL download sites. Microsoft's runtime package must match the application's architecture and be recent enough for its build tools. [Microsoft VC++ runtime guidance](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170)

Rustls does not eliminate OS root-store dependencies when configured with native roots. Minimal Linux images may lack CA bundles; enterprise proxy interception may need a trusted custom CA; corrupted macOS trust records need a useful error. Support an explicit trust-store policy without disabling verification. “Static Linux binary” also does not prove native certificate discovery will work.

Release CI currently builds without `--locked` and uses old actions; dodomorandi did not change these workflows (`U/.github/workflows/release_windows.yaml:16–35`, identical fork path). Establish tested Linux x86_64/aarch64, macOS x86_64/aarch64 and Windows x86_64 support, with armv7 only if maintained and exercised. Declare both compiler and OS minimums; latest stable Rust does not guarantee the manifest's MSRV builds every newly resolved dependency. Run cold-machine smoke tests for auth initialization and file I/O, not just cross-compilation.

All trees still call themselves 3.9.1 (`U/Cargo.toml:3`; `D/Cargo.toml:3`; `T/Cargo.toml:3`). Give the fork a distinct version/build identity and documented behavior changes. #107 proves upstream tags were moved (`I/107.md:9–13`); never repeat that for published source or release assets. Publish checksums, preserve attribution/licenses and use immutable release tags. Do not publish raw debug errors containing credentials as “better diagnostics.”

### Acceptance evidence needed before a first release

The useful next work is a small, focused regression suite rather than broad tests that simply mirror refactors:

1. **Upload protocol:** 503 after zero/partial/full chunk acceptance; lost response after final commit; 308 with/without Range; exact inclusive offsets; expired session; token refresh; zero-byte input; local read/seek error. Assert byte-for-byte output and absence of duplicate creates. Cover create, update and conversion separately.
2. **Download/finalization:** body reset after N bytes; resumed 206, unexpected 200 and 416; source changes; disk full and delayed flush failure; checksum/size mismatch; interrupted rename. Assert the previous destination survives failed transfers.
3. **Filesystem safety:** `a.jpg`/`a.png`/`a.incomplete`, `..`, absolute names, slash/backslash, long Unicode names, Windows reserved names, case collisions, native-export extension conflicts and existing destination symlinks. Test local file/directory symlink policy and cycles.
4. **Scheduling and sync:** thousands of queued jobs with bounded memory/file descriptors; child task panic/cancellation; missing metadata; ambiguous names; partial/incomplete inventory. Assert nonzero failure status and zero deletion on any incomplete plan. Test genuine dry-run equivalence.
5. **Compatibility/auth:** previous tar/token fixtures, existing accounts, archive extra entries, concurrent token writes, redirected stdout, expired/null refresh token, unattended mode and login port collision. Verify callback PKCE/state and cancellation behavior with controlled endpoints.
6. **Listing/CLI:** more than 1,000 files and 100 shared drives, empty page with token, rejected page token, custom query plus parent/drive, zero/negative limits, tab/newline names and broken-pipe output. Existing small documentation/short-flag PRs need only relevant checks.

Use an isolated throwaway test account/shared drive for the final API smoke tests, with non-destructive fixtures unless deletion is deliberately being tested. The supplied sources and this review establish priorities and failure mechanisms; they do not establish actual throughput, live OAuth success, clean-machine packaging, or that either fork's test suite currently passes.
