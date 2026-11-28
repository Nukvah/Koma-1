//! The library scan: finds book files, queues them for indexing, and the
//! jobs that index each one and record every page's hash and location in the
//! catalogue.
//!
//! - A series is a folder; each CBZ, ZIP or CB7 in it is a book, and so is
//!   each photo folder.
//! - Stored pages are located in place (file, offset, length) and never copied.
//! - Compressed pages (deflated ZIP members, every 7z page) are decompressed
//!   once, here, into the blob store, so serving never decompresses anything.
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

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::archive::page::{PageError, PageLimits, copy_page};
use crate::archive::sevenz;
use crate::archive::size::read_size;
use crate::archive::zip::{self, Limits, Method};
use crate::archive::{self};
use crate::blobs::BlobStore;
use crate::catalog::{
    self, Catalog, Chapter, Details, GONE_GRACE_SECS, JobKind, MAX_ATTEMPTS, NewBook, Page, Place,
    Stamp, TocRow,
};
use crate::epub;
use crate::log;
use crate::names;
use crate::variants;

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
    /// Books gone from the library because their file vanished (remembered
    /// for a while, in case they come back).
    pub removed: usize,
    /// Books found again, moved, renamed or back after a while away, and
    /// relinked to their id without being read again.
    pub moved: usize,
    /// Folders read because they are new or changed.
    pub folders_read: usize,
    /// Folders not read because their modification time is unchanged.
    pub folders_skipped: usize,
    /// Pages whose variants were made (or found impossible to make).
    pub variant_pages: usize,
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
            ("moved", &report.moved.to_string()),
            ("folders_read", &report.folders_read.to_string()),
            ("folders_skipped", &report.folders_skipped.to_string()),
            ("variant_pages", &report.variant_pages.to_string()),
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
    let now = unix_now();
    let mut walker = Walker {
        library,
        catalog,
        check,
        report: ScanReport::default(),
        now,
    };
    walker.visit("", None, &meta)?;
    walker.catalog.purge_gone(now - GONE_GRACE_SECS)?;
    Ok(walker.report)
}

