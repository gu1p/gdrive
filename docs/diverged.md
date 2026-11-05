# What changed in the old location: `files diverged`

After a [migration](migrate.md), some people keep working in the old location: they open the old folder,
edit the old files, add new ones. `gdrive files diverged` finds out who, so that you can tell them
personally where the files are now. It looks at what **diverged** in the old location (the origin) from the
copy that the migration made, and at who Drive says changed each of those items, and when.

It works for **any account**, personal Gmail accounts included, which have no audit log, and needs no
permission beyond the access to Drive that every gdrive account has. For a Google Workspace organization,
[`files activity`](activity.md) reads the audit log instead, which also has views and downloads.

```sh
gdrive files diverged <OLD_FOLDER_ID> --from old@gmail.com --to new@example.com
```

```text
Change    When                  Who                 Path
edited    2026-11-05 10:12:44   ana@example.com     Projects/plan.docx
edited    2026-11-04 10:00:00   ana@example.com     Projects/2026/new.pdf
added     2026-11-04 09:00:02   bob@example.com     Projects/2026/new.pdf
added     2026-11-04 08:00:00   bob@example.com     Projects/2026
differs   2026-10-28 08:15:00   carol@example.com   Projects/notes.txt
moved     -                     -                   Projects/2026/b.jpg (was Projects/a.jpg)
trashed   -                     ana@example.com     Projects/old.xlsx
```

Nothing is ever changed by this command, in either account: every request it sends is a read.

The rule behind every row: a definite answer needs evidence. `edited`, a named person and "nothing to
report" are given only when Drive's data shows them. When it does not, the row says so (`differs`, or `-`
for the person or the time); an item is never left out because something is missing.

## Two ways to look, which combine

**Compared with the copy: `--to`.** Give the same arguments as to the migration: the `FILE_ID`s, `--from`,
`--to` and `--into`. gdrive lists the origin and the copy side by side, folder by folder, matched by name as
the migration matched them, and reports what is different in the origin. This needs no date.

```sh
# The migration was:
gdrive files migrate 1AbC... --from old@gmail.com --to new@example.com --into 1XyZ...
# What changed in the origin since:
gdrive files diverged 1AbC... --from old@gmail.com --to new@example.com --into 1XyZ...
```

**By time: `--since`.** Without `--to`, gdrive reports what was created or modified in the origin at or
after a time: give the time of the migration. This needs no copy, for example when the copy was
reorganized since, but it cannot tell an edit from a file that was only moved there.

```sh
gdrive files diverged 1AbC... --from old@gmail.com --since 2026-11-01
```

**Both.** With `--to` and `--since`, the comparison is made and only its rows at or after `--since` are
reported.

## The command

```sh
gdrive files diverged <FILE_ID>... [--from <ACCOUNT>] [--to <ACCOUNT>] [--into <FOLDER_ID>]
    [--since <WHEN>] [--by item|user] [--exclude <EMAIL>]... [--jobs <N>]
    [--skip-header] [--field-separator <SEP>] [--tsv]
```

- `FILE_ID`: one or more ids of files or folders of the origin, as given to `files migrate`. A folder
  counts with everything in it. `root` is the top of My Drive, which the migration copied as a folder named
  `My Drive`. A `FILE_ID` in the trash is refused. Several ids make one report.
- `--from ACCOUNT`: the origin account, as `gdrive account list` prints it; the current account by default.
- `--to ACCOUNT`: the account the files were migrated to, which holds the copy. It must be another account.
- `--into FOLDER_ID`: the folder of the `--to` account that holds the copy, as given to `files migrate`;
  without it, the copy is at the top of its My Drive. Needs `--to`. A folder of a shared drive is listed in
  that drive.
- `--since WHEN`: a duration back from now (`12h`, `30d`, `2w`), a date (`2026-10-01`, from midnight), a
  date and time (`2026-10-01 14:30`, the form the report prints), or an RFC 3339 time
  (`2026-10-01T14:30:00Z`), as for `files activity`. Dates and times without a time zone are in the local
  time zone. There is no limit to how long ago.
