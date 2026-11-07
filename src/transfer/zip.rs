//! A zip file read where it is: its list of entries from its end, and the content of one entry
//! by [`EntrySource`], as the uploader asks for it. Nothing is held on disk.
//!
//! The central directory at the end of the file is what counts, as in other readers: the sizes
//! of a local header are often left out (a zip written to a stream has them after the content).
//! Stored and deflated entries can be read. ZIP64 is read, a zip split over several files is not.
//!
//! An uploader goes back when it sends a chunk again, and a deflate stream can only be read from
//! its start. [`EntrySource`] keeps the last chunk in memory for that, so a retry downloads
//! nothing again. Further back it starts the entry over.

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

use std::{
    collections::VecDeque,
    error::Error,
    fmt::{self, Display, Formatter},
    io,
};

use chrono::{DateTime, TimeZone, Utc};
use flate2::{Decompress, FlushDecompress, Status};

use super::upload::Source;

const END_SIGNATURE: &[u8] = b"PK\x05\x06";
const END_LEN: usize = 22;
const MAX_COMMENT: usize = 65_535;
const LOCATOR_SIGNATURE: &[u8] = b"PK\x06\x07";
const LOCATOR_LEN: usize = 20;
const END64_SIGNATURE: &[u8] = b"PK\x06\x06";
const END64_LEN: usize = 56;
const CENTRAL_SIGNATURE: &[u8] = b"PK\x01\x02";
const CENTRAL_LEN: usize = 46;
const LOCAL_SIGNATURE: &[u8] = b"PK\x03\x04";
const LOCAL_LEN: usize = 30;

const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_TIMESTAMP: u16 = 0x5455;
const EXTRA_UNICODE_PATH: u16 = 0x7075;

const FLAG_ENCRYPTED: u16 = 1;
const FLAG_UTF8: u16 = 1 << 11;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// More than this is no list of files. 1 GiB holds at least 20 million entries.
const MAX_DIRECTORY: u64 = 1 << 30;
/// Deeper than this is no real tree: Drive itself stops at 100 levels.
const MAX_DEPTH: usize = 512;
/// Compressed bytes read, and content bytes made, at a time.
const STEP: usize = 64 * 1024;

/// The bytes 0x80 to 0xFF of code page 850, which names without a UTF-8 mark are in. The format
/// says 437, but Windows wrote the code page of its region: 850 in the Americas and Western
/// Europe. Both read the same for most accented letters; 437 has no `ã` or `õ`.
const CP850: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜø£Ø×ƒáíóúñÑªº¿®¬½¼¡«»░▒▓│┤ÁÂÀ©╣║╗╝¢¥┐└┴┬├─┼ãÃ╚╔╩╦╠═╬¤ðÐÊËÈıÍÎÏ┘┌█▄¦Ì▀ÓßÔÒõÕµþÞÚÛÙýÝ¯´\u{ad}±‗¾¶§÷¸°¨·¹³²■\u{a0}";

/// Why a zip file, or an entry of it, cannot be read.
#[derive(Debug)]
pub(crate) enum ZipError {
    Read(io::Error),
    /// No end record: it is no zip file, or only the start of one.
    NotZip,
    Split,
    Malformed(&'static str),
    Encrypted,
    Method(u16),
    /// The content is not what the list of entries says.
    Corrupt,
}

impl Display for ZipError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ZipError::Read(_) => f.write_str("unable to read the zip file"),
            ZipError::NotZip => f.write_str("it is no zip file, or an incomplete one"),
            ZipError::Split => f.write_str("it is one piece of a zip split over several files"),
            ZipError::Malformed(what) => write!(f, "the zip file is damaged: {what}"),
            ZipError::Encrypted => f.write_str("the file is encrypted in the zip"),
            ZipError::Method(method) => write!(
                f,
                "the file is compressed with method {method}; only stored and deflated files \
                 can be read"
            ),
            ZipError::Corrupt => f.write_str("the content in the zip is damaged (bad CRC-32)"),
        }
    }
}

