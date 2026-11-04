//! A `serde_json` parse error without its message, for files that hold secrets (secret.json,
//! tokens.json): `serde_json` quotes the input it rejects ("invalid type: string \"...\"").

use std::fmt::{self, Display, Formatter};

use serde_json::error::Category;

/// What kind of parse error and where, never what the input was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RedactedJsonError {
    category: Category,
    line: usize,
    column: usize,
}

impl From<&serde_json::Error> for RedactedJsonError {
    fn from(error: &serde_json::Error) -> Self {
        RedactedJsonError {
            category: error.classify(),
            line: error.line(),
            column: error.column(),
        }
    }
}

impl Display for RedactedJsonError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let problem = match self.category {
            Category::Io => "read error",
            Category::Syntax => "invalid JSON",
            Category::Data => "a value is missing or has the wrong type",
            Category::Eof => "unexpected end of file",
        };
        write!(f, "{problem} at line {} column {}", self.line, self.column)
    }
}

#[cfg(test)]
mod tests {
    use super::RedactedJsonError;

    #[test]
    fn keeps_the_position_and_drops_the_quoted_input() {
        let error = serde_json::from_str::<Vec<u8>>(r#""ya29.secret""#).unwrap_err();
        assert!(error.to_string().contains("ya29.secret"), "{error}");

        assert_eq!(
            RedactedJsonError::from(&error).to_string(),
            "a value is missing or has the wrong type at line 1 column 13"
        );
    }
}
