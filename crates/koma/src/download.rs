//! Downloading a book's original: the file as it is, or for a photo folder,
//! a ZIP of its images made while it is sent.
//!
//! Nothing is held in memory or written to disk: a blocking thread reads
//! and sends chunks, and the response streams them. The photo-folder ZIP is
//! stored (no compression, so little CPU and an exact size known before the
//! first byte); each image is read once, its CRC-32 taken, then sent.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use tokio::sync::mpsc;

use crate::archive::crc32::checksum;

/// Bytes sent at a time.
const CHUNK: usize = 256 * 1024;

/// Chunks waiting to be sent: the reader stays at most this far ahead.
const QUEUE: usize = 4;

/// Most bytes of one image put in a download ZIP (as for pages at ingest).
const MAX_IMAGE: u64 = 256 * 1024 * 1024;

/// One image of a photo-folder ZIP: its name in the ZIP, its file, and the
/// length the catalogue has for it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub len: u64,
}

/// The exact size of the stored ZIP of `entries`, or `None` if it needs
/// ZIP64 (over 4 GiB or 65,535 entries): such folders are not offered.
pub fn zip_len(entries: &[Entry]) -> Option<u64> {
    if entries.len() > usize::from(u16::MAX) {
        return None;
    }
    let mut total: u64 = 22; // end of central directory
    for e in entries {
        let name = e.name.len() as u64;
        total += 30 + name + e.len; // local header and data
        total += 46 + name; // central directory entry
    }
    // Every offset and size must fit 32 bits.
    (total <= u64::from(u32::MAX)).then_some(total)
}

/// Writes the stored ZIP of `entries` to `out`. Fails if an image is not the
/// length the catalogue says (changed since the scan) or over its limit.
pub fn write_zip(entries: &[Entry], out: &mut (impl Write + ?Sized)) -> io::Result<()> {
    // MS-DOS date 1980-01-01, 00:00: Koma-1 keeps no image times.
    const DOS_DATE: u16 = (1 << 5) | 1;
    const UTF8: u16 = 1 << 11;
    let mut central = Vec::new();
    let mut offset: u32 = 0;
    for e in entries {
        if e.len > MAX_IMAGE {
            return Err(io::Error::other(format!("{}: too large", e.name)));
        }
        let mut data = Vec::with_capacity(e.len as usize);
        File::open(&e.path)?
            .take(MAX_IMAGE + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 != e.len {
            return Err(io::Error::other(format!(
                "{}: changed since it was scanned",
                e.name
            )));
        }
        let crc = checksum(&data);
        let size = e.len as u32;
        let name = e.name.as_bytes();
        let name_len = name.len() as u16;
        let mut local = Vec::with_capacity(30 + name.len());
        local.extend(0x0403_4b50u32.to_le_bytes());
        local.extend(20u16.to_le_bytes()); // version needed: 2.0
        local.extend(UTF8.to_le_bytes());
        local.extend(0u16.to_le_bytes()); // stored
        local.extend(0u16.to_le_bytes()); // time
        local.extend(DOS_DATE.to_le_bytes());
        local.extend(crc.to_le_bytes());
        local.extend(size.to_le_bytes()); // compressed
        local.extend(size.to_le_bytes()); // uncompressed
        local.extend(name_len.to_le_bytes());
        local.extend(0u16.to_le_bytes()); // extra
        local.extend(name);
        out.write_all(&local)?;
        out.write_all(&data)?;

        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes()); // made by
        central.extend(20u16.to_le_bytes()); // needed
        central.extend(UTF8.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(DOS_DATE.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(name_len.to_le_bytes());
        central.extend([0u8; 12]); // extra, comment, disk, internal and external attributes
        central.extend(offset.to_le_bytes());
        central.extend(name);
        offset += 30 + u32::from(name_len) + size;
    }
    let count = entries.len() as u16;
    let mut end = Vec::with_capacity(22);
    end.extend(0x0605_4b50u32.to_le_bytes());
    end.extend([0u8; 4]); // disk numbers
    end.extend(count.to_le_bytes());
    end.extend(count.to_le_bytes());
    end.extend((central.len() as u32).to_le_bytes());
    end.extend(offset.to_le_bytes());
    end.extend(0u16.to_le_bytes()); // comment
    out.write_all(&central)?;
    out.write_all(&end)
}

/// A response body fed by a blocking writer: see [`spawn`].
pub struct StreamBody {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    len: u64,
}

impl http_body::Body for StreamBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|chunk| chunk.map(|r| r.map(Frame::data)))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.len)
    }
}

/// Buffers writes into [`CHUNK`]-sized pieces for the body. A send that
/// fails means the client went away: the write fails and the work stops.
struct ChunkWriter {
    tx: mpsc::Sender<io::Result<Bytes>>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .blocking_send(Ok(Bytes::from(chunk)))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = CHUNK - self.buf.len();
        let n = data.len().min(room);
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK {
            self.send()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// A body of `len` bytes that `work` writes, on a blocking thread. An error
/// from `work` ends the body with that error, so a cut-short download is
/// seen as failed, never as complete.
pub fn spawn(
    len: u64,
    work: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static,
) -> StreamBody {
    let (tx, rx) = mpsc::channel(QUEUE);
    tokio::task::spawn_blocking(move || {
        let mut w = ChunkWriter {
            tx: tx.clone(),
            buf: Vec::with_capacity(CHUNK),
        };
        // A broken pipe is the client gone: nothing to report.
        if let Err(e) = work(&mut w).and_then(|()| w.flush())
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            crate::log::error("serve: download failed", &[("error", &e.to_string())]);
            let _ = tx.blocking_send(Err(e));
        }
    });
    StreamBody { rx, len }
}

/// Copies the whole file at `path`, which must be `len` bytes long.
pub fn copy_file(path: &std::path::Path, len: u64, out: &mut dyn Write) -> io::Result<()> {
    let copied = io::copy(&mut File::open(path)?.take(len + 1), out)?;
    if copied != len {
        return Err(io::Error::other("file changed since it was scanned"));
    }
    Ok(())
}

/// `Content-Disposition` for a download named `name`: an ASCII fallback and
/// the exact name (RFC 6266, RFC 5987).
pub fn disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        crate::libraries::encode_path_part(name)
    )
}

/// The media type of a book file, by its extension.
pub fn media_type(file_name: &str) -> &'static str {
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "cbz" => "application/vnd.comicbook+zip",
        "zip" => "application/zip",
        "7z" | "cb7" => "application/x-7z-compressed",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_for_headers() {
        assert_eq!(
            disposition("Starfall v01.cbz"),
            "attachment; filename=\"Starfall v01.cbz\"; filename*=UTF-8''Starfall%20v01.cbz"
        );
        assert_eq!(
            disposition("星の港 \"1\".cbz"),
            "attachment; filename=\"___ _1_.cbz\"; \
             filename*=UTF-8''%E6%98%9F%E3%81%AE%E6%B8%AF%20%221%22.cbz"
        );
        assert_eq!(media_type("a.CBZ"), "application/vnd.comicbook+zip");
        assert_eq!(media_type("a.cb7"), "application/x-7z-compressed");
        assert_eq!(media_type("a"), "application/octet-stream");
    }

    #[test]
    fn zip_sizes() {
        let e = |name: &str, len| Entry {
            name: name.into(),
            path: PathBuf::new(),
            len,
        };
        assert_eq!(zip_len(&[]), Some(22));
        assert_eq!(zip_len(&[e("a.png", 10)]), Some(22 + 30 + 5 + 10 + 46 + 5));
        assert_eq!(zip_len(&[e("big", u64::from(u32::MAX))]), None);
    }
}