struct Walker<'a> {
    library: &'a Path,
    catalog: &'a mut Catalog,
    check: Check,
    report: ScanReport,
    now: i64,
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
                    _ => self.report.removed += self.catalog.remove_tree(&child, self.now)?,
                }
            }
            return Ok(complete);
        }

        self.report.folders_read += 1;
        let listing = match list_folder(self.library, rel) {
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

        // The books here: each archive, or the folder itself if it is a
        // photo folder.
        let books: Vec<(String, Stamp)> = match listing.photo_stamp(meta) {
            Some(stamp) if !rel.is_empty() => vec![(rel.to_owned(), stamp)],
            _ => listing
                .archives
                .iter()
                .map(|(path, m)| (path.clone(), (m.len(), mtime_ns(m))))
                .collect(),
        };
        let mut present = HashSet::new();
        for (path, stamp) in &books {
            present.insert(path.as_str());
            if self.catalog.book_stamp(path)? == Some(*stamp)
                || self.catalog.error_stamp(path)? == Some(*stamp)
            {
                self.report.unchanged += 1;
            } else {
                self.catalog.enqueue(JobKind::Index, path)?;
            }
        }
        for (path, _, _) in self.catalog.books_in(rel)? {
            if !present.contains(path.as_str()) && self.catalog.vanish_book(&path, self.now)? {
                self.report.removed += 1;
            }
        }
        let dirs = &listing.dirs;
        let dir_names: HashSet<&str> = dirs.iter().map(|(d, _)| d.as_str()).collect();
        for (path, _) in self.catalog.errors_in(rel)? {
            if !present.contains(path.as_str()) && !dir_names.contains(path.as_str()) {
                self.catalog.remove_error(&path)?;
            }
        }
        for child in self.catalog.subfolders(rel)? {
            if !dir_names.contains(child.as_str()) {
                self.report.removed += self.catalog.remove_tree(&child, self.now)?;
            }
        }

        let mut complete = true;
        for (child, meta) in dirs {
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
    /// A photo folder's own images are listed for this.
    fn check_known_files(&mut self, rel: &str) -> Result<(), ScanError> {
        for (path, size, mtime) in self.catalog.books_in(rel)? {
            match self.current_stamp(&path) {
                Some(stamp) if stamp == (size, mtime) => self.report.unchanged += 1,
                Some(_) => {
                    self.catalog.enqueue(JobKind::Index, &path)?;
                }
                None => {
                    if self.catalog.vanish_book(&path, self.now)? {
                        self.report.removed += 1;
                    }
                }
            }
        }
        for (path, stamp) in self.catalog.errors_in(rel)? {
            let Some(stamp) = stamp else { continue };
            match self.current_stamp(&path) {
                Some(now) if now == stamp => {}
                Some(_) => {
                    self.catalog.enqueue(JobKind::Index, &path)?;
                }
                None => self.catalog.remove_error(&path)?,
            }
        }
        Ok(())
    }

    /// The stamp a book has now: an archive's, or a photo folder's. `None`
    /// if it is gone or no longer a book.
    fn current_stamp(&self, rel: &str) -> Option<Stamp> {
        let meta = fs::symlink_metadata(self.abs(rel)).ok()?;
        if meta.is_file() {
            Some((meta.len(), mtime_ns(&meta)))
        } else if meta.is_dir() {
            list_folder(self.library, rel).ok()?.photo_stamp(&meta)
        } else {
            None
        }
    }
}

/// Entries found in a folder: paths relative to the library, with metadata.
type Found = Vec<(String, Metadata)>;

/// What is directly in a folder that matters to the scan.
struct Listing {
    dirs: Found,
    /// Book archives: CBZ and ZIP.
    archives: Found,
    /// Page images.
    images: Found,
    /// Metadata files (`ComicInfo.xml`, `metadata.json`).
    meta: Found,
}

impl Listing {
    /// A photo folder holds images and nothing else that is read (no
    /// archives, no subfolders): it is one book, each image a page. A series
    /// folder with a `cover.jpg` next to its volumes is not one.
    ///
    /// Its stamp is the total size of its images and metadata files and the
    /// latest time of the folder and those files, so adding, removing or
    /// overwriting one changes it.
    fn photo_stamp(&self, folder: &Metadata) -> Option<Stamp> {
        if self.images.is_empty() || !self.archives.is_empty() || !self.dirs.is_empty() {
            return None;
        }
        let files = || self.images.iter().chain(&self.meta);
        let size = files().map(|(_, m)| m.len()).sum();
        let mtime = files()
            .map(|(_, m)| mtime_ns(m))
            .fold(mtime_ns(folder), i64::max);
        Some((size, mtime))
    }
}

/// Lists a folder. Paths are relative to the library; hidden entries and
/// symbolic links are skipped.
fn list_folder(library: &Path, rel: &str) -> io::Result<Listing> {
    let mut listing = Listing {
        dirs: Vec::new(),
        archives: Vec::new(),
        images: Vec::new(),
        meta: Vec::new(),
    };
    let dir = if rel.is_empty() {
        library.to_path_buf()
    } else {
        library.join(rel)
    };
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
        let child = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        if meta.is_dir() {
            listing.dirs.push((child, meta));
        } else if meta.is_file() && is_archive(name) {
            listing.archives.push((child, meta));
        } else if meta.is_file() && archive::is_page_name(name) {
            listing.images.push((child, meta));
        } else if meta.is_file() && crate::meta::layer_of(name).is_some() {
            listing.meta.push((child, meta));
        }
    }
    Ok(listing)
}

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
    // Once the queue is empty, pages still lacking variants are looked for
    // once: new books, and any left by an older version of the variants.
    let mut looked_for_variants = false;
    while !should_stop() {
        let Some(job) = catalog.claim_job()? else {
            if !looked_for_variants {
                looked_for_variants = true;
                // Indexing is done: bring search up to date before the
                // (long) variants work starts.
                if report.indexed + report.removed + report.moved > 0
                    || catalog.search_needs_filling()?
                {
                    catalog.rebuild_search()?;
                }
                let min_width = variants::WIDTHS[0];
                if catalog.queue_missing_variants(variants::VERSION, min_width)? > 0 {
                    continue;
                }
            }
            break;
        };
        if job.attempts > MAX_ATTEMPTS && job.kind == JobKind::Variants {
            variants::give_up_page(catalog, &job.path)?;
            catalog.finish_job(job.id)?;
            catalog.enqueue(JobKind::Variants, &job.path)?;
            continue;
        }
        if job.attempts > MAX_ATTEMPTS {
            let e = format!(
                "gave up after {MAX_ATTEMPTS} attempts: the worker stopped while reading it each time"
            );
            log::error(
                "scan: skipped a book",
                &[("path", &job.path), ("error", &e)],
            );
            catalog.record_error(&job.path, folder_of(&job.path), None, &e)?;
            catalog.vanish_book(&job.path, unix_now())?;
            catalog.finish_job(job.id)?;
            report.failed += 1;
            continue;
        }
        match job.kind {
            JobKind::Index => index_job(library, &blobs, catalog, &job.path, report)?,
            JobKind::Variants => {
                let (end, pages) =
                    variants::run_job(library, &blobs, catalog, &job.path, should_stop)?;
                report.variant_pages += pages;
                if end == variants::JobEnd::Stopped {
                    catalog.release_job(job.id)?;
                    break;
                }
            }
        }
        catalog.finish_job(job.id)?;
    }
    Ok(())
}

