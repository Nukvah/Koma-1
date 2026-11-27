//! The scan against the made-up library: every page can be found again,
//! byte for byte, and rescans, removals and broken files behave.

use std::fs;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use koma::blobs::BlobStore;
use koma::catalog::Catalog;
use koma::catalog::JobKind;
use koma::scan::{Check, ScanReport, discover, run_jobs, scan};
use koma_fixtures::zip::{Compression, ZipWriter};

struct Setup {
    root: PathBuf,
    library: PathBuf,
    data: PathBuf,
    books: Vec<koma_fixtures::library::Book>,
    catalog: Catalog,
}

impl Setup {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "koma-scan-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let library = root.join("library");
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let books = koma_fixtures::library::write(&library).unwrap();
        backdate(&library);
        let catalog = Catalog::open(&data.join("koma.db")).unwrap();
        Self {
            root,
            library,
            data,
            books,
            catalog,
        }
    }

    fn scan(&mut self) -> ScanReport {
        scan(&self.library, &self.data, &mut self.catalog).unwrap()
    }

    /// A periodic rescan: folder times only.
    fn quick_scan(&mut self) -> ScanReport {
        let mut report = discover(&self.library, &mut self.catalog, Check::Folders).unwrap();
        run_jobs(
            &self.library,
            &self.data,
            &mut self.catalog,
            &mut report,
            &|| false,
        )
        .unwrap();
        report
    }

    /// Reads a page back from wherever the catalogue says it is.
    fn read_page(&self, book_path: &str, page: &koma::catalog::Page) -> Vec<u8> {
        let mut buf = vec![0u8; page.length as usize];
        match page.offset {
            Some(offset) => {
                let path = page.file.as_deref().unwrap_or(book_path);
                let file = fs::File::open(self.library.join(path)).unwrap();
                file.read_exact_at(&mut buf, offset).unwrap();
            }
            None => {
                buf = fs::read(BlobStore::new(&self.data).path(&page.hash, &page.format)).unwrap();
            }
        }
        buf
    }

    fn book_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for series in self.catalog.series().unwrap() {
            for book in self.catalog.books(series.id).unwrap() {
                paths.push(book.path);
            }
        }
        paths
    }
}

/// Sets every folder's modification time to the same moment long ago, as if
/// the library had been written then. (A folder changed in the last seconds is
/// always read again, since coarse file system times could hide a change.)
fn backdate(dir: &Path) {
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            backdate(&entry.path());
        }
    }
    fs::File::open(dir).unwrap().set_modified(old).unwrap();
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn every_page_is_found_again_byte_for_byte() {
    let mut s = Setup::new();
    let report = s.scan();
    assert_eq!(
        report,
        ScanReport {
            indexed: 7,
            folders_read: 4,
            ..ScanReport::default()
        }
    );

    let series: Vec<String> = s
        .catalog
        .series()
        .unwrap()
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert_eq!(
        series,
        [
            "Lone Comet",
            "Mirelle's Garden",
            "Moonforge Chronicles",
            "Starfall"
        ]
    );
    // Natural order of books: v02 before v10, Ch. 001 before Ch. 001.5.
    assert_eq!(
        s.book_paths(),
        [
            "Lone Comet.cbz",
            "Mirelle's Garden/Mirelle's Garden 第1巻.cbz",
            "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.cbz",
            "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.5.cbz",
            "Starfall/Starfall v01.cbz",
            "Starfall/Starfall v02.cbz",
            "Starfall/Starfall v10.cbz",
        ]
    );

    let mut in_place = 0;
    let mut in_blobs = 0;
    for series in s.catalog.series().unwrap() {
        for book in s.catalog.books(series.id).unwrap() {
            let fixture = s.books.iter().find(|b| b.path == book.path).unwrap();
            let pages = s.catalog.pages(book.id).unwrap();
            assert_eq!(pages.len(), fixture.pages.len(), "{}", book.path);
            assert_eq!(book.page_count as usize, pages.len());
            for (page, (name, bytes)) in pages.iter().zip(&fixture.pages) {
                assert_eq!(page.hash, blake3::hash(bytes), "{} {name}", book.path);
                assert_eq!(
                    &s.read_page(&book.path, page),
                    bytes,
                    "{} {name}",
                    book.path
                );
                assert_eq!(page.format, "png");
                let found = s.catalog.page_by_hash(&page.hash).unwrap().unwrap();
                assert_eq!(found.length, bytes.len() as u64);
                match page.offset {
                    Some(_) => in_place += 1,
                    None => in_blobs += 1,
                }
            }
        }
    }
    // Only the deflated book (8 pages) needed the blob store.
    assert_eq!((in_place, in_blobs), (25, 8));
}

