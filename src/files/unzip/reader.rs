//! The one reader of the zip file of a run.
//!
//! Drive limits how often one file can be read. A request per entry ran into that limit after a
//! few hundred entries (`downloadQuotaExceeded`), and from then on Drive refused the whole file.
//! So one reply is read from front to back, and the entries take turns in the order of the zip
//! file. A jump, past what a run needs not read, is read through unless it is really far.
//!
//! What Drive refuses in the end, after the retries of [`DriveSource`], it would refuse for every
//! other entry as well: after that the reader asks Drive nothing more.
//!
//! [`DriveSource`]: crate::transfer::relay::DriveSource

use std::{
    convert::Infallible,
    future::{pending, ready, Future},
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use tokio::sync::{mpsc, oneshot, watch};

use crate::transfer::upload::Source;

/// The longest jump that is read through. A new request takes longer than most of these, and
/// each one counts against Drive's limit.
const READ_THROUGH: u64 = 256 * 1024 * 1024;
const SCRAP: usize = 64 * 1024;

struct Request {
    offset: u64,
    len: usize,
    reply: oneshot::Sender<io::Result<Vec<u8>>>,
}

/// A handle on the reader, for a job: the source's reads borrow their hub, which a job cannot.
#[derive(Clone)]
pub(super) struct ZipReader {
    requests: mpsc::Sender<Request>,
    size: u64,
    broken: Arc<AtomicBool>,
}

/// The other end of a [`ZipReader`], for [`serve`].
pub(super) struct Requests {
    requests: mpsc::Receiver<Request>,
    broken: Arc<AtomicBool>,
}

impl ZipReader {
    /// A reader of a zip file of `size` bytes, working once [`serve`] does.
    pub(super) fn new(size: u64) -> (ZipReader, Requests) {
        let (sender, requests) = mpsc::channel(1);
        let broken = Arc::new(AtomicBool::new(false));
        let reader = ZipReader {
            requests: sender,
            size,
            broken: Arc::clone(&broken),
        };
        (reader, Requests { requests, broken })
    }

    /// Whether Drive refused a read: no entry that still has to be read will get anything.
    pub(super) fn broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }
}

fn gone() -> io::Error {
    io::Error::other("the reader of the zip file has stopped")
}

impl Source for ZipReader {
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let (reply, answer) = oneshot::channel();
        let request = Request {
            offset,
            len: buf.len(),
            reply,
        };
        self.requests.send(request).await.map_err(|_| gone())?;
        let bytes = answer.await.map_err(|_| gone())??;
        buf[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn size(&mut self) -> impl Future<Output = io::Result<u64>> {
        ready(Ok(self.size))
    }
}

/// Answers the reads of the [`ZipReader`] from `source`, until it is dropped.
pub(super) async fn serve<S: Source>(mut source: S, requests: Requests) -> Infallible {
    let Requests {
        mut requests,
        broken,
    } = requests;
    let mut position = None;
    let mut failure = None;
    while let Some(request) = requests.recv().await {
        let result = match &failure {
            Some(message) => Err(io::Error::other(String::clone(message))),
            None => read(&mut source, &mut position, request.offset, request.len).await,
        };
        if let (Err(error), None) = (&result, &failure) {
            failure = Some(error.to_string());
            broken.store(true, Ordering::SeqCst);
        }
        // Nobody asks any more when the job that asked has ended.
        let _ = request.reply.send(result);
    }
    pending().await
}

async fn read<S: Source>(
    source: &mut S,
    position: &mut Option<u64>,
    offset: u64,
    len: usize,
) -> io::Result<Vec<u8>> {
    if let Some(from) = position.filter(|from| {
        offset
            .checked_sub(*from)
            .is_some_and(|jump| jump <= READ_THROUGH)
    }) {
        let mut scrap = vec![0; SCRAP];
        let mut at = from;
        while at < offset {
            let want = usize::try_from(offset - at).map_or(SCRAP, |rest| rest.min(SCRAP));
            let read = source.read_at(at, &mut scrap[..want]).await?;
            if read == 0 {
                break;
            }
            at += read as u64;
        }
    }
    let mut bytes = vec![0; len];
    let read = source.read_at(offset, &mut bytes).await?;
    bytes.truncate(read);
    *position = Some(offset + read as u64);
    Ok(bytes)
}

/// Whose turn it is to read: the places in the order of the zip file, one after the other.
pub(super) struct Turns {
    next: watch::Sender<u64>,
}

/// Holding it is having the turn; dropping it passes it on, also when a job fails.
pub(super) struct Turn<'a> {
    turns: &'a Turns,
    place: u64,
}

impl Turns {
    pub(super) fn new() -> Self {
        Turns {
            next: watch::channel(0).0,
        }
    }

    /// Waits for the turn of `place`. Every place before it must be taken by a job that runs or
    /// has run: the scheduler starts jobs in the order they come.
    pub(super) async fn take(&self, place: u64) -> Turn<'_> {
        let mut next = self.next.subscribe();
        // It cannot fail: the sender is `self`.
        let _ = next.wait_for(|next| *next == place).await;
        Turn { turns: self, place }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.turns.next.send_replace(self.place + 1);
    }
}
