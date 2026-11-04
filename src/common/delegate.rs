use std::{
    fmt::{self, Display},
    str::FromStr,
};

use error_trace::ErrorTrace;
use hyper::{self, http};

// `super::` rather than `crate::common::`, so that tests/resumable_upload.rs can compile this
// module next to human_size.rs and retry.rs.
use super::{
    human_size::human_size,
    retry::{
        classify_status, classify_transport, Decision, RetryPolicy, RetryReason, RetryState,
        Throttle,
    },
};

#[derive(Debug, Clone, Default)]
pub(crate) struct UploadDelegateConfig {
    pub(crate) chunk_size: ChunkSize,
    /// The budget of one upload; a chunk the server accepts starts a new one.
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) print_chunk_errors: bool,
    pub(crate) print_chunk_info: bool,
}

/// The google-drive3 delegate of an upload: chunk size, chunk messages, and the retry decisions
/// of the vendored resumable-upload helper and of google-drive3's request loop, made by the
/// retry policy (`retry.rs`).
///
/// The helper's retry contract (vendor/google-apis-common/GDRIVE-PATCHES.md): after
/// `Retry::After` a failed chunk is resent from its first byte, a transport error resends the
/// same chunk, and a 308 that took none of the chunk is handed to `http_failure` like a failure.
///
/// Throttle limitation: the helper calls the delegate synchronously and never awaits it, so a
/// rate limit signaled to the shared [`Throttle`] by another request does not hold back the
/// chunks of an upload that is already running; only its retries (whose `Retry::After` includes
/// the throttle's hold) and the start of the next upload wait for it. Waiting for the throttle
/// before every chunk needs the gdrive-owned resumable uploader (milestone 2, item 1 of
/// docs/triage/README.md), which sends its chunks itself.
pub(crate) struct UploadDelegate<'a> {
    config: &'a UploadDelegateConfig,
    retry: RetryState,
    resumable_upload_url: Option<String>,
    previous_chunk: Option<google_apis_common::ContentRange>,
}

impl<'a> UploadDelegate<'a> {
    /// A delegate whose rate limits hold back, and which waits for, every request that shares
    /// `throttle` (the hub's).
    ///
    /// The delegate cannot wait before a chunk by itself (google-drive3 calls it synchronously):
    /// callers wait for the throttle before they start the upload, and retries wait for it.
    #[must_use]
    pub(crate) fn new(config: &'a UploadDelegateConfig, throttle: &Throttle) -> Self {
        UploadDelegate {
            config,
            retry: RetryState::new(config.retry_policy, Some(throttle.clone())),
            resumable_upload_url: None,
            previous_chunk: None,
        }
    }

    fn print_chunk_info(&self, chunk: &google_apis_common::ContentRange) {
        if self.config.print_chunk_info {
            if let Some(range) = &chunk.range {
                let chunk_size = if range.last < u64::MAX {
                    (range.last + 1).saturating_sub(range.first)
                } else {
                    (range.last - range.first).saturating_sub(1)
                };

                let action = if Some(chunk) == self.previous_chunk.as_ref() {
                    "Retrying"
                } else {
                    "Uploading"
                };

                println!(
                    "Info: {} {} chunk ({}-{} of {})",
                    action,
                    human_size(chunk_size),
                    range.first,
                    range.last,
                    chunk.total_length
                );
            }
        }
    }

    /// A chunk that starts past the previous one means the server took bytes: progress.
    fn note_progress(&mut self, chunk: &google_apis_common::ContentRange) {
        let previous_first = self
            .previous_chunk
            .as_ref()
            .and_then(|previous| previous.range.as_ref())
            .map(|range| range.first);
        let first = chunk.range.as_ref().map(|range| range.first);
        if let (Some(previous_first), Some(first)) = (previous_first, first) {
            if first > previous_first {
                self.retry.progress();
            }
        }
    }
}

fn to_retry(decision: &Decision) -> google_apis_common::Retry {
    match decision {
        Decision::Retry { after, .. } => google_apis_common::Retry::After(*after),
        Decision::Stop { .. } => google_apis_common::Retry::Abort,
    }
}

