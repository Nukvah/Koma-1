//! A made-up library: a few series with the awkward cases real libraries have.
//!
//! ```text
//! Starfall/Starfall v01.cbz            12 pages "1.png".."12.png", ComicInfo.xml, macOS junk
//! Starfall/Starfall v02.cbz            deflated pages "p001".."p008"
//! Starfall/Starfall v10.cbz            3 pages (v2 must sort before v10)
//! Moonforge Chronicles/... Ch. 001.cbz Zip64, pages in a sub-folder, a double-page spread
//! Moonforge Chronicles/... Ch. 001.5.cbz
//! Mirelle's Garden/Mirelle's Garden 第1巻.cbz   full-width digits in page names
//! Lone Comet.cbz                       one-shot in the library root, code page 437 names
//! ```

use std::fs;
use std::io;
use std::path::Path;

use crate::png;
use crate::zip::{Compression, Name, ZipWriter};

pub const PAGE_WIDTH: u32 = 60;
pub const PAGE_HEIGHT: u32 = 90;

/// A book written by [`write`], with its pages in the order a reader must show them.
#[derive(Debug, Clone)]
pub struct Book {
    /// Path relative to the library root.
    pub path: String,
    /// `(name inside the archive, bytes)` in reading order.
    pub pages: Vec<(String, Vec<u8>)>,
}

/// Writes the library under `root` (which is created) and returns its books
/// in the order the files were written.
pub fn write(root: &Path) -> io::Result<Vec<Book>> {
    let mut seed = 0;
    let mut page = |w: u32, h: u32| {
        seed += 1;
        png::page(w, h, seed)
    };
    let books = vec![
        build(
            "Starfall/Starfall v01.cbz",
            ZipWriter::new(),
            Compression::Stored,
            (1..=12)
                .rev() // written out of order on purpose
                .map(|n| {
                    (
                        Name::Utf8(format!("{n}.png")),
                        page(PAGE_WIDTH, PAGE_HEIGHT),
                    )
                })
                .collect(),
            &[
                (
                    "ComicInfo.xml",
                    b"<ComicInfo><Series>Starfall</Series></ComicInfo>",
                ),
                ("__MACOSX/._1.png", b"\0\x05\x16\x07"),
                (".DS_Store", b"\0\0\0\x01Bud1"),
            ],
        ),
        build(
            "Starfall/Starfall v02.cbz",
            ZipWriter::new(),
            Compression::Deflated,
            (1..=8)
                .map(|n| {
                    (
                        Name::Utf8(format!("p{n:03}.png")),
                        page(PAGE_WIDTH, PAGE_HEIGHT),
                    )
                })
                .collect(),
            &[],
        ),
        build(
            "Starfall/Starfall v10.cbz",
            ZipWriter::new().comment(b"made-up test archive"),
            Compression::Stored,
            (1..=3)
                .map(|n| {
                    (
                        Name::Unflagged(format!("{n:02}.png")),
                        page(PAGE_WIDTH, PAGE_HEIGHT),
                    )
                })
                .collect(),
            &[],
        ),
        build(
            "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.cbz",
            ZipWriter::new().force_zip64(),
            Compression::Stored,
            vec![
                (
                    Name::Utf8("Ch 1/p1.png".into()),
                    page(PAGE_WIDTH, PAGE_HEIGHT),
                ),
                (
                    Name::Utf8("Ch 1/p2-3.png".into()),
                    page(PAGE_WIDTH * 2, PAGE_HEIGHT),
                ),
                (
                    Name::Utf8("Ch 1/p4.png".into()),
                    page(PAGE_WIDTH, PAGE_HEIGHT),
                ),
            ],
            &[("Ch 1/", b"")],
        ),
        build(
            "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.5.cbz",
            ZipWriter::new(),
            Compression::Stored,
            vec![
                (
                    Name::Utf8("extra 2.png".into()),
                    page(PAGE_WIDTH, PAGE_HEIGHT),
                ),
                (
                    Name::Utf8("extra 1.png".into()),
                    page(PAGE_WIDTH, PAGE_HEIGHT),
                ),
            ],
            &[],
        ),
        build(
            "Mirelle's Garden/Mirelle's Garden 第1巻.cbz",
            ZipWriter::new(),
            Compression::Stored,
            ["ページ１０.png", "ページ２.png", "ページ１.png"]
                .into_iter()
                .map(|n| (Name::Utf8(n.into()), page(PAGE_WIDTH, PAGE_HEIGHT)))
                .collect(),
            &[],
        ),
        build(
            "Lone Comet.cbz",
            ZipWriter::new(),
            Compression::Stored,
            vec![
                // "pág 2.png" and "pág 1.png" in code page 437 (á = 0xA0).
                (
                    Name::Raw(b"p\xa0g 2.png".to_vec()),
                    page(PAGE_WIDTH, PAGE_HEIGHT),
                ),
                (
                    Name::Raw(b"p\xa0g 1.png".to_vec()),
                    page(PAGE_WIDTH, PAGE_HEIGHT),
                ),
            ],
            &[],
        ),
    ];

    for (book, bytes) in &books {
        let path = root.join(&book.path);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, bytes)?;
    }
    Ok(books.into_iter().map(|(book, _)| book).collect())
}

