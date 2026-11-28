//! Page variants end to end: the worker makes them after indexing, once per
//! page content, picks up where it stopped (or was killed), and `koma serve`
//! serves them at their immutable URLs.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use koma::blobs::BlobStore;
use koma::catalog::{Catalog, JobKind};
use koma::scan::{Check, discover, run_jobs, scan};
use koma::variants::{self, Format, VERSION};
use koma_fixtures::png;
use koma_fixtures::zip::{Compression, ZipWriter};

fn temp_root(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = std::env::temp_dir().join(format!(
        "koma-variants-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    root
}

/// A gray page, wide enough for two variants, different for every `seed`.
fn big_page(seed: u32) -> Vec<u8> {
    png::page(1200, 1700, seed)
}

/// A colour JPEG page, wide enough for one variant.
fn colour_jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut rgb = Vec::new();
    for y in 0..u32::from(h) {
        for x in 0..u32::from(w) {
            rgb.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 90]);
        }
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 90)
        .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .unwrap();
    out
}

fn cbz(path: &Path, pages: &[Vec<u8>], compression: Compression) {
    let mut z = ZipWriter::new();
    for (n, p) in pages.iter().enumerate() {
        let ext = if p.starts_with(b"\x89PNG") {
            "png"
        } else {
            "jpg"
        };
        z.add(&format!("{:03}.{ext}", n + 1), p, compression);
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// Starfall v01 (stored): pages 1 and 2 big, page 3 small.
/// Starfall v02 (deflated, so its pages are blobs): page 2 of v01 again, and a colour JPEG.
/// Mirelle Set (photo folder): a big page.
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    let shared = big_page(2);
    cbz(
        &lib.join("Starfall/Starfall v01.cbz"),
        &[big_page(1), shared.clone(), png::page(60, 90, 3)],
        Compression::Stored,
    );
    cbz(
        &lib.join("Starfall/Starfall v02.cbz"),
        &[shared, colour_jpeg(1000, 1500)],
        Compression::Deflated,
    );
    fs::create_dir_all(lib.join("Mirelle Set")).unwrap();
    fs::write(lib.join("Mirelle Set/cover.png"), big_page(4)).unwrap();
    lib
}

/// Every page of every present book: `(book path, page)`.
fn all_pages(catalog: &Catalog) -> Vec<(String, koma::catalog::Page)> {
    let mut out = Vec::new();
    for series in catalog.series().unwrap() {
        for book in catalog.books(series.id).unwrap() {
            for page in catalog.pages(book.id).unwrap() {
                out.push((book.path.clone(), page));
            }
        }
    }
    out
}

/// An image's size from its header: JPEG, WebP and AVIF alike.
fn size_of(bytes: &[u8]) -> (u32, u32) {
    let s = koma::archive::size::read_size(bytes, 0, bytes.len() as u64).unwrap();
    (s.width, s.height)
}

/// An image's average of each channel, as Koma decodes it.
fn mean(bytes: &[u8]) -> Vec<f64> {
    let p = variants::decode(bytes, &variants::Limits::default()).unwrap();
    let n = if p.gray { 1 } else { 3 };
    let mut sum = vec![0f64; n];
    for px in p.data.chunks_exact(n) {
        for (s, v) in sum.iter_mut().zip(px) {
            *s += f64::from(*v);
        }
    }
    let count = (p.data.len() / n) as f64;
    sum.into_iter().map(|s| s / count).collect()
}

fn db(data: &Path) -> PathBuf {
    data.join("koma.db")
}

/// Pages whose variants are done, read through a second connection as
/// another process would.
fn variant_pages_done(db: &Path) -> i64 {
    Catalog::open_read_only(db)
        .and_then(|c| c.variant_page_count(VERSION))
        .unwrap_or(0)
}

/// Books in the catalogue, through a second connection.
fn books_indexed(db: &Path) -> i64 {
    Catalog::open_read_only(db)
        .and_then(|c| c.series_with_counts())
        .map(|s| s.iter().map(|(_, n)| n).sum())
        .unwrap_or(0)
}

#[test]
fn every_big_page_gets_its_variants_once() {
    let root = temp_root("once");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();

    let report = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(report.indexed, 3);
    // Four page contents are wider than 720: the shared page counts once,
    // the small page not at all.
    assert_eq!(report.variant_pages, 4);
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), 4);
    assert_eq!(catalog.job_count().unwrap(), 0);

    let blobs = BlobStore::new(&data);
    let mut files = 0;
    for (path, page) in all_pages(&catalog) {
        let size = page.size.unwrap();
        let got = catalog.variants_of(&page.hash).unwrap();
        let want: Vec<(u32, u32, String)> = variants::widths_for(size.width)
            .flat_map(|w| {
                let h = variants::height_for(size.width, size.height, w);
                ["avif", "jpg", "webp"].map(|f| (w, h, f.to_owned()))
            })
            .collect();
        assert_eq!(got, want, "{path} page {}", page.number);
        for (w, h, format) in got {
            let bytes = fs::read(blobs.variant_path(&page.hash, w, VERSION, &format)).unwrap();
            assert_eq!(size_of(&bytes), (w, h));
            files += 1;
        }
    }
    // v01: 3 + 3 + 0 widths (the thumbnail, 720, 1080), v02: 3 (the shared
    // page again) + 2, Mirelle: 3; each in three formats.
    assert_eq!(files, 3 * (6 + 5 + 3));

    // Nothing left to do.
    let again = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(again.variant_pages, 0);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn pages_made_before_thumbnails_get_theirs_from_the_720_webp() {
    let root = temp_root("thumbs");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();
    scan(&lib, &data, &mut catalog).unwrap();

    // As a catalogue made before thumbnails, or upgraded to schema 17: every
    // page's variants without them, and maybe a wrong thumbnail file left.
    let blobs = BlobStore::new(&data);
    let mut before = Vec::new();
    for (_, page) in all_pages(&catalog) {
        let rows: Vec<(u32, u32, String, u64)> = catalog
            .variants_of(&page.hash)
            .unwrap()
            .into_iter()
            .filter(|(w, _, _)| *w != variants::THUMB)
            .map(|(w, h, f)| {
                let len = fs::metadata(blobs.variant_path(&page.hash, w, VERSION, &f))
                    .unwrap()
                    .len();
                (w, h, f, len)
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        let rows: Vec<(u32, u32, &str, u64)> = rows
            .iter()
            .map(|(w, h, f, l)| (*w, *h, f.as_str(), *l))
            .collect();
        catalog
            .store_variants(
                &page.hash,
                VERSION,
                (variants::FORMATS, variants::ALL_WIDTHS),
                &rows,
                None,
            )
            .unwrap();
        for f in ["jpg", "webp"] {
            let left = blobs.variant_path(&page.hash, variants::THUMB, VERSION, f);
            if left.exists() {
                fs::write(left, b"a thumbnail made wrong").unwrap();
            }
        }
        before.push(page);
    }
    before.dedup_by_key(|p| p.hash);

    // The next pass adds a thumbnail to each, and only that.
    let report = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(report.variant_pages, 4);
    for page in &before {
        let got = catalog.variants_of(&page.hash).unwrap();
        let size = page.size.unwrap();
        // The AVIF thumbnail stays missing: AVIF ones were made already.
        assert_eq!(got.len(), 3 * variants::widths_for(size.width).count() - 1);
        for format in ["jpg", "webp"] {
            let (w, h, _) = got
                .iter()
                .find(|(w, _, f)| *w == variants::THUMB && f == format)
                .expect("a thumbnail");
            let bytes = fs::read(blobs.variant_path(&page.hash, *w, VERSION, format)).unwrap();
            assert_eq!(size_of(&bytes), (*w, *h));
        }
        // The same picture, colour pages too. (Only the WebP one is looked
        // at: `image` misreads the colour JPEGs `jpeg-encoder` writes.)
        let thumb = blobs.variant_path(&page.hash, variants::THUMB, VERSION, "webp");
        let from = blobs.variant_path(&page.hash, 720, VERSION, "webp");
        let (thumb, from) = (fs::read(thumb).unwrap(), fs::read(from).unwrap());
        for (t, f) in mean(&thumb).iter().zip(mean(&from)) {
            assert!((t - f).abs() < 4.0, "thumbnail: {t} against {f}");
        }
    }
    assert_eq!(scan(&lib, &data, &mut catalog).unwrap().variant_pages, 0);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_new_version_makes_every_page_again_and_removes_the_old_files() {
    let root = temp_root("version");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();
    scan(&lib, &data, &mut catalog).unwrap();

    // As if made by the version before: the same files under its name.
    let (old, blobs) = (VERSION - 1, BlobStore::new(&data));
    let mut pages = Vec::new();
    for (_, page) in all_pages(&catalog) {
        let made = catalog.variants_of(&page.hash).unwrap();
        if made.is_empty() || pages.contains(&page.hash) {
            continue;
        }
        let mut rows = Vec::new();
        for (w, h, f) in &made {
            let now = blobs.variant_path(&page.hash, *w, VERSION, f);
            let before = blobs.variant_path(&page.hash, *w, old, f);
            fs::rename(&now, &before).unwrap();
            rows.push((*w, *h, f.clone(), fs::metadata(&before).unwrap().len()));
        }
        let rows: Vec<(u32, u32, &str, u64)> = rows
            .iter()
            .map(|(w, h, f, l)| (*w, *h, f.as_str(), *l))
            .collect();
        catalog
            .store_variants(
                &page.hash,
                old,
                (variants::FORMATS, variants::ALL_WIDTHS),
                &rows,
                None,
            )
            .unwrap();
        pages.push(page.hash);
    }
    assert_eq!(pages.len(), 4);
    let count = |dir: &Path, version: i64| -> usize {
        let suffix = format!(".v{version}.");
        walk(dir)
            .iter()
            .filter(|p| p.to_string_lossy().contains(&suffix))
            .count()
    };
    let old_files = count(&data.join("blobs"), old);
    assert!(old_files >= 4 * 6, "{old_files}");

    // The next pass makes each page again under this version, and leaves
    // no file of the old one.
    let report = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(report.variant_pages, 4);
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), 4);
    assert_eq!(count(&data.join("blobs"), old), 0);
    assert_eq!(count(&data.join("blobs"), VERSION), old_files);
    for hash in &pages {
        for (w, h, f) in catalog.variants_of(hash).unwrap() {
            let bytes = fs::read(blobs.variant_path(hash, w, VERSION, &f)).unwrap();
            assert_eq!(size_of(&bytes), (w, h));
        }
    }
    let _ = fs::remove_dir_all(&root);
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn pages_made_before_avif_get_avif_ones_from_the_original() {
    let root = temp_root("avif");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();
    scan(&lib, &data, &mut catalog).unwrap();

    // As a catalogue made before AVIF: JPEG and WebP variants, formats 3.
    let blobs = BlobStore::new(&data);
    let mut before = Vec::new();
    for (_, page) in all_pages(&catalog) {
        let rows: Vec<(u32, u32, String, u64)> = catalog
            .variants_of(&page.hash)
            .unwrap()
            .into_iter()
            .filter(|(_, _, f)| f != "avif")
            .map(|(w, h, f)| {
                let path = blobs.variant_path(&page.hash, w, VERSION, &f);
                (w, h, f, fs::metadata(path).unwrap().len())
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        let rows: Vec<(u32, u32, &str, u64)> = rows
            .iter()
            .map(|(w, h, f, l)| (*w, *h, f.as_str(), *l))
            .collect();
        let old = Format::Jpeg.bit() | Format::Webp.bit();
        catalog
            .store_variants(
                &page.hash,
                VERSION,
                (old, variants::ALL_WIDTHS),
                &rows,
                None,
            )
            .unwrap();
        for f in ["jpg", "webp"] {
            let left = blobs.variant_path(&page.hash, variants::THUMB, VERSION, f);
            if left.exists() {
                fs::write(left, b"a thumbnail made wrong").unwrap();
            }
        }
        before.push(page);
    }
    before.dedup_by_key(|p| p.hash);
    assert_eq!(before.len(), 4);

    // The next pass adds the AVIF ones at every width the page has.
    let report = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(report.variant_pages, 4);
    for page in &before {
        let got = catalog.variants_of(&page.hash).unwrap();
        let size = page.size.unwrap();
        for w in variants::widths_for(size.width) {
            let h = variants::height_for(size.width, size.height, w);
            assert!(got.contains(&(w, h, "avif".to_owned())), "{got:?}");
            let bytes = fs::read(blobs.variant_path(&page.hash, w, VERSION, "avif")).unwrap();
            assert_eq!(size_of(&bytes), (w, h));
        }
        assert_eq!(got.len(), 3 * variants::widths_for(size.width).count());
    }
    assert_eq!(scan(&lib, &data, &mut catalog).unwrap().variant_pages, 0);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn variants_wait_for_indexing() {
    let root = temp_root("order");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();
    scan(&lib, &data, &mut catalog).unwrap();

    // A variants job already waiting, then a new book: the book is indexed first.
    catalog
        .enqueue(JobKind::Variants, "Starfall/Starfall v01.cbz")
        .unwrap();
    cbz(
        &lib.join("Starfall/Starfall v03.cbz"),
        &[big_page(9)],
        Compression::Stored,
    );
    discover(&lib, &mut catalog, Check::Files).unwrap();
    assert_eq!(catalog.claim_job().unwrap().unwrap().kind, JobKind::Index);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_stop_between_pages_keeps_what_is_done() {
    let root = temp_root("stop");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();

    // Index only, then queue v01's variants alone (two big pages) and stop
    // once one is done: the order books are found in depends on the file
    // system, so the test picks the book itself. Pages are made several at
    // once, so its second may be under way by then: it is finished, and
    // kept, before the job stops.
    let mut report = discover(&lib, &mut catalog, Check::Files).unwrap();
    let db_path = db(&data);
    run_jobs(&lib, &data, &mut catalog, &mut report, &|| {
        books_indexed(&db_path) == 3
    })
    .unwrap();
    assert_eq!(report.indexed, 3);
    catalog
        .enqueue(JobKind::Variants, "Starfall/Starfall v01.cbz")
        .unwrap();
    let mut report = discover(&lib, &mut catalog, Check::Files).unwrap();
    run_jobs(&lib, &data, &mut catalog, &mut report, &|| {
        variant_pages_done(&db_path) >= 1
    })
    .unwrap();
    let first = report.variant_pages;
    assert!((1..=2).contains(&first), "{first}");
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), first as i64);
    if first == 1 {
        // v01's job is back in the queue, its attempt not counted.
        assert_eq!(catalog.job_count().unwrap(), 1);
        let job = catalog.claim_job().unwrap().unwrap();
        assert_eq!(job.kind, JobKind::Variants);
        assert_eq!(job.path, "Starfall/Starfall v01.cbz");
        assert_eq!(job.attempts, 1);
        catalog.requeue_running().unwrap();
    }

    let mut rest = discover(&lib, &mut catalog, Check::Files).unwrap();
    run_jobs(&lib, &data, &mut catalog, &mut rest, &|| false).unwrap();
    // What v01 had left, then Mirelle Set's page and v02's JPEG; the page
    // v02 shares with v01 is done by then. Nothing is made twice.
    assert_eq!(first + rest.variant_pages, 4);
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), 4);
    assert_eq!(catalog.job_count().unwrap(), 0);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_page_that_kills_the_worker_each_time_is_given_up_alone() {
    let root = temp_root("give-up");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();

    // Index only: stop once the three books are in.
    let mut report = discover(&lib, &mut catalog, Check::Files).unwrap();
    let db_path = db(&data);
    run_jobs(&lib, &data, &mut catalog, &mut report, &|| {
        books_indexed(&db_path) == 3
    })
    .unwrap();
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), 0);

    // The worker died on v01's variants as many times as it may: only its
    // job is queued, as if Mirelle Set were done.
    catalog
        .enqueue(JobKind::Variants, "Starfall/Starfall v01.cbz")
        .unwrap();
    for _ in 0..koma::catalog::MAX_ATTEMPTS {
        let job = catalog.claim_job().unwrap().unwrap();
        assert_eq!(job.path, "Starfall/Starfall v01.cbz");
        catalog.requeue_running().unwrap();
    }
    let mut report = discover(&lib, &mut catalog, Check::Files).unwrap();
    run_jobs(&lib, &data, &mut catalog, &mut report, &|| false).unwrap();

    // Its first page is given up; its second, and every other book, done.
    let pages: Vec<_> = all_pages(&catalog)
        .into_iter()
        .filter(|(p, _)| p.ends_with("v01.cbz"))
        .map(|(_, p)| p)
        .collect();
    assert!(catalog.variants_of(&pages[0].hash).unwrap().is_empty());
    assert_eq!(catalog.variants_of(&pages[1].hash).unwrap().len(), 9);
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), 4);
    assert_eq!(catalog.job_count().unwrap(), 0);
    let _ = fs::remove_dir_all(&root);
}

