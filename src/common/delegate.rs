//! The upload settings of the command line (`--chunk-size`, `--multipart-threshold`) and the
//! configuration of `files::upload::upload_file`.
//!
//! The google-apis-common delegate that used to live here, which made the retry decisions of
//! google-apis-common's resumable-upload helper, is gone: uploads go through gdrive's own
//! uploader (`transfer::upload`). The module keeps its name so that the callers of
//! `upload_file` and of these types keep compiling.

use std::{
    fmt::{self, Display},
    str::FromStr,
};

use super::retry::RetryPolicy;
use crate::transfer::upload::{UploadOptions, DEFAULT_MULTIPART_THRESHOLD};

/// The configuration of `files::upload::upload_file`, which uploads through
/// [`transfer::upload::upload_reader`](crate::transfer::upload::upload_reader). New callers use
/// `transfer::upload::upload_path` with [`UploadOptions`].
#[derive(Debug, Clone, Default)]
pub(crate) struct UploadDelegateConfig {
    pub(crate) chunk_size: ChunkSize,
    pub(crate) multipart_threshold: MultipartThreshold,
    /// The budget of one upload; a chunk the server accepts starts a new one.
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) print_chunk_errors: bool,
    pub(crate) print_chunk_info: bool,
}

impl UploadDelegateConfig {
    /// The uploader's options for this configuration: its chunk size, multipart threshold and
    /// messages, md5 verification, no session records.
    #[must_use]
    pub(crate) fn options(&self) -> UploadOptions {
        UploadOptions {
            chunk_size: self.chunk_size.in_bytes(),
            multipart_threshold: self.multipart_threshold.in_bytes(),
            print_chunk_errors: self.print_chunk_errors,
            print_chunk_info: self.print_chunk_info,
            ..UploadOptions::default()
        }
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

/// `--multipart-threshold`: files of at most this many MiB go up in one request, larger ones
/// through a resumable session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MultipartThreshold {
    mib: u64,
}

impl Default for MultipartThreshold {
    fn default() -> Self {
        MultipartThreshold {
            mib: DEFAULT_MULTIPART_THRESHOLD >> 20,
        }
    }
}

impl MultipartThreshold {
    #[must_use]
    pub(crate) fn in_bytes(self) -> u64 {
        self.mib << 20
    }
}

/// The largest threshold: Drive's 5 TiB file size limit.
const MAX_MULTIPART_THRESHOLD_MIB: u64 = 5 << 20;

impl FromStr for MultipartThreshold {
    type Err = InvalidMultipartThreshold;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse::<u64>() {
            Ok(mib) if mib <= MAX_MULTIPART_THRESHOLD_MIB => Ok(MultipartThreshold { mib }),
            _ => Err(InvalidMultipartThreshold),
        }
    }
}

impl Display for MultipartThreshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.mib)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidMultipartThreshold;

impl Display for InvalidMultipartThreshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a valid size, must be a whole number of MB between 0 and 5242880")
    }
}

impl std::error::Error for InvalidMultipartThreshold {}

#[cfg(test)]
mod tests {
    use super::{ChunkSize, MultipartThreshold, UploadDelegateConfig};

    #[test]
    fn the_multipart_threshold_is_in_mib() {
        assert_eq!(MultipartThreshold::default().in_bytes(), 5 << 20);
        assert_eq!(MultipartThreshold::default().to_string(), "5");
        assert_eq!("0".parse::<MultipartThreshold>().unwrap().in_bytes(), 0);
        assert_eq!(
            "64".parse::<MultipartThreshold>().unwrap().in_bytes(),
            64 << 20
        );
        for invalid in ["", "-1", "1.5", "5M", "5242881"] {
            assert!(invalid.parse::<MultipartThreshold>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn upload_file_keeps_its_chunk_size_threshold_and_messages() {
        let options = UploadDelegateConfig::default().options();
        assert_eq!(options.multipart_threshold, 5 << 20);

        let options = UploadDelegateConfig {
            chunk_size: ChunkSize::Approx8,
            multipart_threshold: "1".parse().unwrap(),
            print_chunk_info: true,
            ..UploadDelegateConfig::default()
        }
        .options();
        assert_eq!(options.chunk_size, 8 << 20);
        assert_eq!(options.multipart_threshold, 1 << 20);
        assert!(options.print_chunk_info && !options.print_chunk_errors);
        assert!(options.verify_md5);
        assert_eq!(options.session_dir, None);
    }
}
