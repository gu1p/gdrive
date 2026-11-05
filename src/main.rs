mod about;
mod account;
mod app_config;
mod auth;
mod common;
mod completions;
mod drives;
mod files;
mod hub;
mod permissions;
mod proxy;
mod transfer;
mod version;

use std::{num::NonZeroUsize, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use common::drive_file::FileExtension;
use common::{
    delegate::{ChunkSize, MultipartThreshold},
    permission,
};
use files::export::ExportFormats;
use files::list::{ListQuery, ListSortOrder};
use mime::Mime;
use transfer::scheduler::Options;

#[derive(Parser)]
#[command(author, version, about, long_about = None, disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print information about gdrive
    About,

    /// Commands for managing accounts
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },

    /// Commands for managing drives
    Drives {
        #[command(subcommand)]
        command: DriveCommand,
    },

    /// Commands for managing files
    Files {
        #[command(subcommand)]
        command: FileCommand,
    },

    /// Commands for managing file permissions
    Permissions {
        #[command(subcommand)]
        command: PermissionCommand,
    },

    /// Print a shell completion script to stdout
    ///
    /// Load it in the current shell, or save it where your shell looks for completions:
    ///   bash:       source <(gdrive completions bash)
    ///   zsh:        gdrive completions zsh > "${fpath[1]}/_gdrive"
    ///   fish:       gdrive completions fish > ~/.config/fish/completions/gdrive.fish
    ///   powershell: gdrive completions powershell | Out-String | Invoke-Expression
    #[command(verbatim_doc_comment)]
    Completions {
        /// Shell to generate the script for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Print version information
    Version,
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Add an account
    ///
    /// Asks for the Client ID and Client Secret, then for your approval in a browser. By default
    /// gdrive receives the browser's redirect on port 8085 of localhost (see --port); on a remote
    /// or headless machine use --manual.
    ///
    /// With --service-account, adds a service account from its JSON key file instead, and with
    /// --adc an account that uses Application Default Credentials; nothing is asked then. See
    /// `docs/service_accounts.md`.
    #[command(group(clap::ArgGroup::new("no_login").args(["service_account", "adc"])))]
    Add {
        #[command(flatten)]
        login: LoginArgs,

        #[command(flatten)]
        service: ServiceAccountArgs,
    },

    /// Authorize an existing account again
    ///
    /// Use this when the stored authorization expired or was revoked. It runs the same login as
    /// `account add`, with the client credentials stored for the account, and replaces the
    /// account's tokens. Other commands never start a login: when the stored token cannot be
    /// refreshed, they fail at once and point to this command.
    Reauth {
        /// Account name [default: current account]
        account_name: Option<String>,

        #[command(flatten)]
        login: LoginArgs,
    },

    /// List all accounts
    List,

    /// Print current account
    Current,

    /// Switch to a different account
    Switch {
        /// Account name
        account_name: String,
    },

    /// Remove an account
    Remove {
        /// Account name
        account_name: String,
    },

    /// Export account, this will create a zip file of the account which can be imported
    Export {
        /// Account name
        account_name: String,
    },

    /// Import account that was created with the export command
    Import {
        /// Path to archive
        file_path: PathBuf,
    },
}

/// The accounts `account add` adds without a login.
#[derive(clap::Args)]
struct ServiceAccountArgs {
    /// Add a service account from its JSON key file
    ///
    /// gdrive gets a token with the key to check it, then stores the key in the account's
    /// directory. Service accounts have no Drive storage of their own: they work in shared drives
    /// they are a member of and on files shared with them, or, with --subject, as a Google
    /// Workspace user.
    #[arg(long, value_name = "KEY_JSON", conflicts_with_all = ["manual", "port"])]
    service_account: Option<PathBuf>,

    /// Act for this Google Workspace user (domain-wide delegation, set up by an administrator)
    #[arg(
        long,
        value_name = "EMAIL",
        requires = "service_account",
        conflicts_with = "adc"
    )]
    subject: Option<String>,

    /// Use Application Default Credentials
    ///
    /// The service account key named by `GOOGLE_APPLICATION_CREDENTIALS`, the user credentials of
    /// `gcloud auth application-default login`, or the metadata server of Google Cloud. gdrive
    /// stores none of them and looks them up again on every run.
    #[arg(long, conflicts_with_all = ["manual", "port"])]
    adc: bool,

    /// Name of the new account [default: the `client_email` of the key, or "adc"]
    #[arg(long, value_name = "NAME", requires = "no_login")]
    name: Option<String>,
}

/// How the login of `account add` and `account reauth` gets the authorization.
#[derive(clap::Args)]
struct LoginArgs {
    /// Paste the redirect URL instead of waiting for the browser (for remote and headless
    /// machines)
    ///
    /// gdrive prints the authorization URL, which can be opened in a browser on any machine.
    /// After the approval the browser lands on http://localhost:PORT/?code=...&state=...; that
    /// page fails to load. Copy its full address from the address bar and paste it into gdrive.
    #[arg(long, visible_alias = "no-browser")]
    manual: bool,

    /// Port of the redirect URI http://localhost:PORT
    ///
    /// Without --manual, gdrive listens on this port of 127.0.0.1 for the browser's redirect;
    /// 0 picks a free port. Desktop app clients accept any port.
    #[arg(long, value_name = "PORT", default_value_t = auth::login::DEFAULT_PORT)]
    port: u16,
}

impl LoginArgs {
    fn mode(&self) -> auth::login::Mode {
        if self.manual {
            auth::login::Mode::Manual
        } else {
            auth::login::Mode::Browser
        }
    }
}

#[derive(Subcommand)]
enum DriveCommand {
    /// List drives
    List {
        /// Max drives to list [default: all]
        #[arg(long, value_parser = parse_positive_usize)]
        max: Option<usize>,

        /// Don't print header
        #[arg(long)]
        skip_header: bool,

        /// Field separator
        #[arg(long, default_value_t = String::from("\t"))]
        field_separator: String,
    },
}

#[derive(Subcommand)]
enum FileCommand {
    /// Print file info
    Info {
        /// File id
        file_id: String,

        /// Display size in bytes
        #[arg(long, default_value_t = false)]
        size_in_bytes: bool,
    },

    /// List files
    List {
        /// Max files to list
        #[arg(short, long, default_value_t = 30)]
        max: usize,

        /// Query. See <https://developers.google.com/workspace/drive/api/guides/search-files> [default: "'root' in parents and trashed = false"]
        ///
        /// With --parent, lists only the files in that folder that match:
        /// `'<DIRECTORY_ID>' in parents and trashed = false and (<QUERY>)`. With --drive, searches
        /// the whole shared drive instead of its top level: `trashed = false and (<QUERY>)`. In
        /// both cases `trashed = false` is left out if the query mentions trashed, so
        /// `--query 'trashed = true'` lists the trash.
        #[arg(short, long)]
        query: Option<String>,

        /// Order by. See <https://developers.google.com/workspace/drive/api/reference/rest/v3/files/list>
        #[arg(long, default_value_t = ListSortOrder::default())]
        order_by: ListSortOrder,

        /// List files in a specific folder
        ///
        /// Without --query, lists the files in the folder that are not trashed. With --query,
        /// lists the files in the folder that match the query (not trashed, unless the query
        /// mentions trashed).
        #[arg(short, long, value_name = "DIRECTORY_ID")]
        parent: Option<String>,

        /// List files on a shared drive
        ///
        /// Without --query, lists the files that are not trashed at the top level of the drive,
        /// or in the --parent folder. With --query, searches the whole drive, or only the
        /// --parent folder (not trashed, unless the query mentions trashed).
        #[arg(long, value_name = "DRIVE_ID")]
        drive: Option<String>,

        /// Don't print header
        #[arg(long)]
        skip_header: bool,

        /// Show full file name without truncating
        #[arg(long)]
        full_name: bool,

        /// Field separator. Columns stay aligned; for tab-separated values use --tsv
        #[arg(long, default_value_t = String::from("\t"))]
        field_separator: String,

        /// Print tab-separated values for scripts
        ///
        /// Columns are not aligned, names are not truncated, and a tab, newline, carriage return
        /// or backslash in a name is written as `\t`, `\n`, `\r` or `\\`.
        #[arg(long, conflicts_with = "field_separator")]
        tsv: bool,

        /// Print a JSON array of files for scripts
        ///
        /// Each object has the keys id, name, type (as in the Type column), mimeType, size (in
        /// bytes, null when Drive reports none, as for folders and shortcuts) and createdTime
        /// (RFC 3339, UTC).
        #[arg(long, conflicts_with_all = ["tsv", "field_separator", "skip_header"])]
        json: bool,
    },

