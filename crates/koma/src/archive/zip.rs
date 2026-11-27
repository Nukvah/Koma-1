//! The index of a zip archive (CBZ): where each member's bytes are, read from
//! the central directory and checked against each local header. Nothing is
//! decompressed here.
//!
//! Strict on purpose. An archive is rejected when:
//! - its end record is missing or is not exactly at the end of the file;
//! - it spans several disks, or has data before or between its records;
//! - any length or offset points outside the file or into the central directory;
//! - two members' bytes overlap (a known zip-bomb trick);
//! - it exceeds [`Limits`].
//!
//! Zip64 is supported, so offsets and sizes over 4 GiB work.

use std::fmt;
use std::io;
use std::ops::Range;

use super::ReadAt;
use super::crc32;

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD_LEN: usize = 22;
const MAX_COMMENT: usize = 0xFFFF;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_EOCD_LEN: usize = 56;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const CENTRAL_LEN: usize = 46;
const LOCAL_SIG: u32 = 0x0403_4b50;
const LOCAL_LEN: usize = 30;

const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_UNICODE_PATH: u16 = 0x7075;

const FLAG_ENCRYPTED: u16 = 1 << 0;
const FLAG_STRONG_ENCRYPTION: u16 = 1 << 6;
const FLAG_UTF8: u16 = 1 << 11;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;
const METHOD_AES: u16 = 99;

/// Caps on what an index may cost to read. Real comic archives are far below them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: usize,
    pub max_central_directory: u64,
    pub max_name_len: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_central_directory: 32 * 1024 * 1024,
            max_name_len: 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Stored,
    Deflated,
    Other(u16),
}