impl google_apis_common::Delegate for UploadDelegate<'_> {
    fn chunk_size(&mut self) -> u64 {
        self.config.chunk_size.in_bytes()
    }

    fn cancel_chunk_upload(&mut self, chunk: &google_apis_common::ContentRange) -> bool {
        self.note_progress(chunk);
        self.print_chunk_info(chunk);
        self.previous_chunk = Some(chunk.clone());

        false
    }

    fn store_upload_url(&mut self, url: Option<&str>) {
        self.resumable_upload_url = url.map(ToString::to_string);
    }

    fn upload_url(&mut self) -> Option<String> {
        self.resumable_upload_url.clone()
    }

    fn http_error(&mut self, err: &hyper_util::client::legacy::Error) -> google_apis_common::Retry {
        if self.config.print_chunk_errors {
            eprintln!("Warning: Failed attempt to upload chunk: {}", err.trace());
        }
        to_retry(&self.retry.next(classify_transport(err)))
    }

    fn http_failure(
        &mut self,
        res: &google_apis_common::Response,
        err: Option<&serde_json::Value>,
    ) -> google_apis_common::Retry {
        let status = res.status();

        // A 308 Resume Incomplete reaches the delegate only when a chunk made no progress (see
        // vendor/google-apis-common/GDRIVE-PATCHES.md): resend it like a 5xx, within the budget.
        let classified = if status == http::StatusCode::PERMANENT_REDIRECT {
            Decision::Retry {
                after: std::time::Duration::ZERO,
                reason: RetryReason::NoProgress,
            }
        } else {
            classify_status(status, Some(res.headers()), err)
        };
        let decision = self.retry.next(classified);

        if self.config.print_chunk_errors && matches!(decision, Decision::Retry { .. }) {
            eprintln!(
                "Warning: Failed attempt to upload chunk. Status code: {}, body: {:?}",
                status,
                res.body()
            );
        }
        to_retry(&decision)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) enum ChunkSize {
    Approx1,
    Approx2,
    Approx4,
    Approx8,
    Approx16,
    #[default]
    Approx32,
    Approx64,
    Approx128,
    Approx256,
    Approx512,
    Approx1024,
    Approx2048,
    Approx4096,
    Approx8192,
}

impl ChunkSize {
    #[must_use]
    pub(crate) fn in_bytes(&self) -> u64 {
        let exponent = match self {
            ChunkSize::Approx1 => 20,
            ChunkSize::Approx2 => 21,
            ChunkSize::Approx4 => 22,
            ChunkSize::Approx8 => 23,
            ChunkSize::Approx16 => 24,
            ChunkSize::Approx32 => 25,
            ChunkSize::Approx64 => 26,
            ChunkSize::Approx128 => 27,
            ChunkSize::Approx256 => 28,
            ChunkSize::Approx512 => 29,
            ChunkSize::Approx1024 => 30,
            ChunkSize::Approx2048 => 31,
            ChunkSize::Approx4096 => 32,
            ChunkSize::Approx8192 => 33,
        };

        u64::pow(2, exponent)
    }
}

impl FromStr for ChunkSize {
    type Err = InvalidChunkSize;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1" => Ok(ChunkSize::Approx1),
            "2" => Ok(ChunkSize::Approx2),
            "4" => Ok(ChunkSize::Approx4),
            "8" => Ok(ChunkSize::Approx8),
            "16" => Ok(ChunkSize::Approx16),
            "32" => Ok(ChunkSize::Approx32),
            "64" => Ok(ChunkSize::Approx64),
            "128" => Ok(ChunkSize::Approx128),
            "256" => Ok(ChunkSize::Approx256),
            "512" => Ok(ChunkSize::Approx512),
            "1024" => Ok(ChunkSize::Approx1024),
            "2048" => Ok(ChunkSize::Approx2048),
            "4096" => Ok(ChunkSize::Approx4096),
            "8192" => Ok(ChunkSize::Approx8192),
            _ => Err(InvalidChunkSize),
        }
    }
}

impl Display for ChunkSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ChunkSize::Approx1 => "1",
            ChunkSize::Approx2 => "2",
            ChunkSize::Approx4 => "4",
            ChunkSize::Approx8 => "8",
            ChunkSize::Approx16 => "16",
            ChunkSize::Approx32 => "32",
            ChunkSize::Approx64 => "64",
            ChunkSize::Approx128 => "128",
            ChunkSize::Approx256 => "256",
            ChunkSize::Approx512 => "512",
            ChunkSize::Approx1024 => "1024",
            ChunkSize::Approx2048 => "2048",
            ChunkSize::Approx4096 => "4096",
            ChunkSize::Approx8192 => "8192",
        };

        f.write_str(s)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidChunkSize;

impl Display for InvalidChunkSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a valid chunk size, must be a power of 2 between 1 and 8192")
    }
}

impl std::error::Error for InvalidChunkSize {}