impl Error for ZipError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ZipError::Read(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ZipError> for io::Error {
    fn from(error: ZipError) -> Self {
        match error {
            ZipError::Read(error) => error,
            other => io::Error::other(other),
        }
    }
}

/// One entry of a zip file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The folders down to it and its name, without any empty part, `.` or `..`. Never empty.
    pub(crate) path: Vec<String>,
    pub(crate) is_folder: bool,
    pub(crate) size: u64,
    pub(crate) crc32: u32,
    /// As the entry tells it in UTC, else its local time taken for UTC.
    pub(crate) modified: Option<DateTime<Utc>>,
    encrypted: bool,
    method: u16,
    compressed: u64,
    /// Where its local header is.
    offset: u64,
}

impl Entry {
    /// The last part of its path.
    pub(crate) fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    /// Why its content cannot be read, if so.
    pub(crate) fn unreadable(&self) -> Option<ZipError> {
        if self.encrypted {
            Some(ZipError::Encrypted)
        } else if self.size == 0 && self.crc32 != 0 {
            // No read would find out, as nothing of it is read.
            Some(ZipError::Corrupt)
        } else if !matches!(self.method, METHOD_STORED | METHOD_DEFLATE) {
            Some(ZipError::Method(self.method))
        } else {
            None
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | (u64::from(u32_at(bytes, at + 4)) << 32)
}

/// `value` as a length of memory; a value too large for it fails when the memory is asked for.
fn length(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Fills `buf` from `offset` on.
async fn read_exact<S: Source>(
    source: &mut S,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), ZipError> {
    let mut filled = 0;
    while filled < buf.len() {
        let read = source
            .read_at(offset + filled as u64, &mut buf[filled..])
            .await
            .map_err(ZipError::Read)?;
        if read == 0 {
            return Err(ZipError::Malformed("it ends too early"));
        }
        filled += read;
    }
    Ok(())
}

/// The entries of the zip file `source` of `size` bytes, as its central directory lists them.
/// An entry whose path is nothing but `/`, `.` and `..` has nowhere to go and is left out.
pub(crate) async fn read_directory<S: Source>(
    source: &mut S,
    size: u64,
) -> Result<Vec<Entry>, ZipError> {
    let (start, directory_size) = find_directory(source, size).await?;
    if directory_size > MAX_DIRECTORY {
        return Err(ZipError::Malformed("its list of files is too large"));
    }
    let mut directory = vec![0; length(directory_size)];
    read_exact(source, start, &mut directory).await?;
    parse_directory(&directory)
}

/// Where the central directory starts, and its size.
async fn find_directory<S: Source>(source: &mut S, size: u64) -> Result<(u64, u64), ZipError> {
    let tail_len = size.min((END_LEN + MAX_COMMENT) as u64);
    let tail_start = size - tail_len;
    let mut tail = vec![0; length(tail_len)];
    read_exact(source, tail_start, &mut tail).await?;
    // The signature may also be in a comment. The record whose comment ends the file is the end
    // record; only without one, the last that a few bytes follow, which some tools add.
    let found = (0..tail.len().saturating_sub(END_LEN - 1))
        .rev()
        .filter(|&at| tail[at..].starts_with(END_SIGNATURE));
    let comment_end = |at: usize| at + END_LEN + usize::from(u16_at(&tail, at + 20));
    let at = found
        .clone()
        .find(|&at| comment_end(at) == tail.len())
        .or_else(|| found.clone().find(|&at| comment_end(at) < tail.len()))
        .ok_or(ZipError::NotZip)?;
    let end = &tail[at..at + END_LEN];
    let end_at = tail_start + at as u64;

    let (start, directory_size) = if let Some(found) = read_end64(source, end_at).await? {
        found
    } else {
        if u16_at(end, 4) != 0 || u16_at(end, 6) != 0 {
            return Err(ZipError::Split);
        }
        (u64::from(u32_at(end, 16)), u64::from(u32_at(end, 12)))
    };
    if start
        .checked_add(directory_size)
        .is_none_or(|last| last > end_at)
    {
        return Err(ZipError::Malformed("its list of files is out of place"));
    }
    Ok((start, directory_size))
}

/// The start and size of the central directory from a ZIP64 end record, if there is one before
/// the end record at `end_at`.
async fn read_end64<S: Source>(
    source: &mut S,
    end_at: u64,
) -> Result<Option<(u64, u64)>, ZipError> {
    let Some(locator_at) = end_at.checked_sub(LOCATOR_LEN as u64) else {
        return Ok(None);
    };
    let mut locator = [0; LOCATOR_LEN];
    read_exact(source, locator_at, &mut locator).await?;
    if !locator.starts_with(LOCATOR_SIGNATURE) {
        return Ok(None);
    }
    if u32_at(&locator, 16) > 1 {
        return Err(ZipError::Split);
    }
    let mut end = [0; END64_LEN];
    read_exact(source, u64_at(&locator, 8), &mut end).await?;
    if !end.starts_with(END64_SIGNATURE) {
        return Err(ZipError::Malformed("its ZIP64 end record is missing"));
    }
    Ok(Some((u64_at(&end, 48), u64_at(&end, 40))))
}

fn parse_directory(mut bytes: &[u8]) -> Result<Vec<Entry>, ZipError> {
    let mut entries = Vec::new();
    // Below this no sum of sizes overflows, wherever they are added up.
    let mut left = i64::MAX.unsigned_abs();
    while !bytes.is_empty() {
        if bytes.len() < CENTRAL_LEN || !bytes.starts_with(CENTRAL_SIGNATURE) {
            return Err(ZipError::Malformed("its list of files is broken"));
        }
        let name_end = CENTRAL_LEN + usize::from(u16_at(bytes, 28));
        let extra_end = name_end + usize::from(u16_at(bytes, 30));
        let total = extra_end + usize::from(u16_at(bytes, 32));
        if bytes.len() < total {
            return Err(ZipError::Malformed("its list of files is broken"));
        }
        let name = &bytes[CENTRAL_LEN..name_end];
        if let Some(entry) = parse_entry(bytes, name, &bytes[name_end..extra_end])? {
            left = left.checked_sub(entry.size).ok_or(ZipError::Malformed(
                "its files add up to an impossible size",
            ))?;
            entries.push(entry);
        }
        bytes = &bytes[total..];
    }
    Ok(entries)
}

/// The entry of the central header `header`, with its name and extra fields.
fn parse_entry(header: &[u8], name: &[u8], extra: &[u8]) -> Result<Option<Entry>, ZipError> {
    let flags = u16_at(header, 8);
    let name = unicode_path(name, extra).unwrap_or_else(|| decode_name(name, flags));
    let is_folder = name.ends_with(['/', '\\']);
    let path = name
        .split(['/', '\\'])
        .filter(|part| !matches!(*part, "" | "." | ".."))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if path.len() > MAX_DEPTH {
        return Err(ZipError::Malformed("a path in it is too deep"));
    }
    if path.is_empty() {
        return Ok(None);
    }

    let mut size = u64::from(u32_at(header, 24));
    let mut compressed = u64::from(u32_at(header, 20));
    let mut offset = u64::from(u32_at(header, 42));
    let mut modified = dos_time(u16_at(header, 14), u16_at(header, 12));
    for (id, data) in extra_fields(extra) {
        match id {
            EXTRA_ZIP64 => {
                let mut values = data
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|value| u64::from_le_bytes(*value));
                for field in [&mut size, &mut compressed, &mut offset] {
                    if *field == u64::from(u32::MAX) {
                        *field = values
                            .next()
                            .ok_or(ZipError::Malformed("a ZIP64 field of a file is missing"))?;
                    }
                }
            }
            // The flags say which times follow, the modification time first.
            EXTRA_TIMESTAMP if data.len() >= 5 && data[0] & 1 == 1 => {
                let seconds = i32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                // Before 1970 is a clock that was wrong, not a time for Drive.
                if seconds >= 0 {
                    modified = DateTime::from_timestamp(i64::from(seconds), 0).or(modified);
                }
            }
            _ => {}
        }
    }
    Ok(Some(Entry {
        path,
        is_folder,
        size,
        crc32: u32_at(header, 16),
        modified,
        encrypted: flags & FLAG_ENCRYPTED != 0,
        method: u16_at(header, 10),
        compressed,
        offset,
    }))
}

/// A name marked as UTF-8 or that is valid UTF-8 (macOS leaves the mark out), else code page 850.
/// A Unicode Path field goes before all that ([`unicode_path`]).
fn decode_name(name: &[u8], flags: u16) -> String {
    if flags & FLAG_UTF8 != 0 {
        return String::from_utf8_lossy(name).into_owned();
    }
    match std::str::from_utf8(name) {
        Ok(name) => name.to_owned(),
        Err(_) => name
            .iter()
            .map(|&byte| match byte.checked_sub(0x80) {
                Some(high) => CP850.chars().nth(usize::from(high)).unwrap_or('\u{fffd}'),
                None => char::from(byte),
            })
            .collect(),
    }
}

/// The name in the Unicode Path field of `extra`, when that field is for the name `raw`: it holds
/// the CRC-32 of the name it replaces.
fn unicode_path(raw: &[u8], extra: &[u8]) -> Option<String> {
    let (_, data) = extra_fields(extra).find(|(id, _)| *id == EXTRA_UNICODE_PATH)?;
    if data.len() < 5 || data[0] != 1 || u32_at(data, 1) != crc32fast::hash(raw) {
        return None;
    }
    std::str::from_utf8(&data[5..]).ok().map(str::to_owned)
}

/// The id and data of each whole field of `extra`.
fn extra_fields(mut extra: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if extra.len() < 4 {
            return None;
        }
        let id = u16_at(extra, 0);
        let end = 4 + usize::from(u16_at(extra, 2));
        let data = extra.get(4..end)?;
        extra = &extra[end..];
        Some((id, data))
    })
}

