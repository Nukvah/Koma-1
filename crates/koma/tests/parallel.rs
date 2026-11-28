//! Pages made several at once give the same variants as one at a time: the
//! same rows and the same bytes. A file of its own, so that the number of
//! pages at once (a setting of the whole process) is this test's alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use koma::blobs::BlobStore;
use koma::catalog::Catalog;
use koma::scan::scan;
use koma::variants::{self, VERSION};
use koma_fixtures::png;
use koma_fixtures::zip::{Compression, ZipWriter};

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("koma-parallel-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// A colour JPEG page with some detail, different for every `seed`.
fn colour_jpeg(w: u16, h: u16, seed: u32) -> Vec<u8> {
    let mut rgb = Vec::new();
    for y in 0..u32::from(h) {
        for x in 0..u32::from(w) {
            let v = (x * 7 + y * 3 + seed * 31) % 256;
            rgb.extend_from_slice(&[v as u8, (x % 256) as u8, ((y + seed) % 256) as u8]);
        }
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 90)
        .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .unwrap();
    out
}

fn cbz(path: &Path, pages: &[Vec<u8>]) {
    let mut z = ZipWriter::new();
    for (n, p) in pages.iter().enumerate() {
        let ext = if p.starts_with(b"\x89PNG") {
            "png"
        } else {
            "jpg"
        };
        z.add(&format!("{:03}.{ext}", n + 1), p, Compression::Stored);
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// Two books of pages just wide enough for a 720 copy and a thumbnail
/// (AVIF is slow in test builds), gray and colour, one page shared.
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    let shared = png::page(800, 1150, 7);
    let mut v01: Vec<Vec<u8>> = (0..3).map(|n| png::page(780, 1100, n)).collect();
    v01.push(shared.clone());
    let mut v02: Vec<Vec<u8>> = (0..3).map(|n| colour_jpeg(760, 1080, n)).collect();
    v02.push(shared);
    cbz(&lib.join("Starfall/Starfall v01.cbz"), &v01);
    cbz(&lib.join("Starfall/Starfall v02.cbz"), &v02);
    lib
}

/// Every variant made, by page hash: `(width, height, format)` and its bytes.
/// A variant: `(width, height, format)` and its bytes.
type Made = ((u32, u32, String), Vec<u8>);

fn variants_made(lib: &Path, data: &Path) -> BTreeMap<String, Vec<Made>> {
    fs::create_dir_all(data).unwrap();
    let mut catalog = Catalog::open(&data.join("koma.db")).unwrap();
    let report = scan(lib, data, &mut catalog).unwrap();
    assert_eq!(report.variant_pages, 7);
    let blobs = BlobStore::new(data);
    let mut out = BTreeMap::new();
    for series in catalog.series().unwrap() {
        for book in catalog.books(series.id).unwrap() {
            for page in catalog.pages(book.id).unwrap() {
                let made: Vec<_> = catalog
                    .variants_of(&page.hash)
                    .unwrap()
                    .into_iter()
                    .map(|(w, h, f)| {
                        let bytes =
                            fs::read(blobs.variant_path(&page.hash, w, VERSION, &f)).unwrap();
                        ((w, h, f), bytes)
                    })
                    .collect();
                assert!(!made.is_empty(), "page {} of {}", page.number, book.path);
                out.insert(page.hash.to_hex().to_string(), made);
            }
        }
    }
    out
}

#[test]
fn several_pages_at_once_make_the_same_bytes_as_one() {
    let root = temp_root("same");
    let lib = library(&root);

    variants::set_workers(1);
    let one = variants_made(&lib, &root.join("one"));
    variants::set_workers(4);
    assert_eq!(variants::workers(), 4);
    let four = variants_made(&lib, &root.join("four"));

    assert_eq!(one.len(), 7);
    assert_eq!(
        one.keys().collect::<Vec<_>>(),
        four.keys().collect::<Vec<_>>()
    );
    for (hash, made) in &one {
        let other = &four[hash];
        assert_eq!(
            made.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            other.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            "{hash}"
        );
        for ((what, a), (_, b)) in made.iter().zip(other) {
            assert!(a == b, "{hash} {what:?}: not the same bytes");
        }
    }
    let _ = fs::remove_dir_all(&root);
}