- At least one of `--to` and `--since` is required.
- `--by item|user`: the rows of the report (see [The reports](#the-reports)); `item` by default.
- `--exclude EMAIL`: leave out the rows of this person, for example yourself. Not case-sensitive; repeat it
  for several people. A row whose person is `-` is never left out.
- `--jobs N`: send up to N listing requests at once, 1 to 64 (default 4).
- `--skip-header`, `--field-separator SEP`, `--tsv`: as for `gdrive files list`.

## What each change means

### Compared with the copy

The copy of a given folder is the folder of its name in `--into` (or at the top of My Drive). Below it,
items are compared by name with the items of the copy's folder of the same name. Items in the trash are left
out of these listings on both sides.

**Several folders of one name.** Which of two folders `Team` in the copy belongs to which `Team` of the
origin cannot be known, so all folders of one name in a folder are compared as one group, with the copy's
folders of that name: their contents are pooled and matched by name, at every level. One folder on each
side is the same thing with groups of one.

**Files of one name** are matched one to one: each file of the origin is paired with a file of its name in
the copy that has the same content (first one that has the same modification time too), each file of the
copy used once. Then each file of the origin without a partner:

| Case | Row |
|---|---|
| the copy has no file of its name left (for a folder: no folder of its name). Everything in an added folder is added too, one row each | `added`, and `edited` if it was modified after its creation (see below) |
| the files of its name left in the copy are all older than the origin's last modification | `edited` |
| one of them is newer, or a time is missing: a revision of one of them holds the origin's present content | nothing: the copy was made from the origin as it is |
| as above, but no revision holds it, or the revisions cannot be read | `differs` |
| an added file whose size, MD5 checksum and creation time are those of a file of the copy that no origin file was matched or compared with | `moved`, with where it is in the copy: `PATH (was OLD_PATH)` |
| someone moved it to the trash, from a folder of the origin | `trashed` (see [In the trash](#in-the-trash)) |

**The same content** is the migration's own rule for a file with content (a photo, a PDF, an office file):
the same size and MD5 checksum. A Google Docs, Sheets or Slides file has no checksum, and its copy was
exported and converted back, so the two cannot be compared byte by byte; the migration gave the copy the
origin's modification time, so the two are the same when their modification times are. Times are compared
to the millisecond, the precision the migration sends.

**Changed on both sides.** People who moved to the new place edit the copy, which is expected, and makes the
copy the newer side. Whether the origin also changed after the migration is then settled by the copy's
revisions, all of them (Drive lists them in no set order): if one of them is exactly the origin as it is now
(the same size and checksum; for a Google file the same modification time), the copy was made from it and
nothing is reported. Otherwise the row is `differs`: the revision the migration wrote may have been
deleted by Drive, may never have existed (a file the copy's account already had, which the migration
reused), or the origin changed after the migration. A `differs` row carries the origin's last modification
and who made it, which is what to judge by: a time before the migration, or the migration's own account,
tells it was not someone who kept working there.

**Moved and renamed files.** Drive does not tell who renamed or moved a file, and neither changes its
modification time: a `moved` row has no time and no person. The migration gave the copy the origin's
creation time, which a rename or a move keeps: a file uploaded again under another name has a new one and
stays `added`. Only files with a checksum can be matched: a renamed Google file is `added` (and `edited` if
it was modified after its creation).

A given item without a copy fails the command: there is no folder (or file) of its name in `--into` or at
the top of My Drive. Give the folder that holds the copy with `--into`, or run without `--to` and with
`--since`. Reporting a whole tree as added would not help.

### An item that is new: `added` and `edited`

An item that has no copy carries two facts: when it was created, and, if later, when it was last
modified. Each is a row: `added` (its creation time, its creator) and `edited` (its last modification, who
made it). Each row is then kept or left out on its own by `--since` and `--exclude`; when both remain and
name the same person, only the `added` row is printed.

### By time: `--since`

Without `--to`, every item of the origin gives the rows of a new item above, and `--since` keeps those at or
after it: an item created before `--since` and modified since gives only its `edited` row. With `--to`,
`--since` keeps the rows of the comparison at or after it. A row without a time (`moved`, `trashed` in a My
Drive) is kept, since nothing tells it is older.

### In the trash

For every folder of the origin, gdrive lists what was moved from it to the trash, in both modes. A folder in
the trash is one row; what is in it is not listed. Drive tells who moved an item to the trash and when **only
for items in shared drives**. In a My Drive only the owner of a file can move it to the trash, so the owner is
named and the time is `-`. Drive empties the trash after 30 days: what was moved there earlier is gone, and so
is what was deleted for good.

### What is left out

- **Items a migration does not copy**: shortcuts, Google Drawings, Forms and the other Google types without
  an export. They are never in the copy, so a comparison would call them added. They are listed on stderr,
  and reported only with `--since`, by time.
- **Items that both accounts see.** When the copy's folders hold the origin item itself (the same Drive id),
  or the items of its name there include another item of the origin, it is not compared with itself; the
  item is listed on stderr. A given item whose copy is such an item, and an `--into` folder that is one of
  the origin's folders, fail the command.
- **What is only in the copy**, made in the new place or removed from the origin for good: that is expected,
  and nobody can be named for it.

## Who and When

Who is always one person Drive names for that fact, never a guess: the email address, in lower case, or the
name when Drive shows no address. When is a time Drive keeps.

| Change | When | Who | `-` when |
|---|---|---|---|
| `added` | the item's creation | in a My Drive its owner, who created or uploaded it; in a shared drive (no owners) who modified it last, if nobody modified it after its creation | Drive names no owner, or in a shared drive the item was modified after its creation |
| `edited` | its last modification | who modified it last (`lastModifyingUser`) | the last modification was by someone not signed in (a file editable by link), or Drive names nobody. The owner is never named in their place |
| `differs` | the origin's last modification | who modified the origin last | as for `edited` |
| `moved` | `-` | `-` | always: Drive keeps neither for a rename or a move |
| `trashed` | when it was moved to the trash, in shared drives | in a shared drive who moved it there; in a My Drive the owner, the only one who can | the time always outside shared drives |

Said plainly, the limits of this way of looking:

- **Views and downloads cannot be seen.** Someone who only opens the old files changes nothing, and leaves no
  trace in Drive for another account to read. Only an audit log has them: [`files activity`](activity.md), for
  Google Workspace organizations.
- **Only the last person to modify a file is known.** Two people who edited the same file show as the one who
  edited last.
- **Renames and moves have no person and no time** (`moved`).
- **What is only in the copy is not reported.**
- **A file edited in both places** is settled only when a revision of the copy is the origin as it is now;
  otherwise it `differs`. Drive deletes old revisions of files with content after about 30 days, or earlier
  once a file has 100 of them, unless they are kept forever, and may not list every revision of a Google file
  with a long history.
- **A Google file's copy is a conversion**: only the times are compared, not the content.
- **The owner of a My Drive file is taken for who created it**, which is wrong when its ownership was
  transferred later.
- **Matched by name**, as the migration matches: a renamed folder in the origin is `added` with everything in
  it, since the copy's folder of the old name is not looked into. A file the migration could not copy,
  because the copy's account already had another file of its name, is compared with that file: it shows as
  `edited` or `differs` with the origin's own time, which may be before the migration.
- **Read only.** Nothing is ever changed, in either account.

## The reports

### Per item: `--by item`

One row per change: the newest first, the rows without a time last, then by path. An item can have two rows
(`added` and `edited`). Paths start with the name of the given item, as the migration's output does. An
empty report prints only its header.

### Per person: `--by user`

One row per person: the most recent change first, the people without a time next, and the row of the
changes nobody is named for (`-`) last. `Added`, `Edited`, `Trashed` and `Other` (`differs` and `moved`)
count rows; `Items` counts items.

```text
User                Last change           Added   Edited   Trashed   Other   Items
ana@example.com     2026-11-05 10:12:44   0       2        1         0       3
bob@example.com     2026-11-04 09:00:02   2       0        0         0       2
carol@example.com   2026-10-28 08:15:00   0       0        0         1       1
-                   -                     0       0        0         1       1
```

### Output format

Times are printed in the local time zone, as `gdrive files list` and `files activity` print them.
`--skip-header`, `--field-separator` and `--tsv` work as for `files list`: with `--tsv` the columns are
separated by one tab, and a tab, newline, carriage return or backslash in a value is written as `\t`, `\n`,
`\r` or `\\`. Names, paths and addresses come from Drive: control characters in them (an escape sequence, for
example) are printed escaped, as `\u{1b}`, in every format.

### What stderr gets

The report is the only thing on stdout, and it is printed once it is complete. stderr gets what was found,
how the comparison was made, what was left out and why, and a summary:

```text
Found 6 files in 2 folders in the origin, and 1 item moved from them to the trash
Found 6 files in 1 folder in the copy
Compared by name with the copy in the top of My Drive: files by size and MD5 checksum, Google Docs, Sheets and Slides by modification time
Read the revisions of 1 file of the copy, newer than the origin's file of the same name; differs: 1 file of the origin whose present content no revision there holds
Left out 1 item that a migration does not copy and the copy never has (--since reports those created or modified since a time):
  Projects/link: shortcuts have no content
Summary: 6 items diverged in the origin, in 7 changes (2 added, 2 edited, 1 trashed, 1 differs, 1 moved) by 3 people; 1 change without a known person
```

`in 7 changes` appears when there are more rows than items. Without `--to`, the summary is
`Summary: 6 items changed in the origin since 2026-11-01 00:00:00 (...)`. The summary is for people; scripts
should read the report.

**Exit status:** 0 when the report was made, also when it is empty. 1 when it could not be made: a given id
that does not exist or is in the trash, a given item without a copy, a folder of the origin or of the copy
that cannot be listed completely (a page that fails, also after good ones, or Drive answering with
`incompleteSearch`). Then nothing is printed on stdout: a comparison over part of a tree would call things
added or unchanged wrongly. Revisions that cannot be read are no failure: the file `differs`.

## Contacting people

`--by user` gives the list to contact; `--exclude` takes out yourself and the migration's accounts:

```sh
gdrive files diverged 1AbC... --from old@gmail.com --to new@example.com \
    --exclude old@gmail.com --exclude new@example.com --by user
```

A person named by display name only did not make their address visible to the origin account; their row
still tells who to ask about. The rows of `-` cannot name anyone: look at their paths and times.
