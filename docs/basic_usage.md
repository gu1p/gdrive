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
downloaded again from the start (with `--stdout`, the command fails instead). The file is checked against its MD5 checksum, or its size when Drive has no
checksum, before it gets its final name; until then it is a hidden `.gdrive-<id>.part` file next to it,
removed again if the download fails. Resuming works within one run of gdrive, not across runs.

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

Things to know:

- **Google Docs, Sheets and Slides are skipped.** Recursive download currently leaves out all
  Google-native files without a message. Export them one by one with `files export`. Exporting them during
  recursive download is planned.
- gdrive reads the whole folder tree before it downloads the first file, so a large folder can be silent
  for a while. Then it downloads one file at a time.
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
- `--chunk-size <MB>` sets the upload chunk size (a power of two). Larger chunks use more memory.
- `--print-chunk-errors` and `--print-chunk-info` show retries and progress per chunk. They help when
  uploads from a cloud server are slow because of many `429`, `502` or `503` responses.

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
