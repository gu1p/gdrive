# Releasing gdrive

Releases are built and published by [`.github/workflows/release.yml`](../.github/workflows/release.yml)
when a tag `vX.Y.Z` is pushed; binaries are never uploaded by hand.

## Supported platforms

| Platform | Rust target | Release asset | Notes |
|---|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-musl` | `gdrive_linux-x64.tar.gz` | Static binary, runs on any distribution (glibc or musl) |
| Linux arm64 | `aarch64-unknown-linux-musl` | `gdrive_linux-arm64.tar.gz` | Static binary; Raspberry Pi 3/4/5 with a 64-bit OS, AWS Graviton, other arm64 servers |
| macOS Apple silicon | `aarch64-apple-darwin` | `gdrive_macos-arm64.tar.gz` | |
| macOS Intel | `x86_64-apple-darwin` | `gdrive_macos-x64.tar.gz` | Upstream's file of this name held an arm64 binary; this one is x86_64 |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `gdrive_windows-x64.zip` | Static CRT: no Visual C++ Redistributable needed |

Each archive holds the single binary (`gdrive`, or `gdrive.exe` on Windows) at its root, like upstream's
archives. The `linux-x64`, `macos-x64` and `windows-x64` names are upstream's and stay unchanged so existing
install scripts keep working. The newest stable release is always reachable at

    https://github.com/gu1p/gdrive/releases/latest/download/<asset>

Pre-releases (tags such as `v4.0.0-alpha.1`) are not "latest"; download them by tag:
`https://github.com/gu1p/gdrive/releases/download/v4.0.0-alpha.1/<asset>`.

32-bit ARM (armv7, Raspberry Pi OS 32-bit) has no release binary. Building it with
[cross](https://github.com/cross-rs/cross) should work
(`cross build --release --locked --target armv7-unknown-linux-musleabihf`), but CI does not test it.

The binaries are not code signed. On macOS, a binary downloaded with a web browser is quarantined; remove the
flag with `xattr -d com.apple.quarantine ./gdrive` (downloads with `curl` are not affected). On Windows,
SmartScreen may warn on first run.

## Cutting a release

1. Make sure CI is green on `main`.
2. Bump `version` in `Cargo.toml` (for example to `4.0.0`, or `4.0.0-alpha.1` for a pre-release) and update
   the lockfile entry, which release builds check with `--locked`:

   ```sh
   cargo update -p gdrive --offline   # changes only gdrive's own entry in Cargo.lock
   cargo build --locked
   git commit -am "build: release 4.0.0"
   ```

   Merge that commit to `main`.
3. Optional but recommended: run a dry run. In GitHub, open Actions > Release > Run workflow and pick `main`.
   It builds, checks and packages every target exactly as a release would, and keeps the archives and
   `SHA256SUMS` as workflow artifacts for 7 days. It creates no release.
4. Tag the release commit on `main` and push the tag:

   ```sh
   git switch main && git pull
   git tag -a v4.0.0 -m "gdrive 4.0.0"
   git push origin v4.0.0
   ```

5. Watch the Release workflow. It
   - checks that the tag is `v` plus the `Cargo.toml` version, and stops otherwise;
   - builds the five targets with `cargo build --release --locked`, with the commit SHA passed as
     `GDRIVE_GIT_HASH` so `gdrive version` prints it on its `Commit:` line;
   - checks each binary: Linux binaries must be static, macOS binaries must have the architecture their
     name says, the Windows binary must not import the VC++ runtime, and every binary that can run on its
     build machine must answer `gdrive version`;
   - checks that exactly the five expected asset names exist, writes `SHA256SUMS`, and creates the GitHub
     release with the archives, `SHA256SUMS` and notes generated from the merged pull requests. A tag with a
     `-` suffix becomes a pre-release.

   The release only appears once every target has built, so a failed target never leaves a release with
   missing assets.

If a job fails for a transient reason (runner, network), use "Re-run failed jobs" on the workflow run. If the
code has to change, fix it on `main` and release the next patch version under a new tag.

**Never move, delete or reuse a published tag.** Users and package managers pin tags and checksums; upstream
once moved a tag (glotlabs/gdrive#107). To enforce this rather than rely on convention, select "Enable
release immutability" in the Releases section of the repository settings, and add a tag ruleset for `v*`
that restricts updates and deletions. The workflow works with both: `gh` uploads the assets to a draft and
publishes it last.

## Verifying a download

Every release has a `SHA256SUMS` file with one line per archive.

Linux:

```sh
curl -fLO https://github.com/gu1p/gdrive/releases/download/v4.0.0/gdrive_linux-x64.tar.gz
curl -fLO https://github.com/gu1p/gdrive/releases/download/v4.0.0/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

macOS: the same downloads, then `shasum -a 256 --check --ignore-missing SHA256SUMS`.

Windows (PowerShell), compare the two values:

```powershell
(Get-FileHash .\gdrive_windows-x64.zip -Algorithm SHA256).Hash.ToLower()
Select-String gdrive_windows-x64.zip .\SHA256SUMS
```

With the GitHub CLI, `gh release download v4.0.0 -R gu1p/gdrive -p gdrive_linux-x64.tar.gz -p SHA256SUMS`
fetches both files. If release immutability is enabled, `gh release verify-asset v4.0.0
gdrive_linux-x64.tar.gz -R gu1p/gdrive` also checks the file against GitHub's signed release attestation.

## Building from source

You need Rust 1.88 or newer (the `rust-version` in `Cargo.toml`, checked by CI) and a C compiler for the
`ring` crypto crate: gcc or clang on Linux, the Xcode command line tools on macOS, the MSVC build tools on
Windows. OpenSSL is not needed; TLS is rustls.

```sh
git clone https://github.com/gu1p/gdrive.git
cd gdrive
cargo build --release --locked
./target/release/gdrive version
```

Or install straight from the repository into `~/.cargo/bin`:

```sh
cargo install --git https://github.com/gu1p/gdrive --locked            # main
cargo install --git https://github.com/gu1p/gdrive --tag v4.0.0 --locked # a release
```

Always pass `--locked`. It builds with the exact dependency versions in `Cargo.lock`, the ones CI tested.
Without it Cargo resolves the newest compatible versions, which is how some upstream users ended up with
two incompatible `hyper-rustls` versions (glotlabs/gdrive#131).

To reproduce the release binaries:

- Static Linux binary: install `musl-tools` (Debian, Ubuntu) or your distribution's musl-gcc, then
  `rustup target add x86_64-unknown-linux-musl` and
  `cargo build --release --locked --target x86_64-unknown-linux-musl` (use `aarch64-unknown-linux-musl`
  on an arm64 machine).
- macOS Intel binary on Apple silicon: `rustup target add x86_64-apple-darwin` and
  `cargo build --release --locked --target x86_64-apple-darwin`.
- Windows binary without the VC++ runtime dependency (PowerShell):
  `$env:RUSTFLAGS = "-C target-feature=+crt-static"; cargo build --release --locked --target x86_64-pc-windows-msvc`.