#[test]
fn rescans_skip_unchanged_books_and_follow_changes() {
    let mut s = Setup::new();
    s.scan();
    // Nothing changed: no folder is read again, each book is only checked.
    assert_eq!(
        s.scan(),
        ScanReport {
            unchanged: 7,
            folders_skipped: 4,
            ..ScanReport::default()
        }
    );

    // A one-shot is deleted, and a book is overwritten in place by a
    // different file (which leaves its folder's time as it was).
    fs::remove_file(s.library.join("Lone Comet.cbz")).unwrap();
    let mut w = ZipWriter::new();
    w.add(
        "1.png",
        &koma_fixtures::png::page(20, 30, 900),
        Compression::Stored,
    );
    fs::write(s.library.join("Starfall/Starfall v10.cbz"), w.finish()).unwrap();

    assert_eq!(
        s.scan(),
        ScanReport {
            indexed: 1,
            unchanged: 5,
            removed: 1,
            folders_read: 1,
            folders_skipped: 3,
            ..ScanReport::default()
        }
    );
    let series: Vec<String> = s
        .catalog
        .series()
        .unwrap()
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert!(!series.contains(&"Lone Comet".to_string()));
    let starfall = s
        .catalog
        .series()
        .unwrap()
        .into_iter()
        .find(|x| x.name == "Starfall")
        .unwrap();
    let v10 = s.catalog.books(starfall.id).unwrap().pop().unwrap();
    assert_eq!(v10.page_count, 1);
}

#[test]
fn a_restart_reads_nothing_again() {
    let mut s = Setup::new();
    s.scan();
    drop(std::mem::replace(
        &mut s.catalog,
        Catalog::open(&s.data.join("koma.db")).unwrap(),
    ));
    assert_eq!(
        s.quick_scan(),
        ScanReport {
            folders_skipped: 4,
            ..ScanReport::default()
        }
    );
}

#[test]
fn quick_rescans_read_only_changed_folders() {
    let mut s = Setup::new();
    s.scan();
    let starfall = s.library.join("Starfall");
    let one_page = |seed| {
        let mut w = ZipWriter::new();
        w.add(
            "1.png",
            &koma_fixtures::png::page(20, 30, seed),
            Compression::Stored,
        );
        w.finish()
    };

    // A new book: only its folder is read, and only the new book indexed.
    fs::write(starfall.join("Starfall v11.cbz"), one_page(1)).unwrap();
    let report = s.quick_scan();
    assert_eq!((report.indexed, report.folders_read), (1, 1));
    assert_eq!(
        report.unchanged, 3,
        "the folder's other books are only stat'ed"
    );

    // Folders changed seconds ago are read once more (see `backdate`); let
    // them settle so each step below only sees its own change.
    let settle = |s: &mut Setup| {
        backdate(&s.library);
        s.quick_scan();
    };

    // A new series folder, deep down: the folders above it are read too,
    // since adding it changed their times.
    settle(&mut s);
    let deep = s.library.join("Shelf/Moonforge");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("Moonforge 01.cbz"), one_page(2)).unwrap();
    let report = s.quick_scan();
    assert_eq!((report.indexed, report.folders_read), (1, 3));
    assert!(
        s.book_paths()
            .contains(&"Shelf/Moonforge/Moonforge 01.cbz".to_string())
    );

    // A whole folder deleted: its books are forgotten, and its series.
    settle(&mut s);
    fs::remove_dir_all(s.library.join("Shelf")).unwrap();
    let report = s.quick_scan();
    assert_eq!((report.removed, report.folders_read), (1, 1));
    assert!(!s.book_paths().iter().any(|p| p.starts_with("Shelf/")));
    assert!(
        s.catalog
            .series()
            .unwrap()
            .iter()
            .all(|x| x.name != "Moonforge")
    );

    // A folder renamed: its books are found again at the new path without
    // being read (see stable_ids.rs).
    settle(&mut s);
    fs::rename(&starfall, s.library.join("Starfall (old)")).unwrap();
    let report = s.quick_scan();
    assert_eq!((report.indexed, report.removed, report.moved), (0, 4, 4));
    assert!(s.book_paths().iter().all(|p| !p.starts_with("Starfall/")));

    settle(&mut s);
    assert_eq!(s.quick_scan().folders_read, 0);
}