    /// Download file
    Download {
        /// File id
        file_id: String,

        /// Overwrite existing files and folders
        #[arg(short, long)]
        overwrite: bool,

        /// Follow shortcut and download target file (does not work with recursive download)
        #[arg(long)]
        follow_shortcuts: bool,

        /// Download directories
        #[arg(short, long)]
        recursive: bool,

        /// Path where the file/directory should be downloaded to
        #[arg(short, long, value_name = "PATH")]
        destination: Option<PathBuf>,

        /// Write file to stdout
        #[arg(long)]
        stdout: bool,

        /// Download files that Google flagged as malware or spam. Only the owner of a file, or an organizer of its shared drive, can do this
        #[arg(long)]
        acknowledge_abuse: bool,

        /// With --recursive: download up to N files at once (1 to 64)
        #[arg(long, value_name = "N", default_value_t = Options::DEFAULT_JOBS, value_parser = parse_jobs)]
        jobs: NonZeroUsize,

        /// With --recursive: when a file fails, go on with the others instead of stopping. The command still exits with an error
        #[arg(long)]
        continue_on_error: bool,

        /// With --recursive: export Google Docs, Sheets, Slides and Drawings instead of skipping them. SPEC is a profile, `default` (docs=docx, sheets=xlsx, slides=pptx, drawings=png) or `md` (docs=md, the rest as default), and/or TYPE=FORMAT overrides separated by commas, for example `md,sheets=ods` or `docs=pdf,drawings=skip`. Types: docs, sheets, slides, drawings; formats as for `files export`, or skip
        #[arg(long, value_name = "SPEC", requires = "recursive", value_parser = ExportFormats::parse)]
        export: Option<ExportFormats>,
        #[command(flatten)]
        progress: ProgressArgs,
    },

    /// Sync a local directory and a Drive folder one way, like rsync
    ///
    /// `sync up` makes the Drive folder like the local directory, `sync down` the local
    /// directory like the Drive folder: new files are created, changed ones updated (a changed
    /// Drive file gets a new revision, its id stays). Nothing is deleted unless --delete is given,
    /// and then only after everything was transferred and both sides were listed completely:
    /// Drive files go to the Drive trash, local files to LOCAL_DIR/.gdrive-trash/. Use --dry-run
    /// first: it prints the same lines as the real run, one per item. See docs/sync.md.
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },

    /// Upload file
    Upload {
        /// Path of file to upload
        file_path: Option<PathBuf>,

        /// Name of the file on Drive [default: the name of the local file, a random name for stdin]
        #[arg(long, value_name = "NAME", conflicts_with = "recursive", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        name: Option<String>,

        /// Force mime type [default: auto-detect]
        #[arg(long, value_name = "MIME_TYPE")]
        mime: Option<Mime>,

        /// Upload to an existing directory
        #[arg(short, long, value_name = "DIRECTORY_ID")]
        parent: Option<Vec<String>>,

        /// Upload directories. Note that this will always create a new directory on drive and will not update existing directories with the same name
        #[arg(short, long)]
        recursive: bool,

        /// Set chunk size in MB, must be a power of two.
        #[arg(long, value_name = "1|2|4|8|16|32|64|128|256|512|1024|4096|8192", default_value_t = ChunkSize::default())]
        chunk_size: ChunkSize,

        /// Upload files of at most this many MB in a single request; larger files are uploaded in chunks and can be resumed. 0 uploads every non-empty file in chunks
        #[arg(long, value_name = "MB", default_value_t = MultipartThreshold::default())]
        multipart_threshold: MultipartThreshold,

        /// Print errors occuring during chunk upload
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_errors: bool,

        /// Print details about each chunk
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_info: bool,

        /// Print only id of file/folder
        #[arg(long, default_value_t = false)]
        print_only_id: bool,

        /// With --recursive: upload up to N files at once (1 to 64)
        #[arg(long, value_name = "N", default_value_t = Options::DEFAULT_JOBS, value_parser = parse_jobs)]
        jobs: NonZeroUsize,

        /// With --recursive: when a file or folder fails, go on with the others instead of stopping. The command still exits with an error
        #[arg(long)]
        continue_on_error: bool,

        #[command(flatten)]
        progress: ProgressArgs,
    },

    /// Migrate files and folders to another account
    ///
    /// Copies the given files and folders of the source account (--from, the current account by
    /// default) into the account --to, folders with everything in them. The content streams through this
    /// machine without being written to disk. The source account is only read: nothing in it is
    /// changed, trashed or deleted unless --trash-source is given. Running the command again
    /// skips what is already there, so an interrupted migration is finished by running it again.
    /// Google Docs, Sheets and Slides are exported and converted back (up to 10 MB each);
    /// shortcuts, Drawings, Forms and the other Google types are skipped; sharing and comments
    /// are not migrated. Use --dry-run first. See docs/migrate.md.
    Migrate {
        /// Id of a file or folder of the source account. Several ids are migrated in one run
        #[arg(required = true, value_name = "FILE_ID")]
        file_id: Vec<String>,

        /// The account to migrate to, as `gdrive account list` prints it
        #[arg(long, value_name = "ACCOUNT")]
        to: String,

        /// The account to migrate from, as `gdrive account list` prints it [default: the current account]
        #[arg(long, value_name = "ACCOUNT")]
        from: Option<String>,

        /// Id of a folder of the destination account to migrate into [default: the top of My Drive]
        #[arg(long, value_name = "FOLDER_ID")]
        into: Option<String>,

        /// Migrate up to N files at once (1 to 64)
        #[arg(long, value_name = "N", default_value_t = Options::DEFAULT_JOBS, value_parser = parse_jobs)]
        jobs: NonZeroUsize,

        /// When a file or folder fails, go on with the others instead of stopping. The command still exits with an error
        #[arg(long)]
        continue_on_error: bool,

        /// Print what would be done, one line per item, and change nothing: the destination is only read
        #[arg(long)]
        dry_run: bool,

        /// Move each source file to the trash of the source account once it is in the destination, and each source folder once everything in it is. Nothing is deleted for good
        #[arg(long)]
        trash_source: bool,

        /// Set chunk size in MB, must be a power of two.
        #[arg(long, value_name = "1|2|4|8|16|32|64|128|256|512|1024|4096|8192", default_value_t = ChunkSize::default())]
        chunk_size: ChunkSize,

        /// Upload files of at most this many MB in a single request; larger files are uploaded in chunks and can be resumed. 0 uploads every non-empty file in chunks
        #[arg(long, value_name = "MB", default_value_t = MultipartThreshold::default())]
        multipart_threshold: MultipartThreshold,

        /// Print errors occuring during chunk upload
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_errors: bool,

        /// Print details about each chunk
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_info: bool,

        #[command(flatten)]
        progress: ProgressArgs,
    },

    /// Report who used files and folders, from the audit log of a Google Workspace organization
    ///
    /// Reads the Drive audit log (Admin SDK Reports API) of the organization that owns the
    /// given files and folders, folders with everything in them, and prints per person what they
    /// did there: the last time, on how many days, how many views, downloads, edits and other
    /// events, on how many items. People on the sharing lists of the given items without any
    /// activity follow. Use it after a migration to see who still uses the old location. The
    /// account must be an administrator of the organization with the Reports privilege, and
    /// authorized with 'gdrive account reauth <ACCOUNT> --audit-log'. Nothing in Drive is
    /// changed. See docs/activity.md.
    Activity {
        /// Id of a file or folder: also `root`, or the id of a shared drive. Several ids are one report
        #[arg(required = true, value_name = "FILE_ID")]
        file_id: Vec<String>,

        /// The account to read with, as `gdrive account list` prints it [default: the current account]
        #[arg(long, value_name = "ACCOUNT")]
        account: Option<String>,

        /// Start of the time range: a duration back from now (12h, 30d, 2w), a local date (2026-10-01) or time (2026-10-01 14:30), or an RFC 3339 time. The audit log keeps 180 days
        #[arg(long, value_name = "WHEN", default_value = "30d", value_parser = files::activity::when::parse)]
        since: files::activity::when::When,

        /// End of the time range, in the forms of --since [default: now]
        #[arg(long, value_name = "WHEN", value_parser = files::activity::when::parse)]
        until: Option<files::activity::when::When>,

        /// The rows of the report
        #[arg(long, value_enum, default_value_t = files::activity::By::User)]
        by: files::activity::By,

        /// Leave out this email address (case-insensitive): its activity and its row on the sharing lists, for example the account that ran a migration. Repeat for several
        #[arg(long, value_name = "EMAIL")]
        exclude: Vec<String>,

        /// Only people with activity: leave out those on the sharing lists without any. The sharing lists are not read, and the Access column is `-`
        #[arg(long)]
        active_only: bool,

        /// How much of the audit log Google sends; the items are filtered on this machine in every case. The default reads everything and is always complete
        #[arg(long, value_enum, default_value_t = files::activity::Scan::Domain)]
        scan: files::activity::Scan,

        /// Send up to N requests at once (1 to 64)
        #[arg(long, value_name = "N", default_value_t = Options::DEFAULT_JOBS, value_parser = parse_jobs)]
        jobs: NonZeroUsize,

        /// Don't print header
        #[arg(long)]
        skip_header: bool,

        /// Field separator. Columns stay aligned; for tab-separated values use --tsv
        #[arg(long, default_value_t = String::from("\t"))]
        field_separator: String,

        /// Print tab-separated values for scripts
        ///
        /// Columns are not aligned, and a tab, newline, carriage return or backslash in a value is
        /// written as `\t`, `\n`, `\r` or `\\`, another control character as `\u{1b}`.
        #[arg(long, conflicts_with = "field_separator")]
        tsv: bool,
    },

    /// Update file. This will create a new version of the file. The older versions will typically be kept for 30 days.
    Update {
        /// File id of the file you want ot update
        file_id: String,

        /// Path of file to upload
        file_path: Option<PathBuf>,

        /// New name of the file [default: the name of the local file; with stdin the file keeps its name]
        #[arg(long, value_name = "NAME", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        name: Option<String>,

        /// Force mime type [default: auto-detect]
        #[arg(long, value_name = "MIME_TYPE")]
        mime: Option<Mime>,

        /// Set chunk size in MB, must be a power of two.
        #[arg(long, value_name = "1|2|4|8|16|32|64|128|256|512|1024|4096|8192", default_value_t = ChunkSize::default())]
        chunk_size: ChunkSize,

        /// Upload files of at most this many MB in a single request; larger files are uploaded in chunks and can be resumed. 0 uploads every non-empty file in chunks
        #[arg(long, value_name = "MB", default_value_t = MultipartThreshold::default())]
        multipart_threshold: MultipartThreshold,

        /// Print errors occuring during chunk upload
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_errors: bool,

        /// Print details about each chunk
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_info: bool,

        #[command(flatten)]
        progress: ProgressArgs,
    },

    /// Delete file
    Delete {
        /// File id. Several ids are deleted one after another; the command fails if any of them fails
        #[arg(required = true)]
        file_id: Vec<String>,

        /// Delete directory and all it's content
        #[arg(short, long)]
        recursive: bool,
    },

    /// Move file/directory to the trash. A shortcut is trashed itself, not its target
    Trash {
        /// File id. Several ids are trashed one after another; the command fails if any of them fails
        #[arg(required = true)]
        file_id: Vec<String>,

        /// Trash directory and all its content
        #[arg(short, long)]
        recursive: bool,
    },

    /// Restore file/directory from the trash
    Untrash {
        /// File id. Several ids are restored one after another; the command fails if any of them fails
        #[arg(required = true)]
        file_id: Vec<String>,
    },

    /// Create directory
    Mkdir {
        /// Name
        name: String,

        /// Create in an existing directory
        #[arg(short, long, value_name = "DIRECTORY_ID")]
        parent: Option<Vec<String>>,

        /// Print only id of folder
        #[arg(long, default_value_t = false)]
        print_only_id: bool,
    },

    /// Rename file/directory
    Rename {
        /// Id of file or directory
        file_id: String,

        /// New name
        name: String,
    },

    /// Move file/directory
    Move {
        /// Id of file or directory to move
        file_id: String,

        /// Id of folder to move to
        folder_id: String,
    },

    /// Copy file
    Copy {
        /// Id of file or directory to move
        file_id: String,

        /// Id of folder to copy to
        folder_id: String,
    },

    /// Import file as a google document/spreadsheet/presentation.
    /// Supported file types: doc, docx, odt, rtf, pdf, html, md, jpg, jpeg, gif, png (document), xls, xlsx, csv, tsv, ods (spreadsheet), ppt, pptx, odp (presentation)
    Import {
        /// Path to file
        file_path: PathBuf,

        /// Upload to an existing directory
        #[arg(short, long, value_name = "DIRECTORY_ID")]
        parent: Option<Vec<String>>,

        /// Print only id of file
        #[arg(long, default_value_t = false)]
        print_only_id: bool,

        /// Set chunk size in MB, must be a power of two.
        #[arg(long, value_name = "1|2|4|8|16|32|64|128|256|512|1024|4096|8192", default_value_t = ChunkSize::default())]
        chunk_size: ChunkSize,

        /// Upload files of at most this many MB in a single request; larger files are uploaded in chunks and can be resumed. 0 uploads every non-empty file in chunks
        #[arg(long, value_name = "MB", default_value_t = MultipartThreshold::default())]
        multipart_threshold: MultipartThreshold,
        #[command(flatten)]
        progress: ProgressArgs,
    },

    /// Export google document to file
    Export {
        /// File id
        file_id: String,

        /// File path to export to. The file extension will determine the export format. Documents: pdf, odt, docx, epub, rtf, txt, html, md. Spreadsheets: csv, tsv, ods, xlsx, pdf. Presentations: pdf, pptx, odp, txt. Drawings: pdf, jpg (or jpeg), png, svg
        file_path: PathBuf,

        /// Overwrite existing files
        #[arg(short, long)]
        overwrite: bool,

        /// Export to this format, a file extension such as pdf or md, instead of the one the extension of the file path names. The path is then used as given
        #[arg(long, value_name = "EXTENSION", value_parser = parse_export_format)]
        format: Option<FileExtension>,
        #[command(flatten)]
        progress: ProgressArgs,
    },
}

