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
[releases page](https://github.com/gu1p/gdrive/releases), together with `SHA256SUMS`.

| Platform | Archive |
|---|---|
| Linux x86_64 | `gdrive_linux-x64.tar.gz` |
| Linux ARM64 | `gdrive_linux-arm64.tar.gz` |
| macOS, Apple silicon | `gdrive_macos-arm64.tar.gz` |
| macOS, Intel | `gdrive_macos-x64.tar.gz` |
| Windows x86_64 | `gdrive_windows-x64.zip` |

The Linux binaries are statically linked and run on any distribution.

**Linux and macOS**

```sh
TAG="<release tag>"              # as shown on the releases page
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
5. If Windows reports that `VCRUNTIME140.dll` is missing, install the
   [Microsoft Visual C++ Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist).

### Build from source

You need Rust 1.88 or later ([rustup](https://rustup.rs/)).

```sh
cargo install --git https://github.com/gu1p/gdrive --locked
```

This installs `gdrive` into `~/.cargo/bin`. To build from a clone instead, run `cargo build --release
--locked`; the binary is `target/release/gdrive`.

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
                    reauth (4.0.0)
gdrive files        list, info, download, upload, update, delete, mkdir, rename,
                    move, copy, import, export
gdrive drives       list
gdrive permissions  share, list, revoke
gdrive about        information about gdrive
gdrive version      version information
```

Run `gdrive <command> --help` for the options, for example `gdrive files list --help`.

## Credentials and security

gdrive stores accounts under `~/.config/gdrive3/` (on Windows, `%USERPROFILE%\.config\gdrive3\`):

| File | Content |
|---|---|
| `account.json` | The name of the current account |
| `<account>/secret.json` | Your OAuth client ID and client secret |
| `<account>/tokens.json` | Access token and refresh token |

Anyone who can read these files can read, change and delete everything in your Drive. Keep them private.
Archives from `gdrive account export` contain the same secrets; delete them after the import.

`gdrive account remove` deletes only the local files. To revoke access, remove your app at
<https://myaccount.google.com/permissions>.

This fork ships no shared client ID. Each user creates their own OAuth client, so nobody depends on a
client that Google could block for everyone. That is what happened to the client built into gdrive 2.

## Changes compared to 3.9.1

Done:

- Dependencies moved from git forks pinned in 2023 to current crates.io releases: google-drive3 5 to 7,
  google-apis-common 5 to 8, hyper 0.14 to 1, yup-oauth2 8 to 12, exponential-backoff 1 to 2. File I/O
  is asynchronous. This is dodomorandi's work.

For 4.0.0, milestones 0 and 1 of the [roadmap](docs/triage/README.md). These land during the alpha
series; the notes of each release say what it contains.

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
- A clear error instead of a panic when no CA certificates are found; fixes for Windows certificate errors.
- `files update` on a folder gives an error instead of a panic.
- `--acknowledge-abuse` for files that Google flags as abusive; recursive jobs count per-file errors and
  exit non-zero.
- A defined policy for symbolic links in recursive uploads, with cycle detection.

Also in 4.0.0:

- `gdrive account add --manual` for machines without a browser (from the authentication milestone).
- Release archives for Linux x86_64 and ARM64, macOS ARM64 and Intel, and Windows, with `SHA256SUMS`.

Planned, not done yet:

- Faster transfers: several files at a time, resumable uploads that survive a restart, progress output,
  skip and sync modes, export of Google Docs during recursive download, download resume.
- Service accounts, proxy support, PKCE for the login.
- Shell completions; a Windows build without the Visual C++ runtime.
- Trash and untrash, real TSV and JSON output, listing all shared drives, Markdown export and import,
  short options.

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
  ([#132](https://github.com/glotlabs/gdrive/pull/132), SSH tunnel), umaumax
  ([#137](https://github.com/glotlabs/gdrive/pull/137), typo fix), hyoklee
  ([#138](https://github.com/glotlabs/gdrive/pull/138), `files export`), and markkkx000
  ([#161](https://github.com/glotlabs/gdrive/issues/161), the curl-the-redirect trick).
- Community pull requests to upstream that planned features are ported from: CrmsnDragoon
  ([#67](https://github.com/glotlabs/gdrive/pull/67), trash), Defman
  ([#68](https://github.com/glotlabs/gdrive/pull/68), concurrent download), Jasper-Bekkers
  ([#82](https://github.com/glotlabs/gdrive/pull/82), parallel upload), kenchou
  ([#113](https://github.com/glotlabs/gdrive/pull/113), TSV output), dachrisch
  ([#126](https://github.com/glotlabs/gdrive/pull/126), folder size), GikLaang
  ([#141](https://github.com/glotlabs/gdrive/pull/141), system proxy), somsak
  ([#156](https://github.com/glotlabs/gdrive/pull/156), shared-drive paging), lev-n
  ([#159](https://github.com/glotlabs/gdrive/pull/159), Markdown), serebrov
  ([#162](https://github.com/glotlabs/gdrive/pull/162) and [#164](https://github.com/glotlabs/gdrive/pull/164),
  export in recursive download), antoine2711 ([#163](https://github.com/glotlabs/gdrive/pull/163), short
  options). Each port credits its author in the commit.
