# Who still uses a folder: `files activity`

`gdrive files activity` tells who used files and folders, per person: when they last did, on how many days,
how many views, downloads and edits, on how many items. It reads the **Drive audit log** of the Google
Workspace organization that owns the files, which records these events per person.

It is made for the time after a [migration](migrate.md): the files moved from an old account to a new one,
but some people keep opening the old location. You do not want to block them; you want to know who they are,
to tell them personally where the files are now.

```sh
gdrive files activity <OLD_FOLDER_ID> --account admin@old.example.com --exclude admin@old.example.com
```

```text
User                     Access   Last activity         Days   Views   Downloads   Edits   Other   Files
ana@example.com          writer   2026-11-04 14:02:11   12     40      3           7       0       18
bob@example.com          -        2026-10-30 09:11:45   1      2       0           0       0       1
(anonymous)              -        2026-10-12 08:00:03   1      1       0           0       0       1
carol@example.com        reader   -                     0      0       0           0       0       0
group:team@example.com   writer   -                     0      0       0           0       0       0
```

Nothing in Drive is ever changed by this command: every request it sends is a read.

## Requirements

The audit log belongs to a Google Workspace organization, and only its administrators can read it:

- **A Google Workspace administrator.** The Google account gdrive reads with must be an administrator of the
  organization that owns the files, with the **Reports** privilege (a super administrator has it; a custom
  admin role needs it under *Reports*). Personal Gmail accounts have no audit log.
- **The Admin SDK API, enabled** in the Google Cloud project of the account's OAuth client (the project of
  the Client ID you gave `gdrive account add`, see
  [Create Google API credentials](create_google_api_credentials.md)): *APIs & Services*, *Library*,
  *Admin SDK API*, *Enable*.
- **The account authorized for the audit log.** The login asks for read access to the audit log only when
  you ask for it: add the account with `gdrive account add --audit-log`, or authorize an account you have
  already with `gdrive account reauth <ACCOUNT> --audit-log`. Google then asks you to allow read access to
  the audit reports of your organization besides the access to Drive.

Without an audit log, for example for a personal Gmail account, [`files diverged`](diverged.md) tells what
changed in the old location after a migration and who last changed it, from Drive alone (views and downloads
excepted).

Service accounts and accounts with Application Default Credentials cannot read the audit log yet.

The account that reads the audit log also lists the folders and reads the sharing lists, so it must be able
to open the given files and folders too: as their owner, or as someone they are shared with.

## The command

```sh
gdrive files activity <FILE_ID>... [--account <ACCOUNT>] [--since <WHEN>] [--until <WHEN>]
    [--by user|file|event] [--exclude <EMAIL>]... [--active-only]
    [--scan domain|file|drive] [--jobs <N>]
    [--skip-header] [--field-separator <SEP>] [--tsv]
```

- `FILE_ID`: one or more ids of files or folders, also `root` (the top of My Drive) or the id of a shared
  drive. A folder counts with everything in it, at any depth, the folder itself included: opening the old
  folder is using it. Items in the trash do not count, and a `FILE_ID` in the trash (itself, or inside a
  folder in the trash) is refused. Several ids make one report.
- `--account ACCOUNT`: the account to read with, as `gdrive account list` prints it; the current account by
  default.
- `--since WHEN` (default `30d`) and `--until WHEN` (default: now): the time range. `WHEN` is a duration back
  from now (`12h`, `30d`, `2w`), a date (`2026-10-01`, from midnight), a date and time (`2026-10-01 14:30`
  or `2026-10-01 14:30:00`, the form the report prints), or an RFC 3339 time (`2026-10-01T14:30:00Z`).
  Dates and times without a time zone are in the local time zone, the one the report prints times in. The
  audit log keeps 180 days: an older `--since` starts 180 days ago, with a warning.
