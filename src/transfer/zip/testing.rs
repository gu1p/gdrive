//! Zip files for tests, written field by field so that a test can damage any of them.

use std::io::Write;

use flate2::{write::DeflateEncoder, Compression};

/// An entry of a zip file to write.
#[derive(Debug, Clone)]
pub(crate) struct Written {
    pub(crate) name: Vec<u8>,
    pub(crate) content: Vec<u8>,
    pub(crate) method: u16,
    pub(crate) flags: u16,
    /// Date and time fields: 2021-02-03 04:05:06 by default.
    pub(crate) dos: (u16, u16),
    /// The CRC-32 of the content, unless set.
    pub(crate) crc32: Option<u32>,
    /// The extra fields of the central directory.
    pub(crate) extra: Vec<u8>,
}

impl Written {
    fn new(name: &str, content: &[u8], method: u16) -> Self {
        Written {
            name: name.as_bytes().to_vec(),
            content: content.to_vec(),
            method,
            flags: 0,
            dos: ((41 << 9) | (2 << 5) | 3, (4 << 11) | (5 << 5) | 3),
            crc32: None,
            extra: Vec::new(),
        }
    }

    pub(crate) fn stored(name: &str, content: &[u8]) -> Self {
        Self::new(name, content, 0)
    }

    pub(crate) fn deflated(name: &str, content: &[u8]) -> Self {
        Self::new(name, content, 8)
    }

    pub(crate) fn folder(name: &str) -> Self {
        Self::new(name, b"", 0)
    }

    fn crc32(&self) -> u32 {
        self.crc32.unwrap_or_else(|| crc32fast::hash(&self.content))
    }

    /// The bytes the entry has in the zip file.
    fn packed(&self) -> Vec<u8> {
        if self.method != 8 {
            return self.content.clone();
        }
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&self.content).unwrap();
        encoder.finish().unwrap()
    }
}

fn u16_le(value: usize) -> [u8; 2] {
    u16::try_from(value).unwrap().to_le_bytes()
}

fn u32_le(value: usize) -> [u8; 4] {
    u32::try_from(value).unwrap().to_le_bytes()
}

/// A zip file of `entries`.
pub(crate) fn zip_of(entries: &[Written]) -> Vec<u8> {
    write_zip(entries, false)
}

/// A zip file of `entries` with every size and offset in ZIP64 fields only.
pub(crate) fn zip64_of(entries: &[Written]) -> Vec<u8> {
    write_zip(entries, true)
}

fn write_zip(entries: &[Written], zip64: bool) -> Vec<u8> {
    let mut zip = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let offset = zip.len();
        let packed = entry.packed();
        let crc32 = entry.crc32().to_le_bytes();
        // The local header has no sizes, as in a zip written to a stream.
        zip.extend(b"PK\x03\x04");
        zip.extend(20u16.to_le_bytes());
        zip.extend((entry.flags | 8).to_le_bytes());
        zip.extend(entry.method.to_le_bytes());
        zip.extend(entry.dos.1.to_le_bytes());
        zip.extend(entry.dos.0.to_le_bytes());
        zip.extend([0; 12]);
        zip.extend(u16_le(entry.name.len()));
        zip.extend(3u16.to_le_bytes());
        zip.extend(&entry.name);
        // Extra bytes of the local header alone: the content is not right after the name.
        zip.extend(b"xyz");
        zip.extend(&packed);
        zip.extend(b"PK\x07\x08");
        zip.extend(crc32);
        zip.extend(u32_le(packed.len()));
        zip.extend(u32_le(entry.content.len()));

        let mut extra = entry.extra.clone();
        if zip64 {
            extra.extend(EXTRA_ZIP64);
            extra.extend(24u16.to_le_bytes());
            extra.extend((entry.content.len() as u64).to_le_bytes());
            extra.extend((packed.len() as u64).to_le_bytes());
            extra.extend((offset as u64).to_le_bytes());
        }
        directory.extend(b"PK\x01\x02");
        directory.extend(20u16.to_le_bytes());
        directory.extend(20u16.to_le_bytes());
        directory.extend((entry.flags | 8).to_le_bytes());
        directory.extend(entry.method.to_le_bytes());
        directory.extend(entry.dos.1.to_le_bytes());
        directory.extend(entry.dos.0.to_le_bytes());
        directory.extend(crc32);
        if zip64 {
            directory.extend([0xff; 8]);
        } else {
            directory.extend(u32_le(packed.len()));
            directory.extend(u32_le(entry.content.len()));
        }
        directory.extend(u16_le(entry.name.len()));
        directory.extend(u16_le(extra.len()));
        directory.extend([0; 10]);
        if zip64 {
            directory.extend([0xff; 4]);
        } else {
            directory.extend(u32_le(offset));
        }
        directory.extend(&entry.name);
        directory.extend(&extra);
    }

    let start = zip.len();
    let count = entries.len();
    zip.extend(&directory);
    if zip64 {
        let end64 = zip.len();
        zip.extend(b"PK\x06\x06");
        zip.extend(44u64.to_le_bytes());
        zip.extend([20, 0, 20, 0]);
        zip.extend([0; 8]);
        zip.extend((count as u64).to_le_bytes());
        zip.extend((count as u64).to_le_bytes());
        zip.extend((directory.len() as u64).to_le_bytes());
        zip.extend((start as u64).to_le_bytes());
        zip.extend(b"PK\x06\x07");
        zip.extend([0; 4]);
        zip.extend((end64 as u64).to_le_bytes());
        zip.extend(1u32.to_le_bytes());
        zip.extend(b"PK\x05\x06");
        zip.extend([0; 4]);
        zip.extend([0xff; 12]);
        zip.extend([0; 2]);
    } else {
        zip.extend(b"PK\x05\x06");
        zip.extend([0; 4]);
        zip.extend(u16_le(count));
        zip.extend(u16_le(count));
        zip.extend(u32_le(directory.len()));
        zip.extend(u32_le(start));
        zip.extend([0; 2]);
    }
    zip
}

const EXTRA_ZIP64: [u8; 2] = [1, 0];

/// Bytes that do not compress and repeat at no short distance.
pub(crate) fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}
