//! A [`Printer`] into buffers, for tests of what a run prints.

use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use super::Printer;

/// A `Write` into a shared buffer.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub(crate) fn lines(&self) -> Vec<String> {
        self.text().lines().map(String::from).collect()
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A printer and what it printed to stdout (`out`) and stderr (`err`).
pub(crate) struct Output {
    pub(crate) printer: Printer,
    pub(crate) out: Captured,
    pub(crate) err: Captured,
}

/// Needs a runtime: the printer's task is spawned on it.
pub(crate) fn output() -> Output {
    let out = Captured::default();
    let err = Captured::default();
    let printer = Printer::to(out.clone(), err.clone());
    Output { printer, out, err }
}
