# Basic usage

Before you start: install gdrive, [create Google API credentials](create_google_api_credentials.md) and
add your account:

```sh
gdrive account add
```

gdrive asks for the client ID and client secret you created. On a server without a browser, see
[Remote and headless setup](remote_and_headless.md).

Every command has a `--help` option, for example `gdrive files download --help`.

## Accounts

```sh
gdrive account list                       # all accounts
gdrive account current                    # the account commands use
gdrive account switch <ACCOUNT_NAME>      # use another account
gdrive account remove <ACCOUNT_NAME>      # delete the local credentials
gdrive account export <ACCOUNT_NAME>      # write gdrive_export-<name>.tar
gdrive account import <ARCHIVE_PATH>
```

The account name is the email address of the Google account. `account remove` only deletes the files on
your machine. To revoke gdrive's access, remove your app at <https://myaccount.google.com/permissions>.

For CI and servers, gdrive 4.0.0 also takes a service account key (`gdrive account add --service-account
KEY_JSON`) or Application Default Credentials (`gdrive account add --adc`); see
[Service accounts](service_accounts.md).

## Find file and folder ids

Most commands take a file id or a folder id. An id is a string such as
`1a2B3c4D5e6F7g8H9i0JkLmNoPqRsTuVw`, not a name or a path. Ways to get one:

- `gdrive files list` prints it in the `Id` column.
- The address of the file in the Drive web interface contains it:

  | What | Address |
  |---|---|
  | Folder or shared drive | `https://drive.google.com/drive/folders/<ID>` |
  | File | `https://drive.google.com/file/d/<ID>/view` |
  | Google Doc, Sheet, Slides | `https://docs.google.com/document/d/<ID>/edit` (also `spreadsheets/d/`, `presentation/d/`) |

- `gdrive drives list` prints the ids of your shared drives.
- `files upload`, `files mkdir` and `files import` print the new id. With `--print-only-id`, a single
  upload, a `mkdir` and an `import` print only the id (without a trailing newline), which is handy in
  scripts:

  ```sh
  folder=$(gdrive files mkdir backups --print-only-id)
  gdrive files upload --parent "$folder" db.tar.gz
  ```

  `files upload --recursive --print-only-id` prints one line per folder and file instead, as
  `<path>: <id>`. The paths start with the name of the uploaded folder:

  ```text
  photos: 1AbCdEfGhIjKlMnOpQrStUvWxYz012345
  photos/cat.jpg: 1BcDeFgHiJkLmNoPqRsTuVwXyZ123456
  photos/2024: 1CdEfGhIjKlMnOpQrStUvWxYzA234567
  photos/2024/dog.jpg: 1DeFgHiJkLmNoPqRsTuVwXyZAb345678
  ```

`root` is Google's alias for the top folder of My Drive. You can use it wherever gdrive expects a folder
id, for example `--parent root`.

## List files

```sh
gdrive files list
```

This lists up to 30 items in the top folder of My Drive, folders first, then by last change. It does not
list your whole Drive. Options:

```sh
gdrive files list --max 500                 # more results
gdrive files list --parent <FOLDER_ID>      # items in one folder
gdrive files list --full-name               # do not shorten long names
gdrive files list --skip-header --field-separator ','
```

`--query` takes a Drive search query. It replaces the default query, so add `trashed = false` yourself:

```sh
gdrive files list --query "name contains 'invoice' and trashed = false"
gdrive files list --query "mimeType = 'application/vnd.google-apps.folder' and trashed = false"
gdrive files list --query "modifiedTime > '2026-01-01T00:00:00' and trashed = false"
gdrive files list --query "sharedWithMe and trashed = false"
gdrive files list --query "trashed = false" --max 1000   # everything, not only the top folder
```

