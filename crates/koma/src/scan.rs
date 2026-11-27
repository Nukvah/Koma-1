//! The library scan: finds book files, queues them for indexing, and the
//! jobs that index each one and record every page's hash and location in the
//! catalogue.
//!
//! - A series is a folder; each CBZ in it is a book. A CBZ in the library
//!   root is a series of its own.
//! - Stored pages are located in place (file, offset, length) and never copied.
//! - Compressed pages are inflated once, here, into the blob store, so
//!   serving never decompresses anything.
//!
//! The scan is incremental. A folder whose modification time is unchanged
//! since it was last read is not read again: only its known subfolders are
//! looked at. A [`Check::Files`] scan also looks at each known book in those
//! folders, which catches a file overwritten in place (that does not change
//! its folder's time). Books whose size and modification time are unchanged
//! are never read again.
//!
//! Finding work and doing it are separate: the scan only queues an index job
//! per changed book, in the catalogue, and [`run_jobs`] works through the
//! queue. Each job is one transaction, so a worker killed in the middle of
//! one leaves the catalogue as it was, and the job runs again on restart.
//!
//! A file or folder that can't be read is recorded with its reason and
//! skipped; it never stops the scan, and is retried once it changes.

use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::archive::page::{PageError, PageLimits, copy_page};
use crate::archive::zip::{self, Limits, Method};
use crate::archive::{self};
use crate::blobs::BlobStore;
use crate::catalog::{self, Catalog, JobKind, MAX_ATTEMPTS, NewBook, Page};
use crate::log;

/// A folder modified less than this long before it is read is read again
/// next time: file systems with coarse times (FAT, some network shares) could
/// otherwise hide a change made in the same second.
const SETTLE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Books read and stored.
    pub indexed: usize,
    /// Books looked at and found unchanged (not counting those in folders
    /// that were not read in a [`Check::Folders`] scan).
    pub unchanged: usize,
    /// Books that could not be read this time.
    pub failed: usize,
    /// Books forgotten because their file is gone.
    pub removed: usize,
    /// Folders read because they are new or changed.
    pub folders_read: usize,
    /// Folders not read because their modification time is unchanged.
    pub folders_skipped: usize,
}

/// How closely a scan looks at folders that have not changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// Only their modification time: the cheap periodic rescan.
    Folders,
    /// Also the size and time of every known book in them.
    Files,
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

/// Scans `library` into `catalog` with a [`Check::Files`] scan, and runs every
/// queued job, writing inflated pages under `data`.
pub fn scan(library: &Path, data: &Path, catalog: &mut Catalog) -> Result<ScanReport, ScanError> {
    let started = Instant::now();
    let mut report = discover(library, catalog, Check::Files)?;
    run_jobs(library, data, catalog, &mut report, &|| false)?;
    log_report(&report, started);
    Ok(report)
}

/// Logs a finished scan.
pub fn log_report(report: &ScanReport, started: Instant) {
    log::info(
        "scan: done",
        &[
            ("indexed", &report.indexed.to_string()),
            ("unchanged", &report.unchanged.to_string()),
            ("failed", &report.failed.to_string()),
            ("removed", &report.removed.to_string()),
            ("folders_read", &report.folders_read.to_string()),
            ("folders_skipped", &report.folders_skipped.to_string()),
            ("ms", &started.elapsed().as_millis().to_string()),
        ],
    );
}

/// Walks the library and queues an index job for every new or changed book.
/// Books and folders that are gone are forgotten at once.
pub fn discover(
    library: &Path,
    catalog: &mut Catalog,
    check: Check,
) -> Result<ScanReport, ScanError> {
    // If the library itself can't be read (not mounted, say), stop here
    // rather than forget every book.
    let meta = fs::metadata(library).map_err(|e| ScanError::Io(library.into(), e))?;
    if !meta.is_dir() {
        return Err(ScanError::Io(
            library.into(),
            io::Error::new(io::ErrorKind::NotADirectory, "not a folder"),
        ));
    }
    let mut walker = Walker {
        library,
        catalog,
        check,
        report: ScanReport::default(),
    };
    walker.visit("", None, &meta)?;
    Ok(walker.report)
}

