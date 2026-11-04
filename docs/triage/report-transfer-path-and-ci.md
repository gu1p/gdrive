# gdrive (glotlabs, upstream main 8551556): transfer data path, speed proposals, release pipeline

Method: I read the code. I made no benchmarks and no network calls. All throughput figures are estimates from the code
and protocol constants and are labeled as estimates.

Path legend used in the evidence:
- `src/...`: `gdrive-src/src/...` (upstream gdrive 3.9.1)
- `FORK-common`: `~/.cargo/git/checkouts/google-apis-rs-e87ae0754339ff1f/1f75444/google-apis-common/src/lib.rs`.
  This is the prasmussen `resumable-fix` branch, the code compiled into upstream gdrive. Its two commits are
  `9d336ab97` "Retry previous chunk" and `1f7544451` "Retry if reading body fails" (both 2023-01-09).
- `FORK-drive3`: same checkout, `gen/drive3/src/api.rs` (google-drive3 5.0.2-beta-1)
- `GAC8` / `GD7`: crates.io `google-apis-common-8.0.0/src/lib.rs` and `google-drive3-7.0.0+20251218/src/api.rs`
  (the dependencies of dodomorandi's fork)
- `DODO`: `gdrive-data/forks/dodomorandi`; `TOUT`: `gdrive-data/forks/toutizes`

---------------------------------------------------------------------------------------------------------------------

## 1. Upload path

### 1.1 Single file: protocol selection
- `upload_file` uses resumable only when `file_info.size > chunk_size_bytes`. Otherwise it uses `req.upload()`
  (`src/files/upload.rs:242-246`).
- `req.upload()` is called "Simple", but it actually sends `uploadType=multipart`: a single POST to
  `upload/drive/v3/files` with a JSON metadata part and a media part (`FORK-drive3:9031-9039, 9087-9098`). Resumable
  uploads go to `resumable/upload/drive/v3/files?uploadType=resumable` with `X-Upload-Content-Type`
  (`FORK-drive3:9033, 9114-9116`).
- `files update` uses resumable for any size above 0 (`src/files/update.rs:107-111`). This is inconsistent with
  upload: a 1-byte update costs 2 requests.
- Folders are created through the same multipart media endpoint with an empty body (`src/files/mkdir.rs:55-66`,
  `EmptyFile` in `src/common/empty_file.rs`). google-drive3 offers `doit_without_upload` only on `FileUpdateCall`
  (`FORK-drive3:11244`), not on create.

### 1.2 Chunk size
- **The default is 32 MiB (2^25 = 33,554,432 bytes)**: `ChunkSize::default() = Approx32`
  (`src/common/delegate.rs:170-178`), mapped to `u64::pow(2, 25)` (`delegate.rs:197`).
- It is configurable with `--chunk-size <MiB>` on `upload` and `update` (`src/main.rs:200-202, 229-231`). Only
  powers of two from 1 to 8192 MiB are accepted (`delegate.rs:210-231`). The help string omits 2048
  (`main.rs:201, 230`).
- The library default, used when no delegate is set, is 8 MiB (`FORK-common:197-199`). The library minimum is
  256 KiB (`FORK-common:671-675`). All allowed values are multiples of 256 KiB, as Drive requires.

### 1.3 Memory: buffered, not streamed
- **Multipart path:** the whole request, including the entire file, is read into a `Vec` before sending:
  `body_reader.read_to_end(&mut body_reader_bytes)` (`FORK-drive3:9118-9122`). Memory therefore equals the file size
  for every file at or below the chunk size. With `--chunk-size 8192`, files up to 8 GiB are held in RAM and are
  not resumable.
- **Resumable path:** each chunk is read into a fresh `Vec`: `self.reader.take(request_size).read_to_end(...)`
  (`FORK-common:685-687`). This uses about chunk_size of RAM per upload, more while the Vec grows. The file is
  read with blocking `std::io` on the async runtime. Disk read and network send are serial: read the chunk, then
  send it, then wait for the 308.
- **stdin:** stdin is first copied completely to a temporary file (`src/common/file_helper.rs:6-12`), then uploaded
  from that file (`upload.rs:61-69`).

### 1.4 Integrity (MD5)
- **Nothing is verified on upload.** The response asks for `md5Checksum` (`upload.rs:237`), but no local hash is ever
  computed or compared. A local file that changes during upload, or a corrupted chunk, goes undetected.

### 1.5 HTTP requests per file
- **Size at or below the chunk size:** 1 multipart POST, plus an occasional OAuth token refresh.
- **Size above the chunk size:** 1 session-initiation POST (`FORK-drive3:9104-9124`) plus ceil(size / chunk) chunk
  requests. The chunks are sent with method POST, not the documented PUT (`FORK-common:698-710`). Chunk requests
  carry no `Authorization` header; the session URI authorizes them. This is good: uploads longer than the 1 h token
  lifetime do not break. The final chunk's response body is the File JSON.
- Examples: 1 GiB is 1 + 32 requests. The 5 TiB maximum is about 163,841 requests at 32 MiB.

### 1.6 Errors per chunk (retry policy)
- The delegate is `UploadDelegate` (`src/common/delegate.rs:70-117`).
  - Network/hyper errors: `http_error` always retries with backoff (`:90-95`).
  - HTTP failures: `http_failure` retries only on 5xx or 429 (`should_retry`, `:119-121`). Everything else aborts,
    including 403 `userRateLimitExceeded`/`rateLimitExceeded`, which Google documents as retryable, and 408 (issue #36).
- Inside the resumable loop (`FORK-common:711-756`):
  - **Retryable non-2xx:** sleep, then rewind `start` to the beginning of the same chunk (`:734-745`; this is the
    "Retry previous chunk" fix).
  - **Network error:** sleep and resend the same chunk (`:749-752`). `start` was not advanced. No status query is made
    first.
  - **308:** `start += request_size` is assumed and the `Range` header is ignored (`:713-717`). Google documents that
    the server may persist fewer bytes than were sent and that the client must continue from `Range.last+1`.
  - **Body read failure:** prints to **stdout**, rewinds, and retries **immediately, with no backoff and no limit**
    (`:720-728`).
  - **404/410 (session expired):** not handled specially. The delegate aborts and the upload fails.
- Retries on the session-initiation POST and on multipart uploads go through the same delegate
  (`FORK-drive3:9129-9150`).

### 1.7 Backoff parameters
- Library: `exponential-backoff 1.1.0`, factor 2, jitter 0.3.
- `upload`: max_retries 100,000, min 1 s, max 60 s (`upload.rs:42-46`). That is effectively infinite.
- `update`: max_retries 20, min 1 s, max 60 s (`update.rs:35-39`).
- mkdir and generateIds: defaults of 100, 1 s, 60 s (`delegate.rs:130-137`).
- The attempt counter is per file and is **never reset after a successful chunk** (`delegate.rs:140-163`). For
  `update`, 20 transient errors spread across a large file abort it.
- The first retry already waits 2 s, because `attempts` is incremented before `next()` is called.
- **Overflow:** `exponential-backoff 1.1.0` computes `factor.pow(retry_count)` in u32 (`iterator.rs:39`). From
  attempt 32 on, this overflows:
  - Release builds (no `[profile]`, so overflow checks are off) wrap the exponent to 0, and every retry sleeps about
    1 s (the min). Backoff silently collapses from 60 s to 1 s under sustained 429/5xx.
  - Debug builds panic.
  - Version 2.1.1, which DODO uses, switched to `saturating_pow`.

### 1.8 Resume after failure: last confirmed byte or restart?
- **Within one process and one file:** resume happens at chunk granularity, as long as the retry budget lasts.
- **Across processes:** the upload always restarts from byte 0.
  - `UploadDelegate` keeps the session URI only in memory (`delegate.rs:30, 82-88`), and a new delegate is created for
    every file (`upload.rs:232`).
  - google-drive3 never asks the delegate for a stored URL: `should_ask_dlg_for_url` is initialized `false` and never
    set to `true` (`FORK-drive3:9059, 9078`). The resume branch is dead code. In `GD7` the call to `dlg.upload_url()`
    is gone completely (only `store_upload_url` remains, `GD7:13134-13295`).
  - The resumable helper's `start_at: None` path, which uses `query_transfer_status`, is therefore unreachable from
    gdrive (`FORK-drive3:9174`).
- **"<< Restarting upload >>" (issue #87):** this string is **not printed anywhere**. I searched upstream src, both
  forks, the google-apis-rs fork checkout, and the crates.io google-apis-common/google-drive3/yup-oauth2 sources.
  Issue #87 is a bare feature request ("how can I restart a previous upload that didn't finish"); the title looks
  like v2-era wording. The answer to the question is: impossible in 3.x, for the reasons above.

### 1.9 Recursive upload (`--recursive`)
- **Scan:** `FileTree::from_path` walks the whole local tree synchronously before any upload
  (`src/common/file_tree.rs:18-25, 93-139`).
  - Every file is opened just to read its size (`file_tree.rs:213-214`).
  - A Drive ID is pre-generated for every node through `IdGen`, which calls `files.generateIds(count=1000)` once per
    1000 folders plus files (`src/common/id_gen.rs:23-42`).
- **Uploads:** strictly sequential. Folders are sorted by depth, then name (`file_tree.rs:27-46`), and each is
  created and awaited (`upload.rs:136-162`). The files of that folder are then uploaded one at a time and awaited
  (`upload.rs:171-198`). There is no concurrency anywhere.
- **API calls per directory:** 1 `files.create` (multipart, empty media) plus 1/1000 of a generateIds call per node.
  There are **no list calls**, because it always creates a new tree (help text, `main.rs:196`). Each file adds the
  per-file requests from 1.5.
- **Failure:** the first file that fails after its retries aborts the whole run (`upload.rs:192-193`). A re-run
  creates a second copy of the tree, because new IDs are generated and there is no "into existing folder" mode
  (issues #18, #69).

### 1.10 Constraints on the upload design
- Resumable chunks must be sent in order, each acknowledged with 308 before the next is sent. A single file
  therefore cannot be parallelized; Drive has no compose API like GCS.
- Per-chunk idle time is about one RTT plus the server's commit latency, plus the serial disk read (1.3). Estimate:
  at 32 MiB per chunk on a 1 Gbit/s link, each chunk moves in about 0.3 s, so a few hundred ms of per-chunk
  overhead is a large fraction of wall time.

---------------------------------------------------------------------------------------------------------------------

## 2. Download path

### 2.1 Single file
- **Metadata:** `files.get` with an explicit `fields` mask, including shortcutDetails (`src/files/info.rs:35-46`).
- **Body:** `files.get?alt=media`, `supportsAllDrives=true`, no `acknowledgeAbuse` (issues #15, #136), no `Range`
  header (`src/files/download.rs:189-200`). No delegate is set, so `DefaultDelegate` applies: **zero retries on any
  error** (also true for `files.list` and `files.get` metadata).
- **Streaming:** the response body is streamed, not buffered (fixed in 3.0.0-alpha14, issue #2).
  `while let Some(chunk) = body.next()` then `write_all` to an **unbuffered `std::fs::File`**
  (`download.rs:296-319`).
  - Buffer size is whatever hyper yields. Over HTTP/2 that is DATA frames of 16 KiB max
    (`hyper-0.14.23/src/proto/h2/client.rs:42`), so about 64 `write` syscalls per MiB, blocking on the async runtime.
  - Over HTTP/1, hyper's read buffer starts at 8 KiB and adapts up to about 400 KiB (`proto/h1/io.rs:22-30`).
- **MD5:** computed inline as the file is written (`Md5Writer`, `src/common/md5_writer.rs:22-26`), compared after the
  stream ends (`download.rs:315, 393-404`).
  - On a match, `.incomplete` is renamed to the final name.
  - On a mismatch or a stream error, the command errors out and the `.incomplete` file stays behind
    (`download.rs:309-318`).
- **Temporary name:** `file_path.with_extension("incomplete")` (`download.rs:302`). This **replaces** the extension,
  so `a.pdf` and `a.docx` share `a.incomplete`, and a real Drive file named `a.incomplete` gets clobbered. It can
  also make a long name exceed 255 bytes; see 7.

### 2.2 Recursive download
- `FileTreeDrive::from_file` builds the **entire tree before the first byte is downloaded**
  (`src/common/file_tree_drive.rs:17-23, 90-138`). There is no output during this phase, which PR #162 also complains
  about.
  - Each folder costs `ceil(children/1000)` `files.list` calls (`list.rs:16, 81-126`), with
    `pageSize = min(1000, remaining)`.
  - Query: `'<id>' in parents and trashed = false`.
  - `orderBy=folder,modifiedTime desc,name`, which is useless here because the tree is re-sorted locally
    (`file_tree_drive.rs:32-41, 149`).
  - `fields=files(id,name,md5Checksum,mimeType,size,createdTime,parents),nextPageToken`. `createdTime` is unused.
  - The walk is depth-first, sequential, and awaited (`file_tree_drive.rs:121-133`). Total list calls are at least
    the number of folders. Estimate: 10,000 folders at 0.2-0.5 s each means 30-80 minutes of silent listing.
- **Downloads:** sequential, one at a time. Per file:
  1. `local_file_is_identical` re-hashes the existing local file (4 KiB buffer, no size pre-check,
     `download.rs:406-443`).
  2. One `files.get?alt=media`.
  3. Stream to disk as in 2.1 (`download.rs:154-177`).
- **Skip logic:** only reachable with `--overwrite`. Without it, `err_if_file_exists` aborts because the root folder
  exists (`download.rs:82, 334-357`). That is the undocumented "skip" users found (issues #56, #152).
- **Google Docs/Sheets/Slides, shortcuts, and any file without md5Checksum** are silently skipped
  (`file_tree_drive.rs:126-132`; `is_binary = md5_checksum.is_some()`, `src/common/drive_file.rs:285-287`). They are
  also missing from the "Found N files" count. PR #162 adds export to docx/xlsx/pptx and switches to "walk and
  download at the same time".
- **Failure:** any single error (a transient 5xx, `stream error received: unexpected internal error` in issues #25 and
  #56, the HTTP/2 GOAWAY `max_age` in issue #78, or an md5 mismatch) aborts the entire recursive job.

### 2.3 Prior art
- PR #68 (Defman): `stream::unfold` plus concurrency. It is rough: it passes `None` as the md5, which drops
  verification.
- TOUT: `TaskManager` with a `tokio::Semaphore` (`TOUT/src/files/tasks.rs:17-51`), `--workers` (default 1), and
  `2*workers` for downloads (`TOUT/src/main.rs:32-34, 483, 644`).

---------------------------------------------------------------------------------------------------------------------

## 3. Listing (`files list`)
- **Flags:** `--max` (default 30), `--query` (default `'root' in parents and trashed = false`), `--order-by` (default
  `folder,modifiedTime desc,name`), `--parent`, `--drive` (`main.rs:122-155`; `list.rs:128-211`).
- **Precedence:** `--parent` and `--drive` **replace** `--query` instead of AND-ing with it (`main.rs:475-480`;
  issue #155). `--drive` becomes `'<driveId>' in parents` with `includeItemsFromAllDrives`/`supportsAllDrives`. It does
  not use `corpora=drive&driveId=`, so only the top level of the shared drive is listed.
- **Pagination:** a loop on `nextPageToken`, `pageSize = min(1000, max - collected)` (`list.rs:88-122`).
  - Issues #57, #102, #103 were caused by `max_files: min(config.max_files, MAX_PAGE_SIZE)`.
  - PR #104 (merged 2024-01-30, included in 3.9.1) changed it to `max_files: config.max_files`. The current code is
    correct.
  - `--max 0` still sends one pointless `pageSize=0` request.
- **fields:** `files(id,name,md5Checksum,mimeType,size,createdTime,parents),nextPageToken` (`list.rs:105-108`). This
  is already a field mask, and reasonable.
- **Cost on big drives:**
  - Every page is fetched sequentially and **everything is collected in RAM before printing**, since tabwriter aligns
    columns over the whole table (`list.rs:39-70`).
  - With `--query ""` (`ListQuery::None`), the call lists everything visible to the user across all drives, at
    N/1000 sequential calls. Estimate: 100k files is about 100 pages, roughly 1-3 minutes, with no output until the
    end.
  - Drive documents that sorting may be ignored or slow for very large corpora (about 1M files). Dropping `orderBy`
    for internal listings is free.
- **Drives:** `drives list` does no pagination at all, so the API default of 10 applies (`src/drives/list.rs:62-66`;
  issues #119, #142; PRs #144, #156).

---------------------------------------------------------------------------------------------------------------------

## 4. HTTP stack

### 4.1 Upstream
Versions: hyper 0.14.23, hyper-rustls 0.23.2, rustls 0.20.7, h2 0.3.15, yup-oauth2 8.1.0 (`Cargo.lock`).
- **Connector:** `HttpsConnectorBuilder::new().with_native_roots().https_or_http().enable_http1().enable_http2()`,
  then `hyper::Client::builder().build(connector)` (`src/hub.rs:32-43`).
- **HTTP/2 is enabled** through ALPN, and Google negotiates it: the error dumps in issues #31 and #36 show
  `version: HTTP/2.0`.
- **Connection reuse:** yes, through hyper's pool (idle timeout 90 s, `hyper-0.14.23/src/client/client.rs:911`).
  With h2, **all requests multiplex over one TCP+TLS connection per process**. Each CLI invocation builds a new Hub,
  so a new handshake.
- **OAuth:** yup-oauth2's `InstalledFlowAuthenticator::builder` builds its **own separate client**
  (`src/hub.rs:64-71`).
- **HTTP/2 flow control:** hyper defaults are a stream window of 2 MiB, a connection window of 5 MiB, and
  **adaptive window off** (`hyper-0.14.23/src/proto/h2/client.rs:40-43, 64-66`; identical in hyper 1.12,
  `hyper-1.12.0/src/proto/h2/client.rs:48-50, 86`).
  - Download throughput per stream is therefore capped at about 2 MiB / RTT. Estimate: 100 MiB/s at 20 ms, but
    20 MiB/s at 100 ms (VPN, proxy, distant region).
  - All concurrent downloads in one process share the 5 MiB connection window, about 5 MiB / RTT in aggregate.
    **Parallel downloads over a single h2 connection hit this ceiling.**
- **Timeouts:** none. There is no connect timeout (`HttpConnector` default), no request or idle timeout, no HTTP/2
  keep-alive ping, and no TCP keepalive. A stalled connection after a NAT drop can hang forever.
- **GOAWAY:** Google's GFE ends long-lived h2 connections with GOAWAY `max_age` (issue #78; one commenter "fixed" it by
  removing `.enable_http2()`). Without retries, the in-flight download dies.
- **Proxy:** none. No `HTTP(S)_PROXY`/`NO_PROXY` handling for either client (issues #21, #26, #59, #74).
  - PR #141 wraps both clients in `hyper-proxy 0.9` `ProxyConnector`, with `Intercept::All` and basic auth parsed
    from the env URL. hyper-proxy is hyper-0.14-only and unmaintained.
  - PR #141 also prints "using system proxy" to stdout and `unwrap`s the URL parse. Do not merge it as-is.

### 4.2 dodomorandi fork
Files: `DODO/src/hub.rs`, `DODO/src/common/hub_helper.rs`, `DODO/Cargo.toml:14-35`.
- **Stack:** `hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(create_connector()?)`
  (`DODO/src/hub.rs:32-40`). The connector is the same builder chain, including `.enable_http2()`
  (`DODO/src/hub.rs:22-30`).
- **OAuth client:** yup-oauth2 12 gets a client of the same type through
  `InstalledFlowAuthenticator::with_client(... CustomHyperClientBuilder::from(...))` (`DODO/src/hub.rs:60-71`).
  This is the right hook for adding a proxy and timeouts once for both clients.
- **The "only http1 feature" setting does not take effect.** `DODO/Cargo.toml:21` declares
  `hyper-rustls = { version = "0.27.9", default-features = false, features = ["http1"] }`. But
  `google-drive3 7.0.0` depends on `hyper-rustls` with
  `features = ["http2", "rustls-native-certs", "native-tokio"]`, plus `ring` through its default feature. Cargo
  unifies features, so http2 is on in the final binary.
  - Confirmed with `cargo tree --offline --locked -e features -i hyper-rustls`: the "http2" feature comes from
    google-drive3, and "ring" comes from google-drive3's "ring" feature.
  - Consequences:
    1. The fork still negotiates h2, with the same 2 MiB/5 MiB windows and no adaptive window.
    2. `.enable_http2()` compiles only because a dependency happens to enable that feature. If google-drive3 changes
       its features, the build breaks.
    3. To actually force HTTP/1.1 for bulk transfers, call `.enable_http1()` only, and/or use a separate client.
       Removing the Cargo feature does nothing.
  - `hyper-util` itself is declared with `http1`, `http2`, and `client-legacy` (`DODO/Cargo.toml:22`).
- **Regression in DODO's dependency upgrade:** `GAC8` (google-apis-common 8.0.0) does **not** contain prasmussen's
  "Retry previous chunk" fix.
  - On a retryable non-2xx, `start += request_size` has already happened (`GAC8:713`), and the retry `continue`s
    without rewinding (`GAC8:729-735`). The next request then starts at the next chunk's offset, so the upload fails
    or wastes the rest of the file.
  - A body-read error now becomes `unwrap_or_default()` (`GAC8:721`).
  - The 308 `Range` header is still ignored (`GAC8:715-717`).
  - **Anyone adopting DODO's upgrade must re-apply the fix**, either with `[patch.crates-io]` on a vendored
    google-apis-common or by owning the upload protocol (proposal P1).
- **Proxy for hyper 1:** `hyper-util 0.1.21` already ships `client::proxy::matcher` (`Matcher::from_env()` /
  `from_system()`) and `client::legacy::connect::proxy` with `Tunnel` (CONNECT) and SOCKS connectors, behind the
  `client-proxy` / `client-proxy-system` features (`hyper-util-0.1.21/Cargo.toml:71-87`,
  `src/client/proxy/matcher.rs:95,109`).

---------------------------------------------------------------------------------------------------------------------

## 5. Speed proposals (ranked by expected impact for effort)

### 5.1 Drive API constraints that apply to every design
- **Resumable uploads:** chunks must be multiples of 256 KiB, except the last. They are strictly sequential, one in
  flight per session; the next chunk is sent after a 308 whose `Range: bytes=0-N` says where to continue.
  - Status query: `PUT <session>` with `Content-Range: bytes */TOTAL`.
  - 404/410 means the session expired and the upload must restart. Session URIs live about one week.
  - An unknown total size is allowed (`bytes a-b/*`), which enables stdin streaming.
- **Maximum file size:** 5 TiB (5,497,558,138,880 bytes, hard-coded at `FORK-drive3:9093, 9162`).
- **Daily upload cap:** about 750 GB per user per day across My Drive and shared drives. Uploads already in progress
  complete, and one file larger than 750 GB can still be uploaded. After the cap, new uploads fail with 403 until the
  reset. Back off for hours, not seconds, and surface the error clearly.
- **Rate quotas:** per-project and per-user per-minute query quotas. Breaches return 403
  `userRateLimitExceeded`/`rateLimitExceeded` or 429. Google prescribes truncated exponential backoff.
- **Sustained write rate:** file creates and other writes are limited to a few per second per user. Google's usage
  limits doc and rclone's docs both cite about 2-3 creates/s. Parallel uploads of many *small* files are therefore
  capped by write rate (concurrency of about 4 is enough). Parallel *downloads* have no such cap.
- **Per-file download quota:** widely shared files can hit `downloadQuotaExceeded`. Do not retry it quickly.
- **Paging:** `files.list` pageSize is at most 1000. `files.get?alt=media` supports `Range`, which allows resumable
  and segmented downloads. `files.export` is limited to 10 MB of exported content; larger exports need `exportLinks`.

### 5.2 Ranking

| # | id | Proposal | Impact | Effort | Why it ranks here |
|---|----|----------|--------|--------|-------------------|
| 1 | P2 parallel-transfers | Bounded concurrent file transfers (`--jobs N`) | high | M | Recursive jobs are 100% serial today; per-file latency dominates for small and medium files. Downloads gain the most (no write cap); uploads gain about 2-4x until the write-rate cap. |
| 2 | P3 retry-resume-everywhere | Retries for downloads, lists, and gets, plus Range-resume of downloads | high | S-M | A single transient error kills multi-hour jobs (#25 #56 #78 #152). Reliability is effective speed. |
| 3 | P5 fast-listing | Parallel, OR-batched folder listing; pipeline listing with downloading | high (big trees) | M | Tree building is N_folders sequential calls before the first byte. |
| 4 | P1 own-uploader | Own resumable-upload module (streamed body, correct 308/status/410 handling) | high | M | Removes the git-pinned fork, the Vec buffering, the panics, and the GAC8 regression. Foundation for P4, P7, P14 and progress. |
| 5 | P4 chunk-size | Bigger, adaptive chunks; a multipart threshold separate from chunk size | medium-high (single big files) | S, then M | Fewer per-chunk round trips. With streaming, memory no longer limits chunk size. |
| 6 | P6 incremental-sync | `--skip-existing/--checksum/--update` for upload and download, plus mtime preservation | high (re-runs) | L | The fastest transfer is the one avoided (#84 #116 #56 #152). |
| 7 | P8 http-tuning | h2 adaptive window and keepalive, timeouts, optional HTTP/1.1 pool for media | medium (high on high-RTT) | S | Lifts the 2 MiB/5 MiB-per-RTT ceiling. Lets parallel transfers use several TCP connections. |
| 8 | P7 persist-sessions | Resume interrupted uploads across runs, plus a job journal | medium | M | Big files on flaky links (#87). Idempotent re-runs. |
| 9 | P16 segmented-download | Parallel Range download of one big file | medium | M | A single huge file is otherwise one stream. Needs multiple connections (P8). |
| 10 | P9 progress | indicatif progress (#12, #44) | medium (UX) | S-M | Not faster, but the most requested UX feature. |
| 11 | P10 io-pipeline | Buffered and offloaded file I/O, hashing off the reactor, size-first skip | low-medium | S | Removes per-16 KiB syscalls and local re-hash stalls. Matters at high concurrency or bandwidth. |
| 12 | P11 fields-minimization | Drop `orderBy` and `createdTime` in internal lists, lean response fields, `--query` AND `--parent` | low | S | Small server and payload savings. Fixes #155, and #119 via drives pagination. |
| 13 | P12 docs-export | Export Google Docs in recursive download (PR #162) | medium (completeness) | S-M | Makes recursive download a real backup. |
| 14 | P13 proxy | Env and flag proxy through hyper-util (#26 #74, PR #141) | medium for affected users | S | Unblocks restricted networks. |
| 15 | P14 stdin-stream | Stream stdin with an unknown-length resumable upload; `--name` | low-medium | S-M | Avoids a full temp copy; fixes the UUID-name bug. |
| 16 | P17 batch-metadata | Batch folder creates, deletes, and permissions | low-medium | M | Deep trees with thousands of folders. |

### 5.3 Designs

**P2 Bounded parallel transfers**
- `--jobs N` (alias `-j`). Defaults: downloads 8, uploads 4. Do not derive the default from CPU count; the work is
  network- and quota-bound. A global token-bucket limiter for *creates* (governor crate, about 3/s sustained, burst
  10) keeps 403s rare.
- Structure: a producer walks the tree (local `read_dir`, or Drive listing from P5) and pushes jobs into a bounded
  `tokio::mpsc` (capacity 2N). N workers share an `Arc<Hub>`.
- Upload ordering: folders are created level by level, or a folder's files are released as soon as its create
  returns. Pre-generated IDs (`IdGen`) let children reference parents without waiting for listings.
- Per-file errors are collected rather than aborting; `--fail-fast` restores the old behavior. Print a summary and
  return a non-zero exit code on any failure.
- Memory cap: N x chunk with the current Vec helper (8 x 32 MiB = 256 MiB). With P1 streaming, this is negligible.
- Requires unique temporary names; the current `with_extension("incomplete")` collides (B2).
- Prior art: `TOUT/src/files/tasks.rs` (Semaphore), PR #68, PR #82. Do not merge PR #82: it uses unbounded `join_all`
  per folder and `unwrap`s.
- Note for P8: with a single h2 connection, aggregate download is capped by the 5 MiB connection window. Pair with P8.

**P3 Retry and resume everywhere**
- One retry policy, implemented as a tower layer or wrapper, for every request.
  - Retry: 429, 5xx, 408, 403 whose `error.errors[].reason` is in {userRateLimitExceeded, rateLimitExceeded,
    backendError}, connection errors, and h2 GOAWAY/RST.
  - Honor `Retry-After`. Use exponential-backoff 2.x (saturating), and reset the counter after a success.
  - Do not retry quota-day errors (`uploadLimitExceeded`/`downloadQuotaExceeded`); report them.
- Downloads: write to `<dir>/.<name>.<fileid8>.gdpart`. On a mid-stream error, re-GET with `Range: bytes=<len>-` and
  `If-Range` / a check that `md5Checksum` is unchanged. Keep the md5 context running across resumes, or re-hash the
  partial prefix. Verify md5 at the end. On a mismatch, delete the partial file and retry once from 0.

**P5 Fast listing**
- BFS over folder IDs, with up to K = 8-16 list calls in flight.
- Each call ORs up to about 50 parent IDs: `('a' in parents or 'b' in parents ...) and trashed=false`. This is
  rclone's technique. About 2.5 KB of `q`, well within URL limits.
- Request parameters: pageSize 1000, no `orderBy`,
  `fields=nextPageToken,files(id,name,mimeType,size,md5Checksum,modifiedTime,parents,shortcutDetails/targetId)`.
- Route each child by `parents` ∩ batch. Emit file jobs immediately, so downloads start while listing continues
  (streaming, like PR #162).
- Whole-drive or shared-drive downloads: a single `trashed=false` sweep with `corpora=drive&driveId=…` (or
  `corpora=user`), and the tree assembled locally. That is N_files/1000 calls instead of at least N_folders calls.
- Estimate for 10k folders: 10k sequential calls become about 200 batched calls at 8-way concurrency, from about
  1 hour to about 1 minute.

**P1 Own the resumable uploader**
This is about 300-400 LOC in `src/transfer/resumable.rs` and uses the Hub's hyper client and authenticator directly.
- `start()`: POST `…/upload/drive/v3/files?uploadType=resumable&supportsAllDrives=true&fields=<lean>` with JSON
  metadata, `X-Upload-Content-Type`, and `X-Upload-Content-Length`. Read `Location`.
- `send_chunk(off, len)`: PUT with `Content-Range: bytes off-(off+len-1)/total`. The body is a **stream** of the
  file region: `tokio::fs::File` + `seek` + `take(len)` + `ReaderStream` + `StreamBody`. No Vec, and it can count
  bytes for progress.
- Response handling:
  - 308: next offset = `Range.last+1`, or 0 if there is no Range header.
  - 200/201: done.
  - Retryable errors: backoff, then `query_status()` (`PUT`, `Content-Range: bytes */total`), then continue from the
    server offset.
  - 404/410: restart with a new session.
- Hash md5 incrementally over bytes confirmed in order, and compare with the returned `md5Checksum`.
- Use multipart for files of 5 MiB or less (Google's recommendation), streamed as well.
- This removes the git dependency on prasmussen/google-apis-rs and is immune to the GAC8 regression.

**P4 Chunk size**
- Immediate (S), still on the Vec helper: raise the default to 64 MiB, and decouple the simple-upload threshold
  (5 MiB) from chunk size so that `--chunk-size 8192` no longer means "8 GiB file in RAM". Accept any multiple of
  256 KiB with suffixes (`96M`, `1G`), not only powers of two.
- With P1 streaming (M): default 128-256 MiB, or adaptive.
  - Start at 16 MiB. Double while a chunk completes in under about 10 s. Halve after a failure. Clamp to
    [8 MiB, 1 GiB] and round to 256 KiB.
  - Retry cost is no longer a full chunk, because the status query resumes mid-chunk.
- References: Google's Python client defaults to 100 MiB and the Go client to 16 MiB. rclone recommends 64-256 MiB
  for speed.

**P6 Incremental sync**
- Download: skip when the local size equals the remote size and either
  - `--checksum`: md5 is equal, computed only when sizes match; or
  - `--update`: local mtime ≥ remote `modifiedTime`.
  Set the local mtime to `modifiedTime` after download (filetime crate).
- Upload: add "upload into an existing folder" (`--parent <id> --sync`).
  - List remote children (P5) and match by name.
  - Equal → skip. Changed → `files.update` with media, which keeps the ID and history. New → create.
  - Set `modifiedTime` from local mtime on create and update, so mtime comparisons work in both directions.
- Optional hash cache keyed by (path, size, mtime_ns, inode), stored as `~/.cache/gdrive3/hashes.*`.
- `--dry-run`. `--delete` (remote extraneous files to trash) is opt-in and destructive, so it is gated by an explicit
  flag.
- Also fixes the hidden `--overwrite`-means-skip behavior.

**P8 HTTP tuning**
- hyper-util legacy builder:
  - `.http2_adaptive_window(true)`, or explicit `http2_initial_stream_window_size(16 MiB)` and
    `http2_initial_connection_window_size(64 MiB)`.
  - `.http2_keep_alive_interval(30s)`, `.http2_keep_alive_timeout(20s)`, `.http2_keep_alive_while_idle(true)`.
  - `.pool_max_idle_per_host(jobs)`.
- HttpConnector: `set_connect_timeout(30s)`, `set_nodelay(true)`, `set_keepalive(Some(60s))`.
- Per-body idle timeout: abort a stream that produces no bytes for 60 s, then retry or resume (P3).
- `--http1` (config `transfer.http_version = auto|1.1|2`): a second client with `.enable_http1()` only, used for media
  transfers, so N jobs get N TCP connections. rclone disables HTTP/2 for Drive by default. Benchmark both and pick the
  default from data.

**P7 Persist upload sessions**
- Store `~/.local/state/gdrive3/uploads/<sha256(abs_path,size,mtime_ns,parent,name)>.json` holding
  {session_uri, created, total, chunk}.
- On a new run with an identical key and an age under 6 days: `query_status`, then resume. Delete the record on
  success, or on 404/410.
- Recursive jobs keep a journal (`--resume`) mapping local path to {drive_id, state}. Pre-generated IDs make creates
  idempotent: a 409 for an existing ID becomes a metadata check, then skip.

**P16 Segmented download**
- For files of 256 MiB or more and `--segments S` (default 4): preallocate with `set_len`, then run S concurrent
  `Range` GETs written positionally (`FileExt::write_at`, or a seek per writer).
- Hash the contiguous prefix as segments complete, or do one md5 pass at the end.
- Only useful with multiple TCP connections (P8 `--http1`) or when per-connection throughput is limited.

**P9 Progress**
- indicatif `MultiProgress` on **stderr**, `--progress auto|always|never`, auto-off when not a TTY or when
  `--stdout`/`--print-only-id` is used.
- One aggregate bar (bytes/total, files done/total, rate, ETA; totals from `tree.info()`) and up to N per-transfer
  bars.
- Bytes are counted in the download body wrapper and in P1's upload stream. Interim option without P1: update per
  chunk from `Delegate::cancel_chunk_upload` (`delegate.rs:75-80`).

**P10 I/O pipeline**
- `BufWriter::with_capacity(1-4 MiB)` around the download file, or a dedicated writer thread fed by a bounded channel.
- `spawn_blocking` or `tokio::fs` for chunk reads.
- 1 MiB buffer for local md5 (currently 4 KiB, `download.rs:432`).
- Size check before hashing. md5 stays inline; the pure-Rust md5 at about 0.5 GB/s per core is not the bottleneck.

**P11 Fields minimization and listing fixes**
- Remove `orderBy` and `createdTime` from tree listings (`file_tree_drive.rs:108-115`, `list.rs:101, 107`).
- Lean `fields` on upload responses (`id,name,size,md5Checksum,parents`) unless full info is printed.
- AND `--query` with `--parent`/`--drive` (`main.rs:480`); `corpora=drive&driveId=` for `--drive`.
- Paginate `drives list`.
- Stream rows when `--skip-header` is given or the separator is not a tab.

**P12 Docs export in recursive download**
- Default map: Docs → docx, Sheets → xlsx, Slides → pptx, Drawings → pdf. `--export-formats` overrides it;
  `--no-export` keeps the old behavior.
- Use `files.export`, with a fallback to `exportLinks` for content over 10 MB.
- No md5 exists for exports, so skip based on `modifiedTime`. Append the extension through the name sanitizer.
- Count exports in the tree info. Reuse the PR #162 logic.

**P13 Proxy**
- `hyper-util` features `client-proxy` and `client-proxy-system`.
- `Matcher::from_env()` (HTTP_PROXY, HTTPS_PROXY, ALL_PROXY, NO_PROXY), plus a `--proxy URL` flag and config. Use the
  `Tunnel` (CONNECT) or SOCKS5 connector under `HttpsConnector`.
- Build one connector factory used by both the Drive Hub and yup-oauth2 (`CustomHyperClientBuilder`, as in
  `DODO/src/hub.rs:60-66`).

**P14 stdin streaming**
- Resumable session without a known length. Read stdin into chunk-sized buffers (stdin cannot seek, so memory is one
  chunk). Send `bytes a-b/*`; the final chunk carries the total.
- Add `--name` (default `stdin`, or an error) and `--mime`. Fixes B13.

**P17 Batch metadata**
- `multipart/mixed` batch, at most 100 calls per batch, for folder creates (one batch per depth level, using
  pre-generated IDs), recursive delete, and permissions.
- Each inner call still counts against quota, so this saves round trips, not quota.

---------------------------------------------------------------------------------------------------------------------

## 6. Release pipeline

### 6.1 Current state (`.github/workflows/release_{linux,macos,windows}.yaml`)
- **Trigger:** `on: release: types: [created]`. Three nearly identical files. `actions/checkout@v3` runs on node16,
  which is deprecated. `actions/upload-release-asset@v1` is archived; its upload URL is hand-crafted with jq and sed
  (`release_linux.yaml:19-22, 32-40`).
- **Linux:** `docker run clux/muslrust:stable cargo build --release`, x86_64-musl only. It mounts a
  `$HOME/.cargo/registry` that does not exist on fresh runners. No `--locked` (`release_linux.yaml:24-26`).
- **macOS:** `runs-on: macos-latest`, a plain `cargo build`, and the asset is named **`gdrive_macos-x64.tar.gz`**
  (`release_macos.yaml:9, 13, 24-27`). `macos-latest` has been arm64 since 2024, so **any new release from this file
  ships an arm64 binary labeled x64**. No universal or Intel build.
- **Windows:** MSVC without `+crt-static`, so the binary depends on the VC++ runtime DLLs (issue #128). Zipped with 7z
  (`release_windows.yaml:25-32`).
- **Missing entirely:** ARM Linux (issues #63, #79, #99, #101, #135; PRs #130, #153, #154, #158), checksums,
  provenance, caching, CI (tests, clippy, fmt), dependency or security scanning, and dependency bots.
- **build.rs commit hash:** it parses `.git/HEAD` as `ref: …` (`build.rs:10-18`). On a tag or detached checkout,
  which is what release jobs use, it returns None, so binaries print `Commit: unknown`. The 3.9.0 output pasted in
  issue #53 shows this. There is also no `cargo:rerun-if-changed`.

### 6.2 Proposed files

**`.github/workflows/ci.yml`**
- Triggers: `on: [push to main, pull_request]`. `permissions: contents: read`. `concurrency`:
  `ci-${{ github.ref }}`, cancel-in-progress.
- `fmt`: `dtolnay/rust-toolchain@stable` (components rustfmt), then `cargo fmt --all --check`.
- `clippy`: `Swatinem/rust-cache@v2`, then `cargo clippy --all-targets --locked -- -D warnings`.
- `test`: matrix `[ubuntu-latest, ubuntu-24.04-arm, macos-latest, windows-latest]`, then `cargo test --locked`.
  Add `cargo build --locked --target x86_64-unknown-linux-musl` on ubuntu as a musl smoke test.
- `msrv`: set `rust-version` in Cargo.toml; `dtolnay/rust-toolchain@<msrv>`, then `cargo check --locked`.
- `deny`: `EmbarkStudios/cargo-deny-action@v2` with `deny.toml`:
  - `[advisories]`: RustSec. Expect hits on the 2023-era lock (h2 0.3.15, rustls 0.20.7, hyper 0.14.23); run to
    confirm.
  - `[licenses]`: allow MIT, Apache-2.0, BSD, ISC, Unicode, OpenSSL, CDLA.
  - `[bans]`: flag duplicate hyper and rustls versions.
  - `[sources]`: deny git sources. This flags the prasmussen google-apis-rs fork until P1 or a vendored patch removes
    it.
  - Alternative: `rustsec/audit-check` for advisories only.

**`.github/workflows/release.yml`** (one file replacing the three)
- Triggers: `push: tags: ['v*']` plus `workflow_dispatch`. `permissions: contents: write, id-token: write,
  attestations: write`.
- `create-release`: `taiki-e/create-gh-release-action@v1` (notes from CHANGELOG.md, `token: ${{ secrets.GITHUB_TOKEN }}`).
- `upload-assets` (needs create-release), with matrix `include`:
  - `x86_64-unknown-linux-musl` on `ubuntu-latest`
  - `aarch64-unknown-linux-musl` on **`ubuntu-24.04-arm`**. GitHub's arm64 Linux runners are free for public repos,
    so this builds and tests natively; install `musl-tools`. Alternatively cross-compile on x86 with
    `taiki-e/setup-cross-toolchain-action@v1`.
  - `armv7-unknown-linux-musleabihf` (optional, Raspberry Pi, issues #131 #135) on `ubuntu-latest` with
    `taiki-e/setup-cross-toolchain-action@v1`.
  - `aarch64-apple-darwin` and `x86_64-apple-darwin` on `macos-latest` (arm64; cross-compile x86_64 with
    `rustup target add`), or a single `universal2-apple-darwin` (supported by upload-rust-binary-action through lipo).
    Do not depend on the retiring Intel images.
  - `x86_64-pc-windows-msvc` on `windows-latest` with `RUSTFLAGS: -C target-feature=+crt-static` (fixes #128).
    Optional: `aarch64-pc-windows-msvc` on `windows-11-arm`.
- Steps:
  1. `actions/checkout` (current major, SHA-pinned)
  2. `dtolnay/rust-toolchain@stable` plus the target
  3. `Swatinem/rust-cache@v2`
  4. `taiki-e/upload-rust-binary-action@v1` with `bin: gdrive`, `target`,
     `archive: gdrive_$tag_$target`, `tar: unix`, `zip: windows`, `checksum: sha256`, `locked: true`,
     `token: ${{ secrets.GITHUB_TOKEN }}`
  5. `actions/attest-build-provenance@v2` on the produced archives
  - For one or two releases, also upload the legacy names (`gdrive_linux-x64.tar.gz`, `gdrive_macos-x64.tar.gz`,
    `gdrive_windows-x64.zip`), because scripts in the wild download those.
- Add `[package.metadata.binstall]` so `cargo binstall gdrive` finds the assets.
- Release profile: `[profile.release] lto = "thin", codegen-units = 1, strip = true`.
- Fix build.rs: prefer `GITHUB_SHA` or `git rev-parse HEAD`, and emit `cargo:rerun-if-changed=.git/HEAD`.
  Alternatively use `vergen-gitcl`.
- **Alternative:** `dist` (axodotdev, formerly cargo-dist). Run `dist init` with targets
  `x86_64/aarch64-unknown-linux-musl`, `x86_64/aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, installers
  `shell, powershell` (plus homebrew if a tap is wanted), and
  `github-custom-runners = { aarch64-unknown-linux-musl = "ubuntu-24.04-arm" }`. It generates `release.yml`
  (plan, build-local, build-global, host, announce) with checksums and installers. It is more opinionated and
  heavier; check its maintenance cadence before adopting. The taiki-e route is simpler to own.

**`.github/dependabot.yml`**
- `package-ecosystem: cargo`, weekly, `groups: {minor-patch: {update-types: [minor, patch]}}`.
- `package-ecosystem: github-actions`, weekly.
- Alternative: Renovate (`renovate.json`: `config:recommended`, `helpers:pinGitHubActionDigests`,
  `lockFileMaintenance` weekly, automerge for patch versions when CI is green).

---------------------------------------------------------------------------------------------------------------------

## 7. Bugs in the transfer path (file:line)

| id | Severity | Bug | Evidence |
|----|----------|-----|----------|
| B1 | **high (security)** | **Path traversal and '/' in names (#19).** Drive names are joined into local paths unsanitized. `PathBuf::join`/`push` with an absolute component **replaces** the base, and `..` is honored. A shared folder containing a file or folder named `/home/u/.bashrc` or `../../x` writes outside `--destination`; `a/b` fails with ENOENT (#19). Windows-reserved characters and names (`<>:"\|?*`, `CON`) are unhandled. | `src/common/file_tree_drive.rs:157-161, 226`; `src/files/download.rs:123, 156, 164` |
| B2 | medium | **Name too long (#53) and temp-name collisions.** Names over 255 bytes are not truncated. `with_extension("incomplete")` can lengthen a name past the limit, collides for `a.pdf`/`a.docx`, and clobbers a real `a.incomplete`. | `download.rs:302` |
| B3 | medium | **Duplicate names** in one Drive folder map to the same local path. The last one wins, and re-runs download it again forever. | `download.rs:162-176` |
| B4 | high | **No retry** on download, list, or get: `DefaultDelegate` means abort on the first 5xx, 429, or stream error (#25, #56, #78). | `download.rs:189-199`; `src/files/list.rs:92-111`; `src/files/info.rs:35-46` |
| B5 | medium | **Backoff counter never reset**, and `exponential-backoff 1.1.0` overflows `u32::pow` from attempt 32: in release the sleep drops to about 1 s, in debug it panics. With `max_retries=100000` this is reachable on flaky links. | `src/common/delegate.rs:140-163`; `upload.rs:42-46`; `exponential-backoff-1.1.0/src/iterator.rs:39` |
| B6 | medium | 403 rate-limit reasons and 408 are not retried (#36). | `delegate.rs:104-121` |
| B7 | medium | A 308 response's `Range` header is ignored; the code assumes the whole chunk was persisted. | `FORK-common:713-717` (same in `GAC8:713-717`) |
| B8 | medium | **Panics in the transfer path:** Range parse `panic!`; seek/read `unwrap()` on file I/O; `String::from_utf8(...).unwrap()` on the response body; `expect("LOCATION header…")`; `read_to_end(...).unwrap()`. | `FORK-common:574, 678, 687, 730`; `FORK-drive3:9119, 9166` |
| B9 | medium | When reading a response body fails, the helper prints to **stdout** (corrupting `--print-only-id` output) and retries at once with no backoff and no limit, so it can spin forever. | `FORK-common:720-728` |
| B10 | medium | Resume-URL support is dead code: `should_ask_dlg_for_url` is never true, and the URL lives in memory only, so nothing resumes across runs (#87). | `FORK-drive3:9059, 9078`; `delegate.rs:82-88`; `upload.rs:232` |
| B11 | medium | **Upload md5 never verified**: `md5Checksum` is requested but never compared. | `upload.rs:237` |
| B12 | medium | Multipart uploads buffer the **whole file in RAM**, with the threshold equal to the chunk size (up to 8 GiB with `--chunk-size 8192`). Every resumable chunk is also a Vec. | `upload.rs:242`; `FORK-drive3:9118-9119`; `FORK-common:685-687` |
| B13 | medium | **stdin uploads are named with a random UUID** (the temp file name), with mime guessed from it. `files update` from stdin **renames the remote file** to that UUID. `files update` always renames the remote file to the local file's name. | `src/common/file_helper.rs:6-12`; `upload.rs:61-69`; `src/common/file_info.rs:22-26`; `src/files/update.rs:44-46, 92-95` |
| B14 | medium | **Symlinks:** `is_dir()` follows symlinks, so a cycle recurses forever and generates IDs forever. A duplicate `is_file()` branch makes `IsSymlink` unreachable. A broken symlink aborts the whole upload. | `src/common/file_tree.rs:121-132` (dup branch at :129) |
| B15 | low | Wrong path in the error message: the recursive upload reports the root path instead of the failing file. | `upload.rs:172-173` |
| B16 | medium | Recursive download silently skips Google Docs and **any binary file lacking md5Checksum** (classified by md5 presence rather than mime type). Skipped items are not counted. | `file_tree_drive.rs:121-133`; `drive_file.rs:285-287` |
| B17 | low | An md5 mismatch or stream error leaves the `.incomplete` file behind and aborts the whole recursive job. No cleanup, no retry. | `download.rs:309-318, 393-404` |
| B18 | low (perf) | `local_file_is_identical` re-hashes every existing file with a 4 KiB buffer and no size pre-check, and it only runs with `--overwrite` (#152 confusion). | `download.rs:166-168, 406-443`; `download.rs:82, 334-357` |
| B19 | low | A `start + request_size - 1` underflow is possible if `request_size` is 0: when content_length is 0, or after a 308 on the final chunk. This produces a bogus `Content-Range` (u64::MAX). DODO patched the display side in commit 15f7d83. | `FORK-common:680-694`; `delegate.rs:49` |
| B20 | low | `files update` uses resumable for any non-empty file, and has a 20-error budget per file. | `update.rs:35-39, 107` |
| B21 | low | `--query` is silently ignored with `--parent`/`--drive` (#155). `drives list` is not paginated (#119, #142). | `main.rs:475-480`; `src/drives/list.rs:62-66` |
| B22 | medium (for adopters of DODO) | **GAC8 regression:** the "Retry previous chunk" rewind is missing (a retry skips a chunk), and a body-read error becomes `unwrap_or_default`. | `GAC8:712-737` |
| B23 | low | Recursive upload aborts on the first file error, and a re-run duplicates the whole tree. | `upload.rs:185-193`; `main.rs:196` |
| B24 | low (perf) | Tree structures clone heavily, about O(N·depth): `folders()` and `info()` clone subtrees and every node clones its ancestor chain. This is slow and memory-heavy for trees with millions of nodes. | `file_tree.rs:27-67, 106-110, 161-189`; `file_tree_drive.rs:25-65, 101-106, 180-192` |
| B25 | low (release) | Release binaries print `Commit: unknown` (detached HEAD). The macOS asset name says x64 on an arm64 runner. The Windows binary needs the VC++ runtime (#128). | `build.rs:10-18`; `release_macos.yaml:9, 13`; `release_windows.yaml:25-27` |

Recommended fix order: B1 (security) → B4/B6 (P3) → B22 if DODO's dependencies are adopted → B5 (bump to
exponential-backoff 2.x) → B7-B12 through P1 → B13/B14 → the rest.

---------------------------------------------------------------------------------------------------------------------

## Proposals

```json
[
  {"id":"P1","title":"Own resumable-upload module (streamed chunks, 308 Range + status query + 404/410 restart, md5 verify)","impact":"high","effort":"M","depends_on":[],"design":"New src/transfer/resumable.rs using the Hub hyper client + yup-oauth2 token. start(): POST upload/drive/v3/files?uploadType=resumable&supportsAllDrives=true&fields=<lean> with JSON metadata, X-Upload-Content-Type/-Length; read Location. send_chunk(): PUT Content-Range bytes a-b/total with a streamed body (tokio::fs::File+seek+take+ReaderStream+StreamBody, no Vec). 308 -> next=Range.last+1 (0 if absent); 200/201 -> parse File; retryable -> backoff then query_status (PUT Content-Range: bytes */total) and continue from server offset; 404/410 -> new session. Incremental md5 over confirmed bytes compared to md5Checksum. Multipart (streamed) only for <=5 MiB. Removes the prasmussen git fork and avoids the google-apis-common 8.0.0 retry regression (GAC8:712-737) plus panics FORK-common:574/678/687/730."},
  {"id":"P2","title":"Bounded parallel file transfers (--jobs N)","impact":"high","effort":"M","depends_on":["P3"],"design":"Producer walks tree (local read_dir or Drive listing P5) into bounded tokio mpsc (cap 2N); N workers share Arc<Hub>. Defaults: download 8, upload 4; global token bucket for creates (~3/s, burst 10) to respect Drive write-rate limit. Folders created level-by-level or files released once their parent's create returns (pre-generated IDs). Per-file errors collected, --fail-fast optional, summary + non-zero exit. Unique temp names per file. Memory bound = N x chunk until P1 streams. Prior art: toutizes TaskManager (Semaphore), PR #68; do not merge PR #82 (unbounded join_all + unwraps)."},
  {"id":"P3","title":"Uniform retry policy for all requests + Range-resume for downloads","impact":"high","effort":"S","depends_on":[],"design":"Wrapper/tower layer around every Drive call: retry 429, 5xx, 408, 403 with reason userRateLimitExceeded|rateLimitExceeded|backendError, connection errors, h2 GOAWAY/RST; honor Retry-After; exponential-backoff 2.x (saturating), reset after success; never fast-retry uploadLimitExceeded/downloadQuotaExceeded (report). Downloads write to .<name>.<id8>.gdpart, on mid-stream failure re-GET with Range: bytes=<len>- after checking md5Checksum unchanged, verify md5 at end, delete+retry once on mismatch. Fixes #25 #56 #78 #152."},
  {"id":"P4","title":"Bigger/adaptive chunk size, simple-upload threshold decoupled from chunk size","impact":"medium","effort":"S","depends_on":[],"design":"Now: default 64 MiB; multipart only for <=5 MiB (so --chunk-size no longer implies whole-file RAM buffering); accept any 256 KiB multiple with suffixes (96M, 1G). After P1 (streaming): default 128-256 MiB or adaptive: start 16 MiB, double while a chunk finishes in <~10 s, halve on failure, clamp [8 MiB,1 GiB], round to 256 KiB. Constraint: chunks sequential per session; only per-chunk RTT/commit overhead is amortized."},
  {"id":"P5","title":"Fast recursive listing: parallel BFS, OR-batched parents, no orderBy, pipelined with transfers","impact":"high","effort":"M","depends_on":["P3"],"design":"Queue of folder IDs; up to 8-16 list calls in flight; each q = ('a' in parents or ... up to ~50 ids) and trashed=false, pageSize=1000, no orderBy, fields=nextPageToken,files(id,name,mimeType,size,md5Checksum,modifiedTime,parents,shortcutDetails/targetId); route children via parents; emit file jobs immediately so downloads start during listing (PR #162 idea). Whole drive / shared drive: one trashed=false sweep with corpora=drive&driveId (or user) and local tree assembly (N_files/1000 calls instead of >=N_folders)."},
  {"id":"P6","title":"Incremental / rsync-style semantics for upload and download","impact":"high","effort":"L","depends_on":["P5","P2"],"design":"Flags --skip-existing (size), --checksum (md5 only when sizes equal), --update (mtime); download sets local mtime=modifiedTime (filetime); upload into existing folder (--parent X --sync): list remote children, equal->skip, changed->files.update (keeps id/revisions), new->create, set modifiedTime from local mtime. Optional hash cache keyed (path,size,mtime_ns,inode). --dry-run; destructive --delete strictly opt-in. Replaces hidden '--overwrite means skip' behavior (#84 #116 #56 #152)."},
  {"id":"P7","title":"Persist upload sessions and job journal for cross-run resume","impact":"medium","effort":"M","depends_on":["P1"],"design":"~/.local/state/gdrive3/uploads/<sha256(abs_path,size,mtime_ns,parent,name)>.json = {session_uri,created,total,chunk}; on rerun with same key and age<6 days query_status and resume, delete on success or 404/410. Recursive jobs write a journal (local path -> drive id, state); --resume continues; pre-generated IDs make creates idempotent (409 -> verify and skip). Fixes #87."},
  {"id":"P8","title":"HTTP client tuning: h2 adaptive window/keepalive, timeouts, optional HTTP/1.1 pool for media","impact":"medium","effort":"S","depends_on":[],"design":"hyper-util legacy builder: http2_adaptive_window(true) (or 16 MiB stream / 64 MiB conn windows; defaults are 2 MiB/5 MiB, cap ~window/RTT), http2_keep_alive_interval 30s + timeout 20s + while_idle, pool_max_idle_per_host=jobs; HttpConnector connect_timeout 30s, nodelay, TCP keepalive 60s; per-body idle timeout 60s -> retry/resume. --http1 / config transfer.http_version=auto|1.1|2 builds a second client (.enable_http1() only) for media so N jobs use N TCP connections (rclone disables h2 for Drive by default). Note dodomorandi's Cargo 'http1-only' feature is overridden by google-drive3's http2 feature; select protocol in the builder."},
  {"id":"P9","title":"Progress reporting with indicatif","impact":"medium","effort":"S","depends_on":[],"design":"MultiProgress on stderr; --progress auto|always|never (auto off when not a TTY or with --stdout/--print-only-id); aggregate bar (bytes, files, rate, ETA from tree.info()) plus up to N per-transfer bars; byte counting in a download body wrapper and P1's upload stream; interim per-chunk updates via Delegate::cancel_chunk_upload. Fixes #12 #44."},
  {"id":"P10","title":"I/O pipeline: buffered/offloaded file I/O, size-first skip, bigger hash buffers","impact":"low","effort":"S","depends_on":[],"design":"BufWriter 1-4 MiB (or writer thread with bounded channel) instead of write_all per 16 KiB h2 frame; spawn_blocking/tokio::fs for chunk reads; local md5 with 1 MiB buffer and only after size match; keep md5 inline (not a bottleneck below ~4 Gbit/s per core)."},
  {"id":"P11","title":"Fields/query minimization and listing fixes","impact":"low","effort":"S","depends_on":[],"design":"Drop orderBy and createdTime from internal tree listings; lean fields on upload/update responses unless printing; AND --query with --parent/--drive (#155) and use corpora=drive&driveId for --drive; paginate drives list (#119 #142, PR #156); stream rows when header alignment not needed; skip pageSize=0 call."},
  {"id":"P12","title":"Export Google Docs during recursive download (PR #162)","impact":"medium","effort":"S","depends_on":["P15"],"design":"Default map doc->docx, sheet->xlsx, slides->pptx, drawing->pdf; --export-formats override, --no-export legacy behavior; files.export with fallback to exportLinks for >10 MB; skip by modifiedTime (no md5); extension appended via sanitizer; counted in tree info."},
  {"id":"P13","title":"Proxy support via hyper-util client-proxy","impact":"medium","effort":"S","depends_on":[],"design":"Enable hyper-util features client-proxy(+system); Matcher::from_env() for HTTP(S)_PROXY/ALL_PROXY/NO_PROXY plus --proxy flag/config; Tunnel (CONNECT) or SOCKS5 connector under HttpsConnector; one connector factory used by both DriveHub and yup-oauth2 (CustomHyperClientBuilder). Supersedes PR #141 (hyper-proxy 0.9 is hyper-0.14-only, prints to stdout, unwraps)."},
  {"id":"P14","title":"Stream stdin uploads (unknown length) and add --name","impact":"low","effort":"S","depends_on":["P1"],"design":"Resumable session without total; read stdin into chunk-size buffers, send bytes a-b/*, final chunk carries total; --name required/defaulted and --mime; removes temp-file copy and the UUID-name bug for upload and update."},
  {"id":"P15","title":"Safe local path mapping (sanitize, dedupe, truncate)","impact":"high","effort":"S","depends_on":[],"design":"Per-component sanitizer: replace '/' and '\\\\' (and on Windows <>:\"|?* , control chars, trailing dot/space, reserved names) with '_', reject '.'/'..', truncate to 255 bytes on a UTF-8 boundary keeping extension plus short id suffix; dedupe duplicates as 'name (2).ext' or 'name~<id6>.ext'; assert final path starts_with canonical root; temp files named .<id8>.gdpart independent of name length. Fixes path traversal (B1), #19, #53."},
  {"id":"P16","title":"Segmented parallel download of a single large file","impact":"medium","effort":"M","depends_on":["P3","P8"],"design":"For files >=256 MiB and --segments S (default 4): set_len preallocation, S concurrent Range GETs written positionally (write_at), md5 over contiguous completed prefix or a final pass; useful only with multiple TCP connections (HTTP/1.1 pool) or per-connection throttling."},
  {"id":"P17","title":"Batch metadata requests (folder creates, deletes, permissions)","impact":"low","effort":"M","depends_on":[],"design":"multipart/mixed batch of <=100 calls; one batch per tree depth for folder creates using pre-generated IDs; recursive delete/trash and permission changes; saves round trips, not quota (each inner call counts)."},
  {"id":"P18","title":"Fix backoff: exponential-backoff 2.x, reset per success, sane budgets","impact":"medium","effort":"S","depends_on":[],"design":"Bump to exponential-backoff 2.x (saturating_pow) as dodomorandi did; reset attempt counter after each successful chunk/request; budget per request (e.g. 10 attempts, max 64 s, honor Retry-After) instead of 100000 per file; retry 403 rate-limit reasons and 408."},
  {"id":"CI1","title":"CI workflow: fmt, clippy -D warnings, tests on 4 runners, MSRV, cargo-deny","impact":"medium","effort":"S","depends_on":[],"design":".github/workflows/ci.yml on push/PR; jobs fmt (cargo fmt --check), clippy (--all-targets --locked -D warnings), test matrix ubuntu-latest/ubuntu-24.04-arm/macos-latest/windows-latest (cargo test --locked, musl smoke build), msrv (rust-version), deny (EmbarkStudios/cargo-deny-action@v2: advisories, licenses, bans, sources - flags the git google-apis-rs fork); Swatinem/rust-cache@v2; permissions contents:read; concurrency cancel-in-progress."},
  {"id":"CI2","title":"Single release workflow with target matrix, checksums, attestations","impact":"medium","effort":"S","depends_on":["CI1"],"design":".github/workflows/release.yml on tag v*: taiki-e/create-gh-release-action@v1, then matrix with taiki-e/upload-rust-binary-action@v1 (bin gdrive, checksum sha256, locked): x86_64-unknown-linux-musl (ubuntu-latest), aarch64-unknown-linux-musl (ubuntu-24.04-arm, free arm64 runners for public repos; musl-tools), optional armv7-unknown-linux-musleabihf (setup-cross-toolchain-action), universal2-apple-darwin or aarch64+x86_64-apple-darwin (macos-latest arm64), x86_64-pc-windows-msvc with RUSTFLAGS=-C target-feature=+crt-static (#128); actions/attest-build-provenance@v2; keep legacy asset names one cycle; fix build.rs commit hash (GITHUB_SHA / git rev-parse, rerun-if-changed); [profile.release] lto=thin, codegen-units=1, strip=true; binstall metadata. Alternative: dist (axodotdev) with github-custom-runners."},
  {"id":"CI3","title":"Dependency automation (Dependabot or Renovate)","impact":"low","effort":"S","depends_on":["CI1"],"design":".github/dependabot.yml: cargo weekly with minor/patch group, github-actions weekly; or renovate.json with config:recommended, helpers:pinGitHubActionDigests, weekly lockFileMaintenance, automerge patch on green CI."}
]
```
