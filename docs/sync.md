# Sync a folder one way

`gdrive files sync` makes one side like the other, like `rsync` does for two directories:

```sh
gdrive files sync up   <LOCAL_DIR> <FOLDER_ID>    # make the Drive folder like the local directory
gdrive files sync down <FOLDER_ID> <LOCAL_DIR>    # make the local directory like the Drive folder
```

The **contents** of the two folders are synced, not the folders themselves: `LOCAL_DIR/a/b.txt` and
the Drive file `b.txt` in the folder `a` of `FOLDER_ID` are the same item. Find the folder id with
`gdrive files list` (see [Find file and folder ids](basic_usage.md#find-file-and-folder-ids)); `root`
is the top folder of My Drive.

Start with `--dry-run`. It prints what the real run would do, one line per item, and changes nothing:

```sh
gdrive files sync up ~/photos <FOLDER_ID> --dry-run
gdrive files sync up ~/photos <FOLDER_ID>
```

## What happens

A sync runs in three phases.

1. **Inventory.** gdrive lists the Drive folder completely, walks the local directory completely, and
   matches both by relative path. Each item is new (only on the source), changed, unchanged, extra
   (only on the destination) or skipped; see [Skipped items](#skipped-items).
2. **Transfer.** New items are created and changed files are updated, `--jobs` at a time (4 by
   default), every folder before what goes into it.
   - `up` uploads a changed file as a **new revision of the same Drive file**: it keeps its id, its
     sharing and its history (`gdrive files update` does the same for one file).
   - `down` writes every download to a temporary file next to the destination,
     `.gdrive-<id>.part`, and renames it into place only when its size and md5 match Drive's.
     A new file never replaces a local file that appeared after the inventory.
3. **Deletion**, only with `--delete`; see [Deleting extras](#deleting-extras).

The direction decides what is the source:

| | `sync up` | `sync down` |
|---|---|---|
| new | uploaded into the Drive folder | downloaded into the local directory |
| changed | uploaded as a new revision | downloaded over the local file |
| extra | a Drive file that is not local | a local file that is not on Drive |
| symlinks | followed, as by `files upload --recursive` | never written through, never removed |

`down` creates `LOCAL_DIR` when it does not exist. `up` needs it.

### When is a file unchanged?

- Different sizes: changed, without reading the file.
- `down`: same size, and the local modification time is exactly Drive's `modifiedTime`: unchanged,
  without reading the file. Every download sets the local time to Drive's `modifiedTime`, with all its
  precision, so a second `sync down` reads nothing. Otherwise gdrive computes the md5 of the local file:
  equal to Drive's, the file is unchanged and gets Drive's time (so the next run does not read it
  again); different, it is changed.
- `up`: same size: gdrive computes the md5 of the local file and compares it with Drive's. Drive's
  `modifiedTime` is the time of the upload, not the time of the local file, so it says nothing here.
- A Google Docs file exported by `down --export`: unchanged when the local file has the document's
  `modifiedTime`, as in a [recursive download](basic_usage.md#google-docs-in-a-recursive-download).

Nothing else is recorded between runs: no database, no hidden file. A file that was renamed on one
side is an extra under the old name and a new file under the new one.

## The output

Every item gets one line on stdout, the action and the path below the synced folders, with `/` on
every platform; folders end with `/`:

```text
create photos/2024/
create photos/2024/beach.jpg
update notes.txt
unchanged report.pdf
skip:document Budget
extra old.txt
extra->trash tmp/
```

| Line | Meaning |
|---|---|
| `create PATH` | new: created on the destination |
| `update PATH` | changed: transferred again |
| `unchanged PATH` | nothing to do |
| `extra PATH` | only on the destination; left alone (no `--delete`, or deletion skipped) |
| `extra->trash PATH` | only on the destination; moved to the trash |
| `extra->delete PATH` | only on the destination; removed for good (`down --delete-permanently`) |
| `skip:REASON PATH` | not synced and never deleted; see [Skipped items](#skipped-items) |

A folder that exists on both sides has no line of its own. `--dry-run` prints the same lines as the real
run: the real run prints them as it acts, in the order the transfers start, so sort both to compare.

Warnings and errors go to stderr, and so does the last line, a summary with a count per class:

```text
Summary: 2 created, 1 updated, 10 unchanged, 1 moved to the trash, 0 extra kept, 1 skipped (1 document), 0 failed, 0 cancelled
```

**Exit status:** 0 when everything planned was done, skipped items included. 1 when something failed
or was cancelled, when the inventory was incomplete (a folder Drive could not list, an incomplete search,
a local file that could not be read), or when `--delete` was given and nothing could be deleted. A dry
run exits 0 unless the folders themselves cannot be read.

## Deleting extras

Without `--delete`, nothing is ever deleted: extras are listed as `extra PATH` and stay.

With `--delete`, extras are removed after the transfer phase:

- **Drive:** an extra goes to the **Drive trash**, never deleted for good. A folder goes to the trash with
  everything in it, in one request, when everything in it is an extra; otherwise it stays and its extras
  go one by one.
- **Local:** an extra is **moved** to `LOCAL_DIR/.gdrive-trash/<time of the run>/`, under its path below
  `LOCAL_DIR`, for example `LOCAL_DIR/.gdrive-trash/20261104T102030Z/photos/old.jpg`. Before it moves,
  gdrive checks that it is still what the inventory saw (the type, and for a file the size and the
  modification time); a file you edited in the meantime stays and the run fails. With
  `--delete-permanently` (only for `down`, and only together with `--delete`) local extras are removed
  instead: the files the inventory saw, one by one, and then their directories if they are empty.

**Deletion runs only when it is safe.** It is skipped entirely, with `Deletion skipped: ...` on stderr and
exit status 1, when:

- the inventory is incomplete: a Drive folder could not be listed, Drive reported an incomplete search,
  or a local entry could not be read (no permission, a broken symlink, a FIFO or socket);
- the transfer phase had a failure or a cancellation (also with `--continue-on-error`), or was
  interrupted.

**Never deleted, with or without `--delete`:**

- anything skipped, and everything below a skipped folder (see the next section);
- a local file named like a Google Docs file that is skipped, or like any of its exports
  (`Report`, `Report.docx`, `Report.pdf`, ... for the document `Report`), or like a shortcut;
- the names a download would give Drive files with the same name (`report.pdf` and
  `report~1a2b3c4d.pdf`);
- local symlinks (`down`), temporary download files (`.gdrive-*.part`), and `LOCAL_DIR/.gdrive-trash`
  itself, which a sync never looks into.

### Restoring what a sync deleted

- **Drive:** open the trash in Drive and restore the files, or run
  `gdrive files untrash <FILE_ID>` (the ids are in `gdrive files list --query "trashed = true"`). Drive
  empties its trash after 30 days.
- **Local:** move the files back from `LOCAL_DIR/.gdrive-trash/<time>/`; the paths below it are the
  paths below `LOCAL_DIR`:

  ```sh
  cd LOCAL_DIR/.gdrive-trash/20261104T102030Z
  cp -a -n . ../../        # copies everything back, keeps the files that exist again
  mv photos/old.jpg ../../photos/    # or one file
  ```

  gdrive never empties `.gdrive-trash`. Remove the runs you do not need any more yourself.

## Skipped items

A skipped item is printed as `skip:REASON PATH`, is not transferred, and is never deleted. Skips are
not failures: the exit status stays 0.

| Reason | What it is |
|---|---|
| `document` | A Google Docs, Sheets, Slides or Drawings file: it has no content to compare. `down --export SPEC` exports it instead, exactly like `files download --recursive --export` (see [Google Docs in a recursive download](basic_usage.md#google-docs-in-a-recursive-download)). Local files with its name or the name of one of its exports are skipped too. |
| `not-exportable` | A Workspace file gdrive has no export format for (Forms, Sites, My Maps, ...). |
| `shortcut` | A Drive shortcut; shortcuts are not followed. A local file with its name is skipped too. |
| `ambiguous` | Two Drive files with the same name in one folder; names that are the same when letter case and Unicode normalization are ignored (`Report.pdf` and `report.pdf`, `café` written composed and decomposed), on either side or between the sides; a local name that is not valid UTF-8. A warning on stderr names them. Rename one of them. |
| `unusable-name` | A Drive name that cannot be a local name: `.`, `..` or empty. Rename it with `gdrive files rename`. |
| `reserved-name` | A Drive file named `.gdrive-trash` at the top of the folder. |
| `temp-file` | A local file named like a temporary download file, `.gdrive-*.part`. |
| `symlink` | A local symlink, for `down`. |
| `type-conflict` | A folder on one side, a file on the other. |
| `unreadable` | A local entry that could not be read. Also an inventory problem: no deletion, exit status 1. |
| `unlisted` | A Drive folder that could not be listed. Also an inventory problem. |
| `incomplete-listing` | A local entry of a Drive folder that Drive listed incompletely (`up`): it may be on Drive already. |
| `cycle` | A folder that contains itself: a local symlink to an ancestor, or a Drive folder that is its own ancestor. |
| `unsupported` | A Drive file without a size. |
| `protected` | A folder that exists only on the destination and is kept because something in it is never deleted. |

Names are matched the way a recursive download names local files: Drive names are converted to Unicode
NFC, characters that are not allowed in local names (`/`, control characters, on Windows also
`< > : " | ? *`) become `_`, and Windows device names get a `_` (`con.txt` becomes `con_.txt`). Local names
go through the same mapping, so a local `con.txt` meets the Drive `con.txt`. A Drive folder named `..` or
`/x` can never lead outside `LOCAL_DIR`: `..` is skipped, `/x` becomes `_x`.

## Options

| Option | |
|---|---|
| `--dry-run` | print the plan, change nothing; only `files.get` of the folder and `files.list` reach Drive |
| `--delete` | delete extras after the transfers, see [Deleting extras](#deleting-extras) |
| `--delete-permanently` | `down` only, with `--delete`: remove local extras instead of moving them to `.gdrive-trash` |
| `--export SPEC` | `down` only: export Google Docs files, as `files download --recursive --export SPEC` |
| `--acknowledge-abuse` | `down` only: download files Google flagged as malware or spam (owner only) |
| `--jobs N` | transfer up to N files at once, 1 to 64 (default 4); also the number of md5 computations at once |
| `--continue-on-error` | after a failed file, finish the others; the run still exits 1 and deletes nothing |
| `--progress`, `--no-progress` | show the progress of the transfers on stderr, also off a terminal, or never; by default only on a terminal (see [Progress output](basic_usage.md#progress-output)) |

Ctrl-C stops the transfers in progress, removes their temporary files, and deletes nothing. Run the
sync again to finish: what was transferred is unchanged by then.

`up` records the sessions of large uploads like `files upload` does, so a large file that was interrupted
continues where it stopped on the next run (see [Resumable uploads](basic_usage.md#resumable-uploads)).

## Performance

- The inventory lists the whole Drive folder (up to 1000 files per request, up to 50 folders per query,
  2 requests at a time) and walks the whole local directory **before** anything is transferred, and keeps
  both trees in memory: about a few hundred bytes per file. Deletion needs the complete inventory anyway.
- `sync up` reads **every local file whose size matches its Drive file** on every run, to compare md5s:
  a run over 100 GB of unchanged files reads 100 GB from the disk (but sends nothing). `--jobs` files are
  read at a time; on a spinning disk, `--jobs 1` may be faster.
- `sync down` reads a local file only when its size matches and its modification time does not. After a
  first sync, a run over unchanged files reads nothing but the directory entries. A filesystem that keeps
  coarser times than milliseconds (FAT, exFAT, HFS+) never matches Drive's time exactly; there every file
  of the same size is read on every run.
- A changed file is transferred whole; there is no delta transfer.

## Limitations

- One way only: changes on the destination are overwritten (changed files) or, with `--delete`, moved to
  the trash (extras). There is no conflict detection between two sides that both changed.
- Matching is by path. Moving or renaming a file on the source makes it an extra under the old path and a
  new file under the new path. A future version may keep a manifest of Drive ids to follow renames.
- Google Docs files are never uploaded or converted by `up`, and never deleted by either direction.
