//! The export format of each Google Workspace type in a recursive download: the `SPEC` of
//! `files download --recursive --export <SPEC>`.
//!
//! ```text
//! SPEC     := PROFILE | [PROFILE ","] OVERRIDE ("," OVERRIDE)*
//! PROFILE  := "default" | "md"
//! OVERRIDE := TYPE "=" FORMAT
//! TYPE     := "docs" | "sheets" | "slides" | "drawings"
//! FORMAT   := an extension the type exports to (see DocType::supported_export_types) | "skip"
//! ```
//!
//! Overrides apply on top of the profile, `default` when none is given: documents to docx,
//! spreadsheets to xlsx, presentations to pptx, drawings to png. `md` is `default` with
//! documents to md. `skip` leaves that type out, like a download without `--export`. Each type
//! may be overridden once. Names are ASCII case-insensitive; spaces around items are ignored.

use std::fmt::{self, Display, Formatter, Write as _};

use crate::common::drive_file::{DocType, FileExtension};

/// The format each exportable type is exported to; `None` skips the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExportFormats {
    documents: Option<FileExtension>,
    spreadsheets: Option<FileExtension>,
    presentations: Option<FileExtension>,
    drawings: Option<FileExtension>,
}

impl ExportFormats {
    /// The `default` profile: the office formats, which keep every sheet and slide, and png for
    /// drawings.
    pub(crate) const DEFAULT: ExportFormats = ExportFormats {
        documents: Some(FileExtension::Docx),
        spreadsheets: Some(FileExtension::Xlsx),
        presentations: Some(FileExtension::Pptx),
        drawings: Some(FileExtension::Png),
    };

    /// The `md` profile: `default` with documents as Markdown.
    pub(crate) const MARKDOWN: ExportFormats = ExportFormats {
        documents: Some(FileExtension::Md),
        ..ExportFormats::DEFAULT
    };

    /// The format of `doc_type`, `None` when it is skipped.
    #[must_use]
    pub(crate) fn format(self, doc_type: DocType) -> Option<FileExtension> {
        match doc_type {
            DocType::Document => self.documents,
            DocType::Spreadsheet => self.spreadsheets,
            DocType::Presentation => self.presentations,
            DocType::Drawing => self.drawings,
        }
    }

    fn format_mut(&mut self, doc_type: DocType) -> &mut Option<FileExtension> {
        match doc_type {
            DocType::Document => &mut self.documents,
            DocType::Spreadsheet => &mut self.spreadsheets,
            DocType::Presentation => &mut self.presentations,
            DocType::Drawing => &mut self.drawings,
        }
    }

    /// Parses a `SPEC` (see the module documentation). The error is a message for clap.
    pub(crate) fn parse(spec: &str) -> Result<ExportFormats, String> {
        Self::parse_spec(spec).map_err(|error| error.to_string())
    }

    fn parse_spec(spec: &str) -> Result<ExportFormats, SpecError> {
        let spec = spec.to_ascii_lowercase();
        let mut formats = ExportFormats::DEFAULT;
        let mut overridden = Vec::new();
        for (index, item) in spec.split(',').map(str::trim).enumerate() {
            if item.is_empty() {
                return Err(SpecError::EmptyItem);
            }
            let Some((type_name, format_name)) = item.split_once('=') else {
                formats = match (item, index) {
                    (PROFILE_DEFAULT, 0) => ExportFormats::DEFAULT,
                    (PROFILE_MD, 0) => ExportFormats::MARKDOWN,
                    (PROFILE_DEFAULT | PROFILE_MD, _) => {
                        return Err(SpecError::LateProfile(item.to_owned()))
                    }
                    _ => return Err(SpecError::UnknownItem(item.to_owned())),
                };
                continue;
            };

            let (type_name, format_name) = (type_name.trim(), format_name.trim());
            let doc_type = doc_type_of_name(type_name)
                .ok_or_else(|| SpecError::UnknownType(type_name.to_owned()))?;
            if overridden.contains(&doc_type) {
                return Err(SpecError::Repeated(doc_type));
            }
            overridden.push(doc_type);

            *formats.format_mut(doc_type) = if format_name == FORMAT_SKIP {
                None
            } else {
                let format = FileExtension::from_name(format_name)
                    .filter(|&format| doc_type.can_export_to(format))
                    .ok_or_else(|| SpecError::UnsupportedFormat {
                        doc_type,
                        format: format_name.to_owned(),
                    })?;
                Some(format)
            };
        }
        Ok(formats)
    }
}

impl Default for ExportFormats {
    fn default() -> Self {
        ExportFormats::DEFAULT
    }
}

const PROFILE_DEFAULT: &str = "default";
const PROFILE_MD: &str = "md";
const FORMAT_SKIP: &str = "skip";