/// The time of the fields of an entry, in no time zone, as if in UTC. `None` if it is no time.
fn dos_time(date: u16, time: u16) -> Option<DateTime<Utc>> {
    Utc.with_ymd_and_hms(
        1980 + i32::from(date >> 9),
        u32::from((date >> 5) & 0xf),
        u32::from(date & 0x1f),
        u32::from(time >> 11),
        u32::from((time >> 5) & 0x3f),
        u32::from(time & 0x1f) * 2,
    )
    .single()
}

/// The content made last, to serve a read that goes back.
#[derive(Debug)]
struct Window {
    bytes: VecDeque<u8>,
    capacity: usize,
}

impl Window {
    fn push(&mut self, data: &[u8]) {
        let data = &data[data.len().saturating_sub(self.capacity)..];
        let excess = (self.bytes.len() + data.len()).saturating_sub(self.capacity);
        self.bytes.drain(..excess);
        self.bytes.extend(data);
    }

    /// Copies what it holds at `offset` to `buf`, when the content made ends at `end`. `None`
    /// if it does not hold that offset.
    fn copy(&self, offset: u64, end: u64, buf: &mut [u8]) -> Option<usize> {
        let start = end - self.bytes.len() as u64;
        if offset < start || offset >= end {
            return None;
        }
        let skip = length(offset - start);
        let count = buf.len().min(self.bytes.len() - skip);
        for (to, from) in buf.iter_mut().zip(self.bytes.range(skip..skip + count)) {
            *to = *from;
        }
        Some(count)
    }
}