struct Walker<'a> {
    library: &'a Path,
    catalog: &'a mut Catalog,
    check: Check,
    report: ScanReport,
}

impl Walker<'_> {
    fn abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.library.to_path_buf()
        } else {
            self.library.join(rel)
        }
    }

    /// Visits the folder `rel`. Returns whether it and everything under it
    /// was read; if not, its parent is read again next time.
    fn visit(
        &mut self,
        rel: &str,
        parent: Option<&str>,
        meta: &Metadata,
    ) -> Result<bool, ScanError> {
        let mtime = mtime_ns(meta);
        if self.catalog.folder_mtime(rel)? == Some(Some(mtime)) {
            self.report.folders_skipped += 1;
            if self.check == Check::Files {
                self.check_known_files(rel)?;
            }
            let mut complete = true;
            for child in self.catalog.subfolders(rel)? {
                match fs::symlink_metadata(self.abs(&child)) {
                    Ok(m) if m.is_dir() => complete &= self.visit(&child, Some(rel), &m)?,
                    _ => self.report.removed += self.catalog.remove_tree(&child)?,
                }
            }
            return Ok(complete);
        }

        self.report.folders_read += 1;
        let (dirs, files) = match self.read_folder(rel) {
            Ok(found) => found,
            Err(e) => {
                let e = e.to_string();
                log::error("scan: skipped a folder", &[("path", rel), ("error", &e)]);
                self.catalog
                    .record_error(rel, parent.unwrap_or(""), None, &e)?;
                return Ok(false);
            }
        };
        if !rel.is_empty() {
            self.catalog.remove_error(rel)?;
        }

        let mut present = HashSet::new();
        for (path, meta) in &files {
            present.insert(path.as_str());
            let stamp = (meta.len(), mtime_ns(meta));
            if self.catalog.book_stamp(path)? == Some(stamp)
                || self.catalog.error_stamp(path)? == Some(stamp)
            {
                self.report.unchanged += 1;
            } else {
                self.catalog.enqueue(JobKind::Index, path)?;
            }
        }
        for (path, _, _) in self.catalog.books_in(rel)? {
            if !present.contains(path.as_str()) && self.catalog.remove_book(&path)? {
                self.report.removed += 1;
            }
        }
        let dir_names: HashSet<&str> = dirs.iter().map(|(d, _)| d.as_str()).collect();
        for (path, _) in self.catalog.errors_in(rel)? {
            if !present.contains(path.as_str()) && !dir_names.contains(path.as_str()) {
                self.catalog.remove_error(&path)?;
            }
        }
        for child in self.catalog.subfolders(rel)? {
            if !dir_names.contains(child.as_str()) {
                self.report.removed += self.catalog.remove_tree(&child)?;
            }
        }

        let mut complete = true;
        for (child, meta) in &dirs {
            complete &= self.visit(child, Some(rel), meta)?;
        }
        // Recorded last: if the scan stops before this, the folder is read
        // again next time, so nothing below it can be missed.
        let settled = complete && !recently_modified(meta);
        self.catalog
            .set_folder(rel, parent, settled.then_some(mtime))?;
        Ok(complete)
    }

    /// Re-checks the books of a folder that was not read, by size and time.
    fn check_known_files(&mut self, rel: &str) -> Result<(), ScanError> {
        for (path, size, mtime) in self.catalog.books_in(rel)? {
            match fs::symlink_metadata(self.abs(&path)) {
                Ok(m) if m.is_file() && (m.len(), mtime_ns(&m)) == (size, mtime) => {
                    self.report.unchanged += 1;
                }
                Ok(m) if m.is_file() => {
                    self.catalog.enqueue(JobKind::Index, &path)?;
                }
                _ => {
                    if self.catalog.remove_book(&path)? {
                        self.report.removed += 1;
                    }
                }
            }
        }
        for (path, stamp) in self.catalog.errors_in(rel)? {
            let Some(stamp) = stamp else { continue };
            match fs::symlink_metadata(self.abs(&path)) {
                Ok(m) if m.is_file() && (m.len(), mtime_ns(&m)) == stamp => {}
                Ok(m) if m.is_file() => {
                    self.catalog.enqueue(JobKind::Index, &path)?;
                }
                _ => self.catalog.remove_error(&path)?,
            }
        }
        Ok(())
    }

    /// The subfolders and book files directly in a folder, as paths relative
    /// to the library. Hidden entries and symbolic links are skipped.
    fn read_folder(&self, rel: &str) -> io::Result<(Found, Found)> {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in fs::read_dir(self.abs(rel))? {
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
            let child = if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{rel}/{name}")
            };
            if meta.is_dir() {
                dirs.push((child, meta));
            } else if meta.is_file() && is_cbz(name) {
                files.push((child, meta));
            }
        }
        Ok((dirs, files))
    }
}

