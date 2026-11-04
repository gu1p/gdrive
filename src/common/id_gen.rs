use std::{
    error,
    fmt::{Display, Formatter},
};

use crate::{files::generate_ids, hub::Hub};

pub(crate) struct IdGen<'a> {
    source: Source<'a>,
    ids: Vec<String>,
}

enum Source<'a> {
    Drive {
        hub: &'a Hub,
    },
    /// Local sequential ids that are never sent to Drive: for a local tree that is only
    /// compared with Drive (the inventory of `files sync`), and for tests that must not reach
    /// Drive.
    Sequential(u64),
}

impl<'a> IdGen<'a> {
    #[must_use]
    pub(crate) fn new(hub: &'a Hub) -> Self {
        Self {
            source: Source::Drive { hub },
            ids: Vec::new(),
        }
    }

    /// Ids `id-1`, `id-2`, ... made locally, without a request: for a tree whose items are
    /// never created on Drive under them.
    #[must_use]
    pub(crate) fn sequential() -> IdGen<'static> {
        IdGen {
            source: Source::Sequential(0),
            ids: Vec::new(),
        }
    }

    pub(crate) async fn next(&mut self) -> Result<String, NextError> {
        if let Some(id) = self.ids.pop() {
            return Ok(id);
        }

        match &mut self.source {
            Source::Drive { hub } => {
                self.ids = generate_ids::generate_ids(hub, 1000)
                    .await
                    .map_err(NextError::GenerateIds)?;
                let id = self.ids.pop().ok_or(NextError::OutOfIds)?;
                Ok(id)
            }
            Source::Sequential(next) => {
                *next += 1;
                Ok(format!("id-{next}"))
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum NextError {
    // TODO: remove this allocation
    GenerateIds(Box<google_drive3::Error>),
    OutOfIds,
}

impl error::Error for NextError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            NextError::GenerateIds(source) => Some(source),
            NextError::OutOfIds => None,
        }
    }
}

impl Display for NextError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NextError::GenerateIds(_) => "failed generate drive identifiers",
            NextError::OutOfIds => "no more identifiers available",
        };

        f.write_str(s)
    }
}