/// One archive member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Path inside the archive, `/`-separated (Windows `\` is converted).
    pub name: String,
    pub method: Method,
    pub encrypted: bool,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    /// Offset of the local header.
    pub header_offset: u64,
    /// Offset of the member's (possibly compressed) bytes.
    pub data_offset: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }

    /// Byte range of the member's data in the archive file. For a stored
    /// member these are exactly the file's bytes and can be served as is.
    pub fn data_range(&self) -> Range<u64> {
        self.data_offset..self.data_offset + self.compressed_size
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// No end-of-central-directory record: not a zip file.
    NotZip,
    /// The archive contradicts itself or the file it is in.
    Malformed(&'static str),
    /// Valid zip, but a feature Koma-1 does not read (multi-disk archives).
    Unsupported(&'static str),
    /// Over one of the [`Limits`].
    Limit(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "read error: {e}"),
            Error::NotZip => f.write_str("not a zip archive"),
            Error::Malformed(why) => write!(f, "malformed zip: {why}"),
            Error::Unsupported(what) => write!(f, "unsupported zip: {what}"),
            Error::Limit(what) => write!(f, "zip over limit: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::Malformed("offset past the end of the file")
        } else {
            Error::Io(e)
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Reads the index of the zip archive in `r`, whose total length is `len`.
pub fn read_index<R: ReadAt + ?Sized>(r: &R, len: u64, limits: &Limits) -> Result<Vec<Entry>> {
    let end = find_end(r, len)?;
    if end.entries > limits.max_entries as u64 {
        return Err(Error::Limit("too many entries"));
    }
    if end.cd_size > limits.max_central_directory {
        return Err(Error::Limit("central directory too large"));
    }
    if end.entries.saturating_mul(CENTRAL_LEN as u64) > end.cd_size {
        return Err(Error::Malformed(
            "central directory too small for its entries",
        ));
    }
    let cd_end = end
        .cd_offset
        .checked_add(end.cd_size)
        .ok_or(Error::Malformed("central directory offset overflows"))?;
    if cd_end > end.records_start {
        return Err(Error::Malformed(
            "central directory overlaps the end record",
        ));
    }

    let mut cd = vec![0u8; end.cd_size as usize];
    r.read_exact_at(&mut cd, end.cd_offset)?;
    let mut entries = parse_central_directory(&cd, end.entries, limits)?;

    for entry in &mut entries {
        entry.data_offset = resolve_data(r, entry, end.cd_offset)?;
    }
    check_no_overlap(&entries)?;
    Ok(entries)
}

/// Opens `path` and reads its index.
pub fn read_file_index(path: &std::path::Path, limits: &Limits) -> Result<Vec<Entry>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    read_index(&file, len, limits)
}

/// What the end records say about the central directory.
#[derive(Debug)]
struct End {
    entries: u64,
    cd_size: u64,
    cd_offset: u64,
    /// Where the end records begin; the central directory must end before.
    records_start: u64,
}

fn find_end<R: ReadAt + ?Sized>(r: &R, len: u64) -> Result<End> {
    if len < EOCD_LEN as u64 {
        return Err(Error::NotZip);
    }
    // The end record is the last thing in the file, followed only by a comment
    // of at most 64 KiB, so it is in the file's tail.
    let tail_len = len.min((EOCD_LEN + MAX_COMMENT) as u64) as usize;
    let tail_start = len - tail_len as u64;
    let mut tail = vec![0u8; tail_len];
    r.read_exact_at(&mut tail, tail_start)?;

    let at = (0..=tail_len - EOCD_LEN)
        .rev()
        .find(|&i| {
            le_u32(&tail[i..]) == EOCD_SIG
                && i + EOCD_LEN + usize::from(le_u16(&tail[i + 20..])) == tail_len
        })
        .ok_or(Error::NotZip)?;
    let eocd_pos = tail_start + at as u64;
    let mut b = Bytes::new(&tail[at + 4..at + EOCD_LEN]);
    let disk = b.u16()?;
    let cd_disk = b.u16()?;
    let entries_here = b.u16()?;
    let entries = b.u16()?;
    let cd_size = b.u32()?;
    let cd_offset = b.u32()?;

    let needs_zip64 = disk == 0xFFFF
        || cd_disk == 0xFFFF
        || entries_here == 0xFFFF
        || entries == 0xFFFF
        || cd_size == 0xFFFF_FFFF
        || cd_offset == 0xFFFF_FFFF;
    match find_zip64_end(r, eocd_pos)? {
        Some(end) => return Ok(end),
        None if needs_zip64 => return Err(Error::Malformed("zip64 end record missing")),
        None => {}
    }

    if disk != 0 || cd_disk != 0 || entries_here != entries {
        return Err(Error::Unsupported("multi-disk archive"));
    }
    Ok(End {
        entries: entries.into(),
        cd_size: cd_size.into(),
        cd_offset: cd_offset.into(),
        records_start: eocd_pos,
    })
}

/// Reads the Zip64 end record if a locator sits right before the classic end record.
fn find_zip64_end<R: ReadAt + ?Sized>(r: &R, eocd_pos: u64) -> Result<Option<End>> {
    let Some(locator_pos) = eocd_pos.checked_sub(ZIP64_LOCATOR_LEN as u64) else {
        return Ok(None);
    };
    let mut locator = [0u8; ZIP64_LOCATOR_LEN];
    r.read_exact_at(&mut locator, locator_pos)?;
    let mut b = Bytes::new(&locator);
    if b.u32()? != ZIP64_LOCATOR_SIG {
        return Ok(None);
    }
    let disk = b.u32()?;
    let record_pos = b.u64()?;
    let disks = b.u32()?;
    if disk != 0 || disks > 1 {
        return Err(Error::Unsupported("multi-disk archive"));
    }

    let record_end = record_pos
        .checked_add(ZIP64_EOCD_LEN as u64)
        .ok_or(Error::Malformed("zip64 end record offset overflows"))?;
    if record_end > locator_pos {
        return Err(Error::Malformed("zip64 end record out of place"));
    }
    let mut record = [0u8; ZIP64_EOCD_LEN];
    r.read_exact_at(&mut record, record_pos)?;
    let mut b = Bytes::new(&record);
    if b.u32()? != ZIP64_EOCD_SIG {
        return Err(Error::Malformed("bad zip64 end record signature"));
    }
    // The record's size excludes its first 12 bytes; it must end at the locator.
    let size = b.u64()?;
    if record_pos.checked_add(12).and_then(|p| p.checked_add(size)) != Some(locator_pos) {
        return Err(Error::Malformed("zip64 end record size mismatch"));
    }
    b.skip(4)?; // versions
    let disk = b.u32()?;
    let cd_disk = b.u32()?;
    let entries_here = b.u64()?;
    let entries = b.u64()?;
    let cd_size = b.u64()?;
    let cd_offset = b.u64()?;
    if disk != 0 || cd_disk != 0 || entries_here != entries {
        return Err(Error::Unsupported("multi-disk archive"));
    }
    Ok(Some(End {
        entries,
        cd_size,
        cd_offset,
        records_start: record_pos,
    }))
}

fn parse_central_directory(cd: &[u8], count: u64, limits: &Limits) -> Result<Vec<Entry>> {
    let mut b = Bytes::new(cd);
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if b.u32()? != CENTRAL_SIG {
            return Err(Error::Malformed("bad central header signature"));
        }
        b.skip(4)?; // versions
        let flags = b.u16()?;
        let method = b.u16()?;
        b.skip(4)?; // time, date
        let crc32 = b.u32()?;
        let mut compressed_size = u64::from(b.u32()?);
        let mut uncompressed_size = u64::from(b.u32()?);
        let name_len = usize::from(b.u16()?);
        let extra_len = usize::from(b.u16()?);
        let comment_len = usize::from(b.u16()?);
        let mut disk_start = u32::from(b.u16()?);
        b.skip(6)?; // attributes
        let mut header_offset = u64::from(b.u32()?);
        if name_len > limits.max_name_len {
            return Err(Error::Limit("entry name too long"));
        }
        let raw_name = b.take(name_len)?;
        let extra = b.take(extra_len)?;
        b.skip(comment_len)?;

        let mut unicode_name = None;
        for (id, mut data) in ExtraFields(extra) {
            match id {
                EXTRA_ZIP64 => {
                    // Present only for the fields that overflowed, in this order.
                    if uncompressed_size == 0xFFFF_FFFF {
                        uncompressed_size = data.u64()?;
                    }
                    if compressed_size == 0xFFFF_FFFF {
                        compressed_size = data.u64()?;
                    }
                    if header_offset == 0xFFFF_FFFF {
                        header_offset = data.u64()?;
                    }
                    if disk_start == 0xFFFF {
                        disk_start = data.u32()?;
                    }
                }
                EXTRA_UNICODE_PATH => {
                    // Info-ZIP: version, CRC of the plain name, UTF-8 name.
                    let version = data.u8()?;
                    let name_crc = data.u32()?;
                    let utf8 = data.rest();
                    if version == 1 && name_crc == crc32::checksum(raw_name) {
                        unicode_name = std::str::from_utf8(utf8).ok().map(str::to_owned);
                    }
                }
                _ => {}
            }
        }
        if disk_start != 0 {
            return Err(Error::Unsupported("multi-disk archive"));
        }

        let name = match unicode_name {
            Some(name) => name,
            None => decode_name(raw_name, flags & FLAG_UTF8 != 0),
        };
        if name.contains('\0') {
            return Err(Error::Malformed("entry name contains NUL"));
        }
        let method = match method {
            METHOD_STORED => Method::Stored,
            METHOD_DEFLATED => Method::Deflated,
            other => Method::Other(other),
        };
        if method == Method::Stored && compressed_size != uncompressed_size {
            return Err(Error::Malformed("stored entry sizes differ"));
        }
        entries.push(Entry {
            name: name.replace('\\', "/"),
            method,
            encrypted: flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0
                || method == Method::Other(METHOD_AES),
            crc32,
            compressed_size,
            uncompressed_size,
            header_offset,
            data_offset: 0,
        });
    }
    Ok(entries)
}

/// Finds where an entry's data starts by reading its local header, and checks
/// that the data ends before the central directory.
fn resolve_data<R: ReadAt + ?Sized>(r: &R, entry: &Entry, cd_offset: u64) -> Result<u64> {
    let header_end = entry
        .header_offset
        .checked_add(LOCAL_LEN as u64)
        .filter(|&end| end <= cd_offset)
        .ok_or(Error::Malformed("local header outside the archive data"))?;
    let mut header = [0u8; LOCAL_LEN];
    r.read_exact_at(&mut header, entry.header_offset)?;
    let mut b = Bytes::new(&header);
    if b.u32()? != LOCAL_SIG {
        return Err(Error::Malformed("bad local header signature"));
    }
    b.skip(4)?; // version, flags
    let method = b.u16()?;
    b.skip(16)?; // time, date, crc, sizes (may be zero when a data descriptor follows)
    let name_len = u64::from(b.u16()?);
    let extra_len = u64::from(b.u16()?);
    let central_method = match entry.method {
        Method::Stored => METHOD_STORED,
        Method::Deflated => METHOD_DEFLATED,
        Method::Other(m) => m,
    };
    if method != central_method {
        return Err(Error::Malformed("local and central headers disagree"));
    }
    let data_offset = header_end + name_len + extra_len;
    match data_offset.checked_add(entry.compressed_size) {
        Some(data_end) if data_end <= cd_offset => Ok(data_offset),
        _ => Err(Error::Malformed(
            "entry data runs into the central directory",
        )),
    }
}

fn check_no_overlap(entries: &[Entry]) -> Result<()> {
    let mut spans: Vec<(u64, u64)> = entries
        .iter()
        .map(|e| (e.header_offset, e.data_offset + e.compressed_size))
        .collect();
    spans.sort_unstable();
    if spans.windows(2).any(|w| w[1].0 < w[0].1) {
        return Err(Error::Malformed("entries overlap"));
    }
    Ok(())
}

/// Names are UTF-8 when the flag says so; otherwise UTF-8 if they decode as
/// such (many tools omit the flag), else the zip default, code page 437.
fn decode_name(raw: &[u8], utf8_flag: bool) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) if utf8_flag => String::from_utf8_lossy(raw).into_owned(),
        Err(_) => raw
            .iter()
            .map(|&b| match b {
                0..=0x7F => char::from(b),
                _ => CP437_HIGH[usize::from(b - 0x80)],
            })
            .collect(),
    }
}