/// The `TYPE` names of a `SPEC`, in the order of [`DocType::ALL`].
const TYPE_NAMES: [(&str, DocType); 4] = [
    ("docs", DocType::Document),
    ("sheets", DocType::Spreadsheet),
    ("slides", DocType::Presentation),
    ("drawings", DocType::Drawing),
];

fn doc_type_of_name(name: &str) -> Option<DocType> {
    TYPE_NAMES
        .iter()
        .find_map(|&(known, doc_type)| (known == name).then_some(doc_type))
}

fn name_of_doc_type(doc_type: DocType) -> &'static str {
    TYPE_NAMES
        .iter()
        .find_map(|&(name, known)| (known == doc_type).then_some(name))
        .unwrap_or_default()
}

/// An invalid `SPEC`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpecError {
    EmptyItem,
    UnknownItem(String),
    LateProfile(String),
    UnknownType(String),
    Repeated(DocType),
    UnsupportedFormat { doc_type: DocType, format: String },
}

impl Display for SpecError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        const TYPES: &str = "docs, sheets, slides or drawings";
        match self {
            SpecError::EmptyItem => f.write_str(
                "empty item; give a profile (default, md) and/or TYPE=FORMAT items separated by \
                commas, for example `md,sheets=ods`",
            ),
            SpecError::UnknownItem(item) => write!(
                f,
                "'{item}' is neither a profile (default, md) nor a TYPE=FORMAT item such as \
                docs=pdf"
            ),
            SpecError::LateProfile(profile) => write!(
                f,
                "the profile '{profile}' must come first, before the TYPE=FORMAT items"
            ),
            SpecError::UnknownType(name) => {
                write!(f, "unknown type '{name}', the types are {TYPES}")
            }
            SpecError::Repeated(doc_type) => write!(
                f,
                "the format of {} is given twice",
                name_of_doc_type(*doc_type)
            ),
            SpecError::UnsupportedFormat { doc_type, format } => {
                write!(
                    f,
                    "{} cannot be exported as '{format}'; the formats of {} are ",
                    name_of_doc_type(*doc_type),
                    name_of_doc_type(*doc_type)
                )?;
                let mut formats = String::new();
                for format in doc_type.supported_export_types() {
                    let _ = write!(formats, "{format}, ");
                }
                write!(f, "{formats}or {FORMAT_SKIP}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExportFormats, SpecError};
    use crate::common::drive_file::{DocType, FileExtension};

    fn parse(spec: &str) -> ExportFormats {
        ExportFormats::parse(spec).unwrap()
    }

    fn formats(formats: ExportFormats) -> [Option<FileExtension>; 4] {
        DocType::ALL.map(|doc_type| formats.format(doc_type))
    }

    #[test]
    fn the_default_profile_uses_office_formats_and_png() {
        assert_eq!(
            formats(parse("default")),
            [
                Some(FileExtension::Docx),
                Some(FileExtension::Xlsx),
                Some(FileExtension::Pptx),
                Some(FileExtension::Png),
            ]
        );
    }

    #[test]
    fn the_md_profile_exports_documents_as_markdown() {
        assert_eq!(
            formats(parse("md")),
            [
                Some(FileExtension::Md),
                Some(FileExtension::Xlsx),
                Some(FileExtension::Pptx),
                Some(FileExtension::Png),
            ]
        );
    }

    #[test]
    fn overrides_apply_on_top_of_the_profile() {
        assert_eq!(
            formats(parse("docs=pdf,sheets=csv,slides=pdf,drawings=svg")),
            [
                Some(FileExtension::Pdf),
                Some(FileExtension::Csv),
                Some(FileExtension::Pdf),
                Some(FileExtension::Svg),
            ]
        );
        assert_eq!(
            formats(parse("md, Slides=ODP , drawings=skip")),
            [
                Some(FileExtension::Md),
                Some(FileExtension::Xlsx),
                Some(FileExtension::Odp),
                None,
            ]
        );
        assert_eq!(
            formats(parse("sheets=ods")),
            formats(parse("default,sheets=ods"))
        );
    }

    #[test]
    fn invalid_specs_say_what_is_wrong() {
        let error = |spec| ExportFormats::parse_spec(spec).unwrap_err();
        assert_eq!(error(""), SpecError::EmptyItem);
        assert_eq!(error("md,"), SpecError::EmptyItem);
        assert_eq!(error("pdf"), SpecError::UnknownItem(String::from("pdf")));
        assert_eq!(
            error("docs=pdf,md"),
            SpecError::LateProfile(String::from("md"))
        );
        assert_eq!(
            error("forms=pdf"),
            SpecError::UnknownType(String::from("forms"))
        );
        assert_eq!(
            error("docs=pdf,docs=md"),
            SpecError::Repeated(DocType::Document)
        );
        assert_eq!(
            ExportFormats::parse("sheets=md").unwrap_err(),
            "sheets cannot be exported as 'md'; the formats of sheets are csv, tsv, ods, xlsx, \
            pdf, or skip"
        );
    }
}
