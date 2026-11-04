//! The request bodies of an upload: a byte range of the source, read piece by piece while it is
//! sent and hashed in order on the way.
//!
//! A body is never held in memory whole. A pump reads [`PIECE`] bytes at a time from the source
//! and hands them to the request body through a channel of [`DEPTH`] pieces, so an upload holds
//! at most about `(DEPTH + 1) * PIECE` bytes whatever the chunk size. A chunk that is sent again
//! is read again from its offset.

use std::{
    future::Future,
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::mpsc,
};

/// Bytes read from the source and sent at a time.
pub(super) const PIECE: usize = 256 * 1024;
/// Pieces read ahead of the connection.
pub(super) const DEPTH: usize = 4;

/// Where an upload reads its content.
pub(crate) trait Source {
    /// Reads at most `buf.len()` bytes at `offset`; 0 only at the end of the source.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>>;

    /// The size of the source now.
    fn size(&mut self) -> impl Future<Output = io::Result<u64>>;
}

/// A local file, read with tokio's file I/O.
pub(crate) struct FileSource {
    file: tokio::fs::File,
    /// The file position, when it is known.
    position: Option<u64>,
}

impl FileSource {
    #[must_use]
    pub(crate) fn new(file: tokio::fs::File) -> Self {
        FileSource {
            file,
            position: None,
        }
    }
}

impl Source for FileSource {
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        // Unknown until this read completes: a read that is dropped half-way (its request
        // ended) still moves the file position, so the next one seeks.
        let position = self.position.take();
        if position != Some(offset) {
            self.file.seek(SeekFrom::Start(offset)).await?;
        }
        let read = self.file.read(buf).await?;
        self.position = Some(offset + read as u64);
        Ok(read)
    }

    async fn size(&mut self) -> io::Result<u64> {
        Ok(self.file.metadata().await?.len())
    }
}

/// Any seekable reader, for callers that have no path (`files::upload::upload_file`). Its reads
/// block the thread they run on, as google-apis-common's did.
pub(crate) struct ReaderSource<R> {
    reader: R,
    position: Option<u64>,
}

impl<R> ReaderSource<R> {
    #[must_use]
    pub(crate) fn new(reader: R) -> Self {
        ReaderSource {
            reader,
            position: None,
        }
    }
}

impl<R: Read + Seek> Source for ReaderSource<R> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> {
        std::future::ready(self.read_blocking(offset, buf))
    }

    fn size(&mut self) -> impl Future<Output = io::Result<u64>> {
        self.position = None;
        std::future::ready(self.reader.seek(SeekFrom::End(0)))
    }
}

impl<R: Read + Seek> ReaderSource<R> {
    fn read_blocking(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if self.position.take() != Some(offset) {
            self.reader.seek(SeekFrom::Start(offset))?;
        }
        let read = loop {
            match self.reader.read(buf) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => break result?,
            }
        };
        self.position = Some(offset + read as u64);
        Ok(read)
    }
}

/// The md5 of the source's content from its first byte, as far as it has been read in order.
pub(super) struct Md5Progress {
    context: md5::Context,
    /// Bytes hashed: the next byte to hash is at this offset.
    hashed: u64,
}

impl Md5Progress {
    #[must_use]
    pub(super) fn new() -> Self {
        Md5Progress {
            context: md5::Context::new(),
            hashed: 0,
        }
    }

    /// Hashes the bytes of `bytes`, which were read at `offset`, that follow the ones hashed so
    /// far. Bytes read again (a chunk sent again) are not hashed twice.
    fn feed(&mut self, offset: u64, bytes: &[u8]) {
        let end = offset + bytes.len() as u64;
        if offset > self.hashed || end <= self.hashed {
            return;
        }
        let skip = usize::try_from(self.hashed - offset).unwrap_or(bytes.len());
        self.context.consume(&bytes[skip..]);
        self.hashed = end;
    }

    /// Reads and hashes the source up to `offset`, for content the server already holds that
    /// this run did not read (an upload continued from an earlier run).
    pub(super) async fn catch_up<S: Source>(
        &mut self,
        source: &mut S,
        offset: u64,
    ) -> io::Result<()> {
        let mut buf = vec![0; PIECE];
        while self.hashed < offset {
            let want = usize::try_from(offset - self.hashed).map_or(PIECE, |rest| rest.min(PIECE));
            let read = source.read_at(self.hashed, &mut buf[..want]).await?;
            if read == 0 {
                return Err(shrunk());
            }
            let hashed = self.hashed;
            self.feed(hashed, &buf[..read]);
        }
        Ok(())
    }

    /// The md5 of the bytes hashed so far.
    #[must_use]
    pub(super) fn digest(&self) -> md5::Digest {
        self.context.clone().compute()
    }
}

fn shrunk() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the file got shorter while it was being uploaded",
    )
}

