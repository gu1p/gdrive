use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
    fs,
    path::Path,
};

pub(crate) struct FileInfo<'a> {
    pub(crate) name: Cow<'a, str>,
    pub(crate) mime_type: Cow<'a, mime::Mime>,
    pub(crate) parents: Option<Vec<String>>,
    /// The size when the file was looked at, which the progress display starts from. The upload
    /// does not use it: it measures the file when it opens it, as a file can change in between.
    pub(crate) size: u64,
}

pub(crate) struct Config<'a> {
    pub(crate) file_path: &'a Path,
    /// The name on Drive (`--name`) instead of the file name of `file_path`.
    pub(crate) name: Option<&'a str>,
    pub(crate) mime_type: Option<&'a mime::Mime>,
    pub(crate) parents: Option<Vec<String>>,
}

impl<'a> FileInfo<'a> {
    /// The media type, if not given, is guessed from the extension of `file_path`, or else from
    /// that of the name given (a spool of stdin has no extension).
    pub(crate) fn from_file(file: &fs::File, config: Config<'a>) -> Result<Self, FromFileError> {
        let file_name = match config.name {
            Some(name) => Cow::Borrowed(name),
            None => config
                .file_path
                .file_name()
                .map(|s| s.to_string_lossy())
                .ok_or(FromFileError)?,
        };

        let file_size = file.metadata().map_or(0, |m| m.len());

        let mime_type = config.mime_type.map_or_else(
            || {
                mime_guess::from_path(config.file_path)
                    .first()
                    .or_else(|| {
                        config
                            .name
                            .and_then(|name| mime_guess::from_path(name).first())
                    })
                    .map_or(Cow::Borrowed(&mime::APPLICATION_OCTET_STREAM), Cow::Owned)
            },
            Cow::Borrowed,
        );

        Ok(FileInfo {
            name: file_name,
            mime_type,
            parents: config.parents,
            size: file_size,
        })
    }
}

#[derive(Debug)]
pub(crate) struct FromFileError;

impl error::Error for FromFileError {}

impl Display for FromFileError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid file path")
    }
}
