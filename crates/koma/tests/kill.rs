//! M2's exit check for indexing: `koma worker` killed with SIGKILL partway
//! through indexing a library, then started again, finishes cleanly. Books
//! indexed before the kill are not read again, the one being indexed is
//! read again in full, and every page comes back byte for byte.
//! (Variants have their own kill test in `variants.rs`.)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use koma::blobs::BlobStore;
use koma::catalog::Catalog;
use koma_fixtures::zip::{Compression, ZipWriter};

const BOOKS: usize = 40;
const PAGES: u32 = 4;

/// Book `b`, page `p`: different bytes for each, big enough that inflating
/// them into the blob store takes a while, and noisy like a scan, so the
/// CB7 does not compress past the archive ratio limit (a regular pattern
/// would, and be refused as a possible bomb).
fn page(b: usize, p: u32) -> Vec<u8> {
    let seed = b as u32 * 100 + p;
    // The CB7's pages are smaller: LZMA is slow on noise.
    let (w, h) = if b == BOOKS { (150, 220) } else { (600, 900) };
    koma_fixtures::png::gray(w, h, |x, y| {
        let mut h = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b) ^ seed;
        h ^= h >> 15;
        h = h.wrapping_mul(0x2c1b_3c6d);
        (h >> 24) as u8
    })
}

/// Forty books of deflated pages (inflated into the blob store at
/// indexing) and a solid CB7 (every page decoded at indexing).
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    let series = lib.join("Starfall");
    fs::create_dir_all(&series).unwrap();
    for b in 0..BOOKS {
        let mut z = ZipWriter::new();
        for p in 0..PAGES {
            z.add(&format!("{p:03}.png"), &page(b, p), Compression::Deflated);
        }
        fs::write(
            series.join(format!("Starfall v{:02}.cbz", b + 1)),
            z.finish(),
        )
        .unwrap();
    }
    let pages: Vec<(String, Vec<u8>)> = (0..PAGES)
        .map(|p| (format!("{p:03}.png"), page(BOOKS, p)))
        .collect();
    let files: Vec<(&str, &[u8])> = pages
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    fs::write(
        lib.join("Moonforge Chronicles.cb7"),
        koma_fixtures::sevenz::write(&files, true),
    )
    .unwrap();
    lib
}

fn books_indexed(db: &Path) -> i64 {
    Catalog::open_read_only(db)
        .and_then(|c| c.series_with_counts())
        .map(|s| s.iter().map(|(_, n)| n).sum())
        .unwrap_or(0)
}

/// `"key":"value"` of the `scan: done` log line.
fn reported(log: &str, key: &str) -> usize {
    let line = log.lines().find(|l| l.contains("\"scan: done\"")).unwrap();
    let start = line.find(&format!("\"{key}\":\"")).unwrap() + key.len() + 4;
    line[start..start + line[start..].find('"').unwrap()]
        .parse()
        .unwrap()
}

#[test]
fn killing_the_worker_while_indexing_then_restarting_finishes_cleanly() {
    let root = std::env::temp_dir().join(format!("koma-kill-index-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let lib = library(&root);
    let data = root.join("data");
    fs::create_dir_all(&data).unwrap();
    let db = data.join("koma.db");
    let total = BOOKS + 1;
    let koma = env!("CARGO_BIN_EXE_koma");
    let command = |cmd: &str| {
        let mut c = Command::new(koma);
        c.arg(cmd)
            .arg("--library")
            .arg(&lib)
            .arg("--data")
            .arg(&data);
        c.stdout(Stdio::null()).stderr(Stdio::piped());
        c
    };

    let mut worker = command("worker").arg("--rescan").arg("0").spawn().unwrap();
    let started = Instant::now();
    while books_indexed(&db) < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "nothing indexed"
        );
        assert!(worker.try_wait().unwrap().is_none(), "worker exited early");
        std::thread::sleep(Duration::from_millis(2));
    }
    worker.kill().unwrap();
    worker.wait().unwrap();
    let done = usize::try_from(books_indexed(&db)).unwrap();
    assert!(done < total, "the worker finished before it was killed");

    let out = command("scan").output().unwrap();
    let log = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{:?}\n{log}", out.status);
    // Only the books not stored before the kill are read; the one being
    // read when it came is read again from the start.
    assert_eq!(
        reported(&log, "indexed"),
        total - done,
        "done at kill: {done}\n{log}"
    );
    assert_eq!(reported(&log, "failed"), 0, "{log}");

    let catalog = Catalog::open_read_only(&db).unwrap();
    assert_eq!(catalog.job_count().unwrap(), 0);
    assert!(catalog.errors().unwrap().is_empty());
    let tmp = data.join("blobs/tmp");
    assert!(!tmp.exists() || fs::read_dir(&tmp).unwrap().next().is_none());

    // Every page, byte for byte, in order.
    let blobs = BlobStore::new(&data);
    let mut books = 0;
    for series in catalog.series().unwrap() {
        for book in catalog.books(series.id).unwrap() {
            let b = if book.path.ends_with(".cb7") {
                BOOKS
            } else {
                let n: usize = book.path[book.path.len() - 6..book.path.len() - 4]
                    .parse()
                    .unwrap();
                n - 1
            };
            let pages = catalog.pages(book.id).unwrap();
            assert_eq!(pages.len(), PAGES as usize, "{}", book.path);
            for (p, stored) in pages.iter().enumerate() {
                // Deflated and 7z pages are all in the blob store.
                assert_eq!(stored.offset, None, "{}", book.path);
                let bytes = fs::read(blobs.path(&stored.hash, &stored.format)).unwrap();
                assert_eq!(bytes, page(b, p as u32), "{} page {p}", book.path);
            }
            books += 1;
        }
    }
    assert_eq!(books, total);
    let _ = fs::remove_dir_all(&root);
}