/// Entries found in a folder: paths relative to the library, with metadata.
type Found = Vec<(String, Metadata)>;

fn recently_modified(meta: &Metadata) -> bool {
    match meta.modified() {
        Ok(t) => SystemTime::now()
            .duration_since(t)
            .map_or(true, |age| age < SETTLE),
        Err(_) => true,
    }
}

/// Works through the job queue until it is empty or `should_stop` says so
/// (checked between jobs). Counts go into `report`.
pub fn run_jobs(
    library: &Path,
    data: &Path,
    catalog: &mut Catalog,
    report: &mut ScanReport,
    should_stop: &dyn Fn() -> bool,
) -> Result<(), ScanError> {
    let blobs = BlobStore::new(data);
    blobs
        .clean_temp()
        .map_err(|e| ScanError::Io(data.join("blobs"), e))?;
    while !should_stop() {
        let Some(job) = catalog.claim_job()? else {
            break;
        };
        if job.attempts > MAX_ATTEMPTS {
            let e = format!(
                "gave up after {MAX_ATTEMPTS} attempts: the worker stopped while reading it each time"
            );
            log::error(
                "scan: skipped a book",
                &[("path", &job.path), ("error", &e)],
            );
            catalog.record_error(&job.path, folder_of(&job.path), None, &e)?;
            catalog.remove_book(&job.path)?;
            catalog.finish_job(job.id)?;
            report.failed += 1;
            continue;
        }
        match job.kind {
            JobKind::Index => index_job(library, &blobs, catalog, &job.path, report)?,
        }
        catalog.finish_job(job.id)?;
    }
    Ok(())
}

/// Indexes one book file and stores it, or records why it can't be read.
fn index_job(
    library: &Path,
    blobs: &BlobStore,
    catalog: &mut Catalog,
    rel: &str,
    report: &mut ScanReport,
) -> Result<(), ScanError> {
    let path = library.join(rel);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => m,
        _ => {
            // Gone (or replaced by something else) since it was queued.
            if catalog.remove_book(rel)? {
                report.removed += 1;
            }
            catalog.remove_error(rel)?;
            return Ok(());
        }
    };
    // Taken before reading: if the file changes meanwhile, the next scan
    // sees a different time and reads it again.
    let stamp = (meta.len(), mtime_ns(&meta));
    match index_book(&path, blobs) {
        Ok(pages) => {
            let (series_path, series_name) = series_of(rel);
            catalog.replace_book(&NewBook {
                series_path,
                series_name,
                path: rel,
                folder: folder_of(rel),
                name: stem(rel),
                size: stamp.0,
                mtime: stamp.1,
                pages: &pages,
            })?;
            report.indexed += 1;
        }
        Err(e) => {
            log::error("scan: skipped a book", &[("path", rel), ("error", &e)]);
            catalog.record_error(rel, folder_of(rel), Some(stamp), &e)?;
            catalog.remove_book(rel)?;
            report.failed += 1;
        }
    }
    Ok(())
}

/// The folder a file is in, relative to the library; `""` for the root.
fn folder_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn is_cbz(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("cbz"))
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
