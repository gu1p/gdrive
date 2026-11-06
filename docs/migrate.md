# Migrate files to another account

`gdrive files migrate` copies files and folders from one Google account to another one, for example from
an old Gmail account to a new one, or from a personal account to a Google Workspace account. Both accounts
are logged in to gdrive on the same machine. The content streams from the source account through this
machine into the destination account: nothing is written to the local disk, and memory use stays small
however large the files are.

The source account is only read. Nothing in it is changed, moved to the trash or deleted, unless you ask
for it with [`--trash-source`](#moving-instead-of-copying---trash-source), and even then nothing is deleted
for good.

```sh
gdrive files migrate <FOLDER_ID> --to new@example.com --dry-run    # what would be done
gdrive files migrate <FOLDER_ID> --to new@example.com              # do it
```

Run the same command again whenever you like: what is already in the destination is skipped, so an
interrupted migration is finished by running it again.

## Log in to both accounts

Add each account once with `gdrive account add` (see [Basic usage](basic_usage.md#accounts)), then check
the names that gdrive gives them:

```sh
gdrive account add              # sign in as old@example.com
gdrive account add              # sign in as new@example.com
gdrive account list
```

```text
new@example.com
old@example.com
```

`--from` and `--to` take these names. `--from` defaults to the current account (`gdrive account current`,
changed with `gdrive account switch`). `--to` is required and must be another account.

- Both accounts can use the same OAuth client. While the client's app is in **Testing**, add both Google
  accounts as test users ([Add test users](create_google_api_credentials.md#4-add-test-users)).
- Service accounts and Application Default Credentials work on either side
  ([Service accounts](service_accounts.md)), for example a Workspace service account with domain-wide
  delegation as the destination.

## The command

```sh
gdrive files migrate <FILE_ID>... --to <ACCOUNT> [--from <ACCOUNT>] [--into <FOLDER_ID>]
```

- `FILE_ID`: one or more ids of files or folders in the source account. Find them with `gdrive files list`
  while the source account is the current one (see
  [Find file and folder ids](basic_usage.md#find-file-and-folder-ids)). A folder is migrated with everything
  in it. `root` is the top folder of My Drive: it is migrated as a folder named `My Drive`.
- `--into <FOLDER_ID>`: a folder of the **destination** account to put everything in. Without it, the
  files and folders go to the top of the destination's My Drive. A folder of a shared drive works too.

```sh
# Two folders of the current account into the folder "From old" of new@example.com:
gdrive files migrate 1AbC... 1DeF... --to new@example.com --into 1XyZ...

# From an account that is not the current one, 8 files at a time:
gdrive files migrate 1AbC... --from old@example.com --to new@example.com --jobs 8
```

## What happens

1. **Listing.** gdrive lists every source folder completely first and prints
   `Found N files in M folders with a total size of X`. Files in the source's trash are left out.
2. **Folders.** Each source folder gets a destination folder of the same name, created before anything goes
   into it, breadth-first. If the destination folder already has a folder of that name, that folder is used
   instead of a new one.
3. **Files.** Up to `--jobs` files (4 by default) are copied at once. Each one is read from the source and
   uploaded to the destination as it is read; a large file goes up in chunks of `--chunk-size` MB through a
   resumable upload, like `gdrive files upload` does. The new file gets the name, type and modification time
   of the source, and its MD5 checksum is compared with the source's.

Before a file is created, gdrive looks at the files of the same name in its destination folder (one listing
per destination folder):

| The destination has | Result |
|---|---|
| no file of that name | the file is created |
| a file of that name with the same size and MD5 checksum | `already-there`: skipped |
| only files of that name with other content | the item **fails** with `it exists in the destination with different content`; nothing is overwritten |

A Google Docs file has no checksum; it is already there when the destination has a file of its name and the
same Google type. Folders never conflict with files: a folder `a.txt` next to a file `a.txt` is fine.

If Drive answers the listing of a destination folder with `incompleteSearch`, a file of the same name might be
missing from it and would be created a second time. That folder fails instead, with everything in it
(`Drive did not list the destination folder completely`); run the command again later.

**The same file on both sides.** When both accounts can see the same folder, for example a folder of the
source that is shared with the destination account and added to its My Drive, the destination folder may
hold the source item itself, or another item of the source. gdrive compares Drive ids with those of
everything it migrates: an item fails, nothing is copied for it and it never goes to the trash, when its
destination folder holds the item itself (`the source and the destination are the same folder; nothing to
do`, or `file`), or when the destination folder or file of its name is one of the migrated source items
(`the folder of this name in the destination is one of the source items that are migrated; nothing to
do`). An `--into` folder that is one of the folders to migrate, or inside one, is refused before anything
starts.

### The output

stdout gets one line per item, the action and the path from the name of the `FILE_ID` on:

```text
Found 6 files in 4 folders with a total size of 16 MB
create photos/
reuse photos/2024/
create photos/2024/beach.jpg (4.2 MB)
already-there photos/2024/sea.jpg
convert photos/Budget
skip photos/link: shortcuts have no content
trash photos/2024/beach.jpg
```

| Line | Meaning |
|---|---|
| `create PATH/` | a destination folder was created |
| `reuse PATH/` | the destination already had a folder of that name, which is used |
| `create PATH (SIZE)` | a file is copied |
| `convert PATH` | a Google Docs, Sheets or Slides file is exported and converted back |
| `already-there PATH` | the destination has the same file; nothing to do |
| `skip PATH: REASON` | the item cannot be migrated (see below) |
| `trash PATH`, `trash PATH/` | with `--trash-source`: the source file or folder went to the trash |
| `keep PATH: REASON`, `keep PATH/: REASON` | with `--trash-source`: the source file or folder was to go to the trash, but stays (see below) |

The lines come in the order the items start, which is not the order of the folder when several files are
copied at once. Control characters in names (a newline, a carriage return, an escape sequence) are printed
escaped, as `\n`, `\r` or `\u{1b}`, here and in error messages; the files keep their names. Errors go to
stderr when they happen, as `Error: unable to migrate 'PATH': ...`. At the end, stderr gets the list of
skipped items and a summary:

```text
Skipped 1 unsupported item, which cannot be migrated:
  photos/link: shortcuts have no content
Summary: 2 migrated (4.2 MB, 1 Google Workspace file converted), 1 already there, 1 skipped (unsupported), 0 failed, 0 cancelled; 1 folder created, 1 reused
```

Folders that failed are added as `; 1 folder failed`, `--trash-source` adds `; N moved to the trash in the
source` (and `, N kept there`), and an interrupted run ends with `; interrupted`. The line is for people; scripts should use the
exit status.

**Exit status:** 0 when every item was migrated, already there, or skipped as unsupported. 1 when an item
failed (a conflict, a read or upload error) or was cancelled, when the run was interrupted, or when Drive
answered the listing of a source folder with `incompleteSearch` (some files may be missing; a `Warning:` line
names the folder, and `--continue-on-error` accepts it). A dry run has the same exit status as the real run
would have.

**Errors.** By default the first failed item stops the run: nothing new starts and the running copies are
cancelled. With `--continue-on-error`, everything that does not depend on the failure goes on; the command
still exits with 1. A source folder that cannot be listed fails with everything in it.

## What is migrated and what is not

| Item | |
|---|---|
| Files with content (photos, PDFs, archives, office files, ...) | copied byte for byte, checked by MD5 |
| Folders | recreated with the same names and nesting |
| Google Docs, Sheets, Slides | exported (to docx, xlsx, pptx) and converted back to a Google file in the destination |
| Google Docs files larger than 10 MB as an export | **fail**: Drive exports at most 10 MB per file. Copy them by hand, or with Google Takeout |
| Shortcuts | skipped: a shortcut has no content. Migrate its target instead |
| Drawings | skipped: Drive exports them only as images or PDF, and converts nothing back to a Drawing |
| Forms, Sites, My Maps, Apps Script and other Google types without an export | skipped |
| Files in the source's trash | left out |

Not carried over, for any file:

- **sharing and permissions**: the destination account owns the new files, and nobody else has access to
  them until you share them again (`gdrive permissions share`);
- comments, suggestions and the revision history;
- other metadata, such as descriptions, stars and folder colors.

A Google Docs file goes through an export and an import: most documents come back as they were, but
formatting that the office format does not keep can change.

### Memory and disk

Nothing of the content touches the disk. A file is held in memory only a few MB at a time, for each of the
`--jobs` files that are copied at once; only a Google Docs export is held whole, at most 10 MB. The listing
of the source folders is kept in memory too: a few hundred bytes per file.

## Running it again

A migration can always be run again with the same arguments:

- folders that exist in the destination are reused and files that are there with the same content are
  skipped, so the second run copies only what is missing;
- a large file whose upload was interrupted continues where Drive stopped, if the source file did not
  change and the upload started less than a week ago (see
  [Resumable uploads](basic_usage.md#resumable-uploads); the records are kept in the **destination**
  account's `uploads/` directory);
- Ctrl-C stops the run: the copies in progress are cancelled and the command exits with 1. Run it again to
  finish.

A file that conflicts (same name, other content) keeps failing until you rename or remove one of the two
files.

## Moving instead of copying: `--trash-source`

With `--trash-source`, gdrive moves what it migrated to the trash of the **source** account:

- a file, once it is in the destination: copied and checked, or already there with the same MD5 checksum;
- a folder, once everything in it went to the trash. A folder that keeps anything (an item that failed or
  was skipped, a Google Docs file that was already there and so was not compared, a listing that was
  incomplete) stays, and so do the folders around it.

Right before an item goes to the trash, gdrive looks at it again in the source:

- a file is fetched again; if its size, MD5 checksum or modification time differ from what was migrated or
  matched (it changed during the run), it stays: `keep PATH: it changed in the source during the migration`.
  The destination's copy is then older than the source; rename or remove it and run the command again;
- a folder is listed again; if it holds anything that is not in the trash yet (a file added during the
  run), or Drive cannot list it completely, it stays: `keep PATH/: it has files that were not migrated`.
  Running the command again migrates the new files and then moves the folder.

A kept item is not a failure: the exit status stays 0.

Nothing is ever deleted for good. Restore with `gdrive files untrash <FILE_ID>` (as the source account) or
from the trash in Drive, which keeps items for 30 days. A file that went to the trash on its own stays there
when you restore its folder; restore it too.

## Dry run

`--dry-run` prints the same lines as the real run, the failures included, and changes nothing. It lists the
source, and reads the destination only to compare: the `--into` folder (or the top of My Drive) and each
destination folder that exists. No file or folder is created, nothing is uploaded and nothing goes to the
trash. Its summary starts with `Dry run, nothing was changed.` and counts what would be done:

```text
Dry run, nothing was changed. Summary: 4 to migrate (12 MB, 1 Google Workspace file to convert), 1 already there, 1 skipped (unsupported), 0 failed, 0 cancelled; 3 folders to create, 1 reused
```

A conflict fails the dry run as it would fail the real run; with `--continue-on-error` the dry run lists
all of them.

What a dry run cannot tell, because it reads no content and writes nothing:

- **a Google Docs, Sheets or Slides file whose export is larger than 10 MB**: only the export itself tells,
  and the real run fails that item;
- errors of reading or uploading: a file Google flagged as malware or spam, missing permission to download
  a file, the destination's storage quota, a checksum that does not match;
- source files that change during the real run (see `--trash-source` above), and changes to the destination
  between the dry run and the real run.

## One tar file per folder: `--archive`

With `--archive`, each `FILE_ID` arrives in the destination as **one file**, `<name>.tar`, instead of as a
copy of the tree: a plain tar archive (no compression) of the folder with everything in it, or of the one
file. The archive is made while it is uploaded: the bytes stream from the source account through memory
into a resumable upload of the destination account, and nothing is written to the local disk (apart from
the exports cache described below).

```sh
gdrive files migrate <FOLDER_ID> --to new@example.com --archive --dry-run
gdrive files migrate <FOLDER_ID> --to new@example.com --archive
```

```text
Found 6 files in 4 folders with a total size of 16 MB
skip photos/link: shortcuts have no content
create photos.tar (16.1 MB)
```

The archive goes into `--into` (the top of My Drive by default) with the type `application/x-tar`. The
other options keep their meaning: `--jobs` makes several archives at once (each `FILE_ID` is one), and
`--continue-on-error`, `--chunk-size`, `--dry-run` and the progress flags work as without `--archive`; the
progress shows each archive as one transfer. `--archive` cannot be combined with `--trash-source`, and an
archive run never changes anything in the source account.

**What is inside.** A directory entry for every folder, the top one and empty ones included, and a file
entry for every file, all under `<name>/`. Entries come in the order of their names, each folder before
what is in it, whatever order Drive lists them in. Every Drive name becomes one path component with the
rules of a recursive download for Windows, macOS and Linux at once, wherever gdrive runs: `/`, `\`,
control characters and `< > : " | ? *` become `_`, Windows device names and names ending in a dot or space
get a `_`, and a name longer than 255 bytes is shortened with a tag of its file id. Two files of one name in
a folder (also when they differ only in letter case) do not overwrite each other: one keeps the name, the
others get `~` and 8 hex digits of their file id before the extension, `report~1a2b3c4d.pdf`. A name that
cannot be used at all (`.`, `..`, an empty name) fails the archive, with the `gdrive files rename` command
that fixes it. The
archive is POSIX tar (pax): long and non-ASCII paths and files of 8 GiB and more extract with GNU tar,
bsdtar (macOS, Windows) and other tar programs. Every entry has the file's modification time on Drive, mode
644 (folders 755), and owner 0 with no user or group names; the same folder gives the same bytes on any
machine.

**Google Docs, Sheets, Slides and Drawings** are exported as by `files download --recursive --export`
(its `default` profile): Docs as docx, Sheets as xlsx, Slides as pptx, Drawings as png. The entry gets the
extension unless its name already ends with it (`Budget` becomes `Budget.xlsx`). The exports are made
before the upload starts, since their sizes are part of the archive, and are held in memory until it ends.
A Google file goes in as it is when it is exported, not as it was when the folder was listed: one edited in
between goes in with its newer content, and the next run then finds the archive different from the source.

**Left out**, each printed as `skip PATH: REASON` and counted as skipped: shortcuts, Google types without an
export (Forms, Sites, My Maps, Apps Script, ...), and a Google file whose export is larger than the 10 MB
Drive exports. A folder that could not be listed, or that Drive listed incompletely, fails the archive
before anything is uploaded: an archive that silently misses files is worse than none.

**Running it again.** Once an archive is uploaded and checked (see Checks below), it gets a mark: a
property with a digest of what it was made from (every path; the id, size and MD5 checksum of every file, or
its modification time when it has no checksum; the id, modification time and format of every export). A
second run with nothing changed in the source prints `already-there photos.tar` and uploads nothing. When
the folder changed since (a file added, removed, renamed or edited), the archive in the destination no
longer matches: the item fails with `it exists in the destination with different content`, and nothing is
overwritten. Rename or remove the old archive to make a new one. Any other file named `<name>.tar` in the
destination folder fails the same way, and so does an archive without the mark: one that failed its check,
or that could not be marked (that run says so). A folder with a file that has neither an MD5 checksum nor a
modification time is never already there, since nothing tells that file from an edited one. Two `FILE_ID`s
with the same Drive name give two files named `<name>.tar` in the destination folder (Drive allows it), and
a later run finds each one again by its own digest.

**Continuing an interrupted run.** An archive that was interrupted (Ctrl-C, a lost network) is continued by
running the same command again, like a large file: the session is recorded in the destination account's
`uploads/` directory (see [Running it again](#running-it-again)). The next run first reads the part that
Drive holds already again from the source, from the start of the archive, to check it, and then continues
the upload where Drive stopped: that part is downloaded a second time but uploaded only once, and only what
Drive does not hold yet is sent. Exports are not the same bytes twice, so the exports of an archive whose
session is recorded are kept next to the session records, in `uploads/archive-<digest>.exports` (private
to you, like the records), and the next run uses them instead of exporting again. The file is deleted when
the archive is complete, and with the stale session records after a week. An archive is continued only
when it would be the same bytes; otherwise, and for an archive with a file that has no MD5 checksum, it
starts over.

**Checks.** Every archive that is completed, in one run or continued, is checked whole: each file against
its MD5 checksum on Drive, from its first byte, as it is read, and the MD5 checksum Drive computes for the
whole tar against the bytes read. A file that changes in the source while the archive is made (edited,
resized, deleted) fails the archive with an error that names it, `'photos/a.txt' changed in the source while
the archive was made; run the command again`, and no archive is created. Run the command again once the
source is quiet. If Drive's checksum of the tar is not that of the bytes read (an earlier run sent other
bytes than the source gives now, or bytes changed on the way), the item fails and the archive stays in the
destination without its mark: delete it there and run the command again. The mark is written only after
these checks.

**Dry run.** `--dry-run --archive` prints what a real run would (`create NAME.tar`, `already-there`,
`skip`, conflicts) without exporting anything, so the size it shows leaves out the exports and says so:

```text
create photos.tar (12 MB without the exports of 2 Google Workspace files)
Dry run, nothing was changed. Summary: 1 archive to migrate (12 MB without the exports of 2 Google Workspace files), 0 already there, 1 skipped (unsupported), 0 failed, 0 cancelled
```

**Limits.** Drive takes at most 750 GB of uploads per account per day and 5 TB per file, so one archive is
at most 5 TB, and a larger one needs several `FILE_ID`s (the subfolders). The archive's property is visible
only to the OAuth client that created it: a second run with the destination account added with another
client sees a file of the same name with other content.

## Options

| Option | |
|---|---|
| `--to ACCOUNT` | the destination account (required) |
| `--from ACCOUNT` | the source account; the current account by default |
| `--into FOLDER_ID` | a folder of the destination account; the top of My Drive by default |
| `--dry-run` | print what would be done, change nothing |
| `--trash-source` | move migrated source files and folders to the source's trash |
| `--archive` | each `FILE_ID` as one tar file, `<name>.tar` (see [One tar file per folder](#one-tar-file-per-folder---archive)) |
| `--jobs N` | copy up to N files at once, 1 to 64 (default 4); folder listings and creations run besides, 2 at a time |
| `--continue-on-error` | after a failed item, finish the others; the run still exits with 1 |
| `--chunk-size MB`, `--multipart-threshold MB` | how large files are uploaded, as for `files upload` |
| `--print-chunk-errors`, `--print-chunk-info` | details of chunked uploads, as for `files upload` |
| `--progress`, `--no-progress` | the progress display on stderr (see [Progress output](basic_usage.md#progress-output)) |

## Limitations

- Items are matched by name only. A file renamed in the source after a first run is copied again under the
  new name; the old copy stays in the destination.
- Two items with the same name in one source folder are both copied on the first run. On a later run, a file
  is already there when any destination file of its name has its content, and two source folders with the
  same name both use the first destination folder of that name.
- A Google Docs file is matched by name and type, not by content: one that changed in the source after it
  was migrated is not copied again.
- Shared drives' own settings, and the "Shared with me" view, are not migrated: migrate the folders and
  files themselves by id.
