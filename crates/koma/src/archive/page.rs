//! Reading one page's bytes out of an archive, as a stream, with limits.
//!
//! Stored pages are read in place. Deflated pages are inflated, but never
//! past the size the archive declares, never past [`PageLimits`], and the
//! result must match the declared size and CRC-32.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;

use flate2::read::DeflateDecoder;

use super::crc32::Crc32;
use super::zip::{Entry, Method};

#[derive(Clone, Copy, Debug)]
pub struct PageLimits {
    /// Largest page, uncompressed.
    pub max_bytes: u64,
    /// Largest uncompressed-to-compressed ratio for a deflated page.
    pub max_ratio: u64,
}

impl Default for PageLimits {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_ratio: 1000,
        }
    }
}

#[derive(Debug)]
pub enum PageError {
    Io(io::Error),
    /// The page is bigger than allowed, or compresses suspiciously well.
    Limit(&'static str),
    /// The bytes don't match what the archive says about them.
    Corrupt(&'static str),
    Unsupported(&'static str),
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::Io(e) => write!(f, "read error: {e}"),
            PageError::Limit(why) => write!(f, "page over limit: {why}"),
            PageError::Corrupt(why) => write!(f, "corrupt page: {why}"),
            PageError::Unsupported(what) => write!(f, "unsupported page: {what}"),
        }
    }
}

impl std::error::Error for PageError {}

impl From<io::Error> for PageError {
    fn from(e: io::Error) -> Self {
        PageError::Io(e)
    }
}

/// Streams the page's uncompressed bytes into `out` and checks them against
/// the archive's declared size and CRC-32. Returns the number of bytes.
pub fn copy_page(
    file: &File,
    entry: &Entry,
    limits: &PageLimits,
    out: &mut impl Write,
) -> Result<u64, PageError> {
    if entry.encrypted {
        return Err(PageError::Unsupported("encrypted"));
    }
    if entry.uncompressed_size > limits.max_bytes {
        return Err(PageError::Limit("page too large"));
    }
    let raw = RangeReader {
        file,
        pos: entry.data_offset,
        end: entry.data_offset + entry.compressed_size,
    };
    let mut checked = Checked {
        out,
        crc: Crc32::default(),
        len: 0,
    };
    match entry.method {
        Method::Stored => {
            io::copy(&mut { raw }, &mut checked)?;
        }
        Method::Deflated => {
            let ratio = entry.uncompressed_size / entry.compressed_size.max(1);
            if ratio > limits.max_ratio {
                return Err(PageError::Limit("compression ratio too high"));
            }
            // One byte more than declared, to notice a stream that lies.
            let mut inflated = DeflateDecoder::new(raw).take(entry.uncompressed_size + 1);
            io::copy(&mut inflated, &mut checked).map_err(|e| match e.kind() {
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
                    PageError::Corrupt("bad deflate stream")
                }
                _ => PageError::Io(e),
            })?;
        }
        Method::Other(_) => return Err(PageError::Unsupported("compression method")),
    }
    if checked.len != entry.uncompressed_size {
        return Err(PageError::Corrupt("size differs from the archive's"));
    }
    if checked.crc.finish() != entry.crc32 {
        return Err(PageError::Corrupt("CRC-32 mismatch"));
    }
    Ok(checked.len)
}

/// Reads `file[pos..end]` with positioned reads.
struct RangeReader<'a> {
    file: &'a File,
    pos: u64,
    end: u64,
}

impl Read for RangeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = usize::try_from(self.end - self.pos).unwrap_or(usize::MAX);
        let want = buf.len().min(left);
        if want == 0 {
            return Ok(0);
        }
        let n = self.file.read_at(&mut buf[..want], self.pos)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.pos += n as u64;
        Ok(n)
    }
}

/// Passes bytes through while counting them and computing their CRC-32.
struct Checked<'a, W> {
    out: &'a mut W,
    crc: Crc32,
    len: u64,
}

impl<W: Write> Write for Checked<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.crc.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}
