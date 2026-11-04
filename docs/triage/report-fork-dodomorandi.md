# Fork evaluation: dodomorandi/gdrive as the base for our modernization fork

- Target: `gdrive-data/forks/dodomorandi` at `ec37b03` (main), 265 commits on top of upstream `8551556`, all by Edoardo Morandi (2025-07-11 to 2026-09-17).
- Work copy: `gdrive-data/analysis/tmp-fork-dodo/` (logs: `build_release.log`, `test.log`, `clippy.log`; `--help` diff: `helpdiff/`; PR checks: `_prdiffs/`; repros: `_repro_v8/`, `_repro_prasmussen/`). `gdrive-src` and `forks/*` were not modified.
- GitHub metadata (2026-11-04): 3 stars, issues disabled, no CI runs, no PRs sent to glotlabs. A second branch, `google-drive-api-from-ground-up` (7 WIP commits, 2026-09-29 to 2026-10-02, +11.4k lines), starts replacing google-drive3 with the author's own discovery-based code generator (`crates/google-api-codegen`, `crates/google-drive3-generated`). The author looks set to diverge further.

**Verdict:** do not adopt the fork as our base. Start from upstream `main`, do the dependency migration ourselves using the fork's 5 migration commits as a recipe, and cherry-pick a handful of its fixes. Whichever base we pick, we have to patch google-apis-common 8 for the resumable-upload retry bug (section 3). The fork has this bug today.

---

## 1. Build, test and clippy on rustc 1.99.1

| Step | Result | Time | Warnings | Errors |
|---|---|---|---|---|
| `cargo build --release` | OK. Binary reports `gdrive 3.9.1 / Commit ec37b03 / Rust 1.99.1`. | 1m05s (cold, 16 cores) | 0 | 0 |
| `cargo test` | **13 passed**, 0 failed (upstream has 0 tests) | 27s | 0 | 0 |
| `cargo clippy --all-targets` (the crate sets `clippy::pedantic = warn` in Cargo.toml) | OK | 18s | **3**: all `clippy::double_must_use`, a lint that is new in 1.99, at `src/common/file_tree.rs:61`, `src/common/file_tree_drive.rs:54`, `src/files/download.rs:79` | 0 |

- The locked `Cargo.lock` resolved unchanged.
- TLS stack: rustls 0.23 with **ring only** (no aws-lc-rs). `hyper-rustls` features http2, ring and native-tokio come from google-drive3 and yup-oauth2 through feature unification. The fork's own Cargo.toml asks only for `["http1"]`, which is fragile: we should declare them explicitly.
- Runtime smoke tests, run with a fake `$HOME`:
  - A fake account using an **upstream-format** `tokens.json` loads, does TLS to googleapis.com and gets the expected 401 (`files list`).
  - An account archive made with `gdrive account export` on **upstream 3.9.1** imports cleanly with the fork (`account import`, then `list` and `current` work).
  - Full transcripts are in the work copy.

## 2. Commit log categorization (265 commits)

The per-commit mapping is in `tmp-fork-dodo/_commits.txt.cat`.

