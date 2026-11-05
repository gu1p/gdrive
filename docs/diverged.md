# What changed in the old location: `files diverged`

After a [migration](migrate.md), some people keep working in the old location: they open the old folder,
edit the old files, add new ones. `gdrive files diverged` finds out who, so that you can tell them
personally where the files are now. It looks at what **diverged** in the old location (the origin) from the
copy that the migration made, and at who Drive says last changed each of those items, and when.

It works for **any account**, personal Gmail accounts included, which have no audit log, and needs no
permission beyond the access to Drive that every gdrive account has. For a Google Workspace organization,
[`files activity`](activity.md) reads the audit log instead, which also has views and downloads.

```sh
gdrive files diverged <OLD_FOLDER_ID> --from old@gmail.com --to new@example.com --exclude old@gmail.com
```

```text
Change    When                  Who                 Path
edited    2026-11-05 10:12:44   ana@example.com     Projects/plan.docx
added     2026-11-04 09:00:02   bob@example.com     Projects/2026/new.pdf
added     2026-11-04 08:00:00   bob@example.com     Projects/2026
trashed   2026-11-03 16:40:10   ana@example.com     Projects/old.xlsx
differs   2026-10-28 08:15:00   carol@example.com   Projects/notes.txt
moved     -                     -                   Projects/2026/b.jpg (was Projects/a.jpg)
```

Nothing is ever changed by this command, in either account: every request it sends is a read.

## Two ways to look, which combine

**Compared with the copy: `--to`.** Give the same arguments as to the migration: the `FILE_ID`s, `--from`,
`--to` and `--into`. gdrive lists the origin and the copy side by side, folder by folder, matched by name as
the migration matched them, and reports what is different in the origin. This needs no date and tells
apart what changed after the migration from what was there before it.

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

**Both.** With `--to` and `--since`, the comparison is made and only its changes at or after `--since` are
reported: a second migration, a week of changes.

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
  without it, the copy is at the top of its My Drive. Needs `--to`.
- `--since WHEN`: a duration back from now (`12h`, `30d`, `2w`), a date (`2026-10-01`, from midnight), a
  date and time (`2026-10-01 14:30`, the form the report prints), or an RFC 3339 time
  (`2026-10-01T14:30:00Z`), as for `files activity`. Dates and times without a time zone are in the local
  time zone. There is no limit to how long ago.