/// The M2 exit check: kill `koma worker` (SIGKILL, no chance to tidy up)
/// in the middle of making variants, start it again, and it finishes, with
/// nothing redone but the page it was on.
#[test]
fn killing_the_worker_mid_job_then_restarting_finishes_cleanly() {
    let root = temp_root("kill");
    let lib = root.join("library");
    let pages: Vec<Vec<u8>> = (0..12).map(|n| big_page(100 + n)).collect();
    cbz(
        &lib.join("Starfall/Starfall v01.cbz"),
        &pages,
        Compression::Deflated,
    );
    let data = root.join("data");
    let db_path = db(&data);
    let koma = env!("CARGO_BIN_EXE_koma");
    let args = |cmd: &str| {
        let mut c = Command::new(koma);
        c.arg(cmd)
            .arg("--library")
            .arg(&lib)
            .arg("--data")
            .arg(&data);
        c.stdout(Stdio::null()).stderr(Stdio::piped());
        c
    };

    let mut worker = args("worker").arg("--rescan").arg("0").spawn().unwrap();
    let started = Instant::now();
    let done_at_kill = loop {
        let done = variant_pages_done(&db_path);
        if done >= 2 {
            break done;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "no variants made"
        );
        assert!(worker.try_wait().unwrap().is_none(), "worker exited early");
        std::thread::sleep(Duration::from_millis(20));
    };
    worker.kill().unwrap();
    worker.wait().unwrap();
    assert!(
        done_at_kill < 12,
        "the worker finished before it was killed"
    );
    // What was killed may have left a temp file, never a half-made page.
    let done_after_kill = variant_pages_done(&db_path);

    let out = args("scan").output().unwrap();
    assert!(out.status.success());
    let log = String::from_utf8(out.stderr).unwrap();
    assert!(log.contains("resuming interrupted jobs"), "{log}");
    let done_line = log.lines().find(|l| l.contains("\"scan: done\"")).unwrap();
    let redone = 12 - done_after_kill;
    assert!(
        done_line.contains(&format!("\"variant_pages\":\"{redone}\"")),
        "{done_line}"
    );

    let catalog = Catalog::open_read_only(&db_path).unwrap();
    assert_eq!(catalog.variant_page_count(VERSION).unwrap(), 12);
    assert_eq!(catalog.job_count().unwrap(), 0);
    assert!(
        !data.join("blobs/tmp").exists()
            || fs::read_dir(data.join("blobs/tmp"))
                .unwrap()
                .next()
                .is_none()
    );
    let blobs = BlobStore::new(&data);
    for (_, page) in all_pages(&catalog) {
        for (w, h, format) in catalog.variants_of(&page.hash).unwrap() {
            let bytes = fs::read(blobs.variant_path(&page.hash, w, VERSION, &format)).unwrap();
            assert_eq!(size_of(&bytes), (w, h));
        }
    }
    let _ = fs::remove_dir_all(&root);
}