#[test]
fn jobs_cut_short_run_again_and_are_given_up_after_three_tries() {
    let mut s = Setup::new();
    let mut report = discover(&s.library, &mut s.catalog, Check::Files).unwrap();
    assert_eq!(s.catalog.job_count().unwrap(), 7);

    // The worker dies in the middle of a job: it stays claimed.
    let job = s.catalog.claim_job().unwrap().unwrap();
    assert_eq!((job.kind, job.attempts), (JobKind::Index, 1));
    drop(std::mem::replace(
        &mut s.catalog,
        Catalog::open(&s.data.join("koma.db")).unwrap(),
    ));
    assert_eq!(s.catalog.requeue_running().unwrap(), 1);
    run_jobs(&s.library, &s.data, &mut s.catalog, &mut report, &|| false).unwrap();
    assert_eq!(report.indexed, 7);
    assert_eq!(s.catalog.job_count().unwrap(), 0);
    assert!(s.catalog.errors().unwrap().is_empty());

    // A book the worker dies on every time is given up after three tries.
    assert!(s.catalog.enqueue(JobKind::Index, &job.path).unwrap());
    assert!(
        !s.catalog.enqueue(JobKind::Index, &job.path).unwrap(),
        "no duplicates"
    );
    for _ in 0..3 {
        s.catalog.claim_job().unwrap().unwrap();
        s.catalog.requeue_running().unwrap();
    }
    let mut report = ScanReport::default();
    run_jobs(&s.library, &s.data, &mut s.catalog, &mut report, &|| false).unwrap();
    assert_eq!(report.failed, 1);
    let errors = s.catalog.errors().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, job.path);
    assert!(errors[0].1.starts_with("gave up after 3 attempts"));
}

#[test]
fn a_stop_is_honoured_between_jobs() {
    let mut s = Setup::new();
    let mut report = discover(&s.library, &mut s.catalog, Check::Files).unwrap();
    let done = std::cell::Cell::new(0);
    run_jobs(&s.library, &s.data, &mut s.catalog, &mut report, &|| {
        done.set(done.get() + 1);
        done.get() > 2
    })
    .unwrap();
    assert_eq!(report.indexed, 2);
    assert_eq!(s.catalog.job_count().unwrap(), 5);
}