/// `--progress` and `--no-progress` of the commands that transfer files.
#[derive(clap::Args, Debug, Clone, Copy, PartialEq, Eq)]
struct ProgressArgs {
    /// Show transfer progress on stderr, also when stderr is not a terminal. By default it shows only on a terminal; `GDRIVE_PROGRESS=1` or `GDRIVE_PROGRESS=0` changes the default
    #[arg(long, overrides_with = "no_progress")]
    progress: bool,

    /// Do not show transfer progress
    #[arg(long, overrides_with = "progress")]
    no_progress: bool,
}

impl ProgressArgs {
    /// The mode of the flags, else of `GDRIVE_PROGRESS`.
    fn mode(self) -> transfer::progress::Mode {
        transfer::progress::Mode::from_flags(self.progress, self.no_progress)
    }
}

#[derive(Subcommand)]
enum SyncCommand {
    /// Make the Drive folder like the local directory: upload new and changed files
    ///
    /// Files are compared by size and md5 (every local file whose size matches is read).
    /// Symlinks are followed, as by `files upload --recursive`. Google Docs files, shortcuts and
    /// names that are ambiguous (the same when letter case and Unicode normalization are
    /// ignored) are skipped and never deleted.
    Up {
        /// The local directory whose contents are synced
        local_dir: PathBuf,

        /// Id of the Drive folder to sync into
        folder_id: String,

        #[command(flatten)]
        args: SyncArgs,
    },

    /// Make the local directory like the Drive folder: download new and changed files
    ///
    /// A file whose size and modification time match Drive's is unchanged without being read
    /// (downloads set the time); otherwise files of the same size are compared by md5. Local
    /// symlinks are never written through or deleted. Google Docs files are skipped unless
    /// --export is given, and local files with their names are never deleted.
    Down {
        /// Id of the Drive folder whose contents are synced
        folder_id: String,

        /// The local directory to sync into; created if it does not exist
        local_dir: PathBuf,

        #[command(flatten)]
        args: SyncArgs,

        /// With --delete: remove local extras for good instead of moving them to LOCAL_DIR/.gdrive-trash/
        #[arg(long, requires = "delete")]
        delete_permanently: bool,

        /// Export Google Docs, Sheets, Slides and Drawings, as `files download --recursive --export` does. SPEC is a profile, `default` (docs=docx, sheets=xlsx, slides=pptx, drawings=png) or `md`, and/or TYPE=FORMAT overrides separated by commas, for example `md,sheets=ods` or `docs=pdf,drawings=skip`
        #[arg(long, value_name = "SPEC", value_parser = ExportFormats::parse)]
        export: Option<ExportFormats>,

        /// Download files that Google flagged as malware or spam. Only the owner of a file, or an organizer of its shared drive, can do this
        #[arg(long)]
        acknowledge_abuse: bool,
    },
}

/// The options of both directions of `files sync`.
#[derive(clap::Args)]
struct SyncArgs {
    /// Print what would be done, one line per item (create, update, unchanged, extra, extra->trash, skip:REASON), and change nothing
    #[arg(long)]
    dry_run: bool,

    /// Also remove what exists only on the destination, once everything was transferred and both sides were listed completely: Drive files go to the Drive trash, local files to LOCAL_DIR/.gdrive-trash/<time>/
    #[arg(long)]
    delete: bool,

