//! Resumable media downloads (#25, #78): when the body of a download breaks off (a connection
//! reset, an HTTP/2 GOAWAY or stream reset), the content is requested again from the byte where
//! it stopped, with `Range: bytes=<received>-`, within the retry policy of the hub. Bytes that
//! arrive start a new retry budget.
//!
//! The replies to a `Range` request:
//! - 206 is used only when its `Content-Range` starts at the received offset and announces the
//!   same total as before; otherwise the content is requested again from the start.
//! - 200 is the whole content again (Workspace exports ignore `Range`): the writer discards what
//!   it holds and starts over ([`Next::Body`] with `restart`).
//! - 416 means the range starts at the end or past it: complete when the size says so,
//!   otherwise the content is requested again from the start.
//!
//! Resume is in-process only.
// TODO(sync): resume across runs: persist the received offset with the file's identity and
// version (a sidecar next to the temporary file), and send `If-Range` or compare the md5 before
// continuing.

use std::time::Duration;

use futures::StreamExt;
use google_apis_common::Body;
use http_body_util::BodyExt;
use hyper::{
    header::{HeaderMap, CONTENT_LENGTH, CONTENT_RANGE, RANGE},
    Request, StatusCode,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::common::{
    media_client::{MediaClient, MediaError},
    retry::{classify_body_error, Decision, DisplayDuration, RetryReason, RetryState},
};

/// Where a download gets its bytes.
pub(crate) enum MediaSource<'a> {
    /// A Drive request (`files.get?alt=media`, `files.export`) through the media client.
    /// `label` names the transfer in warnings.
    Drive {
        client: MediaClient<'a>,
        url: url::Url,
        label: String,
    },
    /// A body that is already there and cannot be requested again (tests).
    #[cfg(test)]
    Body(Option<Body>),
}

/// A reply to a media request.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,
}

impl MediaSource<'_> {
    fn label(&self) -> &str {
        match self {
            MediaSource::Drive { label, .. } => label,
            #[cfg(test)]
            MediaSource::Body(_) => "download",
        }
    }

    /// Requests the content from `offset` on (all of it for 0), retried per `retry`.
    async fn open(&mut self, offset: u64, retry: &mut RetryState) -> Result<Reply, MediaError> {
        match self {
            MediaSource::Drive { client, url, label } => {
                let response = client
                    .send_with_retry(retry, label, || {
                        let mut request = Request::get(url.as_str());
                        if offset > 0 {
                            request = request.header(RANGE, format!("bytes={offset}-"));
                        }
                        request.body(google_apis_common::to_body::<String>(None))
                    })
                    .await?;
                let (parts, body) = response.into_parts();
                Ok(Reply {
                    status: parts.status,
                    headers: parts.headers,
                    body: body.boxed(),
                })
            }
            #[cfg(test)]
            MediaSource::Body(body) => match body.take() {
                Some(body) => Ok(Reply {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body,
                }),
                None => Err(MediaError::Status {
                    status: StatusCode::GONE,
                    message: Some(String::from("the test body can be read once")),
                    stop: None,
                }),
            },
        }
    }
}

/// What to write next.
pub(crate) enum Next {
    /// Write this body. With `restart`, it is the content from the start: discard what was
    /// written before.
    Body { body: Body, restart: bool },
    /// The content is complete.
    Complete,
}

/// One download, from the first request to the last byte.
pub(crate) struct ResumableDownload<'a> {
    source: MediaSource<'a>,
    retry: RetryState,
    /// Bytes of the content received in order: the next request starts here.
    received: u64,
    /// The furthest `received` has been. Only bytes past it are progress: after a restart, the
    /// same bytes again do not renew the retry budget.
    high_water: u64,
    /// The size Drive's metadata gives, if any.
    expected_size: Option<u64>,
    /// The size the server announced (`Content-Length` of a 200, the total of a 206).
    server_total: Option<u64>,
    /// Request the content from the start next time, without `Range`.
    from_start: bool,
}

impl<'a> ResumableDownload<'a> {
    /// A download of `source` under `retry`. `expected_size` is the size from Drive's metadata.
    #[must_use]
    pub(crate) fn new(
        source: MediaSource<'a>,
        retry: RetryState,
        expected_size: Option<u64>,
    ) -> Self {
        ResumableDownload {
            source,
            retry,
            received: 0,
            high_water: 0,
            expected_size,
            server_total: None,
            from_start: false,
        }
    }

    /// Bytes received in order so far.
    #[must_use]
    pub(crate) fn received(&self) -> u64 {
        self.received
    }

    /// The size the content should have: what the server announced, else Drive's metadata.
    #[must_use]
    pub(crate) fn total(&self) -> Option<u64> {
        self.server_total.or(self.expected_size)
    }

