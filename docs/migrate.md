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
changed with `gdrive account switch`). `--to` is required and must be another account, except with
[`--archive`](#one-tar-file-per-folder---archive).

- Both accounts can use the same OAuth client. While the client's app is in **Testing**, add both Google
  accounts as test users ([Add test users](create_google_api_credentials.md#4-add-test-users)).
- Service accounts and Application Default Credentials work on either side
  ([Service accounts](service_accounts.md)), for example a Workspace service account with domain-wide
  delegation as the destination.

## The command

```sh
gdrive files migrate <FILE_ID>... --to <ACCOUNT> [--from <ACCOUNT>] [--into <FOLDER_ID>] [--contents]
```

- `FILE_ID`: one or more ids of files or folders in the source account. Find them with `gdrive files list`
  while the source account is the current one (see
  [Find file and folder ids](basic_usage.md#find-file-and-folder-ids)). A folder is migrated with everything
  in it. `root` is the top folder of My Drive: it is migrated as a folder named `My Drive`.
- `--into <FOLDER_ID>`: a folder of the **destination** account to put everything in. Without it, the
  files and folders go to the top of the destination's My Drive. A folder of a shared drive works too.
- `--contents`: migrate what is **in** each given folder, not the folder: `FOLDER/*` goes to `DEST/*`
  instead of `DEST/FOLDER/*` (see [Only what is in a folder](#only-what-is-in-a-folder---contents)).

```sh
# Two folders of the current account into the folder "From old" of new@example.com:
gdrive files migrate 1AbC... 1DeF... --to new@example.com --into 1XyZ...

# From an account that is not the current one, 8 files at a time:
gdrive files migrate 1AbC... --from old@example.com --to new@example.com --jobs 8
```

## Only what is in a folder: `--contents`

Without `--contents`, a folder arrives as a folder of its name in the destination: `photos` becomes
`DEST/photos`, with everything in it. With `--contents`, the items of `photos` go straight into the
destination folder, and no `photos` folder is made there:

```sh
gdrive files migrate <FOLDER_ID> --to new@example.com --into <DEST_FOLDER_ID> --contents --dry-run
```

```text
Found 4 files in 4 folders with a total size of 16 B
create 2024/
create a.txt (4 B)
create 2024/d.txt (4 B)
```

The lines name each item by its place in the destination folder, without the name of the given folder.
Everything else works as without the flag: folders of the same name in the destination folder are reused,
files that are already there are skipped, a second run continues the first. A `FILE_ID` that is a file is
migrated as usual. With several folders, what is in all of them goes into the same destination folder; two
subfolders of one name, one in each, are treated like two folders of one name in one source folder (see
[Limitations](#limitations)). With `--trash-source`, the items of the given folder go to the trash and the
folder itself stays. `--contents` cannot be combined with `--archive`.

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
files, let the larger one win with `--replace-smaller`, or decide each one yourself with `--ask`.

## When a file grew in the source: `--replace-smaller`

A destination file of the same name with other content is a conflict: the item fails and nothing is
overwritten. With `--replace-smaller`, one case is settled without you: when the destination's file has
**fewer bytes** than the source's, it gets the source's content. This fits folders where people mostly add
to things, so that the larger file is the newer one.

```sh
gdrive files migrate <FOLDER_ID> --to new@example.com --into <DEST_FOLDER_ID> --replace-smaller --dry-run
```

```text
replace photos/report.xlsx (1.2 MB, was 1.1 MB)
```

- The file is replaced **in place**: the same Drive file gets a new version, so its id, folder, sharing and
  links stay. Drive keeps the earlier content as a previous version of the file for a limited time (30 days
  for most files, see "Manage versions" in Drive).
- Only a smaller file is replaced. A destination file of the same size or larger with other content is the
  conflict it was.
- Only when the destination folder has exactly one file of that name. With two or more, or when the file of
  that name is a Google Docs, Sheets or Slides file, the item is the conflict it was. Google Workspace files
  of the source are never replaced either: they are matched by name and type.
- Right before it is replaced, the destination folder is listed again for that name. The file must still be
  the only one of its name there, with the content that was listed. If it was changed, renamed, moved, put
  in the trash, or got a namesake since, the item fails with `the file of this name in the destination
  changed during the migration; it was not replaced`.
- One run replaces a file once. When two source files of one name would replace the same destination file,
  the first one does and the other one is a conflict. A file that another item of the run found its own
  content in (`already-there`), or created, is not replaced either.
- The new content is checked by MD5 like any migrated file, and the destination file is read once more
  after the upload: it must hold what was sent. With `--trash-source`, the source file goes to the trash
  only after that.
- The summary ends with `; N smaller files replaced` (`to replace` in a dry run).
- One thing gdrive cannot see: someone changing the destination file **while** it is being replaced. Drive
  has no conditional update, so the replacement wins, and their content is the earlier version that Drive
  keeps.

With `--archive`, the same holds for the tar file: when the folder got more files since its archive was
made, the new archive is larger and replaces the old one, as a new version of the same file, and gets its
mark once it is checked. A new archive that is not larger than the file there is the conflict it was. A dry
run knows the size of an archive without its exports only: for an archive with Google Workspace files it
may print `replace photos.tar (...) if the exports make it larger`.

## Deciding each conflict yourself: `--ask`

With `--ask`, gdrive stops at each conflict that a replace can settle, shows both files as Drive has them at
that moment, and asks:

```sh
gdrive files migrate <FOLDER_ID> --to new@example.com --into <DEST_FOLDER_ID> --ask
```

```text
photos/report.xlsx is in the destination with other content:
  source:      1.2 MB, changed 2026-11-05 14:03 UTC by Maria Silva
               https://drive.google.com/file/d/1AbC.../view
  destination: 1.1 MB, changed 2026-10-28 09:15 UTC by João Lima
               https://drive.google.com/file/d/1DeF.../view
Replace the destination's file? [y/N]
```

- `y` or `yes` replaces the destination's file with the source's, in place and with the same checks as
  `--replace-smaller` (above), whichever of the two is larger. The line is `replace PATH (new size, was old
  size)`.
- Anything else, also just Enter, leaves the destination's file as it is: `keep-destination PATH`. That
  item is settled and is **not** a failure, so the run goes on and can end with exit status 0. With
  `--trash-source`, its source file stays where it is.
- The times are UTC. `by NAME` is who changed the file last, when Drive tells. The link under each file
  opens it in a browser, to compare the two; for a new archive it opens the folder it is made of.
- Questions come one at a time, and the lines of the other items wait until the answer. The other
  transfers go on in the background meanwhile. The progress display is off with `--ask`. A warning of a
  transfer in the background (a retry, `--print-chunk-info`) can still appear under a question, which goes
  on waiting for your answer.
- Asked about are the conflicts that one file can settle: the destination folder has exactly one file of
  that name, and it is not a Google Docs, Sheets or Slides file. The others are the failed items they were.
- With `--replace-smaller` too, a smaller destination file is replaced without a question, and only the
  rest is asked about.
- `--ask` needs a terminal. Without one (a script, a pipe into the command) it refuses to start.
- A dry run asks nothing. It prints `ask PATH (size, the destination's has size)` for each question a run
  would ask, and the summary ends with `; N files to replace, M conflicts to ask about`.
- With `--archive`, the question is about the tar file: `new archive: SIZE` against the file there.
- Ctrl-C at a question stops the run like anywhere else.

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

**Inside one account.** With `--archive`, `--to` may name the source account itself: the folder becomes one
tar file in the same Drive, next to it or in `--into`.

```sh
gdrive files migrate <FOLDER_ID> --to me@example.com --archive
```

The folder and its files are only read, and the tar file is the only thing written. It takes the folder's
size of the account's storage a second time. The archive never goes into what it archives: an `--into`
folder that is the folder itself or inside it is refused, and so is all of My Drive (`root`) unless `--into`
names a folder outside it, on a shared drive. Without `--archive`, a migration inside one account is
refused: `gdrive files copy` copies a file there without a transfer.

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

**Running it again.** Once an archive is uploaded and checked (see Checks below), it gets a mark: properties
with a digest of what it was made from (every path; the id, size and MD5 checksum of every file, or its
modification time when it has no checksum; the id, modification time and format of every export) and with
the archive's own MD5 checksum at that point. A second run with nothing changed in the source prints
`already-there photos.tar` and uploads nothing. When the folder changed since (a file added, removed,
renamed or edited), the archive in the destination no longer matches: the item fails with `it exists in the
destination with different content`, and nothing is overwritten. Rename or remove the old archive to make a
new one, or use [`--replace-smaller`](#when-a-file-grew-in-the-source---replace-smaller) to replace it when
the new archive is larger. Any other file named `<name>.tar` in the destination folder fails the same way, and so do an
archive without the mark (one that failed its check) and an archive whose content was replaced in the
destination since it was marked (its MD5 checksum is no longer the marked one). When the marking itself
fails, that run says so: it cannot tell whether the mark was written, and the next run finds the archive
already there if it was, and different otherwise. A folder with a file that has neither an MD5 checksum nor
a modification time, or with a Google file without a modification time, is never already there, since
nothing tells that file from an edited one. Two `FILE_ID`s
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

**Checks.** Every archive that is completed, in one run or continued, is checked: each file that has an MD5
checksum on Drive (practically every uploaded file) against it, from its first byte, as it is read, and the
MD5 checksum Drive computes for the whole tar against the bytes read. A Google Doc, Sheet, Slide or Drawing
has no checksum on Drive to compare its export with: it is covered by the comparison of the whole tar only
(the bytes exported are the bytes in the archive). A file without an MD5 checksum is archived unchecked. A file that changes in the source while the archive is made (edited,
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

## An archive in parts: `--max-part-size`

With `--max-part-size SIZE`, an archive arrives as several tar files of at most SIZE instead of one. A failure
then costs only the part it fell in, and running the command again uploads only the files that no part holds
yet.

```sh
gdrive files migrate <FOLDER_ID> --to new@example.com --archive --max-part-size 3GB --dry-run
gdrive files migrate <FOLDER_ID> --to new@example.com --archive --max-part-size 3GB
```

```text
Found 5000 files in 40 folders with a total size of 8.2 GB
create photos.parts.json
create photos.part00001.tar (3 GB)
create photos.part00002.tar (3 GB)
create photos.part00003.tar (2.2 GB)
Summary: 3 archive parts migrated (8.2 GB), 0 already there, 0 skipped (unsupported), 0 failed, 0 cancelled
```

SIZE is a whole number with `B`, `KB`, `MB`, `GB` or `TB`, in powers of 1000 as gdrive prints sizes: `3GB` is
3 000 000 000 bytes, and `2MB` is 2 000 000. Everything else in [`--archive`](#one-tar-file-per-folder---archive)
holds for each part: what goes in, the checks, the mark and the continued upload of an interrupted part. The
parts of one `FILE_ID` go up one after the other, and `--jobs` makes several `FILE_ID`s at once.

**Each part is a tar of its own**, of whole files. It extracts alone, with the folders of its files, and all of
them extract into the tree:

```sh
for part in photos.part*.tar; do tar -xf "$part"; done
```

A file is never split: one larger than SIZE is alone in a part of its size, and its line says so, `create
photos.part00002.tar (7.7 GB, larger than the maximum: a file is never split)`.

**What tracks the upload.** `<name>.parts.json`, next to the parts, lists each part with the path of everything
in it, so it also tells which part holds a file. A part counts only when the file of its name is in the
destination with the mark of exactly those entries, on the bytes that were checked. A part that was deleted,
replaced or never finished proves nothing, and what it held goes up again. The list is gdrive's own: read it, but
do not edit it. Marks are visible only to the OAuth client that wrote them: a run with the destination
account added with another client finds no part it can count on.

**Running it again** uploads only what no part holds, whatever SIZE the earlier runs used:

```text
already-there photos.part00001.tar
create photos.part00002.tar (3 GB)
create photos.part00003.tar (2.2 GB)
```

An interrupted part continues where Drive stopped, when the same files make it up: its session is recorded as
for a whole archive. A file is told by its path and its content as Drive lists it (id, size and MD5 checksum;
for a Google file, its id, modification time and format). So:

- A file added to the source later goes into a new part, and so does a file that was edited, also when it was
  edited back to what an older part holds. The old part stays. Extracted in the order of their names, the parts
  give the newest content.
- A file removed from the source stays in its part.
- A renamed or moved file goes into a new part under its new path. Its old path stays in the old part.

**Failures.** A part that fails stops the archive, as any failed item stops the run, and the parts before it
stay. With `--continue-on-error`, the next parts are made all the same, so that one unreadable file does not hold
up everything after it. The run still exits with 1, and the next run tries what failed again, including the other
files of that part, in a new part. A part left in the destination without its mark (its check failed, or the marking did) counts
for nothing: gdrive warns about it, sends what it was to hold in a new part, and never deletes it. You can delete
it.

**Names.** Nothing in the destination is overwritten but the run's own list. A new part takes a number above every
part that counts, whose name no file of the folder has, so the order of the names is the order of the uploads. A
file named `<name>.parts.json` that is not such a list fails the archive,
before anything is uploaded: rename or remove it. With no conflicts to settle, `--max-part-size` cannot be
combined with `--replace-smaller` or `--ask`. A `<name>.tar` made without it is left alone and does not count.

**Dry run.** `--dry-run` reads the list and prints what a real run would do, with the sizes of the parts as the
listing tells them. Exports are not made, so they count as empty, and a real run may cut the parts differently.

## Options

| Option | |
|---|---|
| `--to ACCOUNT` | the destination account (required) |
| `--from ACCOUNT` | the source account; the current account by default |
| `--into FOLDER_ID` | a folder of the destination account; the top of My Drive by default |
| `--contents` | what is in each given folder goes straight into the destination folder, without a folder of its name |
| `--replace-smaller` | a destination file of the same name with other content and fewer bytes gets the source's content, instead of a failed item |
| `--ask` | show both files of such a conflict and ask whether to replace the destination's; needs a terminal |
| `--dry-run` | print what would be done, change nothing |
| `--trash-source` | move migrated source files and folders to the source's trash |
| `--archive` | each `FILE_ID` as one tar file, `<name>.tar` (see [One tar file per folder](#one-tar-file-per-folder---archive)) |
| `--max-part-size SIZE` | with `--archive`, each `FILE_ID` as tar files of at most SIZE, such as `3GB` or `2MB`, and a list of what each holds; a later run uploads only what no part holds (see [An archive in parts](#an-archive-in-parts---max-part-size)) |
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