    /// Transfer up to N files at once (1 to 64)
    #[arg(long, value_name = "N", default_value_t = Options::DEFAULT_JOBS, value_parser = parse_jobs)]
    jobs: NonZeroUsize,

    /// When a file fails, go on with the others instead of stopping. Nothing is deleted after a failure, and the command still exits with an error
    #[arg(long)]
    continue_on_error: bool,

    #[command(flatten)]
    progress: ProgressArgs,
}

#[derive(Subcommand)]
enum PermissionCommand {
    /// Grant permission to file
    Share {
        /// File id
        file_id: String,

        /// The role granted by this permission. Allowed values are: owner, organizer, fileOrganizer, writer, commenter, reader
        #[arg(long, default_value_t = permission::Role::default())]
        role: permission::Role,

        /// The type of the grantee. Valid values are: user, group, domain, anyone
        #[arg(long, default_value_t = permission::Type::default())]
        type_: permission::Type,

        /// Email address. Required for user and group type
        #[arg(long)]
        email: Option<String>,

        /// Domain. Required for domain type
        #[arg(long)]
        domain: Option<String>,

        /// Whether the permission allows the file to be discovered through search. This is only applicable for permissions of type domain or anyone
        #[arg(long)]
        discoverable: bool,
    },

    /// List permissions for a file
    List {
        /// File id
        file_id: String,

        /// Don't print header
        #[arg(long)]
        skip_header: bool,

        /// Field separator
        #[arg(long, default_value_t = String::from("\t"))]
        field_separator: String,
    },

    /// Revoke permissions for a file. If no other options are specified, the 'anyone' permission will be revoked
    Revoke {
        /// File id
        file_id: String,

        /// Revoke all permissions (except owner)
        #[arg(long)]
        all: bool,

        /// Revoke specific permission
        #[arg(long, value_name = "PERMISSION_ID")]
        id: Option<String>,
    },
}

/// `--jobs`: 1 to [`Options::MAX_JOBS`].
/// A `--format` of `files export`: a known file extension, without the dot.
fn parse_export_format(value: &str) -> Result<FileExtension, String> {
    FileExtension::from_name(&value.to_ascii_lowercase()).ok_or_else(|| {
        format!(
            "unknown format '{value}', use a file extension such as pdf, docx, xlsx, pptx, md or \
            png"
        )
    })
}

fn parse_jobs(value: &str) -> Result<NonZeroUsize, String> {
    let jobs = value.parse::<usize>().map_err(|err| err.to_string())?;
    NonZeroUsize::new(jobs)
        .filter(|jobs| jobs.get() <= Options::MAX_JOBS)
        .ok_or_else(|| format!("must be between 1 and {}", Options::MAX_JOBS))
}

/// The configuration of a `files sync` command line.
fn sync_config(command: SyncCommand) -> files::sync::Config {
    use files::sync::{Config, DeleteMode, Direction};

    let (direction, local_dir, folder_id, args, permanently, export, acknowledge_abuse) =
        match command {
            SyncCommand::Up {
                local_dir,
                folder_id,
                args,
            } => (
                Direction::Up,
                local_dir,
                folder_id,
                args,
                false,
                None,
                false,
            ),
            SyncCommand::Down {
                folder_id,
                local_dir,
                args,
                delete_permanently,
                export,
                acknowledge_abuse,
            } => (
                Direction::Down,
                local_dir,
                folder_id,
                args,
                delete_permanently,
                export,
                acknowledge_abuse,
            ),
        };
    let delete = match (args.delete, permanently) {
        (false, _) => None,
        (true, false) => Some(DeleteMode::Trash),
        (true, true) => Some(DeleteMode::Permanently),
    };
    Config {
        direction,
        local_dir,
        folder_id,
        dry_run: args.dry_run,
        delete,
        export,
        acknowledge_abuse,
        transfer: Options::new(args.jobs, args.continue_on_error),
        progress: args.progress.mode(),
        upload: transfer::upload::UploadOptions {
            session_dir: transfer::upload::session::account_session_dir(),
            session_identity: transfer::upload::session::account_session_identity(),
            ..transfer::upload::UploadOptions::default()
        },
    }
}

/// The stack of the thread that runs the commands. The future of [`run`] holds the state of
/// every command at once (the largest one sets its size), which in a debug build is more than
/// the 1 MiB that the main thread gets on Windows; a thread of this size has room for it and
/// for growth.
const MAIN_STACK_SIZE: usize = 32 * 1024 * 1024;