/// The content of one entry of a zip file, as the uploader reads it, checked against its CRC-32:
/// a read that would complete damaged content fails instead.
///
/// A read that is dropped half-way loses nothing: its state changes only between its waits.
pub(crate) struct EntrySource<S> {
    source: S,
    method: u16,
    compressed: u64,
    size: u64,
    crc32: u32,
    /// Where the local header of the entry is, and where its content starts once that was read.
    header: u64,
    data: Option<u64>,
    inflate: Decompress,
    input: Vec<u8>,
    /// The part of `input` not inflated yet.
    input_at: usize,
    input_end: usize,
    /// Compressed bytes read so far.
    taken: u64,
    /// Content bytes made so far, all of them part of `hasher`.
    made: u64,
    hasher: crc32fast::Hasher,
    output: Vec<u8>,
    window: Window,
}

impl<S: Source> EntrySource<S> {
    /// The content of `entry`, which must be readable ([`Entry::unreadable`]), from the zip file
    /// `source`. Up to `window` bytes back from the furthest read come from memory: an uploader's
    /// largest request is the right size. Further back the entry starts over. The memory holds
    /// one step more: what was made can be that far ahead of what was read, and no step may
    /// pass over an offset on its way.
    pub(crate) fn new(source: S, entry: &Entry, window: u64) -> Self {
        EntrySource {
            source,
            method: entry.method,
            compressed: entry.compressed,
            size: entry.size,
            crc32: entry.crc32,
            header: entry.offset,
            data: None,
            inflate: Decompress::new(false),
            input: Vec::new(),
            input_at: 0,
            input_end: 0,
            taken: 0,
            made: 0,
            hasher: crc32fast::Hasher::new(),
            output: Vec::new(),
            window: Window {
                bytes: VecDeque::new(),
                capacity: length(window.saturating_add(STEP as u64).min(entry.size)),
            },
        }
    }

