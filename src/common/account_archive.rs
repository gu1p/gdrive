pub(crate) mod errors;

use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    ops::Not,
    path::{Component, Path, PathBuf},
};

use crate::{app_config::account_type::STAGING_DIR_NAME, transfer::upload::session::UPLOADS_DIR};

/// Entries of an account directory that belong to this machine and are neither exported nor
/// imported: the records of resumable upload sessions (`uploads/`, which hold session URLs and
/// local paths) and gdrive's staging directory.
const MACHINE_ENTRIES: [&str; 2] = [UPLOADS_DIR, STAGING_DIR_NAME];

/// Creates an archive of the given source directory, an account directory, without the entries
/// that belong to this machine ([`MACHINE_ENTRIES`]).
///
/// # Panics
///
/// The function panics if `src_path` terminates with a `..`.
pub(crate) fn create(src_path: &Path, archive_path: &Path) -> Result<(), errors::Create> {
    if src_path.exists().not() {
        return Err(errors::Create::SrcPathDoesNotExist);
    }

    if src_path.is_dir().not() {
        return Err(errors::Create::SrcPathNotDirectory);
    }

    let archive_file = File::create_new(archive_path).map_err(errors::Create::CreateArchive)?;
    let mut builder = tar::Builder::new(archive_file);

    let src_dir_name = Path::new(
        src_path
            .file_name()
            .expect("`src_path` should not terminate with `..`"),
    );

    if let Err(source) = append_account_dir(&mut builder, src_dir_name, src_path) {
        return Err(errors::Create::AppendDir {
            dir_path: PathBuf::from(src_dir_name),
            source,
        });
    }

    builder.finish().map_err(errors::Create::FinishArchive)?;

    Ok(())
}

/// Appends the directory `src_path` as `dir_name` and everything in it, except the entries of
/// [`MACHINE_ENTRIES`] at its top.
fn append_account_dir(
    builder: &mut tar::Builder<File>,
    dir_name: &Path,
    src_path: &Path,
) -> io::Result<()> {
    builder.append_dir(dir_name, src_path)?;

    let mut entries = fs::read_dir(src_path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if is_machine_entry(&name) {
            continue;
        }
        let path = entry.path();
        let archive_name = dir_name.join(&name);
        // As `append_dir_all` does, a link to a directory is followed.
        if path.is_dir() {
            builder.append_dir_all(&archive_name, &path)?;
        } else {
            builder.append_path_with_name(&path, &archive_name)?;
        }
    }
    Ok(())
}

fn is_machine_entry(name: &OsStr) -> bool {
    MACHINE_ENTRIES.iter().any(|entry| name == *entry)
}

/// Whether `path`, the path of an archive entry, is or is inside one of the [`MACHINE_ENTRIES`]
/// of the account directory, or gdrive's staging directory next to it. Archives that older
/// versions of gdrive exported may hold them.
fn is_machine_path(path: &Path) -> bool {
    let mut components = path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(Component::as_os_str);
    let account_dir = components.next();
    let in_account_dir = components.next();
    account_dir.is_some_and(|name| name == STAGING_DIR_NAME)
        || in_account_dir.is_some_and(is_machine_entry)
}

/// Unpacks the archive into `dst_path`, except the entries that belong to the machine it was
/// exported from ([`MACHINE_ENTRIES`]).
pub(crate) fn unpack(archive_path: &Path, dst_path: &Path) -> Result<(), errors::Unpack> {
    if archive_path.exists().not() {
        return Err(errors::Unpack::ArchivePathDoesNotExist);
    }

    if dst_path.exists().not() {
        return Err(errors::Unpack::DstDoesNotExist);
    }

    let archive_file = File::open(archive_path).map_err(errors::Unpack::Open)?;
    let mut archive = tar::Archive::new(archive_file);
    let entries = archive.entries().map_err(errors::Unpack::Unpack)?;

    // As `tar::Archive::unpack` does: directories are unpacked last, deepest first, so that
    // their permissions do not keep their content from being written.
    let mut directories = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(errors::Unpack::Unpack)?;
        if is_machine_path(&entry.path().map_err(errors::Unpack::Unpack)?) {
            continue;
        }
        if entry.header().entry_type() == tar::EntryType::Directory {
            directories.push(entry);
        } else {
            entry.unpack_in(dst_path).map_err(errors::Unpack::Unpack)?;
        }
    }
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut directory in directories {
        directory
            .unpack_in(dst_path)
            .map_err(errors::Unpack::Unpack)?;
    }
    Ok(())
}