- `--by user|file|event`: the rows of the report (see [The reports](#the-reports)); `user` by default.
- `--exclude EMAIL`: leave out this person, for example the account that ran the migration, whose reads are
  in the log as downloads: what they did, and their row on the sharing lists (a group of that address too).
  Not case-sensitive; repeat it for several people. It applies to all three reports and to the summary.
- `--active-only`: only the people with activity. The sharing lists are then not read at all, and the
  `Access` column is `-` for everyone.
- `--scan`: how much of the audit log Google sends (see [Scans](#scans---scan)): `domain` by default,
  which is complete; `file` and `drive` are faster on request.
- `--jobs N`: send up to N requests at once, 1 to 64 (default 4).
- `--skip-header`, `--field-separator SEP`, `--tsv`: as for `gdrive files list`.

## Examples

```sh
# After a migration by admin@example.com, who still used the old folder in the last 30 days:
gdrive files activity 1AbC... --account admin@example.com --exclude admin@example.com

# The last week, one row per file and person:
gdrive files activity 1AbC... --since 7d --by file

# Since the migration day, every event, for a script:
gdrive files activity 1AbC... --since 2026-10-01 --by event --tsv --skip-header > events.tsv

# A folder of a shared drive, and a file in My Drive, in one report:
gdrive files activity 1Team... 1DeF...
```

## The reports

### Per person: `--by user`

One row per person with activity, the most recent first, then the people on the sharing lists of the given
items who did nothing, by name.

| Column | |
|---|---|
| `User` | the person's email address, in lower case; `(anonymous)` when the log has none. Sharing-list entries that are not a person are `group:<email>`, `domain:<domain>` and `anyone` (anyone with the link) |
| `Access` | the role on the sharing lists of the given items: `owner`, `organizer`, `fileOrganizer`, `writer`, `commenter` or `reader`, the strongest when the person is on several. `-` when the person is not on those lists: access through a group, a domain, a link, or a share of an item deeper in the folder |
| `Last activity` | the time of the person's newest event; `-` for none |
| `Days` | on how many calendar days the person did something (days of the local time zone) |
| `Views`, `Downloads`, `Edits`, `Other` | how many events of each kind, see below |
| `Files` | how many different items the person touched (folders count) |

### Per item: `--by file`

One row per item and person with activity, by path, then the most recent first. The path starts with the
name of the given item.

```text
Path                     User              Last activity         Views   Downloads   Edits   Other
Projects/2026/plan.pdf   ana@example.com   2026-11-04 14:02:11   12      1           0       0
Projects/2026/plan.pdf   bob@example.com   2026-10-30 09:11:45   2       0           0       0
Projects/Budget          ana@example.com   2026-11-03 10:15:40   20      0           7       0
```

### Per event: `--by event`

One row per event, the oldest first, with the event's name as Google logs it.

```text
Time                  User              Event      Path
2026-10-30 09:11:45   bob@example.com   view       Projects/2026/plan.pdf
2026-11-03 10:15:40   ana@example.com   edit       Projects/Budget
2026-11-04 14:02:11   ana@example.com   download   Projects/2026/plan.pdf
```

The sharing-list rows are only in `--by user`. An empty report prints only its header.

### Which events count

Google logs one or more events per action. An event counts when it is about one of the items, is the primary
event of its action (Google marks the others with `primary_event` false), and its person is not excluded.
Each event counts once.

| Column | Events |
|---|---|
| `Views` | `view`, `preview` |
| `Downloads` | `download`, `print`, `source_copy` (made a copy of it), `email_as_attachment` |
| `Edits` | `edit`, `create`, `upload`, `copy`, `rename`, `move`, `trash`, `untrash`, `delete`, `add_to_folder`, `remove_from_folder` |
| `Other` | every other event, for example sharing changes (`change_user_access`, `change_document_visibility`), `request_access`, `access_item_content`, `sync_item_content`, and names Google adds later |

### Output format

Times are printed in the local time zone, as `gdrive files list` and `gdrive files info` print them.
`--skip-header`, `--field-separator` and `--tsv` work as for `files list`: with `--tsv` the columns are
separated by one tab, and a tab, newline, carriage return or backslash in a value is written as `\t`, `\n`,
`\r` or `\\`. Names, paths and addresses come from Drive and the audit log: control characters in them (an
escape sequence, for example) are printed escaped, as `\u{1b}`, in every format.

### What stderr gets

The report is the only thing on stdout. stderr gets what was found and read, warnings, and a summary:

```text
Found 312 files in 41 folders
Reading the audit log from 2026-10-06 14:00:00 to 2026-11-05 14:00:00: every Drive event of the organization, in 30 windows of 24 hours (--scan domain; --scan file is faster for a few items, --scan drive for items in shared drives)
Read 1843209 events of the audit log, 3512 of them about these items
Summary: 3 people active since 2026-10-06 14:00:00, and anonymous users; 2 with access and no activity
```

On a terminal, a long scan prints a line such as `Read 120000 events so far, 410 about these items (12 of
30 windows done)` every 15 seconds. The summary is for people; scripts should read the report.

**Exit status:** 0 when the report was made, also when it is empty. 1 when it could not be made. A given id
that does not exist or is in the trash, a folder that cannot be listed completely, a sharing list that cannot
be read or a page of the audit log that cannot be read fail the command, and nothing is printed on stdout:
a report over part of the items or of the time would tell wrongly that people are gone.

## Scans: `--scan`

Google cannot select "everything under this folder": an event names one item. So gdrive lists the items
first, and keeps the events about them from what Google sends, whatever the scan. The scan only chooses how
much Google sends. **The default is complete; the faster scans are a choice you make.**

| Scan | What Google sends | Complete? | When to pick it |
|---|---|---|---|
| `domain` (default) | every Drive event of the organization in the time range | yes | always right; slow for a large organization, since all its Drive events are read to find those of the items |
| `file` | the events of each item (`doc_id`), one series of requests per item over the whole time range | yes: an item keeps its id when it is moved or renamed | a few files or a small folder in a large organization. With thousands of items it sends thousands of request series |
| `drive` | the events of the shared drives the given items are in (`shared_drive_id`) | **no**: see below | items that have always been in their shared drive. An error for an item in a My Drive |

**What `--scan drive` misses.** Google records in each event the shared drive the item was in *when the event
happened*. The views of a file from before it was moved into a shared drive name another shared drive, or none
when it was in a My Drive, and `--scan drive` does not get them: people who used the file only before the
move look gone. A [migration](migrate.md) into a shared drive is such a move, so after a migration read with
the default scan, or with `--scan file`.

`domain` and `drive` read the time range in windows of 24 hours, `--jobs` windows at once, and page through
each window. The windows overlap by a second, so that an event at their border is read twice rather than
missed; it counts once. `file` reads the whole time range for each item, `--jobs` items at once. If Google
refuses the filter of a `file` or `drive` scan, use the default.

The events are counted as they arrive: memory holds the events about the items, not the whole audit log. A
rate limit from Google holds back every request for a while and is retried, as for the other commands.

## Limits

- **180 days.** The audit log keeps events for 180 days; older activity cannot be seen.
- **Minutes of delay.** An event appears in the audit log minutes after it happened: the last minutes of the
  time range may be missing.
- **People outside the organization** show as `(anonymous)`, unless the item was shared with their own
  address. The report cannot tell anonymous people apart: one `(anonymous)` row stands for all of them.
- **Supported editions only.** Google logs most Drive events only for files owned by users on Google Workspace
  editions that have the Drive audit log; files owned by other users may have few or no events.
- **Items in the trash or deleted** are not listed, so their events do not count. Restore an item to see it.
- **The sharing list is that of the given items only.** People who have access through a group, a domain, a
  link, or a share of an item deeper in the folder have `-` as their access, and appear only once they did
  something.
- **The migration's own account** read every file it migrated, which the log has as downloads. Leave it out
  with `--exclude`.
- **Read only.** Nothing in Drive or in the organization is changed by this command.

## Errors and what to do

Google's own message is printed, with a hint:

| Error | What to do |
|---|---|
| `403 Forbidden: Request had insufficient authentication scopes.` | the account was authorized without the audit log: run `gdrive account reauth <ACCOUNT> --audit-log` |
| `403 Forbidden: Admin SDK API has not been used in project ... or it is disabled.` | enable the Admin SDK API in the Google Cloud project of the account's OAuth client; Google's message has the link. Wait a few minutes after enabling it |
| `401 Unauthorized: Access denied. You are not authorized to read activity records.` | the Google account is not an administrator of the organization with the Reports privilege, or it is a personal Gmail account |
| `400 Bad Request` with `--scan drive` or `--scan file` | Google refused the filter: run the command again with `--scan domain` |
