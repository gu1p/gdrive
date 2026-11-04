//! Running one operation over several file ids, one after another, as `files delete` and
//! `files trash` do.

use std::{
    error,
    fmt::{self, Display, Formatter},
};

use error_trace::ErrorTrace;

/// Collects the results of an operation run once per file id.
///
/// With a single id its error is kept as it is, so output and exit code stay what they were
/// before several ids were accepted. With several ids every failure is printed to stderr as it
/// happens and the remaining ids are still processed; [`Batch::finish`] then reports how many
/// failed, which makes the command exit non-zero.
pub(crate) struct Batch<E> {
    verb: &'static str,
    total: usize,
    failed: usize,
    single_error: Option<E>,
}

impl<E: error::Error> Batch<E> {
    /// `verb` names the operation in the summary, e.g. "delete".
    pub(crate) fn new(verb: &'static str, total: usize) -> Self {
        Self {
            verb,
            total,
            failed: 0,
            single_error: None,
        }
    }

    pub(crate) fn record(&mut self, file_id: &str, result: Result<(), E>) {
        let Err(err) = result else {
            return;
        };

        self.failed += 1;
        if self.total == 1 {
            self.single_error = Some(err);
        } else {
            let trace = err.trace().to_string();
            eprintln!("Error: {file_id}: {}", trace.trim_end());
        }
    }

    pub(crate) fn finish(self) -> Result<(), BatchError<E>> {
        if let Some(err) = self.single_error {
            return Err(BatchError::Single(err));
        }

        if self.failed == 0 {
            Ok(())
        } else {
            Err(BatchError::Failed {
                verb: self.verb,
                failed: self.failed,
                total: self.total,
            })
        }
    }
}

#[derive(Debug)]
pub(crate) enum BatchError<E> {
    /// The error of the only id, or one that stopped the command before any id was processed.
    /// It displays exactly like `E`.
    Single(E),
    /// Some of several ids failed; their errors have already been printed.
    Failed {
        verb: &'static str,
        failed: usize,
        total: usize,
    },
}

impl<E> From<E> for BatchError<E> {
    fn from(err: E) -> Self {
        BatchError::Single(err)
    }
}

impl<E: Display> Display for BatchError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            BatchError::Single(err) => err.fmt(f),
            BatchError::Failed {
                verb,
                failed,
                total,
            } => write!(f, "failed to {verb} {failed} of {total} files"),
        }
    }
}

impl<E: error::Error> error::Error for BatchError<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            BatchError::Single(err) => err.source(),
            BatchError::Failed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error, fmt};

    use super::{Batch, BatchError};

    #[derive(Debug)]
    struct Failure(&'static str);

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl error::Error for Failure {}

    fn run(results: Vec<Result<(), Failure>>) -> Result<(), BatchError<Failure>> {
        let mut batch = Batch::new("delete", results.len());
        for (index, result) in results.into_iter().enumerate() {
            batch.record(&format!("id-{index}"), result);
        }
        batch.finish()
    }

    #[test]
    fn all_succeeded() {
        assert!(run(vec![Ok(())]).is_ok());
        assert!(run(vec![Ok(()), Ok(()), Ok(())]).is_ok());
    }

    #[test]
    fn single_id_keeps_its_error() {
        let err = run(vec![Err(Failure("unable to delete file"))]).unwrap_err();
        assert!(matches!(err, BatchError::Single(Failure(_))));
        assert_eq!(err.to_string(), "unable to delete file");
    }

    #[test]
    fn several_ids_report_how_many_failed() {
        let err = run(vec![Err(Failure("a")), Ok(()), Err(Failure("c"))]).unwrap_err();
        assert!(matches!(
            err,
            BatchError::Failed {
                failed: 2,
                total: 3,
                ..
            }
        ));
        assert_eq!(err.to_string(), "failed to delete 2 of 3 files");
    }

    #[test]
    fn error_before_any_id_is_kept() {
        fn early() -> Result<(), BatchError<Failure>> {
            Err(Failure("unable to get drive hub"))?;
            Ok(())
        }
        assert_eq!(early().unwrap_err().to_string(), "unable to get drive hub");
    }
}
