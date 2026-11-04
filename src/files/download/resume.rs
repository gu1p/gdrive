//! Resumable media downloads (#25, #78): when the body of a download breaks off (a connection
//! reset, an HTTP/2 GOAWAY or stream reset), the content is requested again from the byte where
//! it stopped, with `Range: bytes=<received>-`, within the retry policy of the hub. Bytes that
//! arrive start a new retry budget.
//!
//! A resume continues only the same content, checked twice:
//! - With an [`Identity`], the file's metadata (`version`, `md5Checksum`, `size`,
//!   `modifiedTime`) is fetched again before each resume; when any of them differs from what the
//!   download started with, the content is downloaded again from the start, and checked against
//!   the new md5 and size.
//! - A strong `ETag` of the reply is sent back as `If-Range` with the `Range`: the server then
//!   continues only the same content and sends all of it (200) when it changed.
//!
//! The replies to a `Range` request:
//! - 206 is used only when its `Content-Range` starts at the received offset and announces the
//!   same total as before; otherwise the content is requested again from the start.
//! - 200 is the whole content again (it changed, or the server ignores `Range` as Workspace
//!   exports do): the writer discards what it holds and starts over ([`Next::Body`] with
//!   `restart`), and the md5 and size to check are fetched again.
//! - 416 means the range starts at the end or past it: complete when the size says so,
//!   otherwise the content is requested again from the start.
//!
//! Resume is in-process only.
// TODO(sync): resume across runs: persist the received offset with the file's revision and
// ETag (a sidecar next to the temporary file) and check both before continuing.

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use google_apis_common::Body;
use http_body_util::BodyExt;
use hyper::{
    header::{HeaderMap, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE},
    Request, StatusCode,
};
use md5::Digest;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::{
    common::{
        media_client::{MediaClient, MediaError},
        parse_md5_digest,
        retry::{
            classify_body_error, retry_read, Decision, DisplayDuration, RetryReason, RetryState,
        },
    },
    hub::Hub,
    // Warnings go above the progress bars.
    transfer::progress::eprintln,
};

/// What a finished download is checked against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Expected {
    /// Drive's `md5Checksum`; Workspace documents and exports have none.
    pub(crate) md5: Option<Digest>,
    /// Drive's `size`. Without it, the size the server announced is checked.
    pub(crate) size: Option<u64>,
}

/// The fields of a file that change with its content. Two revisions differ when a field known in
/// both differs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Revision {
    pub(crate) version: Option<i64>,
    pub(crate) md5: Option<Digest>,
    pub(crate) size: Option<u64>,
    pub(crate) modified_time: Option<DateTime<Utc>>,
}

/// The `fields` mask of a [`Revision`].
pub(crate) const REVISION_FIELDS: &str = "id,version,md5Checksum,size,modifiedTime";

impl Revision {
    #[must_use]
    pub(crate) fn of(file: &google_drive3::api::File) -> Self {
        Revision {
            version: file.version,
            md5: file.md5_checksum.as_deref().and_then(parse_md5_digest),
            size: file.size.and_then(|size| u64::try_from(size).ok()),
            modified_time: file.modified_time,
        }
    }

    fn differs_from(&self, other: &Revision) -> bool {
        fn differ<T: PartialEq>(this: Option<T>, other: Option<T>) -> bool {
            matches!((this, other), (Some(this), Some(other)) if this != other)
        }
        differ(self.version, other.version)
            || differ(self.md5, other.md5)
            || differ(self.size, other.size)
            || differ(self.modified_time, other.modified_time)
    }

    /// Takes the fields this revision does not know from `other`, the same revision.
    fn fill_from(&mut self, other: &Revision) {
        self.version = self.version.or(other.version);
        self.md5 = self.md5.or(other.md5);
        self.size = self.size.or(other.size);
        self.modified_time = self.modified_time.or(other.modified_time);
    }

    /// The md5 and size a download of this revision is checked against.
    #[must_use]
    pub(crate) fn expected(&self) -> Expected {
        Expected {
            md5: self.md5,
            size: self.size,
        }
    }
}

/// The Drive file a download reads, and the revision it started with.
pub(crate) struct Identity<'a> {
    hub: &'a Hub,
    file_id: String,
    revision: Revision,
}

impl<'a> Identity<'a> {
    /// `revision` is what the metadata said before the download started.
    #[must_use]
    pub(crate) fn new(hub: &'a Hub, file_id: &str, revision: Revision) -> Self {
        Identity {
            hub,
            file_id: file_id.to_owned(),
            revision,
        }
    }