Query syntax: [Search query terms and operators](https://developers.google.com/workspace/drive/api/guides/ref-search-terms)
and [Search for files and folders](https://developers.google.com/workspace/drive/api/guides/search-files).
Sort keys for `--order-by`: see `orderBy` in
[files.list](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/list).
The link in the `--help` text of gdrive 3.9.1 now leads to Google's page about searching shared drives,
not files ([upstream #81](https://github.com/glotlabs/gdrive/issues/81)). Use the links above.

In gdrive 3.9.1, `--query` is ignored when you also give `--parent` or `--drive`
([upstream #155](https://github.com/glotlabs/gdrive/issues/155)). A fix is planned. Until then, put the
folder into the query yourself:

```sh
gdrive files list --query "'<FOLDER_ID>' in parents and name contains 'invoice' and trashed = false"
```

Show the details of one file:

```sh
gdrive files info <FILE_ID>
gdrive files info <FILE_ID> --size-in-bytes
```

## Shared drives

```sh
gdrive drives list                          # id and name of each shared drive
gdrive files list --drive <DRIVE_ID>        # top folder of a shared drive
gdrive files list --parent <FOLDER_ID>      # a folder inside a shared drive
```

`drives list` shows at most 10 shared drives today. Paging through all of them is planned.

The other commands work on shared drives too. Pass the id of a file or folder in the shared drive, for
example `gdrive files upload --parent <FOLDER_ID> report.pdf` or
`gdrive files download --recursive <DRIVE_ID>`.

## Download a file

```sh
gdrive files download <FILE_ID>
gdrive files download <FILE_ID> --destination ~/Downloads   # the folder must exist
gdrive files download <FILE_ID> --stdout > copy.bin
gdrive files download <FILE_ID> --overwrite                 # replace a local file with the same name
gdrive files download <SHORTCUT_ID> --follow-shortcuts      # download the target of a shortcut
```

`files download` works for files with binary content: PDFs, images, archives, Office files and so on.
Google Docs, Sheets and Slides have no binary content. Use [`files export`](#google-docs-sheets-and-slides)
for them.

A download that breaks off (a dropped connection, an HTTP/2 `GOAWAY`) continues where it stopped, with a
`Warning:` line on stderr, but only if the file did not change on Drive meanwhile; a changed file is
downloaded again from the start (with `--stdout`, the command fails instead). The file is checked against
its MD5 checksum, or its size when Drive has no checksum, before it gets its final name; until then it is a
hidden `.gdrive-<id>.part` file next to it, removed again if the download fails. Resuming works within one
run of gdrive, not across runs.

All commands retry rate limits (`429`, `403 rateLimitExceeded`), timeouts and server errors (`500`, `502`,
`503`, `504`) with growing, randomized waits, and honor `Retry-After`. They give up after 10 attempts or 10
minutes without progress. Errors that waiting does not fix, such as `404`, permission or quota errors, end
the command at once.

## Download a folder

```sh
gdrive files download --recursive <FOLDER_ID>
gdrive files download --recursive <FOLDER_ID> --destination ~/backup
```

This creates a local folder with the same name and downloads everything in it. To download all of
My Drive, use the `root` alias. It creates a folder named `My Drive`:

```sh
gdrive files download --recursive root --destination ~/backup
```

gdrive downloads 4 files at a time (`--jobs`) and starts while it is still listing the folder. See
[Recursive transfers](#recursive-transfers) for `--jobs`, `--continue-on-error`, the output and the summary
line.

Things to know:

- **Google Docs, Sheets and Slides are skipped.** Recursive download leaves out all Google-native files
  and says how many at the end, for example ``3 Google Docs files skipped; use `gdrive files export` to
  download them``. Export them one by one with `files export`. Exporting them during recursive download is
  planned. Shortcuts are skipped and counted the same way: a recursive download does not follow them.
- **There is no `--skip`.** gdrive 2's `--skip` kept every local file that already existed. gdrive 3 has
  no exact equivalent yet. Without `--overwrite`, a recursive download stops when the local folder already
  exists. A real skip-existing mode is planned (roadmap milestone 2, item 6).
- **`--overwrite` replaces local changes.** When you run a recursive download again with `--overwrite`,
  gdrive skips files whose local copy has the same MD5 checksum as the Drive file. Every other local file
  with the same path is replaced by the Drive version, including files you edited locally. Do not use it
  as a replacement for `--skip` on folders with local edits.
- A Drive name that contains `/` makes the download fail
  ([upstream #19](https://github.com/glotlabs/gdrive/issues/19)). A fix is planned.

## Upload

```sh
gdrive files upload report.pdf                         # into the top folder of My Drive
gdrive files upload --parent <FOLDER_ID> report.pdf    # into a folder
gdrive files upload --recursive ~/photos               # a local folder and everything in it
```

- `--recursive` always creates a new folder on Drive. It does not merge into an existing folder with the
  same name.
- Without a file path, `files upload` reads from standard input. The Drive file then gets a generated
  name; rename it with `gdrive files rename`.
- `--mime <MIME_TYPE>` sets the type instead of guessing it from the file name.
- `--chunk-size <MB>` and `--multipart-threshold <MB>` are explained in
  [Resumable uploads](#resumable-uploads).
- `--print-chunk-errors` and `--print-chunk-info` show retries and progress per chunk. They help when
  uploads from a cloud server are slow because of many `429`, `502` or `503` responses.
- `--recursive` uploads 4 files at a time (`--jobs`); every Drive folder is created before anything goes
  into it. See [Recursive transfers](#recursive-transfers).

## Recursive transfers

`files download --recursive` and `files upload --recursive` transfer several files at a time:

```sh
gdrive files download --recursive <FOLDER_ID> --jobs 8
gdrive files upload --recursive ~/photos --jobs 2 --continue-on-error
```

- `--jobs N` sets how many files are transferred at once, from 1 to 64; the default is 4. Folder listings
  and folder creations run besides, 2 at a time. `--jobs 1` transfers one file after the other, as gdrive
  3.9 did. More jobs help most with many small files; with a few large files the connection is the limit.
  All jobs share one rate-limit backoff: when Drive answers `429` or `403 rateLimitExceeded`, all of them
  wait, also between the chunks of uploads that are already running.
- A recursive download starts while it is still listing the folder: the files of a folder are downloaded
  once that folder is listed, and each local folder is created before the files in it. A recursive upload
  lists the local folder first and creates every Drive folder before it uploads anything into it.
- The lines of files that are transferred at the same time never mix, but they come in the order the
  transfers start and end, not in the order of the folder. A recursive download prints `Found N files in M
  directories ...` once the listing is complete, no longer before the first download. `Downloaded ...` and
  `Uploaded ...` come only when every file succeeded. With `--print-only-id`, the `<path>: <id>` line of a
  folder or file comes once it exists on Drive, a folder's line before the lines of what is in it.
- **Errors.** By default the first failed file or folder stops the command: no further transfer starts,
  the running ones are cancelled and their temporary `.gdrive-<id>.part` files removed. The error of every
  failed file or folder is printed to stderr when it happens, as `Error: unable to download '<path>': ...`
  (or `upload`). With `--continue-on-error`, gdrive goes on with everything that does not depend on the
  failure and reports every failure at the end. A folder that cannot be listed (download) or created
  (upload) takes everything in it along. Either way the command exits with status 1 when anything failed.
- A local file or folder that a recursive upload cannot read (a broken symbolic link, no permission) is
  such a failure too. By default the upload stops before it creates anything on Drive; with
  `--continue-on-error` everything else is uploaded.
- Drive sometimes answers a listing with `incompleteSearch`: the result may miss files. gdrive prints a
  `Warning:` line and the download exits with status 1 at the end, unless `--continue-on-error`.
- Ctrl-C cancels the running transfers, removes their temporary files and exits with status 1. Files that
  were complete stay.

At the end, a recursive transfer prints one summary line to stderr:

```text
Summary: 12 transferred (3.4 MB), 2 unchanged, 3 skipped, 1 failed, 4 cancelled
```

The counts are files:

| Count | Meaning |
|---|---|
| transferred | Downloaded or uploaded. The size in parentheses is their total. |
| unchanged | Not downloaded: with `--overwrite`, the local file has the same MD5 checksum as the Drive file. |
| skipped | Not transferable: Google Docs, Sheets and Slides (use `files export`) and shortcuts. |
| failed | The file failed; its error is printed above. |
| cancelled | Not done because the command stopped (a failure without `--continue-on-error`, Ctrl-C) or because the folder it is in failed. |

Folders that failed are added at the end, as in `...; 1 folder failed`, and an interrupted command ends the
line with `; interrupted`. The line is meant for people; scripts should use the exit status.

### Resumable uploads

A file of at most 5 MB goes up in a single request. A larger file goes up in chunks of `--chunk-size` MB
(a power of two, 32 by default) through a resumable upload session, which `files upload`, `files update`
and `files import` all use. `--multipart-threshold <MB>` on `files upload` and `files update` moves the
line between the two (`0` sends every non-empty file in chunks). Chunks are read from the disk while they
are sent, so a larger chunk size does not take more memory.

- A chunk that fails (a dropped connection, a `429` or a `5xx`) is sent again after a wait: gdrive asks
  Drive how much of the file it holds and continues from there, with a `Warning:` line on stderr.
- An interrupted upload (gdrive stopped, the network gone for longer than the retries last) continues
  where Drive stopped when you run the same command again, if the local file is unchanged (same path,
  size and modification time), it goes to the same place (same folder and name, or the same file for
  `files update`), and the upload started less than a week ago, after which Drive forgets it. Otherwise
  it starts over. Uploads from standard input and the files of `--recursive` start over.
- What gdrive needs to continue is kept in `~/.config/gdrive3/<account>/uploads/`, one small JSON file
  per unfinished upload, readable only by you, deleted when the upload completes; files older than a
  week are deleted too. A file there holds the session URL, which allows uploading to that session:
  keep it as private as the account's tokens.
- After the upload, gdrive compares the MD5 checksum Drive reports with the local file. If they differ,
  for example because the file changed during the upload, the command fails and names the uploaded file's
  id.
- A new file gets its id before the upload starts, so a reply that is lost at the very end does not make
  a second copy: gdrive looks the file up instead. Drive takes no id for an import; it is marked with a
  private app property (`gdriveUploadId`) and searched for by it.

## Progress output

While files are transferred, gdrive shows the progress on stderr, if stderr is a terminal. One file gets a
bar:

```text
[===============>                          ] 120 MB/310 MB (38%), 11.2 MB/s, ETA 17s
```

A recursive transfer gets a line for the whole run and one per file in transfer, at most `--jobs`:

```text
[==========>                ] 14/52 files, 1.2 GB/3.1 GB (38%), 24.5 MB/s, ETA 1m 18s, 1 failed
  photos/2024/IMG_0412.jpg               [=======>            ] 3.1 MB/8 MB, 2.2 MB/s
  photos/2024/IMG_0413.jpg               [==>                 ] 1 MB/7.4 MB, 1.9 MB/s
```

- The bytes are confirmed bytes: for a download, what is written to the local file; for an upload, what
  Drive acknowledged. An upload moves a chunk at a time (`--chunk-size`, 32 MB by default), as Drive
  confirms chunks; a file of at most 5 MB goes up in one request and completes in one step.
- While a recursive download still lists the folder, the totals are `?` and the line says what was found
  so far: `3/? files (12 found), 45.6 MB/? (120 MB found)`. A recursive upload knows its totals from the
  start: it lists the local folder first.
- What a file does not transfer (it is unchanged, skipped, failed or cancelled) leaves the total, so the
  total is what the run transfers. Failed files and folders are counted at the end of the line.
- The rate is that of the last few seconds; ETA is the time left at that rate. Without a known size (an
  export), there is a spinner instead of a bar and no ETA.
- Lines that gdrive prints during the transfer (`Downloading ...`, warnings, errors) appear above the
  bars, whole. The bars disappear when the transfer ends, before the last messages and the summary.
  Nothing of the progress goes to stdout: the output of `--stdout` and `--print-only-id` is the same with
  and without it.

When it shows:

| Setting | Progress |
|---|---|
| (default) | When stderr is a terminal and `TERM` is not `dumb`. With `files download --stdout`, only if stdout is not a terminal as well, so that the bars do not mix with the content. |
| `--progress` | Always. Where stderr is not a terminal (a CI log, `script`, a file), a single line rewritten after a carriage return once a second. |
| `--no-progress` | Never. |
| `GDRIVE_PROGRESS=1`, `GDRIVE_PROGRESS=0` | Like `--progress` and `--no-progress`, when neither flag is given. Other values are ignored with a warning. |

The flags work on `files download`, `files upload`, `files update`, `files import` and `files export`. When
both are given, the last one wins, so a script can force the progress on or off whatever an alias adds.

## Update a file

```sh
gdrive files update <FILE_ID> report.pdf
```

This uploads new content into an existing Drive file. The id, name, folder and sharing stay the same, and
Drive keeps the old content as a previous version, typically for 30 days. `files update` works on files
only. To change a folder, upload it again with `files upload --recursive`.

## Google Docs, Sheets and Slides

Export a Google file to a local file. The file extension selects the format:

```sh
gdrive files export <FILE_ID> notes.docx
gdrive files export <FILE_ID> budget.xlsx
gdrive files export <FILE_ID> slides.pdf --overwrite
```

| Google file | Extensions |
|---|---|
| Docs | `pdf`, `docx`, `odt`, `rtf`, `txt`, `html`, `epub` |
| Sheets | `xlsx`, `ods`, `csv`, `tsv`, `pdf` |
| Slides | `pdf`, `pptx`, `odp`, `txt` |

- `csv` and `tsv` contain only the first sheet. To keep all sheets, export to `xlsx` or `ods`
  ([upstream #98](https://github.com/glotlabs/gdrive/issues/98)).
- Google limits an export to 10 MB. Larger documents fail to export.

Import a local file and convert it to a Google file:

```sh
gdrive files import report.docx
gdrive files import --parent <FOLDER_ID> table.csv
```

Files that can be imported include `doc`, `docx`, `odt`, `pdf`, `html`, `xls`, `xlsx`, `csv`, `ods`,
`ppt`, `pptx` and `odp`.

## Organize

```sh
gdrive files mkdir <NAME>
gdrive files mkdir --parent <FOLDER_ID> <NAME>
gdrive files rename <FILE_ID> <NEW_NAME>
gdrive files move <FILE_ID> <FOLDER_ID>
gdrive files copy <FILE_ID> <FOLDER_ID>       # files only, not folders
gdrive files delete <FILE_ID>
gdrive files delete --recursive <FOLDER_ID>
```

`files delete` deletes permanently. It does not move the file to the trash. A trash command is planned.

## Share

```sh
gdrive permissions share <FILE_ID>                                   # anyone with the link can view
gdrive permissions share <FILE_ID> --type user --email friend@example.com --role writer
gdrive permissions list <FILE_ID>
gdrive permissions revoke <FILE_ID>                                  # remove the "anyone" permission
gdrive permissions revoke <FILE_ID> --id <PERMISSION_ID>
gdrive permissions revoke <FILE_ID> --all                            # everything except the owner
```

Without options, `permissions share` makes the file readable by anyone who has the link. Roles:
`owner`, `organizer`, `fileOrganizer`, `writer`, `commenter`, `reader`. Types: `user`, `group`, `domain`,
`anyone`.

## Coming from gdrive 2

| gdrive 2 | gdrive 3 |
|---|---|
| `gdrive list` | `gdrive files list` |
| `gdrive list --query "..."` | `gdrive files list --query "..."` |
| `gdrive info <ID>` | `gdrive files info <ID>` |
| `gdrive download <ID>` | `gdrive files download <ID>` |
| `gdrive download -r <ID>` | `gdrive files download --recursive <ID>` |
| `gdrive download --path <DIR> <ID>` | `gdrive files download --destination <DIR> <ID>` |
| `gdrive download --force <ID>` | `gdrive files download --overwrite <ID>` |
| `gdrive download --skip -r <ID>` | No exact equivalent. `--overwrite` is not the same: it replaces local files whose content differs (see [Download a folder](#download-a-folder)). Planned. |
| `gdrive upload -p <ID> <FILE>` | `gdrive files upload --parent <ID> <FILE>` |
| `gdrive upload -r <DIR>` | `gdrive files upload --recursive <DIR>` |
| `gdrive update <ID> <FILE>` | `gdrive files update <ID> <FILE>` |
| `gdrive mkdir <NAME>` | `gdrive files mkdir <NAME>` |
| `gdrive delete <ID>` | `gdrive files delete <ID>` |
| `gdrive export --mime <TYPE> <ID>` | `gdrive files export <ID> <FILE.ext>` |
| `gdrive import <FILE>` | `gdrive files import <FILE>` |
| `gdrive share <ID>` | `gdrive permissions share <ID>` |
| `gdrive share list <ID>` | `gdrive permissions list <ID>` |
| `gdrive share revoke <ID> <PERMISSION_ID>` | `gdrive permissions revoke --id <PERMISSION_ID> <ID>` |
| `gdrive about` | `gdrive account current` |
| `gdrive sync upload`, `gdrive sync download` | No equivalent yet. Planned. |
| `gdrive revision ...` | No equivalent. |
| `--service-account` | Not supported yet. Planned. |

gdrive 3 has no short options such as `-p` or `-r` yet. They are planned.