- At least one of `--to` and `--since` is required.
- `--by item|user`: the rows of the report (see [The reports](#the-reports)); `item` by default.
- `--exclude EMAIL`: leave out what this person did, for example the account that ran the migration. Not
  case-sensitive; repeat it for several people.
- `--jobs N`: send up to N listing requests at once, 1 to 64 (default 4).
- `--skip-header`, `--field-separator SEP`, `--tsv`: as for `gdrive files list`.

## What each change means

### Compared with the copy

The copy of a given folder is the folder of its name in `--into` (or at the top of My Drive); then each
folder of the origin is compared with the folder of its name in the copy of its parent, and each item with
the items of its name there. Items in the trash are left out of the listings on both sides.

| Change | When it is reported | When | Who |
|---|---|---|---|
| `added` | the copy's folder has no file of its name (for a folder: no folder of its name). Everything in an added folder is added too, one row each | when it was created | its owner, who created or uploaded it |
| `edited` | the copy has files of its name, none has the same content, and the origin was modified later than all of them | when it was last modified | who modified it last |
| `edited` | as above but the copy is newer, and the copy's revisions show the origin changed after the migration (see below) | when it was last modified | who modified it last |
| `differs` | the content differs, the copy is newer, and the copy's revisions cannot tell whether the origin changed after the migration | when it was last modified | who modified it last |
| `moved` | an added file whose size and MD5 checksum are those of a file of the copy that has no file of its name in the origin: it was renamed or moved in the origin. The row shows where it is in the copy: `PATH (was OLD_PATH)` | `-` | `-` |
| `trashed` | someone moved it to the trash, from a folder of the origin (see [In the trash](#in-the-trash)) | when it was moved there | who moved it there |

**The same content** is the migration's own rule: a file with content (a photo, a PDF, an office file) is the
same when the size and the MD5 checksum are. A Google Docs, Sheets or Slides file has no checksum; its copy
was exported and converted back, so the two cannot be compared byte by byte. The migration gave the copy the
origin's modification time: the two are the same when their modification times are.

**Changed on both sides.** People who moved to the new place edit the copy, which is expected, and makes the
copy the newer side. The origin may still have been edited after the migration, before the copy was. gdrive
reads the revisions of the copy (only for these files, every page, since Drive does not list the oldest
first): the oldest one is what the migration wrote. The origin changed after the migration if it was modified
later than that revision, and for a file with content only if its checksum differs from that revision's too.
Then it is `edited`; if the origin is what the migration copied, nothing is reported. When the revisions
cannot tell (Drive returns none, refuses them, or the oldest one cannot be what the migration wrote because
Drive deleted the older ones), the file is reported as `differs`, so that nothing is hidden. With several
files of its name in the copy, the one with the oldest revision is taken for the migration's copy.

**Moved and renamed files.** Drive does not tell who renamed or moved a file, and neither changes its
modification time: a `moved` row has no time and no person. Each file of the copy is used for one `moved` row
at most. Only files with a checksum can be matched: a Google file that was renamed is `added`.

A given item without a copy fails the command: there is no folder (or file) of its name in `--into` or at
the top of My Drive. Give the folder that holds the copy with `--into`, or run without `--to` and with
`--since`. Reporting a whole tree as added would not help.

### By time: `--since`

An item of the origin counts when it was created or last modified at or after `--since`. Without `--to`, it
is `added` when it was created then (the time is its creation, the person its owner), otherwise `edited`
(the time is its last modification, the person who made it). With `--to`, `--since` keeps the rows of the
comparison whose time is at or after it, and the `moved` rows, which have no time.

### In the trash

For every folder of the origin, gdrive lists what was moved from it to the trash, in both modes. A folder in
the trash is one row; what is in it is not listed. Drive tells who moved an item to the trash and when **only
for items in shared drives**. In a My Drive only the owner of a file can move it to the trash, so the owner is
named and the time is `-`; `--since` keeps such rows, since the time is unknown. Drive empties the trash after
30 days: what was moved there earlier is gone, and so is what was deleted for good.

### What is left out

- **Items a migration does not copy**: shortcuts, Google Drawings, Forms and the other Google types without
  an export. They are never in the copy, so a comparison would call them added. They are listed on stderr,
  and reported only with `--since`, by time.
- **Items that both accounts see.** When the copy's folder holds the origin item itself (the same Drive id),
  or the item of its name there is another item of the origin, it is not compared with itself; the item is
  listed on stderr. A given item whose copy is such an item, and an `--into` folder that is one of the
  origin's folders, fail the command.
- **What is only in the copy**, made in the new place or removed from the origin for good: that is expected,
  and nobody can be named for it.

## Who and When, and what cannot be known

**Who** is Drive's `lastModifyingUser` of the item: the email address, in lower case, or the name when Drive
shows no address; else the item's owner; else `-`. For an `added` item it is the owner, who created or
uploaded it, rather than whoever modified it last; in a shared drive, which has no owners, who modified it
last. **When** is a time Drive keeps: the creation for `added`, the last modification for `edited` and
`differs`, the move to the trash for `trashed`.

Said plainly, the limits of this way of looking:

- **Views and downloads cannot be seen.** Someone who only opens the old files changes nothing, and leaves no
  trace in Drive for another account to read. Only an audit log has them: [`files activity`](activity.md), for
  Google Workspace organizations.
- **Only the last person to modify a file is known.** Two people who edited the same file show as the one who
  edited last.
- **Renames and moves have no person and no time** (`moved`).
- **What is only in the copy is not reported.**
- **A file edited in both places** is settled by the copy's revisions while Drive still has them. Drive
  deletes old revisions of files with content after about 30 days, or earlier once a file has 100 of them,
  unless they are kept forever; then the file `differs`. For a Google file with a long history Drive may not
  list its oldest revisions: an edit of the origin made before the oldest revision listed is then taken for
  what the migration copied.
- **A Google file's copy is a conversion**: only the times are compared, not the content.
- **Matched by name**, as the migration matches: a renamed folder in the origin is `added` with everything in
  it, since the copy's folder of the old name is not looked into.
- **Read only.** Nothing is ever changed, in either account.

## The reports

### Per item: `--by item`

One row per item that diverged: the newest first, the rows without a time last, then by path. Paths start with
the name of the given item, as the migration's output does. An empty report prints only its header.

### Per person: `--by user`

One row per person: the most recent change first, the people without a time next, and the row of the
changes nobody is named for (`-`) last. `Other` counts `differs` and `moved`; `Items` how many items.

```text
User                Last change           Added   Edited   Trashed   Other   Items
ana@example.com     2026-11-05 10:12:44   0       1        1         0       2
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
Read the revisions of 1 file of the copy, newer than the origin's file of the same name; they do not tell whether 1 file of the origin changed after the migration (differs)
Left out 1 item that a migration does not copy and the copy never has (--since reports those created or modified since a time):
  Projects/link: shortcuts have no content
Summary: 6 items diverged in the origin (2 added, 1 edited, 1 trashed, 1 differs, 1 moved) by 3 people; 1 item without a known person
```

Without `--to`, the summary is `Summary: 6 items changed in the origin since 2026-11-01 00:00:00 (...)`. The
summary is for people; scripts should read the report.

**Exit status:** 0 when the report was made, also when it is empty. 1 when it could not be made: a given id
that does not exist or is in the trash, a given item without a copy, a folder of the origin or of the copy
that cannot be listed completely (also when Drive answers with `incompleteSearch`). Then nothing is printed
on stdout: a comparison over part of a tree would call things added or unchanged wrongly. Revisions that
cannot be read are no failure: the file `differs`.

## Contacting people

`--by user` gives the list to contact; `--exclude` takes out yourself and the migration's accounts:

```sh
gdrive files diverged 1AbC... --from old@gmail.com --to new@example.com \
    --exclude old@gmail.com --exclude new@example.com --by user
```

A person named by display name only did not make their address visible to the origin account; their row
still tells who to ask about.