/// Indexes one book (an archive or a photo folder) and stores it, or
/// records why it can't be read.
fn index_job(
    library: &Path,
    blobs: &BlobStore,
    catalog: &mut Catalog,
    rel: &str,
    report: &mut ScanReport,
) -> Result<(), ScanError> {
    let path = library.join(rel);
    // The stamp is taken before reading: if the book changes meanwhile, the
    // next scan sees a different one and reads it again.
    let meta = fs::symlink_metadata(&path).ok();
    let found = match &meta {
        Some(m) if m.is_file() => Some(((m.len(), mtime_ns(m)), Source::Archive)),
        Some(m) if m.is_dir() => list_folder(library, rel)
            .ok()
            .and_then(|l| Some((l.photo_stamp(m)?, Source::Photos(l)))),
        _ => None,
    };
    let Some((stamp, source)) = found else {
        // Gone (or no longer a book) since it was queued.
        if catalog.vanish_book(rel, unix_now())? {
            report.removed += 1;
        }
        catalog.remove_error(rel)?;
        return Ok(());
    };
    let (folder, name, (series_path, series_name)) = match source {
        Source::Archive => (folder_of(rel), stem(rel), series_of(rel)),
        // A photo folder is its own folder, and is named as it is, dots and all.
        Source::Photos(_) => (rel, last_part(rel), series_of_folder(rel)),
    };
    let place = Place {
        path: rel,
        folder,
        name,
        series_path: &series_path,
        series_name: &series_name,
    };
    // A book already known by its stamp is placed without being read again:
    // the same file back at its path, or one moved or renamed outside Koma-1.
    let known = match catalog.book_at(rel)? {
        Some((id, old, gone)) if old == stamp => Some((id, gone)),
        Some(_) => None,
        None => catalog.gone_match(rel, stamp)?.map(|id| (id, true)),
    };
    if let Some((id, gone)) = known {
        catalog.relink(id, &place)?;
        if gone {
            report.moved += 1;
        } else {
            report.unchanged += 1;
        }
        return Ok(());
    }
    let plain = |r: Result<(Vec<Page>, MetaFiles), String>| r.map(|(p, m)| (p, m, Epub::default()));
    let read = match &source {
        Source::Archive if is_epub(rel) => index_epub(&path, blobs),
        Source::Archive if is_7z(rel) => plain(index_7z(&path, blobs)),
        Source::Archive => plain(index_book(&path, blobs)),
        Source::Photos(listing) => plain(index_photos(library, listing)),
    };
    match read {
        Ok((pages, mut meta_files, epub)) => {
            // Written next to an archive by Koma-1: over what is inside it.
            if matches!(source, Source::Archive)
                && let Some(side) = sidecar_meta(library, rel)
            {
                meta_files.push(side);
            }
            // An EPUB's own metadata is the weakest layer.
            let meta = crate::meta::merge(epub.meta, crate::meta::read(&meta_files));
            let details = details(name, &meta);
            catalog.replace_book(
                &NewBook {
                    series_path: &series_path,
                    series_name: &series_name,
                    details: &details,
                    path: rel,
                    folder,
                    name,
                    size: stamp.0,
                    mtime: stamp.1,
                    pages: &pages,
                    chapters: &epub.chapters,
                    toc: &epub.toc,
                },
                unix_now(),
            )?;
            report.indexed += 1;
        }
        Err(e) => {
            log::error("scan: skipped a book", &[("path", rel), ("error", &e)]);
            catalog.record_error(rel, folder, Some(stamp), &e)?;
            catalog.vanish_book(rel, unix_now())?;
            report.failed += 1;
        }
    }
    Ok(())
}