/// `(status, headers, body)` of a GET.
fn get(addr: SocketAddr, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nRemote-User: mirelle\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8(raw[..split].to_vec())
        .unwrap()
        .to_ascii_lowercase();
    (
        headers[9..12].parse().unwrap(),
        headers,
        raw[split + 4..].to_vec(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variants_are_served_at_their_own_immutable_urls() {
    let root = temp_root("serve");
    let paths = koma::cli::Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let mut catalog = Catalog::open(&db(&paths.data)).unwrap();
    scan(&paths.library, &paths.data, &mut catalog).unwrap();
    let pages = all_pages(&catalog);
    let big = pages
        .iter()
        .find(|(_, p)| p.size.unwrap().width == 1200)
        .unwrap()
        .1
        .hash;
    let small = pages
        .iter()
        .find(|(_, p)| p.size.unwrap().width == 60)
        .unwrap()
        .1
        .hash;
    drop(catalog);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(koma::serve::serve(
        listener,
        koma::serve::AppState::with_auth(
            &paths,
            koma::auth::Config::parse(Some("Remote-User"), Some("127.0.0.1")).unwrap(),
        ),
        async {
            let _ = stopped.await;
        },
    ));

    tokio::task::spawn_blocking(move || {
        let hex = big.to_hex();
        for format in Format::ALL {
            let mime = format.mime();
            for width in [720, 1080] {
                let url = variants::url(&big, width, format);
                assert_eq!(url, format!("/p/{hex}.{width}.v{VERSION}.{}", format.ext()));
                let (status, headers, body) = get(addr, &url);
                assert_eq!(status, 200, "{url}");
                assert!(
                    headers.contains(&format!("content-type: {mime}")),
                    "{headers}"
                );
                assert!(
                    headers.contains("cache-control: public, max-age=31536000, immutable"),
                    "{headers}"
                );
                assert!(
                    headers.contains(&format!("etag: \"{hex}.{width}.v{VERSION}\"")),
                    "{headers}"
                );
                assert_eq!(size_of(&body).0, width);
            }
        }
        // The original is still there as it was.
        assert_eq!(get(addr, &format!("/p/{hex}.png")).0, 200);
        let v = VERSION;
        for missing in [
            format!("/p/{hex}.1440.v{v}.jpg"), // not made: the page is 1200 wide
            format!("/p/{hex}.720.v{}.jpg", v + 1), // another version's URL
            format!("/p/{hex}.720.v0{v}.jpg"), // one URL per variant
            format!("/p/{hex}.0720.v{v}.jpg"),
            format!("/p/{hex}.720.jpg"), // no version
            format!("/p/{hex}.720.{v}.jpg"),
            format!("/p/{hex}.720.v{v}.png"), // not a variant format
            format!("/p/{hex}.-720.v{v}.jpg"),
            format!("/p/{hex}.720.v{v}.jpg.x"),
            variants::url(&small, 720, Format::Jpeg), // too small to have any
        ] {
            assert_eq!(get(addr, &missing).0, 404, "{missing}");
        }
    })
    .await
    .unwrap();

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}

/// The library's page-copies setting, as the worker applies it.
fn set_widths(catalog: &Catalog, id: i64, widths: u8) {
    catalog
        .apply_setting(&koma::accounts::SettingChange {
            id,
            folder: String::new(),
            name: "Library".into(),
            kind: "manga".into(),
            direction: "rtl".into(),
            colour: "rose".into(),
            widths,
        })
        .unwrap();
}

#[test]
fn fewer_widths_remove_copies_and_more_make_them_again() {
    let root = temp_root("widths");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&db(&data)).unwrap();
    scan(&lib, &data, &mut catalog).unwrap();
    let blobs = BlobStore::new(&data);
    let big: Vec<_> = {
        let mut pages: Vec<_> = all_pages(&catalog)
            .into_iter()
            .map(|(_, p)| p)
            .filter(|p| p.size.unwrap().width > 720)
            .collect();
        pages.dedup_by_key(|p| p.hash);
        pages
    };
    assert_eq!(big.len(), 4);

    // 720 only: the 1080 and 1440 copies go, files and all.
    set_widths(&catalog, 1, 1);
    let report = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(report.variant_pages, 4);
    for page in &big {
        let size = page.size.unwrap();
        let got = catalog.variants_of(&page.hash).unwrap();
        let want: Vec<u32> = variants::widths_in(size.width, 1).collect();
        assert!(got.iter().all(|(w, _, _)| want.contains(w)), "{got:?}");
        assert_eq!(got.len(), 3 * want.len());
        for w in [1080, 1440] {
            for f in Format::ALL {
                assert!(!blobs.variant_path(&page.hash, w, VERSION, f.ext()).exists());
            }
        }
    }
    assert_eq!(scan(&lib, &data, &mut catalog).unwrap().variant_pages, 0);

    // All three again: made from the pages, the others kept.
    let kept = fs::metadata(blobs.variant_path(&big[0].hash, 720, VERSION, "jpg"))
        .unwrap()
        .modified()
        .unwrap();
    set_widths(&catalog, 2, variants::ALL_WIDTHS);
    let report = scan(&lib, &data, &mut catalog).unwrap();
    assert_eq!(report.variant_pages, 4);
    for page in &big {
        let size = page.size.unwrap();
        let got = catalog.variants_of(&page.hash).unwrap();
        assert_eq!(got.len(), 3 * variants::widths_for(size.width).count());
        for (w, h, f) in got {
            let bytes = fs::read(blobs.variant_path(&page.hash, w, VERSION, &f)).unwrap();
            assert_eq!(size_of(&bytes), (w, h));
        }
    }
    let again = fs::metadata(blobs.variant_path(&big[0].hash, 720, VERSION, "jpg"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(kept, again);
    assert_eq!(scan(&lib, &data, &mut catalog).unwrap().variant_pages, 0);

    // A new book in a library set to no copies gets only its thumbnail.
    set_widths(&catalog, 3, 0);
    cbz(
        &lib.join("Starfall/Starfall v03.cbz"),
        &[big_page(9)],
        Compression::Stored,
    );
    scan(&lib, &data, &mut catalog).unwrap();
    let new = all_pages(&catalog)
        .into_iter()
        .find(|(p, _)| p.ends_with("v03.cbz"))
        .unwrap()
        .1;
    let got = catalog.variants_of(&new.hash).unwrap();
    assert!(!got.is_empty());
    assert!(got.iter().all(|(w, _, _)| *w == variants::THUMB), "{got:?}");
    let _ = fs::remove_dir_all(&root);
}