fn main() -> ExitCode {
    let thread = std::thread::Builder::new()
        .name("gdrive".to_owned())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(|| {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("Error: unable to start the async runtime: {err}");
                    return ExitCode::FAILURE;
                }
            };
            runtime.block_on(async_main())
        });
    match thread {
        Ok(handle) => match handle.join() {
            Ok(code) => code,
            // The thread panicked (its message is printed already): end as a panic in main does.
            Err(panic) => std::panic::resume_unwind(panic),
        },
        Err(err) => {
            eprintln!("Error: unable to start the main thread: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main() -> ExitCode {
    if let Err(err) = run().await {
        eprintln!("{}", failure_message(err.as_ref()));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// What gdrive prints for a failed command, and for each failed file of a recursive transfer:
/// the [`error_message`], and the hint for the current account (a service account's storage,
/// for example) when there is one.
fn failure_message(err: &(dyn std::error::Error + 'static)) -> String {
    failure_message_with(err, common::hub_helper::account_hint)
}

/// [`failure_message`] with the account hint of `hint`.
fn failure_message_with(
    err: &(dyn std::error::Error + 'static),
    hint: impl FnOnce(&(dyn std::error::Error + 'static)) -> Option<String>,
) -> String {
    let mut message = error_message(err);
    if let Some(hint) = hint(err) {
        message.push('\n');
        message.push_str(&hint);
    }
    message
}

/// The message printed for a failed command, in the format of gdrive 3.x: `Error: ` and the
/// message of the error, followed by the messages of its sources separated by `: `. Hints that tell
/// the user which command to run next end the innermost message, on lines of their own.
///
/// When the account has to be authorized again, only that error is printed: it names the command
/// to run, and the layers around it add nothing. A message that the one before it contains already
/// is left out: hyper's and yup-oauth2's errors put the message of their source into their own.
fn error_message(err: &(dyn std::error::Error + 'static)) -> String {
    if let Some(reauth) = auth::find_reauth_required(err) {
        return format!("Error: {reauth}");
    }

    let mut previous = err.to_string().trim_end().to_owned();
    let mut message = format!("Error: {previous}");
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string().trim_end().to_owned();
        if !previous.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        previous = text;
        source = cause.source();
    }
    message
}

async fn run() -> Result<(), Box<dyn std::error::Error + 'static>> {
    let cli = Cli::parse();

    match cli.command {
        Command::About => {
            about::about();
        }

        Command::Account { command } => {
            handle_account_command(command).await?;
        }

        Command::Drives { command } => match command {
            DriveCommand::List {
                max,
                skip_header,
                field_separator,
            } => {
                drives::list(drives::list::Config {
                    skip_header,
                    field_separator,
                    max_drives: max,
                })
                .await?;
            }
        },

        Command::Files { command } => {
            handle_files_command(command).await?;
        }

        Command::Permissions { command } => {
            handle_permissions_command(command).await?;
        }

        Command::Completions { shell } => {
            completions::completions(shell, <Cli as clap::CommandFactory>::command())?;
        }

        Command::Version => {
            version::version();
        }
    }

    Ok(())
}

async fn handle_permissions_command(
    command: PermissionCommand,
) -> Result<(), Box<dyn std::error::Error + 'static>> {
    match command {
        PermissionCommand::Share {
            file_id,
            role,
            type_,
            discoverable,
            email,
            domain,
        } => {
            permissions::share(permissions::share::Config {
                file_id,
                role,
                type_,
                discoverable,
                email,
                domain,
            })
            .await?;
        }

        PermissionCommand::List {
            file_id,
            skip_header,
            field_separator,
        } => {
            permissions::list(permissions::list::Config {
                file_id,
                skip_header,
                field_separator,
            })
            .await?;
        }

        PermissionCommand::Revoke { file_id, all, id } => {
            let action = if all {
                permissions::revoke::RevokeAction::AllExceptOwner
            } else if id.is_some() {
                permissions::revoke::RevokeAction::Id(id.unwrap_or_default())
            } else {
                permissions::revoke::RevokeAction::Anyone
            };

            permissions::revoke(permissions::revoke::Config { file_id, action }).await?;
        }
    }

    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "FileCommand has many variants, pretty big match statement"
)]
async fn handle_files_command(
    command: FileCommand,
) -> Result<(), Box<dyn std::error::Error + 'static>> {
    match command {
        FileCommand::Info {
            file_id,
            size_in_bytes,
        } => {
            files::info(files::info::Config {
                file_id,
                size_in_bytes,
            })
            .await?;
        }

        FileCommand::List {
            max,
            query,
            order_by,
            parent,
            drive,
            skip_header,
            full_name,
            field_separator,
            tsv,
            json,
        } => {
            let output = if json {
                files::list::Output::Json
            } else if tsv {
                files::list::Output::Tsv { skip_header }
            } else {
                files::list::Output::Table {
                    skip_header,
                    truncate_name: !full_name,
                    field_separator,
                }
            };

            files::list(files::list::Config {
                query: ListQuery {
                    query,
                    parent,
                    drive,
                },
                order_by,
                max_files: max,
                output,
            })
            .await?;
        }

        FileCommand::Download {
            file_id,
            overwrite,
            follow_shortcuts,
            recursive,
            destination,
            stdout,
            acknowledge_abuse,
            jobs,
            continue_on_error,
            export,
            progress,
        } => {
            let existing_file_action = if overwrite {
                files::download::ExistingFileAction::Overwrite
            } else {
                files::download::ExistingFileAction::Abort
            };

            let dst = if stdout {
                files::download::Destination::Stdout
            } else if let Some(path) = destination {
                files::download::Destination::Path(path)
            } else {
                files::download::Destination::CurrentDir
            };

            files::download(files::download::Config {
                file_id,
                existing_file_action,
                follow_shortcuts,
                download_directories: recursive,
                destination: dst,
                acknowledge_abuse,
                transfer: Options::new(jobs, continue_on_error),
                export,
                progress: progress.mode(),
            })
            .await?;
        }

        FileCommand::Upload {
            file_path,
            name,
            mime,
            parent,
            recursive,
            chunk_size,
            multipart_threshold,
            print_chunk_errors,
            print_chunk_info,
            print_only_id,
            jobs,
            continue_on_error,
            progress,
        } => {
            files::upload(files::upload::Config {
                file_path,
                name,
                mime_type: mime,
                parents: parent,
                chunk_size,
                multipart_threshold,
                print_chunk_errors,
                print_chunk_info,
                upload_directories: recursive,
                print_only_id,
                transfer: Options::new(jobs, continue_on_error),
                progress: progress.mode(),
            })
            .await?;
        }

        FileCommand::Sync { command } => {
            files::sync(sync_config(command)).await?;
        }

        FileCommand::Migrate {
            file_id,
            to,
            from,
            into,
            jobs,
            continue_on_error,
            dry_run,
            trash_source,
            chunk_size,
            multipart_threshold,
            print_chunk_errors,
            print_chunk_info,
            progress,
        } => {
            files::migrate(files::migrate::Config {
                file_ids: file_id,
                from,
                to,
                into,
                dry_run,
                trash_source,
                chunk_size,
                multipart_threshold,
                print_chunk_errors,
                print_chunk_info,
                transfer: Options::new(jobs, continue_on_error),
                progress: progress.mode(),
            })
            .await?;
        }

        FileCommand::Activity {
            file_id,
            account,
            since,
            until,
            by,
            exclude,
            active_only,
            scan,
            jobs,
            skip_header,
            field_separator,
            tsv,
        } => {
            let output = if tsv {
                files::activity::Output::Tsv { skip_header }
            } else {
                files::activity::Output::Table {
                    skip_header,
                    field_separator,
                }
            };
            // Boxed: the future of run() holds the state of every command at once.
            Box::pin(files::activity(files::activity::Config {
                file_ids: file_id,
                account,
                since,
                until,
                by,
                exclude,
                active_only,
                scan,
                jobs,
                output,
            }))
            .await?;
        }

        FileCommand::Update {
            file_id,
            file_path,
            name,
            mime,
            chunk_size,
            multipart_threshold,
            print_chunk_errors,
            print_chunk_info,
            progress,
        } => {
            files::update(files::update::Config {
                file_id,
                file_path,
                name,
                mime_type: mime,
                chunk_size,
                multipart_threshold,
                print_chunk_errors,
                print_chunk_info,
                progress: progress.mode(),
            })
            .await?;
        }

        FileCommand::Delete { file_id, recursive } => {
            files::delete(files::delete::Config {
                file_ids: file_id,
                delete_directories: recursive,
            })
            .await?;
        }

        FileCommand::Trash { file_id, recursive } => {
            files::trash(files::trash::Config {
                file_ids: file_id,
                action: files::trash::Action::Trash,
                trash_directories: recursive,
            })
            .await?;
        }

        FileCommand::Untrash { file_id } => {
            files::trash(files::trash::Config {
                file_ids: file_id,
                action: files::trash::Action::Untrash,
                trash_directories: false,
            })
            .await?;
        }

        FileCommand::Mkdir {
            name,
            parent,
            print_only_id,
        } => {
            files::mkdir(files::mkdir::Config {
                id: None,
                name,
                parents: parent,
                print_only_id,
            })
            .await?;
        }

        FileCommand::Rename { file_id, name } => {
            files::rename(files::rename::Config { file_id, name }).await?;
        }

        FileCommand::Move { file_id, folder_id } => {
            files::mv(files::mv::Config {
                file_id,
                to_folder_id: folder_id,
            })
            .await?;
        }

        FileCommand::Copy { file_id, folder_id } => {
            files::copy(files::copy::Config {
                file_id,
                to_folder_id: folder_id,
            })
            .await?;
        }

        FileCommand::Import {
            file_path,
            parent,
            print_only_id,
            chunk_size,
            multipart_threshold,
            progress,
        } => {
            files::import(files::import::Config {
                file_path,
                parents: parent,
                print_only_id,
                chunk_size,
                multipart_threshold,
                progress: progress.mode(),
            })
            .await?;
        }

        FileCommand::Export {
            file_id,
            file_path,
            overwrite,
            format,
            progress,
        } => {
            let existing_file_action = if overwrite {
                files::export::ExistingFileAction::Overwrite
            } else {
                files::export::ExistingFileAction::Abort
            };

            files::export(files::export::Config {
                file_id,
                file_path,
                existing_file_action,
                format,
                progress: progress.mode(),
            })
            .await?;
        }
    }

    Ok(())
}

async fn handle_account_command(
    command: AccountCommand,
) -> Result<(), Box<dyn std::error::Error + 'static>> {
    match command {
        AccountCommand::Add { login, service } => {
            if service.adc {
                account::add_adc(service.name).await?;
            } else if let Some(key_path) = service.service_account {
                account::add_service_account(&account::add_service::ServiceAccountConfig {
                    key_path,
                    subject: service.subject,
                    name: service.name,
                })
                .await?;
            } else {
                account::add(&account::add::Config {
                    mode: login.mode(),
                    port: login.port,
                })
                .await?;
            }
        }

        AccountCommand::List => {
            account::list()?;
        }

        AccountCommand::Current => {
            account::current()?;
        }

        AccountCommand::Switch { account_name } => {
            account::switch(&account::switch::Config { account_name })?;
        }

        AccountCommand::Reauth {
            account_name,
            login,
        } => {
            account::reauth(&account::reauth::Config {
                account_name,
                mode: login.mode(),
                port: login.port,
            })
            .await?;
        }

        AccountCommand::Remove { account_name } => {
            account::remove(&account::remove::Config { account_name })?;
        }

        AccountCommand::Export { account_name } => {
            account::export(&account::export::Config { account_name })?;
        }

        AccountCommand::Import { file_path } => {
            account::import(&account::import::Config {
                archive_path: file_path,
            })?;
        }
    }

    Ok(())
}

/// Parses a count that must be at least 1.
fn parse_positive_usize(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(String::from("expected a positive integer")),
    }
}

#[cfg(test)]
mod error_message_tests {
    use super::*;
    use crate::{
        app_config::errors::{LoadAccountConfig, LoadCurrentAccount},
        common::hub_helper::GetHubError,
    };

    #[test]
    fn error_message_starts_with_error_and_ends_with_the_hint() {
        let err = GetHubError::LoadCurrentAccount(LoadCurrentAccount::LoadAccountConfig(
            LoadAccountConfig::AccountConfigMissing,
        ));

        assert_eq!(
            error_message(&err),
            "Error: unable to load current account: unable to load account config: \
             no account has been selected\n\
             Use `gdrive account list` to show all accounts.\n\
             Use `gdrive account switch` to select an account."
        );
    }

    #[test]
    fn error_message_leaves_out_messages_that_were_shown_already() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|layer| layer as _)
            }
        }
        let layer = |text, inner| Some(Box::new(Layer(text, inner)));

        // As yup-oauth2 and hyper's client nest a connection error.
        let err = Layer(
            "unable to refresh the access token",
            layer(
                "Connection failure: Hyper error: client error (Connect)",
                layer(
                    "Hyper error: client error (Connect)",
                    layer(
                        "client error (Connect)",
                        layer("tcp connect error", layer("Connection refused", None)),
                    ),
                ),
            ),
        );
        assert_eq!(
            error_message(&err),
            "Error: unable to refresh the access token: Connection failure: Hyper error: client \
             error (Connect): tcp connect error: Connection refused"
        );
    }

    #[test]
    fn error_message_trims_trailing_newlines_of_messages() {
        let err = google_drive3::Error::Cancelled;

        assert_eq!(
            error_message(&err),
            "Error: Operation cancelled by delegate"
        );
    }

    #[test]
    fn account_errors_keep_the_hints_of_gdrive_3() {
        assert_eq!(
            error_message(&account::list::Error::NoAccounts),
            "Error: no accounts found\nUse `gdrive account add` to add an account."
        );
        assert_eq!(
            error_message(&account::current::Error::NoAccounts),
            "Error: no accounts found\nUse `gdrive account add` to add an account."
        );
        assert_eq!(
            error_message(&account::switch::Error::AccountNotFound(String::from(
                "foo"
            ))),
            "Error: account 'foo' not found"
        );
    }

    #[test]
    fn error_message_prints_only_the_reauth_error_when_there_is_one() {
        let reauth = auth::ReauthRequired {
            account: String::from("me@example.com"),
            reason: auth::ReauthReason::NoStoredToken,
        };
        let expected = format!("Error: {reauth}");
        let err = google_drive3::Error::MissingToken(Box::new(reauth));

        assert_eq!(error_message(&err), expected);
        assert!(
            expected.contains("gdrive account reauth me@example.com"),
            "{expected}"
        );
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{
        parse_jobs, parse_positive_usize, AccountCommand, Cli, Command, DriveCommand,
        ExportFormats, FileCommand, PermissionCommand,
    };
    use crate::files::list::ListQuery;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse_files_list(args: &[&str]) -> ListQuery {
        let cli = Cli::try_parse_from(["gdrive", "files", "list"].iter().chain(args))
            .expect("arguments should parse");
        let Command::Files {
            command:
                FileCommand::List {
                    query,
                    parent,
                    drive,
                    ..
                },
        } = cli.command
        else {
            panic!("expected files list");
        };
        ListQuery {
            query,
            parent,
            drive,
        }
    }

    #[test]
    fn files_list_keeps_default_query_without_flags() {
        let query = parse_files_list(&[]);
        assert_eq!(query.q(), "'root' in parents and trashed = false");
        assert_eq!(query.drive_id(), None);
    }

    #[test]
    fn files_list_query_is_not_dropped_by_parent_or_drive() {
        let query = parse_files_list(&["--query", "name = 'a'", "--parent", "P"]);
        assert_eq!(
            query.q(),
            "'P' in parents and trashed = false and (name = 'a')"
        );

        let query = parse_files_list(&["--drive", "D", "--query", "name = 'a'"]);
        assert_eq!(query.q(), "trashed = false and (name = 'a')");
        assert_eq!(query.drive_id(), Some("D"));
    }

    #[test]
    fn positive_usize_rejects_zero_and_non_numbers() {
        assert_eq!(parse_positive_usize("1"), Ok(1));
        assert_eq!(parse_positive_usize("250"), Ok(250));
        for invalid in ["0", "-1", "", "ten", "1.5"] {
            assert!(parse_positive_usize(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn jobs_are_1_to_64_and_default_to_4() {
        assert_eq!(parse_jobs("1").map(std::num::NonZeroUsize::get), Ok(1));
        assert_eq!(parse_jobs("64").map(std::num::NonZeroUsize::get), Ok(64));
        for invalid in ["0", "65", "-1", "", "four"] {
            assert!(parse_jobs(invalid).is_err(), "{invalid:?}");
        }

        let download = |args: &[&str]| {
            let cli = Cli::try_parse_from(["gdrive", "files", "download", "id"].iter().chain(args))
                .expect("valid flags");
            let Command::Files {
                command:
                    FileCommand::Download {
                        jobs,
                        continue_on_error,
                        ..
                    },
            } = cli.command
            else {
                panic!("expected files download");
            };
            (jobs.get(), continue_on_error)
        };
        assert_eq!(download(&["-r"]), (4, false));
        assert_eq!(
            download(&["-r", "--jobs", "8", "--continue-on-error"]),
            (8, true)
        );

        let cli = Cli::try_parse_from([
            "gdrive",
            "files",
            "upload",
            "dir",
            "-r",
            "--jobs",
            "2",
            "--continue-on-error",
        ])
        .expect("valid flags");
        let Command::Files {
            command:
                FileCommand::Upload {
                    jobs,
                    continue_on_error,
                    ..
                },
        } = cli.command
        else {
            panic!("expected files upload");
        };
        assert_eq!((jobs.get(), continue_on_error), (2, true));
    }

    #[test]
    fn export_takes_a_spec_and_needs_recursive() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(["gdrive", "files", "download", "id"].iter().chain(args))
        };
        let export = |args: &[&str]| {
            let Command::Files {
                command: FileCommand::Download { export, .. },
            } = parse(args).expect("valid flags").command
            else {
                panic!("expected files download");
            };
            export
        };

        assert_eq!(export(&["-r"]), None);
        assert_eq!(
            export(&["-r", "--export", "default"]),
            Some(ExportFormats::DEFAULT)
        );
        assert_eq!(
            export(&["-r", "--export", "md"]),
            Some(ExportFormats::MARKDOWN)
        );
        let error = parse(&["--export", "default"]).err().unwrap();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        let error = parse(&["-r", "--export", "sheets=md"])
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("sheets cannot be exported as 'md'"),
            "{error}"
        );
    }

    #[test]
    fn drives_list_max_defaults_to_all() {
        let parse_max = |args: &[&str]| {
            let cli = Cli::try_parse_from(["gdrive", "drives", "list"].iter().chain(args))?;
            let Command::Drives {
                command: DriveCommand::List { max, .. },
            } = cli.command
            else {
                panic!("expected drives list");
            };
            Ok::<_, clap::Error>(max)
        };

        assert_eq!(parse_max(&[]).expect("no flags"), None);
        assert_eq!(parse_max(&["--max", "250"]).expect("--max 250"), Some(250));
        assert!(parse_max(&["--max", "0"]).is_err());
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once(&"gdrive").chain(args))
    }

    #[test]
    fn account_add_takes_a_service_account_key_with_subject_and_name() {
        let cli = parse(&[
            "account",
            "add",
            "--service-account",
            "key.json",
            "--subject",
            "user@example.com",
            "--name",
            "robot",
        ])
        .expect("arguments should parse");
        let Command::Account {
            command: AccountCommand::Add { login, service },
        } = cli.command
        else {
            panic!("expected account add");
        };
        assert_eq!(
            service.service_account.as_deref(),
            Some(std::path::Path::new("key.json"))
        );
        assert_eq!(service.subject.as_deref(), Some("user@example.com"));
        assert_eq!(service.name.as_deref(), Some("robot"));
        assert!(!login.manual);

        let cli = parse(&["account", "add", "--adc", "--name", "ci"]).expect("--adc --name");
        let Command::Account {
            command: AccountCommand::Add { service, .. },
        } = cli.command
        else {
            panic!("expected account add");
        };
        assert!(service.adc);
        assert_eq!(service.name.as_deref(), Some("ci"));

        // The OAuth login still parses as before.
        assert!(parse(&["account", "add"]).is_ok());
        assert!(parse(&["account", "add", "--manual", "--port", "9000"]).is_ok());
    }

    #[test]
    fn service_account_flags_do_not_mix_with_the_oauth_login() {
        for args in [
            &["account", "add", "--subject", "user@example.com"][..],
            &["account", "add", "--name", "robot"],
            &[
                "account",
                "add",
                "--service-account",
                "key.json",
                "--manual",
            ],
            &[
                "account",
                "add",
                "--service-account",
                "key.json",
                "--port",
                "9000",
            ],
            &["account", "add", "--service-account", "key.json", "--adc"],
            &["account", "add", "--adc", "--subject", "user@example.com"],
            &["account", "add", "--adc", "--manual"],
        ] {
            assert!(parse(args).is_err(), "{args:?} should not parse");
        }
    }

    fn files_list_flags(args: &[&str]) -> (String, bool, bool) {
        let cli = parse(&[&["files", "list"], args].concat()).expect("arguments should parse");
        let Command::Files {
            command:
                FileCommand::List {
                    field_separator,
                    tsv,
                    json,
                    ..
                },
        } = cli.command
        else {
            panic!("expected files list");
        };
        (field_separator, tsv, json)
    }

    /// 3.9 compatibility: the separator is used as given, so the two characters `\t` stay two
    /// characters; real tabs come from --tsv.
    #[test]
    fn field_separator_is_taken_literally() {
        assert_eq!(files_list_flags(&[]).0, "\t");
        assert_eq!(files_list_flags(&["--field-separator", r"\t"]).0, r"\t");
        assert_eq!(files_list_flags(&["--field-separator", "\t"]).0, "\t");
        assert_eq!(files_list_flags(&["--field-separator", ","]).0, ",");

        let cli = parse(&["drives", "list", "--field-separator", r"\t"]).expect("parse");
        let Command::Drives {
            command: DriveCommand::List {
                field_separator, ..
            },
        } = cli.command
        else {
            panic!("expected drives list");
        };
        assert_eq!(field_separator, r"\t");

        let cli = parse(&["permissions", "list", "F", "--field-separator", r"\t"]).expect("parse");
        let Command::Permissions {
            command: PermissionCommand::List {
                field_separator, ..
            },
        } = cli.command
        else {
            panic!("expected permissions list");
        };
        assert_eq!(field_separator, r"\t");
    }

    #[test]
    fn files_list_output_flags() {
        assert_eq!(
            files_list_flags(&["--tsv"]),
            (String::from("\t"), true, false)
        );
        assert_eq!(
            files_list_flags(&["--json"]),
            (String::from("\t"), false, true)
        );
        assert!(parse(&["files", "list", "--tsv", "--skip-header", "--full-name"]).is_ok());
        assert!(parse(&["files", "list", "--json", "--full-name", "--max", "5"]).is_ok());

        for conflicting in [
            &["--tsv", "--field-separator", ","][..],
            &["--json", "--tsv"],
            &["--json", "--field-separator", ","],
            &["--json", "--skip-header"],
        ] {
            let err = parse(&[&["files", "list"], conflicting].concat())
                .err()
                .unwrap_or_else(|| panic!("{conflicting:?} should be rejected"));
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }
}

#[cfg(test)]
mod flag_tests {
    use std::collections::HashSet;

    use clap::CommandFactory;

    use std::path::PathBuf;

    use clap::Parser;

    use super::{Cli, Command, FileCommand};
    use crate::common::drive_file::{DocType, FileExtension};

    fn words(text: &str) -> HashSet<&str> {
        text.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect()
    }

    fn files_subcommand(name: &str) -> clap::Command {
        Cli::command()
            .find_subcommand("files")
            .and_then(|files| files.find_subcommand(name))
            .cloned()
            .unwrap_or_else(|| panic!("files {name} should exist"))
    }

    #[test]
    fn cli_definition_is_valid() {
        // Catches duplicated short or long flags, which clap only reports at runtime.
        Cli::command().debug_assert();
    }

    fn all_commands(command: &clap::Command) -> Vec<&clap::Command> {
        let mut out = vec![command];
        for sub in command.get_subcommands() {
            out.extend(all_commands(sub));
        }
        out
    }

    #[test]
    fn short_s_is_reserved() {
        // -s is kept free for a future sync flag.
        let cli = Cli::command();
        for command in all_commands(&cli) {
            for arg in command.get_arguments() {
                assert_ne!(
                    arg.get_short(),
                    Some('s'),
                    "{} {:?} uses -s",
                    command.get_name(),
                    arg.get_id(),
                );
            }
        }
    }

    fn parse(args: &[&str]) -> FileCommand {
        let cli = Cli::try_parse_from(std::iter::once("gdrive").chain(args.iter().copied()))
            .unwrap_or_else(|err| panic!("{args:?}: {err}"));
        match cli.command {
            Command::Files { command } => command,
            _ => panic!("{args:?} should be a files command"),
        }
    }

    #[test]
    fn transfer_commands_take_progress_flags_and_the_last_one_wins() {
        let transfers: [&[&str]; 5] = [
            &["files", "download", "id"],
            &["files", "upload", "path"],
            &["files", "update", "id", "path"],
            &["files", "import", "a.md"],
            &["files", "export", "id", "a.pdf"],
        ];
        for command in transfers {
            let progress = |flags: &[&str]| {
                let args = command.iter().chain(flags).copied().collect::<Vec<_>>();
                match parse(&args) {
                    FileCommand::Download { progress, .. }
                    | FileCommand::Upload { progress, .. }
                    | FileCommand::Update { progress, .. }
                    | FileCommand::Import { progress, .. }
                    | FileCommand::Export { progress, .. } => {
                        (progress.progress, progress.no_progress)
                    }
                    _ => panic!("{args:?} should be a transfer"),
                }
            };
            assert_eq!(progress(&[]), (false, false), "{command:?}");
            assert_eq!(progress(&["--progress"]), (true, false), "{command:?}");
            assert_eq!(progress(&["--no-progress"]), (false, true), "{command:?}");
            assert_eq!(progress(&["--progress", "--no-progress"]), (false, true));
            assert_eq!(progress(&["--no-progress", "--progress"]), (true, false));
        }
    }

    #[test]
    fn short_flags_parse() {
        let FileCommand::List {
            max, query, parent, ..
        } = parse(&["files", "list", "-m", "5", "-q", "name = 'a'", "-p", "dir"])
        else {
            panic!("expected list");
        };
        assert_eq!(max, 5);
        assert_eq!(query.as_deref(), Some("name = 'a'"));
        assert_eq!(parent.as_deref(), Some("dir"));

        let FileCommand::Download {
            overwrite,
            recursive,
            destination,
            ..
        } = parse(&["files", "download", "id", "-o", "-r", "-d", "out"])
        else {
            panic!("expected download");
        };
        assert!(overwrite && recursive);
        assert_eq!(destination, Some(PathBuf::from("out")));

        let FileCommand::Upload {
            parent, recursive, ..
        } = parse(&["files", "upload", "path", "-p", "dir", "-r"])
        else {
            panic!("expected upload");
        };
        assert_eq!(parent, Some(vec![String::from("dir")]));
        assert!(recursive);

        assert!(matches!(
            parse(&["files", "delete", "id", "-r"]),
            FileCommand::Delete {
                recursive: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["files", "trash", "id", "-r"]),
            FileCommand::Trash {
                recursive: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["files", "mkdir", "name", "-p", "dir"]),
            FileCommand::Mkdir {
                parent: Some(_),
                ..
            }
        ));
        assert!(matches!(
            parse(&["files", "import", "a.md", "-p", "dir"]),
            FileCommand::Import {
                parent: Some(_),
                ..
            }
        ));
        assert!(matches!(
            parse(&["files", "export", "id", "a.md", "-o"]),
            FileCommand::Export {
                overwrite: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["files", "export", "id", "notes", "--format", "MD"]),
            FileCommand::Export {
                format: Some(FileExtension::Md),
                ..
            }
        ));
        assert!(
            Cli::try_parse_from(["gdrive", "files", "export", "id", "x", "--format", "exe"])
                .is_err()
        );
    }

    #[test]
    fn delete_and_trash_take_several_ids() {
        let FileCommand::Delete { file_id, .. } = parse(&["files", "delete", "a", "b", "-r"])
        else {
            panic!("expected delete");
        };
        assert_eq!(file_id, ["a", "b"]);

        let FileCommand::Trash { file_id, .. } = parse(&["files", "trash", "a", "b"]) else {
            panic!("expected trash");
        };
        assert_eq!(file_id, ["a", "b"]);

        let FileCommand::Untrash { file_id } = parse(&["files", "untrash", "a", "b"]) else {
            panic!("expected untrash");
        };
        assert_eq!(file_id, ["a", "b"]);

        for command in ["delete", "trash", "untrash"] {
            assert!(
                Cli::try_parse_from(["gdrive", "files", command]).is_err(),
                "files {command} needs at least one id"
            );
        }
    }

    #[test]
    fn import_help_lists_every_supported_type() {
        let import = files_subcommand("import");
        let about = import.get_about().unwrap().to_string();
        let listed = words(&about);

        for extension in DocType::SUPPORTED_INPUT_TYPES {
            let extension = extension.to_string();
            assert!(listed.contains(extension.as_str()), "{extension}: {about}");
        }
    }

    #[test]
    fn export_help_lists_every_supported_type() {
        let export = files_subcommand("export");
        let file_path = export
            .get_positionals()
            .find(|arg| arg.get_id() == "file_path")
            .unwrap();
        let help = file_path.get_help().unwrap().to_string();
        let listed = words(&help);

        for doc_type in DocType::ALL {
            for extension in doc_type.supported_export_types() {
                let extension = extension.to_string();
                assert!(listed.contains(extension.as_str()), "{extension}: {help}");
            }
        }
    }

    #[test]
    fn import_takes_the_chunk_size_and_threshold_of_upload() {
        let FileCommand::Import {
            chunk_size,
            multipart_threshold,
            ..
        } = parse(&[
            "files",
            "import",
            "a.md",
            "--chunk-size",
            "1",
            "--multipart-threshold",
            "0",
        ])
        else {
            panic!("expected import");
        };
        assert_eq!(chunk_size.in_bytes(), 1 << 20);
        assert_eq!(multipart_threshold.in_bytes(), 0);

        // The same defaults as upload.
        let FileCommand::Import {
            chunk_size,
            multipart_threshold,
            ..
        } = parse(&["files", "import", "a.md"])
        else {
            panic!("expected import");
        };
        let FileCommand::Upload {
            chunk_size: upload_chunk_size,
            multipart_threshold: upload_multipart_threshold,
            ..
        } = parse(&["files", "upload", "a.md"])
        else {
            panic!("expected upload");
        };
        assert_eq!(chunk_size.in_bytes(), upload_chunk_size.in_bytes());
        assert_eq!(multipart_threshold, upload_multipart_threshold);

        // And the same parsing: what upload rejects, import rejects.
        for (flag, value) in [
            ("--chunk-size", "3"),
            ("--chunk-size", "0"),
            ("--multipart-threshold", "-1"),
            ("--multipart-threshold", "many"),
        ] {
            for command in ["upload", "import"] {
                let result = Cli::try_parse_from(["gdrive", "files", command, "a.md", flag, value]);
                assert!(result.is_err(), "files {command} {flag} {value}");
            }
        }
    }

    #[test]
    fn migrate_takes_ids_accounts_and_the_transfer_flags() {
        let FileCommand::Migrate {
            file_id,
            to,
            from,
            into,
            jobs,
            continue_on_error,
            dry_run,
            trash_source,
            chunk_size,
            multipart_threshold,
            print_chunk_errors,
            print_chunk_info,
            progress,
        } = parse(&[
            "files",
            "migrate",
            "a",
            "b",
            "--to",
            "new@example.com",
            "--from",
            "old@example.com",
            "--into",
            "folder",
            "--jobs",
            "8",
            "--continue-on-error",
            "--dry-run",
            "--trash-source",
            "--chunk-size",
            "1",
            "--multipart-threshold",
            "0",
            "--print-chunk-errors",
            "--print-chunk-info",
            "--progress",
            "--no-progress",
        ])
        else {
            panic!("expected migrate");
        };
        assert_eq!(file_id, ["a", "b"]);
        assert_eq!(to, "new@example.com");
        assert_eq!(from.as_deref(), Some("old@example.com"));
        assert_eq!(into.as_deref(), Some("folder"));
        assert_eq!(jobs.get(), 8);
        assert!(continue_on_error && dry_run && trash_source);
        assert_eq!(chunk_size.in_bytes(), 1 << 20);
        assert_eq!(multipart_threshold.in_bytes(), 0);
        assert!(print_chunk_errors && print_chunk_info);
        assert_eq!((progress.progress, progress.no_progress), (false, true));
    }

    #[test]
    fn migrate_defaults_to_the_current_account_and_my_drive() {
        let FileCommand::Migrate {
            from,
            into,
            jobs,
            continue_on_error,
            dry_run,
            trash_source,
            chunk_size,
            multipart_threshold,
            progress,
            ..
        } = parse(&["files", "migrate", "id", "--to", "new@example.com"])
        else {
            panic!("expected migrate");
        };
        assert_eq!((from, into), (None, None));
        assert_eq!(jobs.get(), 4);
        assert!(!continue_on_error && !dry_run && !trash_source);
        assert_eq!((progress.progress, progress.no_progress), (false, false));
        // The same defaults as upload.
        let FileCommand::Upload {
            chunk_size: upload_chunk_size,
            multipart_threshold: upload_multipart_threshold,
            ..
        } = parse(&["files", "upload", "a.md"])
        else {
            panic!("expected upload");
        };
        assert_eq!(chunk_size.in_bytes(), upload_chunk_size.in_bytes());
        assert_eq!(multipart_threshold, upload_multipart_threshold);

        for args in [
            &["files", "migrate", "id"][..],
            &["files", "migrate", "--to", "new@example.com"],
            &["files", "migrate", "id", "--to"],
            &["files", "migrate", "id", "--to", "x", "--jobs", "0"],
            &["files", "migrate", "id", "--to", "x", "--chunk-size", "3"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("gdrive").chain(args.iter().copied())).is_err(),
                "{args:?} should be rejected"
            );
        }
    }

    #[test]
    fn activity_takes_ids_the_time_range_and_the_report_flags() {
        use crate::files::activity::{when::When, By, Scan};

        let FileCommand::Activity {
            file_id,
            account,
            since,
            until,
            by,
            exclude,
            active_only,
            scan,
            jobs,
            skip_header,
            field_separator: _,
            tsv,
        } = parse(&[
            "files",
            "activity",
            "a",
            "b",
            "--account",
            "admin@example.com",
            "--since",
            "2026-10-01T08:00:00Z",
            "--until",
            "12h",
            "--by",
            "event",
            "--exclude",
            "migrator@example.com",
            "--exclude",
            "other@example.com",
            "--active-only",
            "--scan",
            "file",
            "--jobs",
            "8",
            "--skip-header",
            "--tsv",
        ])
        else {
            panic!("expected activity");
        };
        assert_eq!(file_id, ["a", "b"]);
        assert_eq!(account.as_deref(), Some("admin@example.com"));
        let october = chrono::DateTime::parse_from_rfc3339("2026-10-01T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(since, When::At(october));
        assert_eq!(until, Some(When::Ago(chrono::TimeDelta::hours(12))));
        assert_eq!(by, By::Event);
        assert_eq!(exclude, ["migrator@example.com", "other@example.com"]);
        assert!(active_only && skip_header && tsv);
        assert_eq!(scan, Scan::File);
        assert_eq!(jobs.get(), 8);

        for (value, expected) in [("user", By::User), ("file", By::File)] {
            let FileCommand::Activity { by, .. } =
                parse(&["files", "activity", "a", "--by", value])
            else {
                panic!("expected activity");
            };
            assert_eq!(by, expected);
        }
        for (value, expected) in [("domain", Scan::Domain), ("drive", Scan::Drive)] {
            let FileCommand::Activity { scan, .. } =
                parse(&["files", "activity", "a", "--scan", value])
            else {
                panic!("expected activity");
            };
            assert_eq!(scan, expected);
        }
    }

    #[test]
    fn activity_defaults_to_30_days_per_person_of_the_current_account() {
        use crate::files::activity::{when::When, By, Scan};

        let FileCommand::Activity {
            account,
            since,
            until,
            by,
            exclude,
            active_only,
            scan,
            jobs,
            skip_header,
            field_separator,
            tsv,
            ..
        } = parse(&["files", "activity", "id"])
        else {
            panic!("expected activity");
        };
        assert_eq!(account, None);
        assert_eq!(since, When::Ago(chrono::TimeDelta::days(30)));
        assert_eq!(until, None);
        assert_eq!((by, scan), (By::User, Scan::Domain));
        assert!(exclude.is_empty() && !active_only && !skip_header && !tsv);
        assert_eq!(jobs.get(), 4);
        assert_eq!(field_separator, "\t");

        for args in [
            &["files", "activity"][..],
            &["files", "activity", "id", "--since", "yesterday"],
            &["files", "activity", "id", "--until", "30m"],
            &["files", "activity", "id", "--jobs", "0"],
            &["files", "activity", "id", "--jobs", "65"],
            &["files", "activity", "id", "--by", "person"],
            &["files", "activity", "id", "--scan", "everything"],
            &["files", "activity", "id", "--scan", "auto"],
            &["files", "activity", "id", "--tsv", "--field-separator", ","],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("gdrive").chain(args.iter().copied())).is_err(),
                "{args:?} should be rejected"
            );
        }
        let error = Cli::try_parse_from(["gdrive", "files", "activity", "id", "--since", "soon"])
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("'soon' is not a time; use a duration"),
            "{error}"
        );
    }

    #[test]
    fn upload_and_update_take_a_name() {
        let FileCommand::Upload { name, .. } = parse(&["files", "upload", "--name", "a b.txt"])
        else {
            panic!("expected upload");
        };
        assert_eq!(name.as_deref(), Some("a b.txt"));
        let FileCommand::Upload { name, .. } = parse(&["files", "upload", "local.txt"]) else {
            panic!("expected upload");
        };
        assert_eq!(name, None);

        let FileCommand::Update { file_id, name, .. } =
            parse(&["files", "update", "id", "--name", "report"])
        else {
            panic!("expected update");
        };
        assert_eq!((file_id.as_str(), name.as_deref()), ("id", Some("report")));

        for args in [
            &["files", "upload", "dir", "--recursive", "--name", "x"][..],
            &["files", "upload", "--name", ""],
            &["files", "update", "id", "--name", ""],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("gdrive").chain(args.iter().copied())).is_err(),
                "{args:?} should be rejected"
            );
        }
    }
}