    /// Requests the content where it stopped (from the start the first time).
    pub(crate) async fn next(&mut self) -> Result<Next, MediaError> {
        loop {
            let offset = if self.from_start { 0 } else { self.received };
            let reply = self.source.open(offset, &mut self.retry).await?;

            match reply.status {
                StatusCode::OK => {
                    self.from_start = false;
                    let restart = self.received > 0;
                    self.received = 0;
                    self.server_total = content_length(&reply.headers);
                    return Ok(Next::Body {
                        body: reply.body,
                        restart,
                    });
                }
                StatusCode::PARTIAL_CONTENT if offset > 0 => {
                    let known_total = self.total();
                    let continues = content_range(&reply.headers).is_some_and(|range| {
                        range.first == offset
                            && range.total.is_none_or(|total| range.last < total)
                            && match (range.total, known_total) {
                                (Some(total), Some(known)) => total == known,
                                _ => true,
                            }
                    });
                    if continues {
                        if self.server_total.is_none() {
                            self.server_total =
                                content_range(&reply.headers).and_then(|range| range.total);
                        }
                        return Ok(Next::Body {
                            body: reply.body,
                            restart: false,
                        });
                    }
                    self.failed(RetryReason::BadRange).await?;
                    self.from_start = true;
                }
                StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
                    let size = unsatisfied_range_total(&reply.headers).or(self.total());
                    if size == Some(self.received) {
                        return Ok(Next::Complete);
                    }
                    self.failed(RetryReason::BadRange).await?;
                    self.from_start = true;
                }
                status => {
                    return Err(MediaError::Status {
                        status,
                        message: None,
                        stop: None,
                    })
                }
            }
        }
    }

    /// `bytes` bytes were written.
    pub(crate) fn advance(&mut self, bytes: u64) {
        self.received = self.received.saturating_add(bytes);
        if self.received > self.high_water {
            self.high_water = self.received;
            self.retry.progress();
        }
    }

    /// The body broke off with `error`. Waits and returns `Ok` when the download is to be
    /// resumed with [`ResumableDownload::next`], or gives the error back when not.
    pub(crate) async fn interrupted(&mut self, error: hyper::Error) -> Result<(), hyper::Error> {
        match self.retry.next(classify_body_error(&error)) {
            Decision::Retry { after, reason } => {
                self.warn(&reason, after);
                tokio::time::sleep(after).await;
                Ok(())
            }
            Decision::Stop { .. } => Err(error),
        }
    }

    /// The body ended. `true` when the content is complete; a body that ended early without an
    /// error is a failure like a broken one, and `false` means resume.
    pub(crate) async fn ended(&mut self) -> Result<bool, MediaError> {
        match self.total() {
            Some(total) if self.received < total => {
                self.failed(RetryReason::BodyRead).await?;
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    /// Counts a failed attempt and waits before the next one, or gives up.
    async fn failed(&mut self, reason: RetryReason) -> Result<(), MediaError> {
        match self.retry.next(Decision::Retry {
            after: Duration::ZERO,
            reason,
        }) {
            Decision::Retry { after, reason } => {
                self.warn(&reason, after);
                tokio::time::sleep(after).await;
                Ok(())
            }
            Decision::Stop { reason } => Err(MediaError::Resume { reason }),
        }
    }

    fn warn(&self, reason: &RetryReason, after: Duration) {
        eprintln!(
            "Warning: {}: interrupted after {} bytes ({reason}), resuming in {} (attempt {} of {})",
            self.source.label(),
            self.received,
            DisplayDuration(after),
            self.retry.failures() + 1,
            self.retry.policy().max_attempts,
        );
    }
}

/// Why streaming a body stopped.
pub(crate) enum BodyError {
    Read(hyper::Error),
    Write(std::io::Error),
}

/// Writes `body` to `writer` and counts the bytes in `download`.
pub(crate) async fn copy_body<W>(
    body: Body,
    writer: &mut W,
    download: &mut ResumableDownload<'_>,
) -> Result<(), BodyError>
where
    W: AsyncWrite + Unpin,
{
    let mut data = body.into_data_stream();
    while let Some(chunk) = data.next().await {
        let chunk = chunk.map_err(BodyError::Read)?;
        writer.write_all(&chunk).await.map_err(BodyError::Write)?;
        download.advance(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
    }
    Ok(())
}

fn header_str(headers: &HeaderMap, name: hyper::header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, CONTENT_LENGTH)?.trim().parse().ok()
}

/// `Content-Range: bytes <first>-<last>/<total or *>` of a 206.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentRange {
    first: u64,
    last: u64,
    total: Option<u64>,
}

fn content_range(headers: &HeaderMap) -> Option<ContentRange> {
    parse_content_range(header_str(headers, CONTENT_RANGE)?)
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first = first.trim().parse().ok()?;
    let last = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    (first <= last).then_some(ContentRange { first, last, total })
}

/// `Content-Range: bytes */<total>` of a 416.
fn unsatisfied_range_total(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, CONTENT_RANGE)?
        .trim()
        .strip_prefix("bytes */")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_content_range, ContentRange};

    #[test]
    fn content_range_values() {
        assert_eq!(
            parse_content_range("bytes 5-9/10"),
            Some(ContentRange {
                first: 5,
                last: 9,
                total: Some(10)
            })
        );
        assert_eq!(
            parse_content_range("bytes 5-9/*"),
            Some(ContentRange {
                first: 5,
                last: 9,
                total: None
            })
        );
        assert_eq!(parse_content_range("bytes */10"), None);
        assert_eq!(parse_content_range("bytes 9-5/10"), None);
        assert_eq!(parse_content_range("items 5-9/10"), None);
        assert_eq!(parse_content_range("bytes 5-x/10"), None);
    }
}
