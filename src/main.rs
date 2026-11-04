mod about;
mod account;
mod app_config;
mod common;
mod completions;
mod drives;
mod files;
mod hub;
mod permissions;
mod version;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use common::{delegate::ChunkSize, permission};
use error_trace::ErrorTrace;
use files::list::{ListQuery, ListSortOrder};
use mime::Mime;

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
    Add,

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

#[derive(Subcommand)]
enum DriveCommand {
    /// List drives
    List {
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

        /// Query. See <https://developers.google.com/drive/search-parameters>
        #[arg(short, long, default_value_t = ListQuery::default())]
        query: ListQuery,

        /// Order by. See <https://developers.google.com/drive/api/v3/reference/files/list>
        #[arg(long, default_value_t = ListSortOrder::default())]
        order_by: ListSortOrder,

        /// List files in a specific folder
        #[arg(short, long, value_name = "DIRECTORY_ID")]
        parent: Option<String>,

        /// List files on a shared drive
        #[arg(long, value_name = "DRIVE_ID")]
        drive: Option<String>,

        /// Don't print header
        #[arg(long)]
        skip_header: bool,

        /// Show full file name without truncating
        #[arg(long)]
        full_name: bool,

        /// Field separator
        #[arg(long, default_value_t = String::from("\t"))]
        field_separator: String,
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
    },

    /// Upload file
    Upload {
        /// Path of file to upload
        file_path: Option<PathBuf>,

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

        /// Print errors occuring during chunk upload
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_errors: bool,

        /// Print details about each chunk
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_info: bool,

        /// Print only id of file/folder
        #[arg(long, default_value_t = false)]
        print_only_id: bool,
    },

    /// Update file. This will create a new version of the file. The older versions will typically be kept for 30 days.
    Update {
        /// File id of the file you want ot update
        file_id: String,

        /// Path of file to upload
        file_path: Option<PathBuf>,

        /// Force mime type [default: auto-detect]
        #[arg(long, value_name = "MIME_TYPE")]
        mime: Option<Mime>,

        /// Set chunk size in MB, must be a power of two.
        #[arg(long, value_name = "1|2|4|8|16|32|64|128|256|512|1024|4096|8192", default_value_t = ChunkSize::default())]
        chunk_size: ChunkSize,

        /// Print errors occuring during chunk upload
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_errors: bool,

        /// Print details about each chunk
        #[arg(long, value_name = "", default_value_t = false)]
        print_chunk_info: bool,
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
    },

    /// Export google document to file
    Export {
        /// File id
        file_id: String,

        /// File path to export to. The file extension will determine the export format. Documents: pdf, odt, docx, epub, rtf, txt, html, md. Spreadsheets: csv, tsv, ods, xlsx, pdf. Presentations: pdf, pptx, odp, txt
        file_path: PathBuf,

        /// Overwrite existing files
        #[arg(short, long)]
        overwrite: bool,
    },
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

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(err) = run().await {
        eprintln!("{}", err.trace());
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
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
                skip_header,
                field_separator,
            } => {
                drives::list(drives::list::Config {
                    skip_header,
                    field_separator,
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
        } => {
            let parent_query = parent.map(|folder_id| ListQuery::FilesInFolder { folder_id });
            let drive_query = drive.map(|drive_id| ListQuery::FilesOnDrive { drive_id });
            let q = parent_query.or(drive_query).unwrap_or(query);

            files::list(files::list::Config {
                query: q,
                order_by,
                max_files: max,
                skip_header,
                truncate_name: !full_name,
                field_separator,
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
            })
            .await?;
        }

        FileCommand::Upload {
            file_path,
            mime,
            parent,
            recursive,
            chunk_size,
            print_chunk_errors,
            print_chunk_info,
            print_only_id,
        } => {
            files::upload(files::upload::Config {
                file_path,
                mime_type: mime,
                parents: parent,
                chunk_size,
                print_chunk_errors,
                print_chunk_info,
                upload_directories: recursive,
                print_only_id,
            })
            .await?;
        }

        FileCommand::Update {
            file_id,
            file_path,
            mime,
            chunk_size,
            print_chunk_errors,
            print_chunk_info,
        } => {
            files::update(files::update::Config {
                file_id,
                file_path,
                mime_type: mime,
                chunk_size,
                print_chunk_errors,
                print_chunk_info,
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
        } => {
            files::import(files::import::Config {
                file_path,
                parents: parent,
                print_only_id,
            })
            .await?;
        }

        FileCommand::Export {
            file_id,
            file_path,
            overwrite,
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
        AccountCommand::Add => {
            account::add().await?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use clap::CommandFactory;

    use std::path::PathBuf;

    use clap::Parser;

    use super::{Cli, Command, FileCommand};
    use crate::common::drive_file::DocType;

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
    fn short_flags_parse() {
        let FileCommand::List {
            max, query, parent, ..
        } = parse(&["files", "list", "-m", "5", "-q", "name = 'a'", "-p", "dir"])
        else {
            panic!("expected list");
        };
        assert_eq!(max, 5);
        assert_eq!(query.to_string(), "name = 'a'");
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

        for doc_type in [
            DocType::Document,
            DocType::Spreadsheet,
            DocType::Presentation,
        ] {
            for extension in doc_type.supported_export_types() {
                let extension = extension.to_string();
                assert!(listed.contains(extension.as_str()), "{extension}: {help}");
            }
        }
    }
}