/// What a request body consists of: `prefix`, `len` bytes of the source from `offset`, then
/// `suffix` (the parts around the content of a multipart request).
pub(super) struct Segment {
    pub(super) offset: u64,
    pub(super) len: u64,
    pub(super) prefix: Bytes,
    pub(super) suffix: Bytes,
}

impl Segment {
    /// `len` bytes of the source from `offset`, nothing around them.
    #[must_use]
    pub(super) fn content(offset: u64, len: u64) -> Self {
        Segment {
            offset,
            len,
            prefix: Bytes::new(),
            suffix: Bytes::new(),
        }
    }

    /// The `Content-Length` of the body.
    #[must_use]
    pub(super) fn body_len(&self) -> u64 {
        self.prefix.len() as u64 + self.len + self.suffix.len() as u64
    }
}

/// A request body fed by [`pump`]. It announces its exact length; when the pump stops early (a
/// read error), the body ends short of it and the HTTP client fails the request.
pub(super) struct PipeBody {
    pieces: mpsc::Receiver<Bytes>,
    remaining: u64,
}

impl PipeBody {
    #[must_use]
    pub(super) fn new(pieces: mpsc::Receiver<Bytes>, len: u64) -> Self {
        PipeBody {
            pieces,
            remaining: len,
        }
    }
}

impl Body for PipeBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        match self.pieces.poll_recv(cx) {
            Poll::Ready(Some(piece)) => {
                self.remaining = self.remaining.saturating_sub(piece.len() as u64);
                Poll::Ready(Some(Ok(Frame::data(piece))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// Sends `segment` into `pieces`, reading the source piece by piece and hashing the content
/// with `hasher` on the way. Returns when everything is sent or the request no longer reads its
/// body (the receiver is gone); a read error of the source is returned as is.
pub(super) async fn pump<S: Source>(
    source: &mut S,
    mut hasher: Option<&mut Md5Progress>,
    segment: &Segment,
    pieces: mpsc::Sender<Bytes>,
) -> io::Result<()> {
    if !segment.prefix.is_empty() && pieces.send(segment.prefix.clone()).await.is_err() {
        return Ok(());
    }
    let end = segment.offset + segment.len;
    let mut offset = segment.offset;
    while offset < end {
        let want = usize::try_from(end - offset).map_or(PIECE, |rest| rest.min(PIECE));
        let mut piece = vec![0; want];
        let read = source.read_at(offset, &mut piece).await?;
        if read == 0 {
            return Err(shrunk());
        }
        piece.truncate(read);
        if let Some(hasher) = hasher.as_deref_mut() {
            hasher.feed(offset, &piece);
        }
        offset += read as u64;
        if pieces.send(Bytes::from(piece)).await.is_err() {
            return Ok(());
        }
    }
    if !segment.suffix.is_empty() {
        // A closed receiver here means the request is over; nothing else to do.
        let _ = pieces.send(segment.suffix.clone()).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use futures::FutureExt;

    use super::{FileSource, Md5Progress, ReaderSource, Source};

    #[test]
    fn bytes_read_again_are_hashed_once() {
        let data: Vec<u8> = (0..=255).collect();
        let mut progress = Md5Progress::new();
        progress.feed(0, &data[..100]);
        // Resent from an earlier offset, and past what was hashed.
        progress.feed(50, &data[50..200]);
        // A gap: ignored until the bytes before it are hashed.
        progress.feed(220, &data[220..]);
        progress.feed(200, &data[200..]);
        assert_eq!(progress.digest(), md5::compute(&data));
    }

    #[tokio::test]
    async fn file_reads_follow_their_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let data: Vec<u8> = (0..=255).collect();
        std::fs::write(&path, &data).unwrap();
        let mut source = FileSource::new(tokio::fs::File::open(&path).await.unwrap());

        let mut buf = [0; 16];
        assert_eq!(source.read_at(0, &mut buf).await.unwrap(), 16);
        assert_eq!(buf, data[..16]);
        // A read dropped before it completes (polled once: the read is in flight), then the
        // same read again, as for a chunk that is sent again.
        let _ = source.read_at(16, &mut buf).now_or_never();
        assert_eq!(source.read_at(16, &mut buf).await.unwrap(), 16);
        assert_eq!(buf, data[16..32]);
        assert_eq!(source.read_at(100, &mut buf).await.unwrap(), 16);
        assert_eq!(buf, data[100..116]);
        assert_eq!(source.read_at(116, &mut buf).await.unwrap(), 16);
        assert_eq!(buf, data[116..132]);
        assert_eq!(source.read_at(256, &mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn catch_up_reads_what_was_not_hashed() {
        let data: Vec<u8> = (0..1_000_000_u32).map(|i| (i % 251) as u8).collect();
        let mut source = ReaderSource::new(Cursor::new(data.clone()));
        let mut progress = Md5Progress::new();
        progress.catch_up(&mut source, 700_000).await.unwrap();
        progress.feed(700_000, &data[700_000..]);
        assert_eq!(progress.digest(), md5::compute(&data));

        let error = Md5Progress::new()
            .catch_up(&mut source, 2_000_000)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
