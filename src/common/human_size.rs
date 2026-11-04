//! Byte counts in the format of gdrive 3.x output, which scripts parse: base 1000, upper-case
//! units, no trailing `.0` (`999 B`, `1.5 KB`, `2 MB`).

/// Formats `bytes` like gdrive 3.x did, with `human_bytes` built without its `si-units` feature.
pub(crate) fn human_size(bytes: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "f64 is exact up to 2^53 bytes, and only one decimal is printed"
    )]
    let bytes = bytes as f64;
    human_bytes::human_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn matches_gdrive_3_output() {
        let cases = [
            (0, "0 B"),
            (1, "1 B"),
            (999, "999 B"),
            (1_000, "1 KB"),
            (1_500, "1.5 KB"),
            (12_345, "12.3 KB"),
            (999_999, "1000 KB"),
            (1_000_000, "1 MB"),
            (1_048_576, "1 MB"),
            (2_000_000, "2 MB"),
            (33_554_432, "33.6 MB"),
            (1_000_000_000, "1 GB"),
            (5_497_558_138_880, "5.5 TB"),
        ];

        for (bytes, expected) in cases {
            assert_eq!(human_size(bytes), expected, "{bytes} bytes");
        }
    }
}