pub(crate) fn get_account_name(archive_path: &Path) -> Result<String, errors::GetAccountName> {
    let archive_file = File::open(archive_path).map_err(errors::GetAccountName::Open)?;
    let mut archive = tar::Archive::new(archive_file);
    let entries = archive
        .entries()
        .map_err(errors::GetAccountName::ReadEntries)?;

    let mut dir_names = entries.filter_map(|entry| {
        let entry = entry.ok()?;
        let path = entry.path().ok()?;

        if entry.header().entry_type() == tar::EntryType::Directory && !is_machine_path(&path) {
            let file_name = path.file_name()?;
            Some(file_name.to_string_lossy().to_string())
        } else {
            None
        }
    });

    let name = dir_names
        .next()
        .ok_or(errors::GetAccountName::NoDirectories)?;
    if dir_names.next().is_some() {
        return Err(errors::GetAccountName::MultipleDirectories);
    }

    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{create, get_account_name, unpack};

    /// The paths of the entries of the archive at `path`.
    fn entry_paths(path: &Path) -> Vec<String> {
        let mut archive = tar::Archive::new(fs::File::open(path).unwrap());
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().display().to_string())
            .collect()
    }

    /// An account directory `work` under `root` with its tokens and a session record.
    fn account_with_session_record(root: &Path) -> std::path::PathBuf {
        let account = root.join("work");
        fs::create_dir_all(account.join("uploads")).unwrap();
        fs::write(account.join("secret.json"), "{}").unwrap();
        fs::write(account.join("tokens.json"), "[]").unwrap();
        fs::write(
            account.join("uploads/0123abcd.json"),
            r#"{"session_url": "https://upload.example/session"}"#,
        )
        .unwrap();
        fs::create_dir_all(account.join(".staging/work.new")).unwrap();
        account
    }

    #[test]
    fn the_export_leaves_out_the_session_records() {
        let dir = tempfile::tempdir().unwrap();
        let account = account_with_session_record(dir.path());
        let archive = dir.path().join("export.tar");

        create(&account, &archive).unwrap();

        let mut paths = entry_paths(&archive);
        paths.sort();
        assert_eq!(
            paths,
            ["work", "work/secret.json", "work/tokens.json"],
            "no uploads/ and no .staging"
        );
        assert_eq!(get_account_name(&archive).unwrap(), "work");
    }

    #[test]
    fn the_import_leaves_out_session_records_of_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        // An archive as earlier versions exported it, with everything in the account directory,
        // and with a staging directory next to it.
        let account = account_with_session_record(dir.path());
        let archive = dir.path().join("export.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive).unwrap());
        builder.append_dir_all("work", &account).unwrap();
        builder
            .append_path_with_name(account.join("tokens.json"), ".staging/work.new/tokens.json")
            .unwrap();
        builder.finish().unwrap();
        drop(builder);
        assert!(entry_paths(&archive)
            .iter()
            .any(|path| path == "work/uploads/0123abcd.json"));

        assert_eq!(get_account_name(&archive).unwrap(), "work");
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        unpack(&archive, &config).unwrap();

        assert_eq!(
            fs::read_to_string(config.join("work/tokens.json")).unwrap(),
            "[]"
        );
        assert!(config.join("work/secret.json").is_file());
        assert!(!config.join("work/uploads").exists());
        assert!(!config.join("work/.staging").exists());
        assert!(!config.join(".staging").exists());
    }
}