/// Writes one archive. Pages go in the order given; the expected reading
/// order comes from [`reading_order`], which is spelled out per book rather
/// than computed, so it can't share a bug with the reader.
fn build(
    path: &str,
    mut zip: ZipWriter,
    compression: Compression,
    pages: Vec<(Name, Vec<u8>)>,
    extras: &[(&str, &[u8])],
) -> (Book, Vec<u8>) {
    let mut written = Vec::new();
    for (name, data) in pages {
        let display = match &name {
            Name::Utf8(s) | Name::Unflagged(s) | Name::RawWithUnicodePath(_, s) => s.clone(),
            Name::Raw(b) => b
                .iter()
                .map(|&c| if c == 0xA0 { 'á' } else { c as char })
                .collect(),
        };
        zip.add_named(name, &data, compression);
        written.push((display, data));
    }
    for (name, data) in extras {
        zip.add(name, data, Compression::Stored);
    }
    let order = reading_order(path);
    let pages = order
        .iter()
        .map(|want| {
            written
                .iter()
                .find(|(name, _)| name == want)
                .cloned()
                .unwrap_or_else(|| panic!("{path}: no page {want}"))
        })
        .collect();
    let book = Book {
        path: path.to_owned(),
        pages,
    };
    (book, zip.finish())
}

fn reading_order(path: &str) -> Vec<String> {
    let names: Vec<String> = match path {
        "Starfall/Starfall v01.cbz" => (1..=12).map(|n| format!("{n}.png")).collect(),
        "Starfall/Starfall v02.cbz" => (1..=8).map(|n| format!("p{n:03}.png")).collect(),
        "Starfall/Starfall v10.cbz" => vec!["01.png".into(), "02.png".into(), "03.png".into()],
        "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.cbz" => {
            vec![
                "Ch 1/p1.png".into(),
                "Ch 1/p2-3.png".into(),
                "Ch 1/p4.png".into(),
            ]
        }
        "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.5.cbz" => {
            vec!["extra 1.png".into(), "extra 2.png".into()]
        }
        "Mirelle's Garden/Mirelle's Garden 第1巻.cbz" => {
            vec![
                "ページ１.png".into(),
                "ページ２.png".into(),
                "ページ１０.png".into(),
            ]
        }
        "Lone Comet.cbz" => vec!["pág 1.png".into(), "pág 2.png".into()],
        other => panic!("no reading order for {other}"),
    };
    names
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_every_book() {
        let root = std::env::temp_dir().join(format!("koma-fixtures-{}", std::process::id()));
        let books = super::write(&root).unwrap();
        assert_eq!(books.len(), 7);
        for book in &books {
            assert!(root.join(&book.path).is_file(), "{}", book.path);
            assert!(!book.pages.is_empty());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