| Category | Count | Representative commits |
|---|---|---|
| Error-handling overhaul (one error enum per function, `source()` chains, error-trace printing, inlined `err_if_*` helpers) | 79 | 3813d73 (error-trace in main), ca66519…0ac76e5 (account and app_config), fb074c2 (removes global `app_config::Error`), 5253cbb, 194e3c9, 06b28f3, 5086f7e, a221c45, e865342 |
| Refactors (traits FolderLike/FileTreeLike, `pub(crate)`, inlining, const tables, file_tree API) | 65 | 61c90b5, 7bd7acc, 2e53017, 0b62bdf (split `run`), 09ca46d, 84784b4, d67a833, ebdcffe, b66e480, c2368a4 |
| Perf / allocation and clone avoidance (Copy derives, `&Config`, Cow, `LazyLock` mime statics, cached paths) | 52 | ef03f10, 61590b2/ff67214/f3b0b0b (mime statics), 4cd4393…0765898 (OnceCell path cache), 4475cf1, 1b4d045, d7db79d, 0e17322, 3efb773 |
| Style / lint / docs (fmt, clippy pedantic, `#[must_use]`, nightly rustfmt import style) | 34 | 7756e15, c73ab87, d86a2e8 (pedantic), 060dad3, d193776, 0886a28 (lints `unreachable_pub`, `dead_code_pub_in_binary`), 213c699, ee3fba8 |
| Dependency / build | 17 | **9d49372** (git fork to crates.io google-drive3 5.0.5, which dropped the resumable fix), **205f756** (google-drive3 7.0.0, google-apis-common 8, hyper 1, hyper-rustls 0.27, yup-oauth2 12), **8ca71ac** (use crates directly instead of google_drive3 re-exports), e858a8c, **ec37b03** (exponential-backoff 2), 69b553a (human_bytes to bytesize), 394a647 (clap), 936e3f6 (MSRV 1.88), and lock bumps 06fbf63…f7924e9 |
| Tests | 10 | 5d1136a, e6083ab, 700c2d5, 32e7b85, 0302f26, acd2df5, 9e55628, 167ac65, 910ca96, a95fb15. All are unit tests of pure helpers; none touch upload, download or auth. |
| Bug fixes (`fix:`) | 8 | f4bfea4 (a negative Drive size is now an error instead of an i64-to-u64 wrap), a730431 (checked page_size cast), 18d8afe (TOCTOU in archive creation, via `File::create_new`), ff460e1, 15f7d83 (chunk-size arithmetic in `--print-chunk-info`), 7b9a68e (stdin temp file could be deleted while still in use), 515af9b, 6738e9f |

