//! The naming baseline: every example in `docs/naming.md` is checked here,
//! read from the document itself so the two can't drift apart. Then the
//! same rules end to end: metadata files and file names through a scan.

use std::fs;
use std::path::PathBuf;

use koma::catalog::{Catalog, StoredDetails};
use koma::names::{book_name, cosplay_name};
use koma::scan::scan;
use koma_fixtures::zip::{Compression, ZipWriter};

/// The rows of the table under `## {heading}`, as cells. `—` becomes "".
fn table(heading: &str) -> Vec<Vec<String>> {
    let doc = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/naming.md"))
        .expect("docs/naming.md");
    let start = doc
        .find(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("no section {heading}"));
    let section = &doc[start + 1..];
    let section = &section[..section[3..].find("\n## ").map_or(section.len(), |e| e + 3)];
    let rows: Vec<Vec<String>> = section
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            l.trim_matches('|')
                .split(" | ")
                .map(|c| match c.trim() {
                    "—" => String::new(),
                    c => c.trim_matches('`').to_owned(),
                })
                .collect()
        })
        .collect();
    assert!(rows.len() >= 4, "{heading}: only {} examples", rows.len());
    rows
}

fn stem(file: &str) -> &str {
    file.rsplit_once('.')
        .filter(|(_, ext)| ext.len() <= 4 && !ext.contains(' '))
        .map_or(file, |(stem, _)| stem)
}