    /// The file's revision now.
    async fn fetch(&self) -> Result<Revision, MediaError> {
        let hub = self.hub;
        let file_id = self.file_id.as_str();
        let (_, file) = retry_read(hub.retrier(), "files.get", |mut delegate| async move {
            hub.files()
                .get(file_id)
                .param("fields", REVISION_FIELDS)
                .supports_all_drives(true)
                .add_scope(google_drive3::api::Scope::Full)
                .delegate(&mut delegate)
                .doit()
                .await
        })
        .await
        .map_err(|err| MediaError::Metadata(Box::new(err)))?;
        Ok(Revision::of(&file))
    }
}

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

    /// Requests the content from `offset` on (all of it for 0), retried per `retry`. With
    /// `if_range` (an `ETag`), the server sends the range only if the content is still that one.
    async fn open(
        &mut self,
        offset: u64,
        if_range: Option<&str>,
        retry: &mut RetryState,
    ) -> Result<Reply, MediaError> {
        match self {
            MediaSource::Drive { client, url, label } => {
                let response = client
                    .send_with_retry(retry, label, || {
                        let mut request = Request::get(url.as_str());
                        if offset > 0 {
                            request = request.header(RANGE, format!("bytes={offset}-"));
                            if let Some(etag) = if_range {
                                request = request.header(IF_RANGE, etag);
                            }
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
    /// The md5 and size from Drive's metadata, if any; replaced when the file changes.
    expected: Expected,
    /// The size the server announced (`Content-Length` of a 200, the total of a 206).
    server_total: Option<u64>,
    /// The strong `ETag` of the content being received, sent back as `If-Range`.
    etag: Option<String>,
    /// The file to check before each resume, if the source is a Drive file.
    identity: Option<Identity<'a>>,
    /// Request the content from the start next time, without `Range`.
    from_start: bool,
}

impl<'a> ResumableDownload<'a> {
    /// A download of `source` under `retry`, checked against `expected` from Drive's metadata.
    #[must_use]
    pub(crate) fn new(source: MediaSource<'a>, retry: RetryState, expected: Expected) -> Self {
        ResumableDownload {
            source,
            retry,
            received: 0,
            high_water: 0,
            expected,
            server_total: None,
            etag: None,
            identity: None,
            from_start: false,
        }
    }

    /// Resumes only while the file is still at the revision of `identity` (see the module
    /// documentation).
    #[must_use]
    pub(crate) fn with_identity(mut self, identity: Identity<'a>) -> Self {
        self.identity = Some(identity);
        self
    }

    /// What the content is checked against when it is complete: Drive's md5 and size, of the
    /// revision that was downloaded.
    #[must_use]
    pub(crate) fn expected(&self) -> Expected {
        self.expected
    }

    /// Bytes received in order so far.
    #[must_use]
    pub(crate) fn received(&self) -> u64 {
        self.received
    }

    /// The size the content should have: what the server announced, else Drive's metadata.
    #[must_use]
    pub(crate) fn total(&self) -> Option<u64> {
        self.server_total.or(self.expected.size)
    }

    /// Requests the content where it stopped (from the start the first time).
    pub(crate) async fn next(&mut self) -> Result<Next, MediaError> {
        loop {
            if self.received > 0 && !self.from_start && self.file_changed().await? {
                self.from_start = true;
            }
            let offset = if self.from_start { 0 } else { self.received };
            let if_range = if offset > 0 { self.etag.clone() } else { None };
            let reply = self
                .source
                .open(offset, if_range.as_deref(), &mut self.retry)
                .await?;

            match reply.status {
                StatusCode::OK => {
                    if offset > 0 {
                        // The whole content to a Range request: with If-Range, it changed.
                        self.refresh_expected().await?;
                    }
                    self.from_start = false;
                    let restart = self.received > 0;
                    self.received = 0;
                    self.server_total = content_length(&reply.headers);
                    self.etag = strong_etag(&reply.headers);
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

    /// Fetches the file's revision and compares it with the one the download started with. On
    /// a change, the new revision's md5 and size become what the download is checked against.
    async fn file_changed(&mut self) -> Result<bool, MediaError> {
        let Some(identity) = self.identity.as_mut() else {
            return Ok(false);
        };
        let current = identity.fetch().await?;
        if identity.revision.differs_from(&current) {
            eprintln!(
                "Warning: {}: the file changed on Drive since the download started, downloading \
                 it again from the start",
                self.source.label()
            );
            self.expected = current.expected();
            identity.revision = current;
            Ok(true)
        } else {
            identity.revision.fill_from(&current);
            Ok(false)
        }
    }

    /// The server sends the content from the start again: take the md5 and size of the
    /// file's revision now, which that content should match.
    async fn refresh_expected(&mut self) -> Result<(), MediaError> {
        if let Some(identity) = self.identity.as_mut() {
            let current = identity.fetch().await?;
            self.expected = current.expected();
            identity.revision = current;
        }
        Ok(())
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

/// A strong `ETag` (`"..."`); a weak one (`W/"..."`) cannot be used with `If-Range`.
fn strong_etag(headers: &HeaderMap) -> Option<String> {
    let value = header_str(headers, ETAG)?.trim();
    (value.len() >= 2 && value.starts_with('"') && value.ends_with('"')).then(|| value.to_owned())
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
    use hyper::header::{HeaderMap, HeaderValue, ETAG};

    use super::{parse_content_range, strong_etag, ContentRange, Revision};

    #[test]
    fn revisions_differ_only_in_fields_both_know() {
        let md5 = md5::compute(b"a");
        let base = Revision {
            version: Some(7),
            md5: Some(md5),
            size: Some(10),
            modified_time: None,
        };
        let same = Revision {
            modified_time: Some("2026-11-01T10:00:00Z".parse().unwrap()),
            ..base
        };
        assert!(
            !base.differs_from(&same),
            "a field one side lacks is no change"
        );
        assert!(!base.differs_from(&Revision::default()));
        for changed in [
            Revision {
                version: Some(8),
                ..base
            },
            Revision {
                md5: Some(md5::compute(b"b")),
                ..base
            },
            Revision {
                size: Some(11),
                ..base
            },
        ] {
            assert!(base.differs_from(&changed), "{changed:?}");
        }

        let mut filled = base;
        filled.fill_from(&same);
        assert_eq!(filled, same);
        let later = Revision {
            modified_time: Some("2026-11-01T10:05:00Z".parse().unwrap()),
            ..base
        };
        assert!(
            filled.differs_from(&later),
            "a filled-in field counts from then on"
        );
    }

    #[test]
    fn only_strong_etags_are_kept() {
        let etag = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(ETAG, HeaderValue::from_static(value));
            strong_etag(&headers)
        };
        assert_eq!(etag("\"abc\""), Some(String::from("\"abc\"")));
        assert_eq!(etag("W/\"abc\""), None);
        assert_eq!(etag("abc"), None);
        assert_eq!(strong_etag(&HeaderMap::new()), None);
    }

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
