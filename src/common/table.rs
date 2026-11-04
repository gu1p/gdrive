use std::{
    borrow::Cow,
    fmt::{self, Display},
    io::{self, BufWriter, Write},
};

use tabwriter::TabWriter;

pub(crate) struct Table<H: Display, V: Display, const COLUMNS: usize> {
    pub(crate) header: [H; COLUMNS],
    pub(crate) values: Vec<[V; COLUMNS]>,
}

#[derive(Debug, Clone)]
pub(crate) struct DisplayConfig<'a> {
    pub(crate) skip_header: bool,
    pub(crate) separator: &'a str,
}

impl DisplayConfig<'_> {
    fn display_row<'a, T>(&'a self, value: &'a [T]) -> DisplayRow<'a, T>
    where
        T: Display,
    {
        DisplayRow(self, value)
    }
}

impl Default for DisplayConfig<'static> {
    fn default() -> Self {
        Self {
            skip_header: false,
            separator: "\t",
        }
    }
}

pub(crate) fn write<W: Write, H: Display, V: Display, const COLUMNS: usize>(
    writer: W,
    table: Table<H, V, COLUMNS>,
    config: &DisplayConfig,
) -> Result<(), io::Error> {
    let mut tw = TabWriter::new(writer).padding(3);

    if !config.skip_header {
        writeln!(&mut tw, "{}", config.display_row(&table.header))?;
    }

    for value in table.values {
        writeln!(&mut tw, "{}", config.display_row(&value))?;
    }

    tw.flush()
}

/// Treats a closed stdout as the normal end of the output: when the reader has gone away, as in
/// `gdrive files list | head -n 1`, the listing stops quietly and exits 0, as 3.9 did. Other write
/// errors are kept.
pub(crate) fn ignore_broken_pipe(result: Result<(), io::Error>) -> Result<(), io::Error> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

/// A writer whose every write and flush fails with the given kind, for tests.
#[cfg(test)]
pub(crate) struct FailingWriter(pub(crate) io::ErrorKind);

#[cfg(test)]
impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(self.0.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(self.0.into())
    }
}

/// Writes the table as tab-separated values: one line per row, no alignment, and tab, newline,
/// carriage return and backslash inside a value written as `\t`, `\n`, `\r` and `\\`.
pub(crate) fn write_tsv<W: Write, H: Display, V: Display, const COLUMNS: usize>(
    writer: W,
    table: &Table<H, V, COLUMNS>,
    skip_header: bool,
) -> Result<(), io::Error> {
    let mut writer = BufWriter::new(writer);

    if !skip_header {
        write_tsv_row(&mut writer, &table.header)?;
    }

    for value in &table.values {
        write_tsv_row(&mut writer, value)?;
    }

    writer.flush()
}

fn write_tsv_row<W: Write, T: Display>(writer: &mut W, row: &[T]) -> Result<(), io::Error> {
    for (index, value) in row.iter().enumerate() {
        if index > 0 {
            writer.write_all(b"\t")?;
        }
        writer.write_all(escape_tsv(&value.to_string()).as_bytes())?;
    }
    writer.write_all(b"\n")
}

/// Escapes the characters that would break a TSV value.
pub(crate) fn escape_tsv(value: &str) -> Cow<'_, str> {
    if !value.contains(['\t', '\n', '\r', '\\']) {
        return Cow::Borrowed(value);
    }

    let mut escaped = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\\' => escaped.push_str("\\\\"),
            c => escaped.push(c),
        }
    }
    Cow::Owned(escaped)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DisplayRow<'a, T>(&'a DisplayConfig<'a>, &'a [T]);

impl<T> Display for DisplayRow<'_, T>
where
    T: Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut columns = self.1.iter();
        if let Some(column) = columns.next() {
            write!(f, "{column}")?;
            for column in columns {
                write!(f, "{}{column}", self.0.separator)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{escape_tsv, ignore_broken_pipe, write, write_tsv, DisplayConfig, Table};

    fn sample() -> Table<&'static str, &'static str, 2> {
        Table {
            header: ["Id", "Name"],
            values: vec![["a1", "plain"], ["b22", "tab\there\nnew\rline\\slash"]],
        }
    }

    #[test]
    fn escape_tsv_escapes_separators_and_backslash() {
        assert_eq!(escape_tsv("plain name.txt"), "plain name.txt");
        assert_eq!(escape_tsv("a\tb"), r"a\tb");
        assert_eq!(escape_tsv("a\nb\r\n"), r"a\nb\r\n");
        assert_eq!(escape_tsv(r"C:\dir\t"), r"C:\\dir\\t");
        assert_eq!(escape_tsv("ünï\tcode 🤡"), r"ünï\tcode 🤡");
    }

    #[test]
    fn write_tsv_keeps_one_line_per_row() {
        let mut out = Vec::new();
        write_tsv(&mut out, &sample(), false).expect("write to vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "Id\tName\na1\tplain\nb22\ttab\\there\\nnew\\rline\\\\slash\n"
        );
    }

    #[test]
    fn write_tsv_skips_header() {
        let mut out = Vec::new();
        write_tsv(&mut out, &sample(), true).expect("write to vec");
        assert!(String::from_utf8(out)
            .expect("utf-8")
            .starts_with("a1\tplain\n"));
    }

    #[test]
    fn write_aligns_columns() {
        let table = Table {
            header: ["Id", "Name"],
            values: vec![["a1", "x"], ["b22", "y"]],
        };
        let mut out = Vec::new();
        write(&mut out, table, &DisplayConfig::default()).expect("write to vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "Id    Name\na1    x\nb22   y\n"
        );
    }

    #[test]
    fn broken_pipe_ends_output_quietly() {
        assert!(ignore_broken_pipe(Ok(())).is_ok());
        assert!(ignore_broken_pipe(Err(io::ErrorKind::BrokenPipe.into())).is_ok());
        for kind in [io::ErrorKind::StorageFull, io::ErrorKind::PermissionDenied] {
            let err = ignore_broken_pipe(Err(kind.into())).expect_err("other errors are kept");
            assert_eq!(err.kind(), kind);
        }
    }
}