/// Where a book at `rel` belongs, as the scan would place it: its folder,
/// name, and series `(key, name)`. `photos`: a photo folder.
pub(crate) fn place_of(rel: &str, photos: bool) -> (String, String, (String, String)) {
    if photos {
        (
            rel.to_owned(),
            last_part(rel).to_owned(),
            series_of_folder(rel),
        )
    } else {
        (
            folder_of(rel).to_owned(),
            stem(rel).to_owned(),
            series_of(rel),
        )
    }
}

/// The `metadata.json` Koma-1 writes for a book, relative to the library:
/// next to an archive (`Starfall v07.metadata.json`), inside a photo folder.
pub fn sidecar_of(rel: &str, photos: bool) -> String {
    if photos {
        format!("{rel}/metadata.json")
    } else {
        match folder_of(rel) {
            "" => format!("{}.metadata.json", stem(rel)),
            dir => format!("{dir}/{}.metadata.json", stem(rel)),
        }
    }
}

/// An archive's `metadata.json` next to it, as the strongest layer.
fn sidecar_meta(library: &Path, rel: &str) -> Option<(usize, Vec<u8>)> {
    let path = library.join(sidecar_of(rel, false));
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > crate::meta::MAX_META_BYTES {
        return None;
    }
    let layer = crate::meta::layer_of("metadata.json")?;
    Some((layer, fs::read(path).ok()?))
}

/// What only an EPUB has: its chapters, contents and package metadata.
#[derive(Default)]
struct Epub {
    chapters: Vec<Chapter>,
    toc: Vec<TocRow>,
    meta: crate::meta::Meta,
}

enum Source {
    Archive,
    /// A photo folder, with its images and metadata files.
    Photos(Listing),
}

/// The folder a file is in, relative to the library; `""` for the root.
pub(crate) fn folder_of_path(rel: &str) -> &str {
    folder_of(rel)
}

/// The folder a file is in, relative to the library; `""` for the root.
fn folder_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Book files: `.cbz`, `.zip` (photo sets are often kept as plain zips),
/// 7z (`.cb7`, `.7z`), and EPUB.
fn is_archive(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && ["cbz", "zip", "cb7", "7z", "epub"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// An EPUB book (`.epub`).
pub fn is_epub(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("epub"))
}

fn is_7z(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("cb7") || ext.eq_ignore_ascii_case("7z"))
}

/// Books in a series named "Unsorted": loose files in the library root whose
/// name is only a number (`Ch. 12.cbz`).
pub const UNSORTED: &str = "Unsorted";