None of the 265 commits fixes an open upstream GitHub issue. For example, drives-list pagination (#119/#142) is still missing in `src/drives/list.rs`, and the slash-in-filename bug (#19) is untouched. No features were added.

### Commits that change user-visible behavior or CLI flags

**CLI flags: no changes.** I diffed `--help` for all 31 commands and subcommands between the upstream 3.9.1 binary and the fork binary (`helpdiff/`). The only differences are clap wording ("Print help information" became "Print help", and "error: unrecognized subcommand" changed) and URLs wrapped in `<>` (521b184).

Output and behavior changes:

1. **69b553a (size formatting).**
   - human_bytes (default features: base 1000, upper-case "KB", trims ".0") was replaced by bytesize `.display().si()` (base 1000, "kB", always one decimal).
   - Example: `2 MB` becomes `2.0 MB` and `1.5 KB` becomes `1.5 kB`.
   - Affects the `files list` Size column, `files info` Size (without `--size-in-bytes`), the "Found/Uploaded N files … total size" lines in upload and download, and `--print-chunk-info` lines.
   - It is not marked breaking. **Scripts that parse sizes will notice.**
2. **3813d73 plus the error overhaul (stderr format).**
   - `Error: <msg>` becomes `<msg>\n\nCaused by:\n o …`, and messages are now lower-case.
   - The `Error:` prefix is gone.
   - Actionable hints were lost. Upstream (`src/app_config.rs:251-253`) printed "No account has been selected / Use `gdrive account list` … / Use `gdrive account switch` …". The fork prints "unable to get drive hub / Caused by: unable to load current account / unable to load account config / account config is missing" (`src/app_config/errors.rs:208`). I verified this by running both binaries.
   - The exit code is still 1.
3. **11318dc (cosmetic regression).** A `\` line continuation drops the separating space: `account add` now prints "…found here:https://…" and "…access to yourGoogle Drive." (`src/account/add.rs:13,62`). Verified in the smoke run.
4. **205f756 (through yup-oauth2 12): the OAuth redirect_uri changed from `http://127.0.0.1:8085` to `http://localhost:8085`.** Details in section 4.
5. **205f756 (through google-apis-common 8): resumable-upload retry regression.** Details in section 3.
6. **ea3bf2b and f59bc5a: download writes moved to `tokio::fs::File` with no `flush()` before `md5()` and `rename()`** (`src/files/download.rs:233-271`).
   - Tokio documents that an error from the last write is only reported on the next write or flush (`tokio-1.53.1/src/fs/file.rs:47,104-106`).
   - So an ENOSPC or EIO on the final chunk is silently swallowed, and the `.incomplete` file is renamed before the last write is guaranteed to have finished.
   - This is a latent correctness regression. Upstream used blocking `std::fs::File` writes, which report errors immediately. The fix is two lines.
7. **9d49372 (improvement): a failure in `with_native_roots()` is now an error instead of a panic** (`src/hub.rs:22-30`). Relevant to upstream issue #112.
8. Minor changes:
   - 1ffbaf4: `permissions revoke` prints details for an unknown role or type instead of falling back to "Revoking permission with id".
   - 66c8816: different text for an invalid `--chunk-size` value.
   - f4bfea4: a negative Drive size now gives an error.
   - Retry budget: exponential-backoff 2 gives `max_retries - 1` real retries, which is negligible.
9. **c2368a4 "feat!: overhaul file info printing system" is not actually user-visible.** Its message says "keys related to empty values are not printed anymore", but upstream `print_fields` already skipped `None` values (`if let Some(value) = &field.value`). The field order, labels, date format and `True`/`False` are identical. The `!` is misleading.

## 3. Resumable upload: the prasmussen fix is LOST (confirmed by code and by an A/B runtime repro)

**Background.** Upstream builds google-drive3 from `prasmussen/google-apis-rs@resumable-fix` (`1f75444`). That branch adds two commits to `google-apis-common/src/lib.rs`:
- `9d336ab` "Retry previous chunk"
- `1f75444` "Retry if reading body fails"

The fork switched to crates.io google-drive3 in its very first commit (9d49372, 5.0.5) and is now on google-apis-common **8.0.0**, which has neither fix.

### (a) What happens to the offset and the reader on a retried chunk

**prasmussen `1f75444`** (`~/.cargo/git/checkouts/google-apis-rs-e87ae0754339ff1f/1f75444/google-apis-common/src/lib.rs`):
- L678: the top of the loop seeks the reader with `seek(SeekFrom::Start(start))`. L680-687 read `request_size` bytes.
- L713: `start += request_size` runs as soon as any HTTP response arrives.
- L715-716: on a 308, continue.
- L720-727: if reading the body fails, `start = max(start - request_size, 0); continue`. The chunk is resent immediately, with no delegate call or backoff, so in theory it can retry forever.
- L735-744: on a non-2xx status where the delegate returns `Retry::After`, it sleeps, then resets `start = max(start - request_size, 0)` and continues. The loop top re-seeks the reader, so **the same chunk is resent**.

**google-apis-common 8.0.0** (`~/.cargo/registry/src/index.crates.io-*/google-apis-common-8.0.0/src/lib.rs`):
- L678: seek. L680-687: read. L713: `start += request_size`. L715-716: on a 308, continue. All the same as above.
- L721: `to_bytes(body).await.unwrap_or_default()`. A failed body read silently becomes an empty body.
- L728-735: on a non-2xx status with `Retry::After`, it does `sleep(d).await; continue;` with **no reset**. The next iteration seeks the reader to the advanced offset and sends the **next** chunk, so the failed chunk is never resent.
- L739-744: a transport error (`Err`) does not advance `start`. Upstream PR Byron/google-apis-rs#286 (merged 2021-06-07) fixed only this transport-error path.

**Runtime repro** (`tmp-fork-dodo/_repro_v8/src/main.rs`; the same harness built against the prasmussen code is in `_repro_prasmussen/`):
- A mock resumable server accepts only contiguous offsets, like Google. It injects one transient 503 on chunk 2 of a 1 MiB upload (4 × 256 KiB chunks).
- The delegate uses the fork's retry decision.
- With **8.0.0**, the client sends `bytes 0-262143` (308), then `262144-524287` (503, injected), then `524288-786431` and `786432-1048575` (503 "upload offset … exceeds already uploaded size"), then repeats zero-byte `bytes 1048576-1048575/1048576` until the retry budget runs out. Result: **503, the server holds 262144 of 1048576 bytes**.
- With **prasmussen**, the client resends `262144-524287` after the 503 and gets 200. All 1 MiB is received and intact.
- With **8.0.0 plus a 1-line patch** (`start -= request_size;` before `continue` at L733-734, using a vendored copy and `[patch.crates-io]`), the result is 200 and the data is intact.

**Real-world evidence.** Byron/google-apis-rs **#438** (open since 2023-08-08, google-drive3 5.0.2) shows the same server reply from Google: `503 … "Invalid request. According to the Content-Range header, the upload offset is 41943040 byte(s), which exceeds already uploaded size of 35389440 byte(s)."`

### (b) How the fork's delegate makes it worse

- `src/common/delegate.rs:94-113` (`http_failure`) returns `self.backoff.retry()` for any 5xx or 429 (`should_retry`, :116-118).
- The 503 that Google sends for a wrong offset is itself a 5xx, so the delegate keeps saying "retry". Each retry advances the offset further, then the loop sends zero-length chunks at EOF.
- `src/files/upload.rs:62-68` sets `max_retries: 100_000` with a 60s cap on the sleep. After one transient 5xx or 429 mid-upload, **`gdrive files upload` of any file larger than the chunk size (default 32 MiB, `upload.rs:352`) either stalls for a very long time (up to about 100k × 60s) or fails**. It never recovers. If Google rejects the malformed zero-length range with a 400, it fails at that point instead; I did not verify which happens against the real server.
- The 429 case becomes common once uploads run in parallel (PR #82). That makes this bug directly relevant to our speed goals.
- `--print-chunk-info` exposes it: the "Retrying" label (`delegate.rs:48-52`) never appears, because the resent chunk is never equal to `previous_chunk`.
- **Silent corruption: refuted.** Google rejects offsets that go past the committed size, and the repro shows the committed bytes stay intact. The failure is "the session is stuck or rejected", not bad data.
- **Body-read failure (fix `1f75444`): lost, and worse than an error.** With a 2xx final response whose body read fails, the body becomes empty (L721). google-drive3 7.0.0 then returns `JsonDecodeError` (`google-drive3-7.0.0+20251218/src/api.rs:13305-13314`). gdrive reports "unable to upload file" even though Drive most likely created the file, so re-running it creates a duplicate.
- **Flags kept:** `--chunk-size`, `--print-chunk-errors` and `--print-chunk-info` exist unchanged (help diff). The chunk error printing (`delegate.rs:87-113`) and chunk info printing (`delegate.rs:39-64`) are kept. Errors are now printed with error-trace.

### (c) Has anyone fixed it upstream?

- crates.io google-apis-common releases: 5.0.x, 6.0.0-6.0.4, 7.0.0 (2024-10-15), 8.0.0 (2025-07-24). **8.0.0 is the latest and it is not fixed.**
- `Byron/google-apis-rs` main, fetched 2026-11-04 (latest lib.rs commit `9d345e752`, 2026-10-17), still has `start += request_size` at L754 and the retry `continue` without a reset at L772-775.
- The only open PR is #572 (URL percent-encoding). Issue #438 is open; the maintainer suggests patching locally.
- **Nobody has fixed it.**

### (d) Smallest fix

The bug is real, so we need a fix whichever base we choose.

- **Minimal fix (verified):** vendor google-apis-common 8.0.0 under `[patch.crates-io]` and add `start -= request_size;` before the `continue` in the non-2xx retry branch (L733-734). The reader needs no extra seek, because the loop top seeks to `start`. Optionally also turn the L721 body-read failure into a retry of the same chunk.
- **Robust fix (recommended in the same patch):** on a retryable failure, query the session for its committed range (a `Content-Range: bytes */N` request returns 308 with `Range: bytes=0-X`) and resume at `X+1`.
  - Also honor the `Range` header on ordinary 308 replies. Both versions currently ignore it (L715-716) and assume the whole chunk was committed.
  - Note that `query_transfer_status` returns `last`, not `last + 1` (8.0.0 L644 `return Ok(h.0.last)`, used as `start` at L663-666), so add 1.
  - Then upstream the patch to Byron/google-apis-rs to fix #438.
- **Alternative with no crate patch:** have our delegate keep the upload URL even when google-drive3 calls `store_upload_url(None)` (api.rs:13295), return `Abort` on 5xx during the chunk phase, and call `upload_resumable` again so google-drive3 resumes from the committed status. This is hackier.
- **Long term:** our own resumable uploader (hyper or reqwest) would also make parallel chunked uploads possible.

This is decisive against adopting the dodomorandi fork **as-is**. It does not decide between the two bases, because any migration to google-drive3 7 needs this patch.

## 4. OAuth with yup-oauth2 12 (`src/hub.rs`, `src/account/add.rs`)

- **Redirect port:** unchanged. `InstalledFlowReturnMethod::HTTPPortRedirect(8085)` (`src/hub.rs:60-62`). The listener still binds `127.0.0.1:8085` (yup-oauth2 8.1.0 `installed.rs:287`, 12.1.2 `installed.rs:275`).
- **Redirect URI sent to Google: changed.**
  - yup-oauth2 8.1.0 sent `http://127.0.0.1:8085` (`installed.rs:192,242`, `format!("http://{}", addr)`). 12.1.2 sends `http://localhost:8085` (`installed.rs:191,236`).
  - Desktop-app OAuth clients accept any loopback address, so no change for them.
  - Users who created a **Web-application** client and registered `http://127.0.0.1:8085` (upstream issue #109) will get `redirect_uri_mismatch` on the next `account add` or re-consent.
  - Re-consent is frequent for apps in "Testing" status, whose refresh tokens expire after 7 days (#22).
  - Token refresh sends no redirect_uri, so existing tokens keep working. The fix is a doc note: register both URIs, or use a Desktop client.
  - The README text ("redirected to `http://localhost:8085`") now matches exactly.
  - With the remote-server tunnel or curl trick (#161), `localhost` may resolve to `::1` first while the server listens on IPv4 only. Browsers and curl fall back to IPv4, but this is worth a note.
- **Client construction:** `InstalledFlowAuthenticator::with_client(…, CustomHyperClientBuilder::from(hyper_util legacy client))` reuses the same rustls connector as the Drive hub (`src/hub.rs:22-30,60-71`). The ApplicationSecret fields and the OOB `redirect_uris` placeholder are unchanged (`hub.rs:77-91`). The flow delegate prints the same text.
- **Token storage format:** unchanged.
  - Between yup-oauth2 8.1 and 12.1 the `JSONToken` and `TokenInfo` serde format is identical: `[{scopes, token:{access_token, refresh_token, expires_at, id_token}}]` (`storage.rs:190-235`).
  - The diff only swaps itertools sort/unique for `sort_unstable` plus `dedup` (same ordering), and anyhow for a `TokenStorageError` type.
  - `~/.config/gdrive3/{account.json, <account>/secret.json, <account>/tokens.json}` keep the same names and serde structs (`src/app_config.rs:12-16,259-283`), including 0600 permissions.
  - Verified at runtime: an upstream-format tokens.json is used by the fork.
- **`account export` / `import`:** compatible. Same tar layout (`<account>/` holding secret.json and tokens.json) and same file name `gdrive_export-<normalized>.tar` (normalize_name gives identical output; it was rewritten with Cow in 2003186). Verified at runtime: an upstream 3.9.1 export imports and works in the fork.
  - Small change: export onto an existing file now fails with "File exists (os error 17)", without the path (18d8afe).

## 5. Code quality and maintainability, and fit with the feature PRs we want

**Positives:**
- Current dependencies, MSRV 1.88, a clean build, clippy pedantic clean apart from 3 new-lint warnings, the first unit tests, and `source()` error chains.
- A few real latent fixes (7b9a68e, f4bfea4, 18d8afe), and a panic replaced by an error in `with_native_roots`.
- The hub and connector are centralized in `create_connector()` (`src/hub.rs:22-30`), which actually makes adding a proxy easier.

**Negatives (over-engineering for a roughly 6k-line CLI):**
- Lines of code grow **+32% (6,082 to 8,048) with zero new features**.
- 79 commits create one error enum per function. `app_config/errors.rs` is 333 lines, and there are `download/errors.rs`, `file_tree*/errors.rs`, and so on.
- `permissions/revoke.rs` went from 235 to 458 lines: a hand-written iterator enum with fn-pointer `Filter` types and `size_hint` tests, all to avoid one Vec for about five permissions.
- `LazyLock` mime statics generated by a macro, and a const-eval duplicate check over a fixed-size `[(FileExtension, DocType); 18]` table (`src/common/drive_file.rs:1-140`). Adding Markdown (#159) means touching about 5 places plus the array length.
- `FolderLike` and `FileTreeLike` traits (159 lines), and Cow and borrowed `DisplayConfig<'a>` everywhere.
- Pedantic clippy as warnings, plus `unreachable_pub`, puts a tax on every contributor.
- It introduced 3 regressions: the missing flush, lost UX hints and missing spaces. It also inherited the upstream bug where `upload.rs:277` reports the directory path instead of the file path on an open error.

**All 8 target PRs apply cleanly to upstream `8551556` and all 8 fail on the fork** (`git apply --check`). Hunk counts are from `patch --dry-run -F3` (fuzz=3) on the fork:

| PR | Hunks | Clean/offset | Fuzz | Failed |
|---|---|---|---|---|
| #67 trash | 18 | 4 | 9 | 5 |
| #68 concurrent download | 15 | 2 | 8 | 5 |
| #82 parallel upload | 13 | 1 | 1 | 11 |
| #113 tsv | 15 | 3 | 9 | 3 |
| #141 proxy | 7 | 1 | 3 | 3 |
| #156 drives pagination | 4 | 1 | 0 | 3 |
| #159 markdown export | 14 | 3 | 9 | 2 |
| #162 recursive export | 11 | 0 | 5 | 6 |

Fuzzed hunks against refactored code will not compile as-is.

The three PRs you asked about, tested with `git apply --3way` on the fork (which has upstream's blobs in its history):
- **#68 (concurrent directory download):** 11 conflict regions in 6 files (download.rs 6 regions and 276 lines, main.rs 216, files.rs 69). The PR is "very rough" by its author's own words. It rewrites the directory walk with `futures::stream::buffer_unordered`, plus tokio-util compat and `InspectReader` over a hyper 0.14 body. **Port: a rewrite against the fork's FileTreeDrive and tokio::fs and hyper-1 Body, about 1 day.** It needs hyper-1 work on either base.
- **#82 (parallel upload):** 10 conflict regions in 4 files, mostly rustfmt churn on both sides. The real change is about 20 lines: `join_all` over a folder's files. It has unbounded concurrency, it **ignores per-file errors** (the `join_all` result is dropped) and it adds `unwrap()`. **Port: easy, about 2-4 h.** Re-implement in the fork's `upload_directory` loop (`src/files/upload.rs:273-304`) with `buffer_unordered(N)` and error collection. `UploadDelegate<'a>` borrows `&UploadDelegateConfig`, so it is safe to share. **It needs the section 3 patch first**, or 429s will stall uploads.
- **#141 (proxy):** 6 conflict regions in 3 files (hub.rs 4 regions and 119 lines). It uses `hyper-proxy 0.9`, which is hyper 0.14 only and unmaintained. **Port: a rewrite with hyper-util 0.1.20 `client::legacy::connect::proxy::Tunnel` plus `client::proxy::matcher::Matcher::from_env()` inside the fork's `create_connector()`, about 0.5-1 day.** The effort is the same on either base, because hyper 1 forces the rewrite.

Overall: the fork's churn means every community PR becomes a re-implementation rather than a merge. The heavy style (an error type per function, alloc-free micro-optimizations) will keep conflicting with outside contributions, and the author is heading toward his own codegen.

## 6. License and attribution

- `LICENSE` is unchanged: MIT, "Copyright (c) 2023 glotlabs". There is no `license` field in Cargo.toml (same as upstream).
- `README.md`, `docs/` and `.github/` are **unchanged** (`git diff --stat 8551556..HEAD`), so they still point to glotlabs URLs and the old CI (actions/checkout@v3, deprecated upload-release-asset, no tests or clippy in CI).
- There is no CONTRIBUTING file in either repo. The fork adds no copyright line of its own, and its commits are not signed off.
- Taking code from it is straightforward under MIT: keep the LICENSE and credit "Edoardo Morandi (dodomorandi)" in the cherry-picked commits (`git cherry-pick -x`) or in the changelog.

## 7. Option A (dodomorandi as base) vs option B (upstream main plus our own migration)

The fork's own migration is small, about 150 changed source lines outside Cargo.lock, which gives a reliable effort estimate for option B:
- 9d49372: hub.rs, hub_helper.rs, account/add.rs
- 205f756: Cargo.toml, delegate.rs, download.rs, export.rs, hub.rs (+207/−195 including Cargo.toml)
- 8ca71ac: 7 files, ±25 lines, re-export paths
- ec37b03: delegate.rs backoff iterator
- e858a8c: extra direct dependencies

| | A: adopt dodomorandi | B: upstream main and migrate ourselves |
|---|---|---|
| Dependency migration | done | **1.5-2.5 days**, using the fork's 5 commits as a recipe (hub/connector, delegate signatures, Body via http_body_util, yup-oauth2 `with_client`, backoff iterator, re-export paths) |
| Resumable patch (section 3) | 0.5 day (needed) | 0.5 day (needed) |
| Fix fork regressions (flush, restore `Error:` and hints, missing spaces, decide on size format) | about 1 day | n/a |
| CI modernization (tests, clippy, ARM, new actions) | 0.5-1 day | 0.5-1 day |
| Merge or port the 8 target PRs | **about 4-5 days**: every one is a re-implementation | **about 3-4 days**: they apply cleanly; #68 and #141 still need hyper-1 adaptation and #82 needs a quality rework |
| Script compatibility (output, stderr, sizes) | broken in 3 places, must be restored or documented | preserved |
| Ongoing cost | high: heavy style, a diverging author (codegen branch), no upstream relationship | low: familiar upstream layout, easy for outside contributors |
| Risk | medium: hidden regressions in 265 unreviewed commits (I found 3 by sampling) | low-medium: known recipe, small diff that is easy to review |
| **Total to the first release with the 8 PRs** | **about 6.5-8.5 days** | **about 6-8 days** |

**Recommendation: option B.** Fork upstream `glotlabs/gdrive` main and do the migration in a few reviewable commits that follow dodomorandi's recipe. Include the google-apis-common resumable patch from day one, with the mock-server repro as a regression test. Then cherry-pick the small, high-value fork commits listed below.

Do not take the error-handling overhaul, the perf micro-refactors, `revoke.rs` or the bytesize switch wholesale. Doing so would break script-facing output and turn every community PR into a port. Treat dodomorandi as a reference and a source of cherry-picks, and credit him.

---

## Adoptable items

```json
[
  {"item": "Dependency migration recipe: google-drive3 7.0.0 / google-apis-common 8 / hyper 1 + hyper-util legacy client / hyper-rustls 0.27 / yup-oauth2 12 with_client / direct crates instead of re-exports", "commits": ["9d49372", "205f756", "8ca71ac", "e858a8c"], "value": "high", "risk": "medium", "note": "Port by hand onto upstream main (it will not cherry-pick cleanly after 200+ refactors). About 150 source lines. MUST be paired with the google-apis-common resumable patch, because 8.0.0 skips a chunk after a 5xx or 429 (lib.rs:713/733-734). Also declare hyper-rustls features (ring, http2, native-tokio) explicitly instead of relying on feature unification."},
  {"item": "exponential-backoff 2 iterator-based Backoff", "commits": ["ec37b03"], "value": "medium", "risk": "low", "note": "Small delegate.rs change. With 2.x, max_retries=N gives N-1 retries. Keep the 100_000 upload budget only after the resumable patch, otherwise a stuck session spins for days."},
  {"item": "with_native_roots() error instead of panic (Hub::new returns io::Result)", "commits": ["9d49372"], "value": "medium", "risk": "low", "note": "Helps upstream #112 'could not load platform certs'. Consider adding a webpki-roots fallback."},
  {"item": "Checked numeric casts: negative Drive size becomes an error; page_size usize to i32 checked", "commits": ["f4bfea4", "a730431"], "value": "low", "risk": "low", "note": "Defensive and tiny."},
  {"item": "Keep the stdin temp file alive while it is open (Temp owned by the returned handle)", "commits": ["7b9a68e"], "value": "medium", "risk": "low", "note": "Real latent bug for update/import from stdin, worst on Windows. Reimplement minimally (return the Temp with the File); skip the File<'a> wrapper abstraction."},
  {"item": "TOCTOU-free archive creation via File::create_new", "commits": ["18d8afe"], "value": "low", "risk": "low", "note": "Keep upstream's path-bearing error message ('X' already exists)."},
  {"item": "Chunk-size arithmetic guard in --print-chunk-info", "commits": ["15f7d83"], "value": "low", "risk": "low", "note": "The else-branch formula is off by 2 but unreachable. A simple saturating (last+1)-first is enough."},
  {"item": "Unit tests for pure helpers (truncate_middle ascii and multibyte, md5 parsing, normalize_name, revoke matching)", "commits": ["9e55628", "167ac65", "e6083ab", "700c2d5", "910ca96"], "value": "medium", "risk": "low", "note": "Adapt them to the upstream function signatures. Add our own mock-server test for resumable upload (see analysis/tmp-fork-dodo/_repro_v8)."},
  {"item": "MSRV declaration and clap 4.5 bump", "commits": ["936e3f6", "394a647"], "value": "low", "risk": "low", "note": "Help text changes only cosmetically ('Print help')."},
  {"item": "Error source() chains and error-trace printing", "commits": ["3813d73", "fb074c2", "5253cbb", "194e3c9"], "value": "low", "risk": "medium", "note": "Do NOT adopt wholesale: it drops the 'Error:' prefix and the actionable account hints. If wanted, add source() to the existing upstream enums and keep upstream's Display text."},
  {"item": "human_bytes to bytesize", "commits": ["69b553a"], "value": "low", "risk": "medium", "note": "Changes script-visible output (2 MB to 2.0 MB, KB to kB). Only adopt with a formatter that reproduces the old strings, or as a documented breaking change."},
  {"item": "tokio::fs for download writes", "commits": ["ea3bf2b", "f59bc5a"], "value": "low", "risk": "medium", "note": "As committed it omits flush() before md5 and rename, so last-write errors are swallowed (tokio fs/file.rs:47,104-106). If adopted, add writer.flush().await? (or keep std::fs and a BufWriter)."},
  {"item": "Centralized create_connector() shared by DriveHub and the authenticator", "commits": ["205f756"], "value": "medium", "risk": "low", "note": "A good seam for the proxy feature (#141) via hyper-util Tunnel + proxy::Matcher::from_env()."}
]
```