#[test]
fn broken_files_are_recorded_and_skipped() {
    let mut s = Setup::new();
    fs::write(
        s.library.join("Starfall/Broken v99.cbz"),
        b"not a zip at all",
    )
    .unwrap();
    let mut w = ZipWriter::new();
    w.add("ComicInfo.xml", b"<ComicInfo/>", Compression::Stored);
    fs::write(s.library.join("Starfall/No pages.cbz"), w.finish()).unwrap();

    let report = s.scan();
    assert_eq!((report.indexed, report.failed), (7, 2));
    // Unchanged broken files are not read again.
    let report = s.scan();
    assert_eq!((report.indexed, report.failed), (0, 0));
    assert_eq!(s.catalog.errors().unwrap().len(), 2);
    let errors = s.catalog.errors().unwrap();
    assert_eq!(
        errors,
        [
            (
                "Starfall/Broken v99.cbz".to_string(),
                "not a zip archive".to_string()
            ),
            ("Starfall/No pages.cbz".to_string(), "no pages".to_string()),
        ]
    );

    // Fixed files are picked up and their errors cleared.
    fs::remove_file(s.library.join("Starfall/Broken v99.cbz")).unwrap();
    fs::remove_file(s.library.join("Starfall/No pages.cbz")).unwrap();
    let report = s.scan();
    assert_eq!(report.failed, 0);
    assert!(s.catalog.errors().unwrap().is_empty());
}

/// Offset of the first central directory header of a classic zip.
fn central_header(zip: &[u8]) -> usize {
    let eocd = zip.len() - 22;
    u32::from_le_bytes(zip[eocd + 16..eocd + 20].try_into().unwrap()) as usize
}

fn one_page_zip(compression: Compression) -> Vec<u8> {
    let mut w = ZipWriter::new();
    w.add("1.png", &koma_fixtures::png::page(20, 30, 7), compression);
    w.finish()
}

fn scan_single(zip: Vec<u8>) -> String {
    let dir = std::env::temp_dir().join(format!(
        "koma-scan-bad-{}-{}",
        std::process::id(),
        blake3::hash(&zip)
    ));
    let library = dir.join("library");
    fs::create_dir_all(&library).unwrap();
    fs::write(library.join("Bad.cbz"), zip).unwrap();
    let mut catalog = Catalog::open(&dir.join("koma.db")).unwrap();
    let report = scan(&library, &dir, &mut catalog).unwrap();
    let errors = catalog.errors().unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(report.failed, 1);
    errors[0].1.clone()
}

#[test]
fn corrupt_page_bytes_fail_the_crc_check() {
    let mut zip = one_page_zip(Compression::Stored);
    zip[100] ^= 0xFF; // inside the page data
    assert_eq!(scan_single(zip), "corrupt page: CRC-32 mismatch");
}

#[test]
fn a_deflated_page_that_lies_about_its_size_is_rejected() {
    let mut zip = one_page_zip(Compression::Deflated);
    let cd = central_header(&zip);
    let size = u32::from_le_bytes(zip[cd + 24..cd + 28].try_into().unwrap());
    zip[cd + 24..cd + 28].copy_from_slice(&(size - 1).to_le_bytes());
    assert_eq!(
        scan_single(zip),
        "corrupt page: size differs from the archive's"
    );
}

#[test]
fn a_page_that_claims_a_huge_ratio_is_rejected_before_inflating() {
    let mut zip = one_page_zip(Compression::Deflated);
    let cd = central_header(&zip);
    zip[cd + 24..cd + 28].copy_from_slice(&200_000_000u32.to_le_bytes());
    assert_eq!(
        scan_single(zip),
        "page over limit: compression ratio too high"
    );
}

#[test]
fn hidden_files_and_symlinks_are_ignored() {
    let mut s = Setup::new();
    let hidden = s.library.join(".trash");
    fs::create_dir_all(&hidden).unwrap();
    fs::copy(s.library.join("Lone Comet.cbz"), hidden.join("Old.cbz")).unwrap();
    std::os::unix::fs::symlink(
        s.library.join("Lone Comet.cbz"),
        s.library.join("Link to Lone Comet.cbz"),
    )
    .unwrap();
    assert_eq!(s.scan().indexed, 7);
}

