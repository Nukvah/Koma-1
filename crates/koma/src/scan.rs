//! The library scan: finds CBZ files, indexes each one, and records every
//! page's hash and location in the catalogue.
//!
//! - A series is a folder; each CBZ in it is a book. A CBZ in the library
//!   root is a series of its own.
//! - Stored pages are located in place (file, offset, length) and never copied.
//! - Compressed pages are inflated once, here, into the blob store, so
//!   serving never decompresses anything.
//! - Books whose size and modification time are unchanged are skipped.
//!
//! A file that can't be read is recorded with its reason and skipped; it
//! never stops the scan.

use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use crate::archive::page::{PageError, PageLimits, copy_page};
use crate::archive::zip::{self, Limits, Method};
use crate::archive::{self};
use crate::blobs::BlobStore;
use crate::catalog::{self, Catalog, NewBook, Page};
use crate::log;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub indexed: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub removed: usize,
}

#[derive(Debug)]
pub enum ScanError {
    Catalog(catalog::Error),
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Catalog(e) => write!(f, "catalogue: {e}"),
            ScanError::Io(path, e) => write!(f, "{}: {e}", path.display()),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<catalog::Error> for ScanError {
    fn from(e: catalog::Error) -> Self {
        ScanError::Catalog(e)
    }
}

/// Scans `library` into `catalog`, writing inflated pages under `data`.
pub fn scan(library: &Path, data: &Path, catalog: &mut Catalog) -> Result<ScanReport, ScanError> {
    let started = Instant::now();
    let blobs = BlobStore::new(data);
    blobs
        .clean_temp()
        .map_err(|e| ScanError::Io(data.join("blobs"), e))?;

    let mut files = Vec::new();
    find_books(library, library, &mut files).map_err(|e| ScanError::Io(library.into(), e))?;

    let mut report = ScanReport::default();
    let mut good = HashSet::new();
    catalog.clear_errors()?;
    for (rel, path, meta) in &files {
        let size = meta.len();
        let mtime = mtime_ns(meta);
        if catalog.book_stamp(rel)? == Some((size, mtime)) {
            report.unchanged += 1;
            good.insert(rel.clone());
            continue;
        }
        match index_book(path, &blobs) {
            Ok(pages) => {
                let (series_path, series_name) = series_of(rel);
                catalog.replace_book(&NewBook {
                    series_path,
                    series_name,
                    path: rel,
                    name: stem(rel),
                    size,
                    mtime,
                    pages: &pages,
                })?;
                report.indexed += 1;
                good.insert(rel.clone());
            }
            Err(e) => {
                log::error("scan: skipped a book", &[("path", rel), ("error", &e)]);
                catalog.record_error(rel, &e)?;
                report.failed += 1;
            }
        }
    }
    report.removed = catalog.retain_books(&|path| good.contains(path))?;

    log::info(
        "scan: done",
        &[
            ("indexed", &report.indexed.to_string()),
            ("unchanged", &report.unchanged.to_string()),
            ("failed", &report.failed.to_string()),
            ("removed", &report.removed.to_string()),
            ("ms", &started.elapsed().as_millis().to_string()),
        ],
    );
    Ok(report)
}

/// Collects `(path relative to the library, path, metadata)` of every CBZ
/// under `dir`. Hidden entries and symbolic links are skipped.
fn find_books(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf, Metadata)>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            let lossy = entry.path().to_string_lossy().into_owned();
            log::error(
                "scan: skipped a name that is not UTF-8",
                &[("path", &lossy)],
            );
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata()?; // does not follow symlinks
        let path = entry.path();
        if meta.is_dir() {
            find_books(root, &path, out)?;
        } else if meta.is_file()
            && is_cbz(name)
            && let Some(rel) = relative(root, &path)
        {
            out.push((rel, path, meta));
        }
    }
    Ok(())
}

fn is_cbz(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("cbz"))
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// The series a book belongs to: its folder, or the book itself in the root.
fn series_of(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((dir, _)) => (dir, dir.rsplit('/').next().unwrap_or(dir)),
        None => (rel, stem(rel)),
    }
}

fn stem(rel: &str) -> &str {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

fn mtime_ns(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// Indexes one CBZ: every page hashed, stored pages located, compressed
/// pages inflated into the blob store.
fn index_book(path: &Path, blobs: &BlobStore) -> Result<Vec<Page>, String> {
    let entries = zip::read_file_index(path, &Limits::default()).map_err(|e| e.to_string())?;
    let pages = archive::pages(&entries);
    if pages.is_empty() {
        return Err("no pages".into());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let limits = PageLimits::default();
    pages
        .iter()
        .enumerate()
        .map(|(number, entry)| {
            let format = format_of(&entry.name);
            let located = match entry.method {
                Method::Stored => {
                    let mut hasher = blake3::Hasher::new();
                    copy_page(&file, entry, &limits, &mut hasher)?;
                    (hasher.finalize(), Some(entry.data_offset))
                }
                _ => (
                    inflate_to_blob(&file, entry, &limits, blobs, &format)?,
                    None,
                ),
            };
            Ok(Page {
                number: number as i64,
                hash: located.0,
                format,
                length: entry.uncompressed_size,
                offset: located.1,
            })
        })
        .collect::<Result<Vec<_>, PageError>>()
        .map_err(|e| e.to_string())
}

fn inflate_to_blob(
    file: &File,
    entry: &zip::Entry,
    limits: &PageLimits,
    blobs: &BlobStore,
    format: &str,
) -> Result<blake3::Hash, PageError> {
    let (temp, out) = blobs.temp()?;
    let mut tee = Tee {
        hasher: blake3::Hasher::new(),
        out: BufWriter::new(out),
    };
    let written = copy_page(file, entry, limits, &mut tee).and_then(|_| Ok(tee.out.flush()?));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    let hash = tee.hasher.finalize();
    blobs.commit(&temp, &hash, format)?;
    Ok(hash)
}

/// The page's file extension, lower case; `jpeg` becomes `jpg`.
fn format_of(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map_or("", |(_, e)| e)
        .to_ascii_lowercase();
    if ext == "jpeg" { "jpg".into() } else { ext }
}

/// Hashes everything written while writing it on.
struct Tee<W> {
    hasher: blake3::Hasher,
    out: W,
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_is_the_folder() {
        assert_eq!(
            series_of("Starfall/Starfall v01.cbz"),
            ("Starfall", "Starfall")
        );
        assert_eq!(
            series_of("Shelf/Starfall/v01.cbz"),
            ("Shelf/Starfall", "Starfall")
        );
        assert_eq!(
            series_of("Lone Comet.cbz"),
            ("Lone Comet.cbz", "Lone Comet")
        );
    }

    #[test]
    fn names() {
        assert!(is_cbz("a.CBZ"));
        assert!(!is_cbz(".cbz"));
        assert!(!is_cbz("a.zip"));
        assert_eq!(stem("x/Ch. 10.5.cbz"), "Ch. 10.5");
        assert_eq!(format_of("p/1.JPEG"), "jpg");
    }
}