const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{A0}',
];

/// Iterates `(id, data)` over an extra field block. A truncated trailing field
/// is ignored rather than trusted.
struct ExtraFields<'a>(&'a [u8]);

impl<'a> Iterator for ExtraFields<'a> {
    type Item = (u16, Bytes<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let mut b = Bytes::new(self.0);
        let id = b.u16().ok()?;
        let len = usize::from(b.u16().ok()?);
        let data = b.take(len).ok()?;
        self.0 = b.rest();
        Some((id, Bytes::new(data)))
    }
}

/// A little-endian reader over a byte slice that fails instead of panicking.
struct Bytes<'a> {
    buf: &'a [u8],
}

impl<'a> Bytes<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.buf.len() {
            return Err(Error::Malformed("record truncated"));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    fn rest(&self) -> &'a [u8] {
        self.buf
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(le_u16(self.take(2)?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(le_u32(self.take(4)?))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

/// Callers guarantee at least 2 bytes.
fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// Callers guarantee at least 4 bytes.
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp437_names() {
        assert_eq!(decode_name(b"p\xa0g 1.png", false), "pág 1.png");
        assert_eq!(decode_name("pág".as_bytes(), false), "pág");
        assert_eq!(decode_name(b"\xff", true), "\u{FFFD}");
    }

    #[test]
    fn extra_fields_stop_at_truncation() {
        let extra = [1, 0, 2, 0, 9, 9, 7, 0, 5, 0, 1];
        let fields: Vec<_> = ExtraFields(&extra).map(|(id, b)| (id, b.rest())).collect();
        assert_eq!(fields, vec![(1, &[9u8, 9][..])]);
    }
}