/// The series a book file belongs to, as `(key, name)`: its folder. A book
/// loose in the library root goes in the series its file name names
/// (`Starfall - Ch. 03.cbz` → Starfall), so loose chapters of one series
/// group together; the key of such a series starts with a NUL, which no
/// folder name has.
fn series_of(rel: &str) -> (String, String) {
    match rel.rsplit_once('/') {
        Some((dir, _)) => (dir.to_owned(), last_part(dir).to_owned()),
        None => {
            let parsed = names::book_name(stem(rel));
            let name = match parsed.series {
                Some(series) => series,
                None if parsed.number.is_some() => UNSORTED.to_owned(),
                None => stem(rel).to_owned(),
            };
            (format!("\0{}", name.to_lowercase()), name)
        }
    }
}

/// The series a photo folder belongs to: the folder above it, or the photo
/// folder itself in the root.
fn series_of_folder(rel: &str) -> (String, String) {
    match rel.rsplit_once('/') {
        Some((dir, _)) => (dir.to_owned(), last_part(dir).to_owned()),
        None => (rel.to_owned(), rel.to_owned()),
    }
}

fn last_part(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn stem(rel: &str) -> &str {
    let file = last_part(rel);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

/// Most pages a photo folder may have.
const MAX_PHOTO_PAGES: usize = 10_000;

/// Indexes a photo folder: each image is a page, read in place from its own
/// file. `cover.*` comes first, then natural order.
fn index_photos(library: &Path, listing: &Listing) -> Result<(Vec<Page>, MetaFiles), String> {
    let images = &listing.images;
    if images.len() > MAX_PHOTO_PAGES {
        return Err(format!("too many pages: over {MAX_PHOTO_PAGES}"));
    }
    let limits = PageLimits::default();
    let mut order: Vec<&(String, Metadata)> = images.iter().collect();
    order.sort_by(|(a, _), (b, _)| {
        is_cover(b)
            .cmp(&is_cover(a))
            .then_with(|| crate::natsort::compare_paths(a, b))
    });
    let pages = order
        .into_iter()
        .enumerate()
        .map(|(number, (rel, meta))| {
            if meta.len() > limits.max_bytes {
                return Err(format!("{}: page over limit: too large", last_part(rel)));
            }
            let mut file = File::open(library.join(rel)).map_err(|e| e.to_string())?;
            let mut hasher = blake3::Hasher::new();
            let read = io::copy(&mut (&mut file).take(limits.max_bytes + 1), &mut hasher)
                .map_err(|e| format!("{}: read error: {e}", last_part(rel)))?;
            if read != meta.len() {
                return Err(format!("{}: changed while being read", last_part(rel)));
            }
            let size = read_size(&file, 0, read);
            Ok(Page {
                number: number as i64,
                hash: hasher.finalize(),
                format: format_of(rel),
                length: read,
                offset: Some(0),
                file: Some(rel.clone()),
                size,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let meta_files = listing
        .meta
        .iter()
        .filter(|(_, m)| m.len() <= crate::meta::MAX_META_BYTES)
        .filter_map(|(rel, _)| {
            let layer = crate::meta::layer_of(rel)?;
            Some((layer, fs::read(library.join(rel)).ok()?))
        })
        .collect();
    Ok((pages, meta_files))
}

/// `cover.jpg`, `Cover.PNG`… (any page extension).
fn is_cover(rel: &str) -> bool {
    stem(rel).eq_ignore_ascii_case("cover")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub(crate) fn mtime_ns(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// Indexes one CBZ: every page hashed, stored pages located, compressed
/// pages inflated into the blob store.
fn index_book(path: &Path, blobs: &BlobStore) -> Result<(Vec<Page>, MetaFiles), String> {
    let entries = zip::read_file_index(path, &Limits::default()).map_err(|e| e.to_string())?;
    let pages = archive::pages(&entries);
    if pages.is_empty() {
        return Err("no pages".into());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let meta_files = archive_meta_files(&file, &entries);
    let limits = PageLimits::default();
    let pages = pages
        .iter()
        .enumerate()
        .map(|(number, entry)| page_of(&file, entry, number as i64, &limits, blobs))
        .collect::<Result<Vec<_>, PageError>>()
        .map_err(|e| e.to_string())?;
    Ok((pages, meta_files))
}

/// One page of a zip book: hashed in place if stored, else inflated into
/// the blob store.
fn page_of(
    file: &File,
    entry: &zip::Entry,
    number: i64,
    limits: &PageLimits,
    blobs: &BlobStore,
) -> Result<Page, PageError> {
    let format = format_of(&entry.name);
    let length = entry.uncompressed_size;
    let (hash, offset, size) = match entry.method {
        Method::Stored => {
            let mut hasher = blake3::Hasher::new();
            copy_page(file, entry, limits, &mut hasher)?;
            let size = read_size(file, entry.data_offset, length);
            (hasher.finalize(), Some(entry.data_offset), size)
        }
        _ => {
            let hash = inflate_to_blob(file, entry, limits, blobs, &format)?;
            let blob = File::open(blobs.path(&hash, &format))?;
            (hash, None, read_size(&blob, 0, length))
        }
    };
    Ok(Page {
        number,
        hash,
        format,
        length,
        offset,
        file: None,
        size,
    })
}

/// Indexes one EPUB: its package, its pictures as pages (the cover first),
/// its chapters cleaned into the blob store, and its contents. See
/// [`crate::epub`].
fn index_epub(path: &Path, blobs: &BlobStore) -> Result<(Vec<Page>, MetaFiles, Epub), String> {
    let entries = zip::read_file_index(path, &Limits::default()).map_err(|e| e.to_string())?;
    let file = File::open(path).map_err(|e| e.to_string())?;
    let find = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name && !e.is_dir() && !e.encrypted)
    };
    let xml_limits = PageLimits {
        max_bytes: epub::MAX_XML_BYTES,
        ..PageLimits::default()
    };
    let read = |name: &str| -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        copy_page(&file, find(name)?, &xml_limits, &mut bytes).ok()?;
        Some(bytes)
    };
    let package_path = read("META-INF/container.xml")
        .and_then(|c| epub::rootfile(&c))
        .ok_or("not an EPUB: META-INF/container.xml names no package")?;
    let package = epub::package(
        &package_path,
        &read(&package_path).ok_or("EPUB package missing or unreadable")?,
    );
    if package.fixed_layout {
        return Err("fixed-layout EPUB: not read yet (comes in M11)".into());
    }
    if package.spine.is_empty() {
        return Err("EPUB with no chapters".into());
    }
    // Pictures are the book's pages: the cover first, so it is the cover.
    // A picture that cannot be read is left out, not the book.
    let limits = PageLimits::default();
    let mut pages = Vec::new();
    let mut pictures = HashMap::new();
    for name in package.images() {
        let Some(entry) = find(name) else { continue };
        let Ok(page) = page_of(&file, entry, pages.len() as i64, &limits, blobs) else {
            continue;
        };
        pictures.insert(
            name.to_owned(),
            epub::Picture {
                url: format!("/p/{}.{}", page.hash.to_hex(), page.format),
                width: page.size.map(|s| s.width),
                height: page.size.map(|s| s.height),
            },
        );
        pages.push(page);
    }
    let toc = package
        .nav
        .as_deref()
        .and_then(|p| Some(epub::nav(p, &read(p)?)))
        .filter(|t| !t.is_empty())
        .or_else(|| {
            let p = package.ncx.as_deref()?;
            Some(epub::ncx(p, &read(p)?))
        })
        .unwrap_or_default();
    let numbers: HashMap<String, usize> = package
        .spine
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i + 1))
        .collect();
    let mut chapters = Vec::new();
    for (i, chapter) in package.spine.iter().enumerate() {
        // A chapter that cannot be read stays, empty, so the numbers hold.
        let html = epub::clean(
            &read(chapter).unwrap_or_default(),
            &epub::Links {
                path: chapter,
                chapters: &numbers,
                pictures: &pictures,
            },
        );
        let hash = blobs
            .put(html.as_bytes(), "html")
            .map_err(|e| e.to_string())?;
        let title = toc
            .iter()
            .find(|t| &t.path == chapter)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        chapters.push(Chapter {
            number: i as i64,
            hash,
            length: html.len() as u64,
            title,
        });
    }
    let toc = toc
        .into_iter()
        .filter_map(|t| {
            Some(TocRow {
                chapter: *numbers.get(&t.path)? as i64 - 1,
                level: t.level,
                title: t.title,
                anchor: t.anchor,
            })
        })
        .collect();
    let meta_files = archive_meta_files(&file, &entries);
    Ok((
        pages,
        meta_files,
        Epub {
            chapters,
            toc,
            meta: package.meta,
        },
    ))
}

/// Indexes one 7z book (CB7): read once, front to back, every page
/// decompressed into the blob store as it comes.
fn index_7z(path: &Path, blobs: &BlobStore) -> Result<(Vec<Page>, MetaFiles), String> {
    let limits = PageLimits::default();
    let wanted = |m: &sevenz::Member| -> Option<u64> {
        if m.is_dir || archive::is_hidden(&m.name) {
            None
        } else if archive::is_page_name(&m.name) {
            Some(limits.max_bytes)
        } else if crate::meta::layer_of(&m.name).is_some() {
            Some(crate::meta::MAX_META_BYTES)
        } else {
            None
        }
    };
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut found: Vec<(String, blake3::Hash, u64)> = Vec::new();
    let mut meta_files = Vec::new();
    let mut temps = Vec::new();
    let read = sevenz::read(
        io::BufReader::new(file),
        &sevenz::Limits::default(),
        wanted,
        |member, data| {
            if let Some(layer) = crate::meta::layer_of(&member.name)
                && !archive::is_page_name(&member.name)
            {
                let mut bytes = Vec::new();
                data.read_to_end(&mut bytes)?;
                meta_files.push((layer, bytes));
                return Ok(());
            }
            let (temp, out) = blobs.temp()?;
            temps.push(temp.clone());
            let mut tee = Tee {
                hasher: blake3::Hasher::new(),
                out: BufWriter::new(out),
            };
            let length = io::copy(data, &mut tee)?;
            tee.out.flush()?;
            if length != member.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "size differs from the archive's",
                ));
            }
            let format = format_of(&member.name);
            let hash = tee.hasher.finalize();
            blobs.commit(&temp, &hash, &format)?;
            temps.pop();
            found.push((member.name.clone(), hash, length));
            Ok(())
        },
    );
    for temp in temps {
        let _ = fs::remove_file(temp);
    }
    read.map_err(|e| e.to_string())?;
    if found.is_empty() {
        return Err("no pages".into());
    }
    found.sort_by(|a, b| crate::natsort::compare_paths(&a.0, &b.0));
    let pages = found
        .into_iter()
        .enumerate()
        .map(|(number, (name, hash, length))| {
            let format = format_of(&name);
            let size = File::open(blobs.path(&hash, &format))
                .ok()
                .and_then(|blob| read_size(&blob, 0, length));
            Page {
                number: number as i64,
                hash,
                format,
                length,
                offset: None,
                file: None,
                size,
            }
        })
        .collect();
    Ok((pages, meta_files))
}

/// Metadata files found in a book, as `(layer, bytes)` for [`crate::meta::read`].
type MetaFiles = Vec<(usize, Vec<u8>)>;

/// The metadata files of an archive, in any of its folders. One that can't
/// be read (too big, corrupt) is left out: metadata never fails a book.
fn archive_meta_files(file: &File, entries: &[zip::Entry]) -> MetaFiles {
    let limits = PageLimits {
        max_bytes: crate::meta::MAX_META_BYTES,
        ..PageLimits::default()
    };
    entries
        .iter()
        .filter(|e| !e.is_dir() && !e.encrypted && !archive::is_hidden(&e.name))
        .filter_map(|e| {
            let layer = crate::meta::layer_of(&e.name)?;
            let mut bytes = Vec::new();
            copy_page(file, e, &limits, &mut bytes).ok()?;
            Some((layer, bytes))
        })
        .collect()
}

/// A book's details: its metadata, with gaps filled from its file name.
fn details(stem: &str, meta: &crate::meta::Meta) -> Details {
    let from_name = names::book_name(stem);
    let from_meta = |s: &str| s.trim().parse::<f64>().ok().filter(|n| n.is_finite());
    // Metadata wins; the file name fills the gap.
    let (kind, number) = match (from_meta(&meta.number), from_meta(&meta.volume)) {
        // A number the name also gives keeps the name's chapter or volume.
        (Some(n), _) if from_name.number == Some(n) => (from_name.kind, Some(n)),
        (Some(n), _) => (Some(names::Kind::Chapter), Some(n)),
        (None, Some(n)) => (Some(names::Kind::Volume), Some(n)),
        (None, None) => (from_name.kind, from_name.number),
    };
    let or_name = |list: &Vec<String>, name: Option<String>| {
        if list.is_empty() {
            name.into_iter().collect()
        } else {
            list.clone()
        }
    };
    Details {
        title: meta.title.clone(),
        number,
        kind: kind.map(|k| k.as_str()),
        summary: meta.summary.clone(),
        year: meta.year,
        language: meta.language.clone(),
        direction: meta.direction.map(|d| d.as_str()),
        cover_page: meta.cover_page,
        authors: or_name(&meta.authors, from_name.author),
        publishers: or_name(&meta.publishers, from_name.publisher),
        genres: meta.genres.clone(),
        tags: meta.tags.clone(),
    }
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

    fn pair(key: &str, name: &str) -> (String, String) {
        (key.to_owned(), name.to_owned())
    }

    #[test]
    fn series_is_the_folder() {
        assert_eq!(
            series_of("Starfall/Starfall v01.cbz"),
            pair("Starfall", "Starfall")
        );
        assert_eq!(
            series_of("Shelf/Starfall/v01.cbz"),
            pair("Shelf/Starfall", "Starfall")
        );
    }

    #[test]
    fn loose_books_group_by_the_series_their_name_names() {
        assert_eq!(
            series_of("Lone Comet.cbz"),
            pair("\0lone comet", "Lone Comet")
        );
        assert_eq!(
            series_of("[Moonforge (Mirelle)] Starfall Academy - Ch. 03.cbz"),
            pair("\0starfall academy", "Starfall Academy")
        );
        assert_eq!(
            series_of("Starfall Academy Vol. 2.cbz"),
            pair("\0starfall academy", "Starfall Academy")
        );
        assert_eq!(series_of("Ch. 12.cbz"), pair("\0unsorted", UNSORTED));
    }

    #[test]
    fn photo_folder_series() {
        assert_eq!(
            series_of_folder("Mirelle/Set 1.5"),
            pair("Mirelle", "Mirelle")
        );
        assert_eq!(series_of_folder("Set 1.5"), pair("Set 1.5", "Set 1.5"));
        assert!(is_cover("Set/Cover.JPG"));
        assert!(!is_cover("Set/cover 2.jpg"));
    }

    #[test]
    fn names() {
        assert!(is_archive("a.CBZ"));
        assert!(is_archive("a.zip"));
        assert!(!is_archive(".cbz"));
        assert!(!is_archive("a.cbr"));
        assert_eq!(stem("x/Ch. 10.5.cbz"), "Ch. 10.5");
        assert_eq!(format_of("p/1.JPEG"), "jpg");
    }
}