    fn start_over(&mut self) {
        self.inflate.reset(false);
        self.input_at = 0;
        self.input_end = 0;
        self.taken = 0;
        self.made = 0;
        self.hasher = crc32fast::Hasher::new();
        self.window.bytes.clear();
    }

    /// Where the content starts, from the local header: its name and extra fields may not be
    /// those of the central directory.
    async fn data(&mut self) -> Result<u64, ZipError> {
        if let Some(data) = self.data {
            return Ok(data);
        }
        let mut header = [0; LOCAL_LEN];
        read_exact(&mut self.source, self.header, &mut header).await?;
        if !header.starts_with(LOCAL_SIGNATURE) {
            return Err(ZipError::Malformed(
                "the header of a file is not where the list says",
            ));
        }
        let names = u64::from(u16_at(&header, 26)) + u64::from(u16_at(&header, 28));
        let data = self.header + LOCAL_LEN as u64 + names;
        let zip_size = self.source.size().await.map_err(ZipError::Read)?;
        if data
            .checked_add(self.compressed)
            .is_none_or(|end| end > zip_size)
        {
            return Err(ZipError::Malformed(
                "a file reaches past the end of the zip",
            ));
        }
        self.data = Some(data);
        Ok(data)
    }

    /// Makes the next content bytes, at most `self.output.len()`, into `self.output`.
    async fn next_stored(&mut self, data: u64, wanted: usize) -> Result<usize, ZipError> {
        if self.compressed != self.size {
            return Err(ZipError::Malformed("a stored file has two sizes"));
        }
        let read = self
            .source
            .read_at(data + self.made, &mut self.output[..wanted])
            .await
            .map_err(ZipError::Read)?;
        if read == 0 {
            return Err(ZipError::Malformed("it ends too early"));
        }
        Ok(read)
    }

    /// Like [`Self::next_stored`], for a deflated entry.
    async fn next_inflated(&mut self, data: u64, wanted: usize) -> Result<usize, ZipError> {
        loop {
            if self.input_at == self.input_end && self.taken < self.compressed {
                self.input.resize(STEP, 0);
                let want = length(self.compressed - self.taken).min(STEP);
                let read = self
                    .source
                    .read_at(data + self.taken, &mut self.input[..want])
                    .await
                    .map_err(ZipError::Read)?;
                if read == 0 {
                    return Err(ZipError::Malformed("it ends too early"));
                }
                self.input_at = 0;
                self.input_end = read;
                self.taken += read as u64;
            }
            let (total_in, total_out) = (self.inflate.total_in(), self.inflate.total_out());
            let status = self
                .inflate
                .decompress(
                    &self.input[self.input_at..self.input_end],
                    &mut self.output[..wanted],
                    FlushDecompress::None,
                )
                .map_err(|_| ZipError::Corrupt)?;
            let used = length(self.inflate.total_in() - total_in);
            let made = length(self.inflate.total_out() - total_out);
            self.input_at += used;
            if made > 0 {
                return Ok(made);
            }
            // The stream ended, or nothing moves, before all of its size.
            if used == 0 || status == Status::StreamEnd {
                return Err(ZipError::Corrupt);
            }
        }
    }

    /// Makes the next content bytes, into the window.
    async fn advance(&mut self) -> Result<(), ZipError> {
        let data = self.data().await?;
        self.output.resize(STEP, 0);
        let wanted = length(self.size - self.made).min(STEP);
        let made = if self.method == METHOD_STORED {
            self.next_stored(data, wanted).await?
        } else {
            self.next_inflated(data, wanted).await?
        };
        self.hasher.update(&self.output[..made]);
        let total = self.made + made as u64;
        if total == self.size && self.hasher.clone().finalize() != self.crc32 {
            return Err(ZipError::Corrupt);
        }
        self.made = total;
        self.window.push(&self.output[..made]);
        Ok(())
    }
}

impl<S: Source> Source for EntrySource<S> {
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || offset >= self.size {
            return Ok(0);
        }
        loop {
            if let Some(count) = self.window.copy(offset, self.made, buf) {
                return Ok(count);
            }
            if offset < self.made {
                self.start_over();
            }
            self.advance().await?;
        }
    }

    fn size(&mut self) -> impl std::future::Future<Output = io::Result<u64>> {
        std::future::ready(Ok(self.size))
    }
}