#[test]
fn identical_pages_share_one_blob() {
    let mut s = Setup::new();
    let page = koma_fixtures::png::page(20, 30, 4242);
    for name in ["Twins/A.cbz", "Twins/B.cbz"] {
        let mut w = ZipWriter::new();
        w.add("1.png", &page, Compression::Deflated);
        let path = s.library.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, w.finish()).unwrap();
    }
    s.scan();
    let blob = BlobStore::new(&s.data).path(&blake3::hash(&page), "png");
    assert_eq!(fs::read(blob).unwrap(), page);
    let blob_dir: &Path = &s.data.join("blobs");
    let count = fs::read_dir(blob_dir.join(&blake3::hash(&page).to_hex()[..2]))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&blake3::hash(&page).to_hex()[..])
        })
        .count();
    assert_eq!(count, 1);
}

#[test]
fn photo_folders_are_books_with_the_cover_first() {
    let mut s = Setup::new();
    let set = s.library.join("Mirelle/Set 1.5");
    fs::create_dir_all(&set).unwrap();
    let page = |seed| koma_fixtures::png::page(20, 30, seed);
    for (name, seed) in [("10.png", 10), ("2.png", 2), ("Cover.PNG", 99)] {
        fs::write(set.join(name), page(seed)).unwrap();
    }
    fs::write(set.join("notes.txt"), b"not a page").unwrap();
    fs::write(set.join(".thumbs.jpg"), page(7)).unwrap();
    // A series folder with a cover image next to its volumes is not a photo folder.
    fs::write(s.library.join("Starfall/cover.jpg"), page(5)).unwrap();
    // A set kept as a plain zip is a book too.
    let mut w = ZipWriter::new();
    w.add("1.png", &page(1), Compression::Stored);
    fs::write(s.library.join("Mirelle/Set 2.zip"), w.finish()).unwrap();

    let report = s.scan();
    assert_eq!((report.indexed, report.failed), (9, 0));
    let mirelle = s
        .catalog
        .series()
        .unwrap()
        .into_iter()
        .find(|x| x.name == "Mirelle")
        .unwrap();
    let books = s.catalog.books(mirelle.id).unwrap();
    let names: Vec<&str> = books.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["Set 1.5", "Set 2"]);
    assert_eq!(books[0].path, "Mirelle/Set 1.5");

    let pages = s.catalog.pages(books[0].id).unwrap();
    let files: Vec<&str> = pages.iter().map(|p| p.file.as_deref().unwrap()).collect();
    assert_eq!(
        files,
        [
            "Mirelle/Set 1.5/Cover.PNG",
            "Mirelle/Set 1.5/2.png",
            "Mirelle/Set 1.5/10.png"
        ]
    );
    for (p, seed) in pages.iter().zip([99, 2, 10]) {
        assert_eq!(s.read_page(&books[0].path, p), page(seed));
        let at = s.catalog.page_by_hash(&p.hash).unwrap().unwrap();
        assert_eq!(
            (at.book_path.as_str(), at.offset),
            (p.file.as_deref().unwrap(), Some(0))
        );
    }

    // An image overwritten in place is found by a files check; one added, by
    // a quick rescan.
    backdate(&s.library);
    s.quick_scan();
    fs::write(set.join("2.png"), page(3)).unwrap();
    fs::File::open(&set)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))
        .unwrap();
    assert_eq!(s.quick_scan().indexed, 0);
    assert_eq!(s.scan().indexed, 1);
    fs::write(set.join("3.png"), page(4)).unwrap();
    assert_eq!(s.quick_scan().indexed, 1);
    let book = s.catalog.books(mirelle.id).unwrap().remove(0);
    let pages = s.catalog.pages(book.id).unwrap();
    assert_eq!(pages.len(), 4);
    assert_eq!(s.read_page(&books[0].path, &pages[1]), page(3));

    // A subfolder makes it a folder of books again: the photo book goes.
    fs::create_dir(set.join("Extras")).unwrap();
    fs::write(set.join("Extras/1.png"), page(8)).unwrap();
    let report = s.scan();
    assert_eq!((report.indexed, report.removed), (1, 1));
    assert!(
        s.book_paths()
            .contains(&"Mirelle/Set 1.5/Extras".to_string())
    );
    assert!(!s.book_paths().contains(&"Mirelle/Set 1.5".to_string()));
}
