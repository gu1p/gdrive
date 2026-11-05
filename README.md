# gdrive

gdrive is a command line tool for Google Drive. It lists, uploads, downloads, exports and shares files, and
it works well in scripts. It is a single binary written in Rust.

This repository, [gu1p/gdrive](https://github.com/gu1p/gdrive), is a maintained fork of
[glotlabs/gdrive](https://github.com/glotlabs/gdrive), gdrive 3 by Petter Rasmussen. gdrive 3 is the
successor of [prasmussen/gdrive](https://github.com/prasmussen/gdrive) (gdrive 2). Upstream's last release
is 3.9.1 from February 2024, and it is no longer maintained.

The code is based on [dodomorandi/gdrive](https://github.com/dodomorandi/gdrive), which moved gdrive to
current libraries: google-drive3 7, hyper 1, yup-oauth2 12 and tokio. Commands, options and output stay
compatible with gdrive 3.9.x.

## Status

**4.0.0-alpha.** The fork is under active work and has no stable release yet. All 3.9.1 commands work.
Existing accounts in `~/.config/gdrive3` and archives from `gdrive account export` work without changes.

The plan, with milestones and the triage of every upstream issue and pull request, is in the
[roadmap](docs/triage/README.md). Report problems with this fork in
[its issue tracker](https://github.com/gu1p/gdrive/issues), not upstream.

## Install

### Release binaries

Download the archive for your platform from the
[releases page](https://github.com/gu1p/gdrive/releases), together with `SHA256SUMS`. The first release
is the pre-release `v4.0.0-alpha.1`. Pre-releases are not "latest", so download them by tag:
`https://github.com/gu1p/gdrive/releases/download/<tag>/<archive>`.

| Platform | Archive |
|---|---|
| Linux x86_64 | `gdrive_linux-x64.tar.gz` |
| Linux ARM64 | `gdrive_linux-arm64.tar.gz` |
| macOS, Apple silicon | `gdrive_macos-arm64.tar.gz` |
| macOS, Intel | `gdrive_macos-x64.tar.gz` |
| Windows x86_64 | `gdrive_windows-x64.zip` |

The Linux binaries are statically linked and run on any distribution. The Windows binary has the C
runtime linked in and needs no Visual C++ Redistributable. There is no 32-bit ARM binary;
[docs/releasing.md](docs/releasing.md#supported-platforms) has a build command for it that CI does not test.

**Linux and macOS**

```sh
TAG=v4.0.0-alpha.1               # the release tag, as shown on the releases page
ASSET=gdrive_linux-x64.tar.gz    # your archive from the table
curl -fLO "https://github.com/gu1p/gdrive/releases/download/$TAG/$ASSET"
curl -fLO "https://github.com/gu1p/gdrive/releases/download/$TAG/SHA256SUMS"
grep "$ASSET" SHA256SUMS | sha256sum -c      # macOS: grep "$ASSET" SHA256SUMS | shasum -a 256 -c
tar -xzf "$ASSET"
sudo install -m 755 gdrive /usr/local/bin/gdrive
gdrive version
```

The checksum line must end in `OK`. Without `sudo`, install into a directory in your `PATH`, such as
`~/.local/bin`.

The binaries are not code signed. If you downloaded the archive with a browser and macOS refuses to run
gdrive, remove the quarantine flag:

```sh
xattr -d com.apple.quarantine /usr/local/bin/gdrive
```

**Windows**

1. Download `gdrive_windows-x64.zip` and `SHA256SUMS`.
2. Check the archive in PowerShell and compare the hash with the line in `SHA256SUMS`:

   ```powershell
   Get-FileHash .\gdrive_windows-x64.zip -Algorithm SHA256
   ```

3. Unpack `gdrive.exe` into a folder in your `PATH`, for example `%USERPROFILE%\bin`.
4. SmartScreen may warn about an unknown publisher, because the binary is not signed. Click
   **More info**, then **Run anyway**.

### Build from source

You need Rust 1.88 or later ([rustup](https://rustup.rs/)).

```sh
cargo install --git https://github.com/gu1p/gdrive --locked
```

This installs `gdrive` from `main` into `~/.cargo/bin`; add `--tag v4.0.0-alpha.1` for the release. From a
clone, `make install` builds the release binary and puts it in `~/.local/bin` (set `PREFIX=/usr/local`
or `BINDIR=...` for another place); it tells you if that directory is not on your PATH. `make check`
runs the same formatting, lint and test gate as CI, and `make help` lists the targets.

## Quick start

1. Create your own Google OAuth client: [Create Google API credentials](docs/create_google_api_credentials.md).
2. Add your account and follow the link that gdrive prints:

   ```sh
   gdrive account add
   ```

3. Try it:

   ```sh
   gdrive files list
   gdrive files upload notes.txt
   ```

More examples are in [Basic usage](docs/basic_usage.md).

### Using gdrive on a remote server

The login sends your browser to `localhost:8085` on the machine that runs gdrive. On a server without a
browser, see [Remote and headless setup](docs/remote_and_headless.md): export and import an account, an
SSH tunnel, `gdrive account add --manual` (4.0.0), and unattended use in cron jobs.

## Commands

```text
gdrive account      add, list, current, switch, remove, export, import
                    reauth, add --service-account, add --adc (4.0.0)
gdrive files        list, info, download, upload, update, delete, mkdir, rename,
                    move, copy, import, export, sync up, sync down, migrate (4.0.0)
gdrive drives       list
gdrive permissions  share, list, revoke
gdrive about        information about gdrive
gdrive version      version information
```

Run `gdrive <command> --help` for the options, for example `gdrive files list --help`.

- One-way sync of a local directory and a Drive folder, with `--dry-run` and a `--delete` that only
  moves things to a trash: [Sync a folder one way](docs/sync.md).
- Migration of files and folders to another Google account that is logged in too, streamed through this
  machine without local copies, safe to run again, with `--dry-run` and `--trash-source`:
  [Migrate files to another account](docs/migrate.md). With `--archive`, each folder arrives as one tar
  file, made on the fly.

- Service accounts and Application Default Credentials, for CI and servers:
  [Service accounts](docs/service_accounts.md).
- Behind a proxy (`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, SOCKS5):
  [E. Behind a proxy](docs/remote_and_headless.md#e-behind-a-proxy).

## Credentials and security

gdrive stores accounts under `~/.config/gdrive3/` (on Windows, `%USERPROFILE%\.config\gdrive3\`):

| File | Content |
|---|---|
| `account.json` | The name of the current account |
| `<account>/secret.json` | Your OAuth client ID and client secret |
| `<account>/tokens.json` | Access token and refresh token |
| `<account>/account.json` | Service accounts and ADC accounts only: the account type (4.0.0) |
| `<account>/service_account.json` | Service accounts only: the service account's key (4.0.0) |

Anyone who can read these files can read, change and delete everything in your Drive. Keep them private.
Archives from `gdrive account export` contain the same secrets; delete them after the import.

`gdrive account remove` deletes only the local files. To revoke access, remove your app at
<https://myaccount.google.com/permissions>.

This fork ships no shared client ID. Each user creates their own OAuth client, so nobody depends on a
client that Google could block for everyone. That is what happened to the client built into gdrive 2.

## Changes compared to 3.9.1

Done:

- Dependencies moved from git forks pinned in 2023 to current crates.io releases: google-drive3 5 to 7,
  google-apis-common 5 to 8, hyper 0.14 to 1, yup-oauth2 8 to 12. File I/O is asynchronous. This is
  dodomorandi's work.

For 4.0.0, milestones 0 and 1 of the [roadmap](docs/triage/README.md). [CHANGELOG.md](CHANGELOG.md)
lists every change of a release and what behaves differently from 3.9.1.

- Resumable uploads retry a failed chunk from the right offset again. The migrated library advanced the
  offset before a retry, which could hang or fail large uploads.
- `files download` flushes and closes a file before it renames it, so a failed final write is reported.
- Error messages start with `Error:` again and tell you which command to run next. Sizes print as in
  3.9.1 (`2 MB`), so scripts keep working.
- `gdrive version` reports 4.0.0-alpha and the commit.
- CI checks formatting, lints, tests and a locked build.
- Drive names are sanitized before they become local paths. A name with `/` or `..` can no longer break a
  download or write outside `--destination`. Temporary download names no longer collide.
- `--query` is combined with `--parent` and `--drive` instead of being dropped.
- Retries cover rate limits (`403 rateLimitExceeded`, `429`), timeouts and dropped connections for all
  requests, with jitter and `Retry-After`.
- Commands other than `account add` and `account reauth` never start a browser login. When the token is
  no longer valid they fail fast with an error instead of waiting on port 8085. The login port is
  configurable (`--port`).
- `gdrive account reauth` signs in again and keeps the stored refresh token if Google sends none.
- A clear error instead of a panic when no CA certificates are found.
- `files update` on a folder gives an error instead of a panic.
- `--acknowledge-abuse` for files that Google flags as abusive; recursive jobs count per-file errors and
  exit non-zero.
- A defined policy for symbolic links in recursive uploads, with cycle detection.

Also in 4.0.0:

- `gdrive account add --manual` for machines without a browser; the login sends PKCE and checks `state`.
- Service accounts (`gdrive account add --service-account`, with `--subject` for domain-wide delegation)
  and Application Default Credentials (`gdrive account add --adc`).
- Proxy support through `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, including SOCKS5.
- Release archives for Linux x86_64 and ARM64, macOS ARM64 and Intel, and Windows (without the Visual C++
  runtime), with `SHA256SUMS`.
- gdrive's own resumable uploads: chunks are read from the disk as they are sent, the MD5 checksum is
  checked after the upload, and an interrupted `files upload`, `files update` or `files import` continues
  in the next run. `--multipart-threshold` sets which files go up in a single request.
- A download that breaks off continues where it stopped (`Range`), within the same run; the size is
  checked also for files without an MD5 checksum.
- Recursive download and upload transfer 4 files at a time (`--jobs N`). A recursive download starts
  while it still lists the folder. `--continue-on-error` finishes the other files after a failure; every
  failure is reported and makes the command exit non-zero, and a summary line goes to stderr.
- `files download --recursive --export default` exports Google Docs, Sheets, Slides and Drawings (to
  docx, xlsx, pptx and png, or a format per type such as `--export md,sheets=ods`) instead of skipping
  them, and does not export an unchanged document again. `files export --format` picks the format.
- Progress on stderr for downloads, uploads, updates, imports and exports: a bar with the rate and the
  time left, and for recursive transfers a line for the run and one per file. On by default on a
  terminal; `--progress`, `--no-progress` and `GDRIVE_PROGRESS` change that. See
  [Progress output](docs/basic_usage.md#progress-output).
- `files trash` and `files untrash`; `files delete`, `trash` and `untrash` take several ids.
- `files list --tsv` and `--json` for scripts; `drives list` lists all shared drives (`--max N`).
- Markdown export and import for Google Docs.
- `--name` for `files upload` and `files update`; an update from stdin keeps the file's name.
- Short options: `-m`, `-q`, `-p`, `-r`, `-o`, `-d`.
- Shell completions: `gdrive completions bash` (also zsh, fish, powershell, elvish).
- `files info` prints `Trashed: yes` for a file in the trash.
- `files sync up` and `files sync down` sync a local directory and a Drive folder one way: new and changed
  files only, changed Drive files updated in place, `--dry-run`, and `--delete` that runs only after a
  complete listing and a transfer without failures and moves extras to the Drive trash or
  `.gdrive-trash/` ([docs/sync.md](docs/sync.md)).

Planned, not done yet:

- A mode of `files download --recursive` that skips files that exist already (`files sync down` updates
  them instead).
- Download resume across runs; today a broken download resumes only within the same run.
- From the roadmap's backlog: folder size, `.gdriveignore`, deleting local files after an upload,
  `--account` per command, raw access tokens, a credential store outside the config directory.

## Upstream

gdrive was created by Petter Rasmussen. The upstream project, its community channels and its sponsorship
are at [glotlabs/gdrive](https://github.com/glotlabs/gdrive). This fork is maintained independently.

## License and credits

MIT, see [LICENSE](LICENSE).

- [Petter Rasmussen](https://github.com/prasmussen) and [glotlabs](https://github.com/glotlabs) wrote
  gdrive 2 and gdrive 3.
- [dodomorandi](https://github.com/dodomorandi) migrated the dependencies, converted file I/O to async and
  added tests. This fork starts from that work, and the commits keep their authorship.
- Community contributions to upstream used in this fork's documentation: anlif
  ([#66](https://github.com/glotlabs/gdrive/pull/66), basic usage), fseasy
  ([#96](https://github.com/glotlabs/gdrive/pull/96), credentials guide), TombLL
  ([#132](https://github.com/glotlabs/gdrive/pull/132), SSH tunnel guide), umaumax
  ([#137](https://github.com/glotlabs/gdrive/pull/137), credentials guide fix), hyoklee
  ([#138](https://github.com/glotlabs/gdrive/pull/138), basic usage fix), and markkkx000
  ([#161](https://github.com/glotlabs/gdrive/issues/161), the curl-the-redirect trick).
- Community pull requests to upstream whose work was ported or adapted (where code was ported, the commit
  names the pull request):
  - trash: CrmsnDragoon ([#67](https://github.com/glotlabs/gdrive/pull/67))
  - Markdown export and import: Loh1917 ([#146](https://github.com/glotlabs/gdrive/pull/146)) and lev-n
    ([#159](https://github.com/glotlabs/gdrive/pull/159))
  - the idea for TSV output: kenchou ([#113](https://github.com/glotlabs/gdrive/pull/113))
  - shared-drive paging: somsak ([#156](https://github.com/glotlabs/gdrive/pull/156)) and iamjoeker
    ([#144](https://github.com/glotlabs/gdrive/pull/144))
  - ARM builds in CI: mike-kfed ([#130](https://github.com/glotlabs/gdrive/pull/130)), gueraf
    ([#154](https://github.com/glotlabs/gdrive/pull/154)) and stulluk ([#158](https://github.com/glotlabs/gdrive/pull/158))
  - proxy support, the requirement: GikLaang ([#141](https://github.com/glotlabs/gdrive/pull/141))
  - export in recursive download: serebrov ([#162](https://github.com/glotlabs/gdrive/pull/162) and
    [#164](https://github.com/glotlabs/gdrive/pull/164))
  - short options: antoine2711 ([#163](https://github.com/glotlabs/gdrive/pull/163))
  - the ideas for concurrent download and upload: Defman ([#68](https://github.com/glotlabs/gdrive/pull/68)) and
    Jasper-Bekkers ([#82](https://github.com/glotlabs/gdrive/pull/82))