#[test]
fn every_manga_and_comics_example() {
    let mut wrong = Vec::new();
    for row in table("Manga and comics") {
        let [file, series, kind, number, author, publisher] = &row[..] else {
            panic!("row needs 6 cells: {row:?}");
        };
        let got = book_name(stem(file));
        let got = [
            got.series.unwrap_or_default(),
            got.kind.map(|k| k.as_str().to_owned()).unwrap_or_default(),
            got.number.map(|n| n.to_string()).unwrap_or_default(),
            got.author.unwrap_or_default(),
            got.publisher.unwrap_or_default(),
        ];
        let want = [series, kind, number, author, publisher].map(String::as_str);
        if got != want {
            wrong.push(format!("{file}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

#[test]
fn every_cosplay_example() {
    let mut wrong = Vec::new();
    for row in table("Cosplay") {
        let [name, circle, cosplayer, set, variant] = &row[..] else {
            panic!("row needs 5 cells: {row:?}");
        };
        let got = cosplay_name(name);
        let got = [got.circle, got.cosplayer, got.set, got.variant];
        let want = [circle, cosplayer, set, variant].map(String::as_str);
        if got != want {
            wrong.push(format!("{name}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

struct Library {
    root: PathBuf,
    catalog: Catalog,
}

impl Library {
    /// A library holding these books, scanned once.
    fn scan(name: &str, books: &[(&str, Vec<u8>)]) -> Self {
        let root = std::env::temp_dir().join(format!("koma-naming-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let library = root.join("library");
        for (path, bytes) in books {
            let path = library.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let mut catalog = Catalog::open(&root.join("koma.db")).unwrap();
        let report = scan(&library, &root, &mut catalog).unwrap();
        assert_eq!(report.failed, 0, "{:?}", catalog.errors().unwrap());
        Self { root, catalog }
    }

    fn details(&self, path: &str) -> StoredDetails {
        self.catalog.details(path).unwrap().expect(path)
    }

    /// `(series name, [book paths])`, in order.
    fn series(&self) -> Vec<(String, Vec<String>)> {
        self.catalog
            .series()
            .unwrap()
            .into_iter()
            .map(|s| {
                let books = self.catalog.books(s.id).unwrap();
                (s.name, books.into_iter().map(|b| b.path).collect())
            })
            .collect()
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A one-page CBZ with these extra files.
fn cbz(extras: &[(&str, &[u8])]) -> Vec<u8> {
    static SEED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seed = SEED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut zip = ZipWriter::new();
    zip.add(
        "01.png",
        &koma_fixtures::png::page(20, 30, seed),
        Compression::Stored,
    );
    for (name, bytes) in extras {
        zip.add(name, bytes, Compression::Deflated);
    }
    zip.finish()
}

const COMIC_INFO: &[u8] = br#"<?xml version="1.0"?>
<ComicInfo>
  <Title>The Long Night</Title>
  <Series>Starfall Academy</Series>
  <Number>3</Number>
  <Summary>Mirelle &amp; friends stay up late.</Summary>
  <Writer>Mirelle</Writer>
  <Publisher>Moonforge Press</Publisher>
  <Genre>Comedy, Drama</Genre>
  <Tags>school; night</Tags>
  <Year>2021</Year>
  <LanguageISO>en</LanguageISO>
  <Manga>YesAndRightToLeft</Manga>
</ComicInfo>"#;

const METADATA_JSON: &[u8] = br#"{
  "title": "The Longest Night",
  "artists": ["Solene"],
  "genres": ["Mystery", "comedy"],
  "tags": [{"tag": "lantern", "female": true}, "night"],
  "direction": "ltr",
  "thumbnail": 0
}"#;

#[test]
fn metadata_json_wins_over_comic_info_and_lists_combine() {
    let lib = Library::scan(
        "layers",
        &[(
            "Starfall Academy/Starfall Academy - Ch. 03.cbz",
            cbz(&[
                ("ComicInfo.xml", COMIC_INFO),
                ("info/metadata.json", METADATA_JSON),
            ]),
        )],
    );
    let got = lib.details("Starfall Academy/Starfall Academy - Ch. 03.cbz");
    assert_eq!(
        got,
        StoredDetails {
            title: "The Longest Night".into(),
            number: Some(3.0),
            kind: Some("chapter".into()),
            summary: "Mirelle & friends stay up late.".into(),
            year: Some(2021),
            language: "en".into(),
            direction: Some("ltr".into()),
            cover_page: Some(0),
            authors: vec!["Solene".into()],
            publishers: vec!["Moonforge Press".into()],
            genres: vec!["Mystery".into(), "comedy".into(), "Drama".into()],
            tags: vec!["lantern ♀".into(), "night".into(), "school".into()],
        }
    );
}

#[test]
fn the_file_name_fills_what_metadata_leaves_out() {
    let lib = Library::scan(
        "gaps",
        &[
            (
                "Starfall Academy/[Moonforge (Mirelle)] Starfall Academy Vol. 02.cbz",
                cbz(&[(
                    "ComicInfo.xml",
                    b"<ComicInfo><Title>Spring</Title></ComicInfo>",
                )]),
            ),
            (
                "Starfall Academy/Extra.cbz",
                cbz(&[(
                    "ComicInfo.xml",
                    b"<ComicInfo><Number>4.5</Number></ComicInfo>",
                )]),
            ),
            (
                "Starfall Academy/Broken.cbz",
                cbz(&[
                    ("metadata.json", b"{ not json"),
                    ("ComicInfo.xml", b"<Comic"),
                ]),
            ),
        ],
    );
    let named = lib.details("Starfall Academy/[Moonforge (Mirelle)] Starfall Academy Vol. 02.cbz");
    assert_eq!(named.title, "Spring");
    assert_eq!(
        (named.kind.as_deref(), named.number),
        (Some("volume"), Some(2.0))
    );
    assert_eq!(named.authors, ["Mirelle"]);
    assert_eq!(named.publishers, ["Moonforge"]);

    let extra = lib.details("Starfall Academy/Extra.cbz");
    assert_eq!(
        (extra.kind.as_deref(), extra.number),
        (Some("chapter"), Some(4.5))
    );

    // Unreadable metadata is ignored, never an error.
    assert_eq!(
        lib.details("Starfall Academy/Broken.cbz"),
        StoredDetails::default()
    );
}

#[test]
fn loose_books_in_the_root_group_by_series_name() {
    let lib = Library::scan(
        "loose",
        &[
            (
                "[Moonforge (Mirelle)] Starfall Academy - Ch. 01.cbz",
                cbz(&[]),
            ),
            ("Starfall Academy - Ch. 02.cbz", cbz(&[])),
            ("starfall academy v03.cbz", cbz(&[])),
            ("Lone Comet.cbz", cbz(&[])),
            ("Ch. 12.cbz", cbz(&[])),
            ("Moonforge Chronicle/Moonforge Chronicle 2.cbz", cbz(&[])),
        ],
    );
    let mut series = lib.series();
    series.sort();
    assert_eq!(
        series,
        [
            ("Lone Comet".to_owned(), vec!["Lone Comet.cbz".to_owned()]),
            (
                "Moonforge Chronicle".to_owned(),
                vec!["Moonforge Chronicle/Moonforge Chronicle 2.cbz".to_owned()]
            ),
            (
                "Starfall Academy".to_owned(),
                vec![
                    "[Moonforge (Mirelle)] Starfall Academy - Ch. 01.cbz".to_owned(),
                    "Starfall Academy - Ch. 02.cbz".to_owned(),
                    "starfall academy v03.cbz".to_owned(),
                ]
            ),
            ("Unsorted".to_owned(), vec!["Ch. 12.cbz".to_owned()]),
        ]
    );
}

#[test]
fn photo_folders_read_their_metadata_too() {
    let root = std::env::temp_dir().join(format!("koma-naming-photos-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let set = root.join("library/Mirelle/Starfall Bunny");
    fs::create_dir_all(&set).unwrap();
    fs::write(set.join("01.png"), koma_fixtures::png::page(20, 30, 1)).unwrap();
    fs::write(
        set.join("metadata.json"),
        br#"{"title": "Starfall Bunny", "groups": ["Lumen Studio"]}"#,
    )
    .unwrap();
    let mut catalog = Catalog::open(&root.join("koma.db")).unwrap();
    scan(&root.join("library"), &root, &mut catalog).unwrap();
    let got = catalog.details("Mirelle/Starfall Bunny").unwrap().unwrap();
    fs::remove_dir_all(&root).unwrap();
    assert_eq!(got.title, "Starfall Bunny");
    assert_eq!(got.publishers, ["Lumen Studio"]);
}
