# gdrive modernization plan

Triage of every upstream issue (133) and pull request (31) of [glotlabs/gdrive](https://github.com/glotlabs/gdrive),
done on 2026-11-04 for the fork at [gu1p/gdrive](https://github.com/gu1p/gdrive). Upstream has not been
touched since 2024-02-01 (v3.9.1) and is effectively unmaintained.

Method: five review agents classified the issues in batches, three reviewed every PR diff and patch-checked
it against upstream main and the dodomorandi fork, two evaluated the active forks (including building and
testing them), and one audited the upload/download path and the release pipeline. An independent review by a
different model (codex, gpt-6-astra) covered the same material and is included as a cross-check. Where the two
reviews disagree it is noted.

Files in this directory:

| File | Content |
|---|---|
| `issues.md` | All 133 issues: category, area, priority, speed/modernization flags, effort, what addresses it, recommendation |
| `pulls.md` | All 31 PRs: quality, whether the patch still applies, recommendation (merge / rework / cherry-pick idea / skip), review notes |
| `triage-data.json` | The same data, machine readable |
| `report-fork-dodomorandi.md` | Evaluation of the dodomorandi fork as base: build, tests, behavior changes, resumable-upload regression |
| `report-fork-toutizes.md` | Evaluation of the toutizes fork (parallel transfers, sync): architecture, data-safety bugs |
| `report-transfer-path-and-ci.md` | Audit of upload/download internals, retries, HTTP stack, release workflows, with ranked proposals |
| `report-codex-peer-review.md` | Independent peer review: strategy, top 25, transfer design, PR and issue dispositions, risks |

## 1. Base tree decision

`main` of this fork is the [dodomorandi/gdrive](https://github.com/dodomorandi/gdrive) tree (commit `ec37b03`,
265 commits ahead of upstream). It already migrated the dependencies that block upstream:

| Dependency | upstream 3.9.1 | this fork |
|---|---|---|
| google-drive3 | 5.0.2 from a git fork (prasmussen, branch `resumable-fix`, Jan 2023) | 7.0.0 from crates.io |
| google-apis-common | 5.0.2 (git fork) | 8.0.0 |
| hyper / hyper-rustls | 0.14 / 0.23 | 1.x / 0.27 (hyper-util client) |
| yup-oauth2 | 8.1 | 12.1 |
| exponential-backoff | 1.1 (overflows after 32 attempts) | 2.1 |

Verified on rustc 1.99.1: release build with 0 warnings, 13/13 tests pass, clippy has 3 trivial warnings,
existing `~/.config/gdrive3` tokens and `account export` archives work unchanged, no CLI flag changed.

The dedicated fork evaluation preferred starting from upstream and redoing the migration (reasons: the code
grew 32% from per-function error types, size and error-message formatting changed, feature PRs conflict
textually). The peer review preferred dodomorandi. We chose dodomorandi because the objections are small fixes
(milestone 0), the PRs that conflict are the ones we rewrite anyway, and the migration plus the async file I/O
conversion plus the tests are work we would otherwise repeat. Upstream remains the behavioral reference
(`upstream` remote) and dodomorandi's commits keep their authorship.

## 2. Release blocker found in the migrated dependency

google-apis-common 8.0.0 (the crate that performs resumable uploads for google-drive3 7) advances the byte
offset before a failed chunk is retried and never rewinds it. The old git fork fixed exactly this
("Retry previous chunk"). Confirmed with a mock server: one 503 on chunk 2 makes the upload hang or fail; the
same test passes with the git fork and with 8.0.0 plus a one-line rewind. Also lost: the retry on a failed
response-body read, so a successful upload can be reported as an error and a re-run creates a duplicate file.
Nobody has fixed this upstream (Byron/google-apis-rs issue 438 is open since 2023).

Any tree on google-drive3 7 has this problem. Fix options, in order of preference:

1. Write gdrive's own resumable upload module over the shared hyper client (milestone 2). This also gives
   progress, true resume across runs, md5 verification, correct 308 `Range` handling and PUT semantics.
2. Until then, pin a patched google-apis-common via `[patch.crates-io]` with the rewind and body-read fixes,
   covered by the mock-server test from `report-fork-dodomorandi.md`.

## 3. Milestones

### Milestone 0: make the base safe to ship (days)

- Patch google-apis-common chunk retry (section 2) and add the mock-server regression test.
- `files download`: flush and close before rename; a write error on the last chunk is currently lost.
- Restore the `Error:` prefix and the "run `gdrive account ...`" hints that the fork's error overhaul dropped.
- Keep upstream's size formatting (`2 MB`, `KB`) so scripts keep working; `bytesize` prints `2.0 MB`, `kB`.
- Set a distinct version (4.0.0-alpha), keep `Commit:` populated in release builds (currently "unknown").
- CI workflow: `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo test`, `cargo build --locked`,
  Dependabot for cargo and actions.

### Milestone 1: correctness and safety (P0 and P1 bugs)

| Issue | Problem | Fix |
|---|---|---|
| #19, #53 (P0) | Drive names are joined into local paths unchecked: `/` breaks the download, `../x` or an absolute name writes outside `--destination`; `.incomplete` temp names collide (`a.pdf` and `a.docx`) and can exceed 255 bytes | Sanitize every path component, reject traversal, unique temp names keyed by file id, name mapping kept in a manifest |
| #155, #117 | `--query` is silently dropped when `--parent` or `--drive` is given (`src/main.rs`) | Compose the predicates |
| #36, #25, #78 | Retries cover only 5xx and 429; 403 rateLimitExceeded and 408 abort; downloads and listings never retry; one HTTP/2 GOAWAY kills a long download; backoff counter never resets | Reason-aware retry policy with jitter and Retry-After for all request kinds; Range resume for downloads; reset after progress |
| #147, #33, #94, #149 | A failed token refresh silently starts the browser login and holds port 8085 forever; cron jobs hang | Non-login commands use a refresh-only credential path and fail fast; configurable port |
| #150, #51, #22 | A re-authorization that returns no refresh token overwrites the stored one | Preserve the existing refresh token; explicit `account reauth` |
| #112, #58, #80 | No CA bundle panics (fixed on the fork: now an error); Windows UnknownIssuer | rustls-platform-verifier and a clear trust-store error |
| #18 | `files update <dir>` panics on an unwrap inside the library | Validate early |
| #136, #15 | Abusive-file 403 aborts a recursive job | `--acknowledge-abuse` and per-file error accounting with non-zero exit |
| #42 | Symlink policy inconsistent | Define it; detect cycles |

### Milestone 2: transfer engine (the speed work)

Today: zero concurrency (one file at a time in recursive upload and download), 32 MiB chunks held entirely
in RAM, small files sent as one in-memory multipart request, full tree materialized before any transfer,
md5 not verified on upload, session URL only in memory. Both reviews converged on the same design:

1. **Own resumable uploader**: streamed 256 KiB-multiple chunks from disk, server-driven offset from the
   308 `Range` reply, PUT for chunk and status requests, persisted session record (account, target, source
   identity and mtime, session URL, confirmed offset) for resume across runs (#87, #36), md5 check after
   finalize, pre-generated ids to avoid duplicate creates. Multipart threshold decoupled from chunk size.
2. **Bounded scheduler**: separate limits for listing and media jobs (start with 4 transfers and 2 listings,
   `--jobs N`), bounded pending queue, typed per-file outcome, every handle awaited, non-zero exit on any
   failure, `--continue-on-error`. Create parent folders before their children, overlap unrelated subtrees.
   Reuse the shape of PR #68 (lazy per-folder listing feeding a bounded stream) and the per-folder task
   breakdown of toutizes; rewrite everything else (both drop errors with exit 0, #82 is unbounded and panics
   past the fd limit, toutizes corrupts files through temp-name collisions).
3. **Incremental listing**: page-by-page discovery with purpose-specific `fields` masks, downloads start
   during the walk, up to ~50 parent ids per query, `incompleteSearch` honored.
4. **Download resume and integrity**: Range resume with 206 and Content-Range validation, 200-to-Range means
   restart, 416 rechecks, size check even without md5, atomic replace. Optional 2 to 4 ranges per large file
   later, benchmark first.
5. **Progress** (#12, #44: the most requested feature): event driven, throttled, on stderr, off when not a
   TTY, per file and aggregate, confirmed bytes not bytes sent.
6. **Skip and sync semantics** (#152, #56, #84, #16, #116, #69, #65, #55): explicit `--skip-existing` /
   checksum modes, one-way update of changed files by md5 and mtime, deletion as a separate explicit phase that
   never runs on an incomplete inventory, trash instead of delete, `--dry-run`.
7. **Recursive export of Google Docs** (#123, #64, PR #162 / #164): export during recursive download with
   explicit format, counted as unsupported rather than silently skipped, name collisions handled.
8. **Chunk size**: benchmark 8/32/64 MiB with 1/2/4/8 concurrent files; cap aggregate buffered bytes.
9. **HTTP**: shared pooled hyper 1 client for OAuth and Drive, separate connect/header/body/operation
   timeouts, HTTP/2 window sizes reviewed (2 MiB per stream caps throughput per round trip).

Drive API constraints: resumable chunks must be sequential within one session (file-level concurrency is the
parallelism), 750 GB per user per day upload cap, 5 TB max file, session URLs expire after a week, export has a
10 MB limit, batching does not cover media. Projects created from May 2026 use Google's quota-unit model.

### Milestone 3: authentication

- **Service accounts** (#49, 23 reactions): yup-oauth2 12 already has `ServiceAccountAuthenticator` with
  `subject()` for delegation; the fork disables the feature flag, re-enable and wire `account add --service-account`.
  Document that service accounts have no storage quota (shared drives or delegation).
- **ADC and raw access tokens** (#37): same provider interface.
- **Headless login** (#125, #161, #71, #114, #30): `account add --manual` accepting the pasted redirect URL,
  keep the SSH-tunnel guide (PR #132). No OOB; remove the stale `urn:ietf:wg:oauth:2.0:oob` string.
- **PKCE and state** on the loopback flow (yup-oauth2 12 sends neither).
- **Proxy** (#26, #74, #21, #59, #151; PRs #140/#141): hyper-util's built-in proxy support (env vars,
  NO_PROXY, CONNECT, SOCKS5) under the rustls connector, one change in `hub.rs`; never print the proxy URL.
- **Credentials guide rewrite** (#129, #160, #109, #115, #121, #35, #24; PR #96 screenshots): Desktop client
  type, test users and the seven-day Testing refresh token, publishing to production, app-name rules.
- Account import: stage and validate archive entries before unpacking into the config root; restrictive
  permissions on create, atomic token writes.

### Milestone 4: platforms and releases

- One matrix release workflow: linux x86_64-musl, linux aarch64-musl on the free `ubuntu-24.04-arm` runner,
  macOS arm64 and x86_64, windows x86_64; `--locked`; checksums; immutable tags. Use a maintained upload
  action (taiki-e/upload-rust-binary-action or cargo-dist); armv7 via cargo-zigbuild only if someone maintains
  it. Fix the macOS asset: `macos-latest` is arm64 but the asset is named x64 (#63, #101, #135, #99, #79;
  PRs #130, #153, #154, #158).
- Windows: static CRT or documented VC++ runtime (#128).
- Shell completions from clap (#77).

### Milestone 5: smaller features (community PRs to port by hand)

| Item | Source | Note |
|---|---|---|
| Trash / untrash instead of delete | PR #67 | Fix `--skip-trashed` (list fields omit `trashed`), drop duplicated module |
| Real TSV and JSON output | PR #113, #52 | Escape tabs and newlines, no truncation |
| Shared-drive pagination | PR #156 (#119, #142) | usize, list all by default; skip PR #144 (bundles IDE files and a dependency downgrade) |
| Markdown export and import | PR #159 (#143) | Preferred over #146; keep #146's help text |
| Short flags | PR #163 | Decide `-s` before adding sync |
| Docs fixes | PRs #137, #138, #132 | Merge as-is |
| Delete many ids, folder size, `.gdriveignore`, delete-local-after-upload, per-command `--account`, secret-store backend | #14, PR #126, #86, #93, #72, #76 | Backlog |

## 4. PR dispositions (18 open upstream PRs)

Merge as-is: #132, #137, #138, #163 (re-apply by hand on this tree). Rework: #67, #113, #156, #159, #162.
Cherry-pick the idea only: #68, #82, #141, #154, #158, #130. Skip: #96 (fold into the guide rewrite), #146
(superseded by #159), #157 (TUI, does not belong in a scriptable CLI; its upload progress wrapper is the only
reusable part). Closed-unmerged PRs worth mining: #164 (export, mtime, name sanitizing; it is serebrov's fork
main opened by accident, not a migration), #126. Reject: #75, #140, #144, #153, #127.

## 5. Issue backlog for this fork

Forks start with an empty tracker. Import these as canonical issues (duplicates folded in):

- Transfers: #19 (+#53), #25 (+#78), #36 (+#87), #12 (+#44), #84 (+#16, #116, #69, #65, #55), #152 (+#56),
  #123 (+#64), #136 (+#15), #86, #14, #93
- Auth: #49, #37, #125 (+#161, #71, #114, #30), #147 (+#33, #94, #149), #150 (+#51), #129 (+#160, #109,
  #115, #121, #35, #24), #26 (+#74, #21, #59, #151), #43, #72, #76, #83
- Listing and output: #155 (+#117), #119 (+#142), #52, #73, #34 (+#13), #50 (+#111), #61
- Platform: #63 (+#101, #135, #99, #79, #131), #128 (+#122), #112 (+#58, #80), #77, #46, #70
- Docs: #106 (+#62, #48, #40, #148), #81, #98
- Code: #18, #42, #143

Answer and close (already fixed, questions, thanks, v2 confusion): #6, #23, #28, #39, #45, #57, #60, #88,
#89, #90, #95, #102, #134, #145.

## 6. Risks to keep in view

- Google OAuth: Testing apps get seven-day refresh tokens; restricted Drive scopes; personal use under 100
  users can stay unverified with a warning. The fork must not ship a shared client id.
- yup-oauth2 12 builds the loopback redirect as `localhost`, upstream used `127.0.0.1`; only matters for
  users who registered a Web-application client with the exact URI. Document Desktop client type.
- Account archives are tar (the help text says zip). Keep the on-disk layout stable.
- All trees still say version 3.9.1; upstream once moved a tag (#107). Never move published tags.
- Nothing in this triage measured throughput. Benchmark before choosing concurrency and chunk defaults.
