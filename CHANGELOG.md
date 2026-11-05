# Changelog

Changes of [gu1p/gdrive](https://github.com/gu1p/gdrive), written for users of gdrive 3.9.1, the last
release of [glotlabs/gdrive](https://github.com/glotlabs/gdrive). Issue and pull request numbers (#N)
are those of glotlabs/gdrive.

## 4.0.0-alpha.1 (unreleased)

The first pre-release of the fork. Accounts in `~/.config/gdrive3` and archives of `gdrive account export`
from 3.9.1 work without changes, and every 3.9.1 command and flag is still there.

### Breaking or behavior changes

- **Recursive transfers run 4 files at a time** (`--jobs N`, 1 to 64; `--jobs 1` for one at a time). The
  stdout lines of `files download --recursive` and `files upload --recursive` (`Creating directory ...`,
  `Downloading file '...'`, `Uploading file '...' with id: ...`, the `<path>: <id>` lines of
  `--print-only-id`) come in the order the transfers start and end, not in the order of the folder. A
  recursive download prints `Found N files in M directories ...` when the listing is complete, no longer
  before the first download.
- **New lines on stderr after a recursive transfer**: a summary,
  `Summary: 12 transferred (3.4 MB), 2 unchanged, 3 skipped, 1 failed, 4 cancelled`, and for downloads one
  line per kind of item that was skipped (Google Docs, shortcuts, Google types without an export format),
  which 3.9.1 left out silently. Retries, renamed local names and incomplete listings print `Warning:`
  lines on stderr. stdout keeps its lines.
- **Exit status of recursive transfers**: 1 whenever any file or folder failed. The first failure still
  stops the run; with the new `--continue-on-error` the other files are finished and the command still
  exits with 1. A recursive download also exits with 1 when Drive reports an incomplete listing
  (`incompleteSearch`) unless `--continue-on-error` is given. Ctrl-C cancels the running transfers, removes
  their temporary files and exits with 1.
- **Commands other than `account add` and `account reauth` never start a browser login.** When the stored
  token cannot be refreshed they fail at once with `Error: account 'NAME' needs to be authorized again:
  ...; run 'gdrive account add' (or 'gdrive account reauth NAME') to authorize again`. 3.9.1 started the
  browser flow and waited on port 8085, also in cron jobs. A token refresh gives up after 60 seconds.
- **The login's redirect URI is `http://localhost:PORT`** (3.9.1: `http://127.0.0.1:8085`). OAuth clients
  of type Desktop app accept it; a Web application client that was registered with the 3.9.1 URI needs the
  new one, or a Desktop app client. The authorization URL asks for consent every time, so that Google
  always returns a refresh token. The login waits at most 10 minutes for the browser, and a busy port is an
  error that suggests `--port` or `--manual`.
- **Proxy variables are honored.** `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` (also in lower
  case) now apply to every connection; 3.9.1 ignored them. A value gdrive cannot use (`socks4://`, not a
  URL) is an error.
- **Progress on stderr.** On a terminal, downloads, uploads, updates, imports and exports draw a progress
  bar on stderr. `--no-progress` or `GDRIVE_PROGRESS=0` turns it off; when stderr is not a terminal nothing
  is drawn unless `--progress` is given or `GDRIVE_PROGRESS=1` is set. stdout is unchanged.
- **`files info`** prints a last line `Trashed: yes` (or `Trashed: yes (explicitly)`) for a file in the
  trash. The output for any other file is unchanged.
- **Local names of downloads.** A Drive name that cannot be a local file name is changed: `/`, `\` and
  control characters (on Windows also `<>:"|?*`) become `_`, Windows device names (`CON`, `NUL`, ...) and
  names ending in a dot or a space get a `_`, and names longer than 255 bytes are shortened. Two files of
  one folder with the same name no longer overwrite each other: the one with the lower id keeps the name,
  the other gets `~<8 hex digits>` before the extension. Renamed files are printed as warnings. A name
  that is empty, `.` or `..` fails with a hint to rename the file.
- **Temporary download files** are `.gdrive-<file id>.part` next to the target (3.9.1:
  `<name>.incomplete`), and are removed when the download fails.
- **Recursive upload and symbolic links**: links to files and folders are followed and uploaded under the
  link's name (3.9.1 refused links to files). A link that leads back into a folder being uploaded is
  skipped with a warning instead of recursing without end. A broken link or an unreadable entry is a failed
  item: by default the upload stops before it creates anything, with `--continue-on-error` the rest is
  uploaded.
- **Upload retries**: an upload gives up after 10 attempts or 10 minutes without progress, and every chunk
  that Drive confirms starts a new budget. 3.9.1 retried `files upload` up to 100,000 times and
  `files update` 20 times. Every other request now retries too (see New).
- **Uploads are checked**: the MD5 checksum Drive reports is compared with the local file, and a file
  that changes size during its upload fails the command; the error names the uploaded file's id. Files of
  at most 5 MB go up in a single request (`--multipart-threshold`).
- **`files export`** gives the exported file the document's modification time on Drive.
- **Account archives**: `account export` leaves out the account's new `uploads/` directory (records of
  unfinished uploads, which hold their session URLs), and `account import` skips `uploads/` and `.staging`
  entries. `account import` refuses an archive with an entry outside the account directory (`..`, a drive
  prefix), and `account.json` and `.staging` are not accepted as account names.
- **Error messages** still start with `Error:`, exit with status 1 and give the hints of 3.9.1 (`Use
  gdrive account list ...`), but their wording comes from the new code base, with the causes on the same
  line: `Error: unable to download file from drive: ...` instead of `Error: Failed to download file: ...`.
- **`gdrive version`** prints `4.0.0-alpha.1` and the commit it was built from.

### New

- **One-way sync.** `files sync up <LOCAL_DIR> <FOLDER_ID>` and `files sync down <FOLDER_ID> <LOCAL_DIR>`
  copy new and changed files one way, print one line per item (`create`, `update`, `unchanged`, `extra`,
  `skip:<reason>`), and exit 1 when anything failed. `--dry-run` prints the same plan without touching
  anything. `--delete` runs only after a complete inventory and a fully successful transfer phase; Drive
  extras go to the Drive trash and local extras to `<LOCAL_DIR>/.gdrive-trash/<time>/`
  (`--delete-permanently` for real removal). Duplicate or ambiguous names, Google Docs without
  `--export`, shortcuts, symlinks and unreadable entries are skipped and never deleted. Takes `--jobs`,
  `--continue-on-error`, `--export` (down), `--acknowledge-abuse` (down) and the progress flags.
  See docs/sync.md. Closes the rsync-style requests #84, #16, #116, #69.
- **Migration between accounts.** `files migrate <FILE_ID>... --to <ACCOUNT> [--from <ACCOUNT>] [--into
  <FOLDER_ID>]` copies files and folders of one logged-in account (the current one by default) into
  another, streaming the content through this machine without writing it to disk. Folders are recreated
  before what goes into them; files are checked by MD5 and keep their modification time; Google Docs,
  Sheets and Slides are exported and converted back (10 MB export limit); shortcuts, Drawings, Forms
  and the other Google types are skipped and listed. Running it again reuses destination folders of the same name
  and skips files that are already there with the same content; a file of the same name with other content
  is a failed item, never overwritten, and an item that both accounts see (the same Drive id) is never
  copied onto itself. Interrupted large uploads continue on the next run. `--dry-run` only reads the
  destination; `--trash-source` moves migrated files, and folders whose whole content was migrated, to
  the source's trash, after checking each again: a file that changed or a folder that got new files
  stays. Prints one line per item (`create`, `reuse`, `convert`,
  `already-there`, `skip`, `trash`) and a summary; takes `--jobs`, `--continue-on-error`, the chunk and
  progress flags of `files upload`. Sharing, comments and revisions are not migrated. See
  docs/migrate.md.
- **Migration as an archive.** `files migrate <FILE_ID>... --to <ACCOUNT> --archive` makes each `FILE_ID`
  one plain tar file, `<name>.tar`, in the destination, streamed from the source through memory into a
  resumable upload. Google Docs, Sheets, Slides and Drawings go in as docx, xlsx, pptx and png; shortcuts
  and Google types without an export are skipped and listed. Names are made portable to Windows, macOS
  and Linux, the order and the bytes do not depend on Drive's listing, and long and non-ASCII paths and
  files of 8 GiB and more use pax headers. Each file is checked against its MD5 as it is read; a file that
  changes during the run fails the archive. A second run finds the archive already there (by a digest in
  its `appProperties`), fails on a changed folder instead of overwriting, and continues an interrupted
  upload with the exports of the first run. Not with `--trash-source`. See docs/migrate.md.

Accounts and login:

- `gdrive account reauth [NAME]` signs in again with the account's stored client and replaces its
  tokens, after checking that it is the same Google account.
- `account add --manual` (alias `--no-browser`) for machines without a browser: open the printed URL
  anywhere and paste the address the browser lands on. `--port N` sets the redirect port (`0` picks a free
  one, in browser mode only; `--manual` needs a fixed port). The login sends PKCE and checks `state`; a
  redirect of another login is ignored.
- Service accounts: `account add --service-account KEY_JSON [--subject EMAIL] [--name NAME]`, with
  `--subject` for domain-wide delegation. See [Service accounts](docs/service_accounts.md).
- Application Default Credentials: `account add --adc [--name NAME]` uses `GOOGLE_APPLICATION_CREDENTIALS`,
  gcloud's application default credentials or the metadata server of Google Cloud, looked up on every run.
- Proxies: `http://`, `https://`, `socks5://` and `socks5h://` URLs with user and password, and `NO_PROXY`.
  The proxy URL is never printed. See
  [Behind a proxy](docs/remote_and_headless.md#e-behind-a-proxy).

Transfers:

- Retries for every request (listings, metadata, downloads, uploads): `408`, `429`, `500`, `502`, `503`,
  `504`, `403` rate limits, dropped connections, and replies of reads that break off, with backoff and
  jitter. `Retry-After` is honored, and a rate limit seen by one request holds back all requests of the
  command.
- A download that breaks off continues with a `Range` request from the byte where it stopped, only while
  the file on Drive is the same revision. The byte count is checked also for files without an MD5
  (exports, Workspace files). The resume works within one run.
- gdrive's own resumable uploader for `files upload`, `files update`, `files import` and recursive
  uploads: chunks are streamed from the disk (a larger `--chunk-size` takes no more memory), Drive's
  `Range` decides where to continue, and a lost final reply does not create a second file (new files get
  their id before the upload; imports are found by a marker).
- An interrupted `files upload`, `files update` or `files import` of an unchanged file continues in the
  next run, for up to a week. The session records are in `~/.config/gdrive3/<account>/uploads/`.
- `--multipart-threshold MB` on `files upload`, `files update` and `files import`; `files import` also
  takes `--chunk-size`.
- `--jobs N` and `--continue-on-error` on `files download --recursive` and `files upload --recursive`.
  A recursive download lists folders page by page, up to 50 folders per request, and starts downloading
  while it lists.
- `files download --recursive --export SPEC` exports Google Docs, Sheets, Slides and Drawings instead of
  skipping them: `default` (docx, xlsx, pptx, png), `md` (Docs as Markdown), or per type such as
  `md,sheets=ods` or `docs=pdf,drawings=skip`. An export whose local file has the document's modification
  time is not exported again.
- `files export --format EXTENSION` picks the format regardless of the file name; Drawings can be exported
  (pdf, jpg, png, svg).
- Progress on stderr (see above): a bar with rate and time left; for recursive transfers a line for the
  run and one per file. `--progress`, `--no-progress` and `GDRIVE_PROGRESS`.
- `files download --acknowledge-abuse` downloads files that Google flagged as malware or spam, for their
  owner; also with `--recursive` and `--follow-shortcuts`.

Commands and output:

- `files trash` and `files untrash`; `files delete`, `files trash` and `files untrash` take several ids.
- `files list --tsv` (tab-separated, names not cut, tabs and newlines escaped) and `--json`.
- `files upload --name NAME` and `files update --name NAME`.
- Markdown: `files export <ID> notes.md` and `files import notes.md`.
- Short flags: `-m` (`--max`), `-q` (`--query`), `-p` (`--parent`), `-r` (`--recursive`), `-o`
  (`--overwrite`), `-d` (`--destination`).
- `gdrive completions bash|zsh|fish|powershell|elvish`.
- `drives list --max N`.

Platforms and releases:

- Release archives for Linux x86_64 and ARM64 (static musl binaries), macOS Apple silicon and Intel (the
  `macos-x64` archive now holds an x86_64 binary), and Windows x86_64 with the C runtime linked in, so no
  Visual C++ Redistributable is needed. Each release has a `SHA256SUMS` file.
- CI checks formatting and clippy, builds with `--locked` and runs the tests on Linux, macOS and Windows,
  and builds with the minimum Rust version, 1.88.

### Fixed

- Drive names are no longer joined into local paths unchecked: a name with `/` broke a recursive
  download, and `../x` or an absolute name wrote outside `--destination` (see Local names above).
- Temporary download files no longer collide: `a.pdf` and `a.docx` shared `a.incomplete`, a Drive file
  named `a.incomplete` was overwritten, and a long name grew past the file system's limit.
- `files list` no longer drops `--query` when `--parent` or `--drive` is given: the query is combined with
  them (`'<parent>' in parents and trashed = false and (<query>)`), and with `--drive` it searches the whole
  shared drive. Scripts that passed both get the filtered result now.
- `drives list` lists every shared drive, not at most 10.
- A cron job whose token cannot be refreshed no longer hangs on port 8085 waiting for a browser, and the
  next run no longer fails with "Address in use".
- Signing in again no longer loses the refresh token: when Google returns none, the stored one is kept.
- `tokens.json` is replaced atomically through a private temporary file (mode 0600 on Unix), so a full
  disk or two processes writing at once can no longer leave it empty or mixed.
- Errors about a malformed `secret.json` or `tokens.json` no longer quote the file, which holds secrets.
- No CA certificates on the system is an error instead of a panic.
- `files update <ID> <folder>` is an error instead of a panic; special files are refused too.
- `files update <ID>` with the content on stdin no longer renames the Drive file to the name of a
  temporary file.
- A download refused as malware or spam ends with the hint to rerun with `--acknowledge-abuse`.

Regressions of the library update, fixed before any release (3.9.1 did not have them):

- google-apis-common 8, which google-drive3 7 uses for resumable uploads, lost the chunk-retry fix that
  3.9.1 built from a patched fork: a retried chunk was skipped and the upload stalled, and a cut final
  reply was reported as an error. gdrive vendors the crate with the fixes
  ([GDRIVE-PATCHES.md](vendor/google-apis-common/GDRIVE-PATCHES.md)); uploads now go through gdrive's own
  uploader.
- With asynchronous file writes, an error on the last write of a download could be lost: the file is
  flushed and closed before its MD5 is checked and it is renamed into place.

### Upstream issues addressed

| Change | Issues |
|---|---|
| Short flags | #5 |
| Progress on stderr | #12, #44 |
| Several ids for `files delete`, `trash`, `untrash` | #14 |
| `--acknowledge-abuse` and its hint | #15, #136 |
| Credentials guide for the current Google Auth Platform | #17, #24, #35, #109, #115, #121, #129, #160 |
| `files update` on a folder | #18 |
| Safe and unique local names | #19, #53 |
| Proxy support | #21, #26, #59, #74, #151 |
| No browser login outside `account add` and `account reauth`, `--port` | #22, #33, #51, #94, #147, #149 |
| `--name` for uploads from stdin | #23 |
| Retries and `Range` resume of downloads | #25, #78 |
| `account add --manual` | #30, #71, #114, #125, #161 |
| Retries of uploads and listings, uploads continued in the next run | #36, #87 |
| Application Default Credentials (no raw access tokens yet) | #37 |
| Basic usage guide, gdrive 2 commands, export notes | #40, #62, #98, #106, #148 |
| Symbolic links in recursive uploads | #42 |
| Service accounts | #49 |
| `files list --tsv` and `--json` | #52 |
| Missing or untrusted CA certificates no longer panic (library update) | #58, #112 |
| ARM64 release binaries, correct macOS archives | #63, #99, #101, #135 |
| Export of Google Docs in recursive downloads | #64, #123 |
| Shell completions | #77 |
| Builds from the lockfile, minimum Rust version checked | #79, #131 |
| `--query` combined with `--parent` and `--drive`, current links in its help | #81, #117, #155 |
| All shared drives in `drives list` | #119, #142 |
| Windows build without the Visual C++ runtime | #128 |
| Markdown export and import | #143 |
| Refresh token kept on a new login | #150 |

Ported or adapted from upstream pull requests: #67 (trash), #113 (TSV idea), #144 and #156 (shared drive
paging), #146 and #159 (Markdown), #162 and #164 (export in recursive downloads), #163 (short flags), #68
and #82 (concurrent transfers, the idea), #130, #154 and #158 (ARM builds, the idea), #141 (proxy, the
requirement), #132 (SSH tunnel guide), #66, #96, #137 and #138 (documentation). The authors are named in
the [README](README.md#license-and-credits).
