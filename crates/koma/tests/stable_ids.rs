//! A book and a series keep their id (and so, later, their progress and
//! edits) and their date added through moves and renames, even ones made
//! outside Koma-1. Rebuilt from the earlier reader's tests, with made-up names.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use koma::catalog::{Book, Catalog, GONE_GRACE_SECS};
use koma::scan::{ScanReport, scan};
use koma_fixtures::zip::{Compression, ZipWriter};

struct Shelf {
    root: PathBuf,
    library: PathBuf,
    data: PathBuf,
    catalog: Catalog,
}

impl Shelf {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "koma-ids-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let library = root.join("library");
        let data = root.join("data");
        fs::create_dir_all(&library).unwrap();
        fs::create_dir_all(&data).unwrap();
        let catalog = Catalog::open(&data.join("koma.db")).unwrap();
        Self {
            root,
            library,
            data,
            catalog,
        }
    }

    /// Writes a one-page book; `seed` makes its content (and size) differ.
    fn book(&self, path: &str, seed: u32) {
        let mut w = ZipWriter::new();
        w.add(
            "001.png",
            &koma_fixtures::png::page(20 + seed, 30, seed),
            Compression::Stored,
        );
        let path = self.library.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, w.finish()).unwrap();
    }

    fn mv(&self, from: &str, to: &str) {
        let to = self.library.join(to);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(self.library.join(from), to).unwrap();
    }

    fn scan(&mut self) -> ScanReport {
        scan(&self.library, &self.data, &mut self.catalog).unwrap()
    }

    fn books(&self) -> Vec<(String, Book)> {
        let mut out = Vec::new();
        for series in self.catalog.series().unwrap() {
            for book in self.catalog.books(series.id).unwrap() {
                out.push((series.name.clone(), book));
            }
        }
        out
    }

    fn book_at(&self, path: &str) -> Book {
        self.books()
            .into_iter()
            .map(|(_, b)| b)
            .find(|b| b.path == path)
            .unwrap_or_else(|| panic!("no book at {path}"))
    }

    fn series_of(&self, path: &str) -> i64 {
        self.book_at(path).series_id
    }
}

impl Drop for Shelf {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_book_moved_outside_koma_keeps_its_id_and_date() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.scan();
    let before = s.book_at("Starfall/Starfall 01.cbz");
    s.mv(
        "Starfall/Starfall 01.cbz",
        "Starfall Deluxe/Starfall 01.cbz",
    );
    let report = s.scan();
    assert_eq!((report.indexed, report.moved), (0, 1), "not read again");
    let after = s.book_at("Starfall Deluxe/Starfall 01.cbz");
    assert_eq!(after.id, before.id);
    assert_eq!(after.added, before.added);
    let (series, _) = s.books().pop().unwrap();
    assert_eq!(series, "Starfall Deluxe");
}

#[test]
fn a_book_renamed_outside_koma_keeps_its_id() {
    let mut s = Shelf::new();
    s.book("Starfall/starfall_1.cbz", 1);
    s.scan();
    let id = s.book_at("Starfall/starfall_1.cbz").id;
    s.mv("Starfall/starfall_1.cbz", "Starfall/Starfall 01.cbz");
    s.scan();
    assert_eq!(s.book_at("Starfall/Starfall 01.cbz").id, id);
}

#[test]
fn of_several_lookalikes_the_one_with_the_same_name_wins() {
    let mut s = Shelf::new();
    s.book("A/Same.cbz", 1);
    s.book("A/Other.cbz", 1);
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for f in ["A/Same.cbz", "A/Other.cbz"] {
        fs::File::options()
            .write(true)
            .open(s.library.join(f))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    s.scan();
    let same = s.book_at("A/Same.cbz").id;
    s.mv("A/Same.cbz", "B/Same.cbz");
    s.mv("A/Other.cbz", "B/Renamed.cbz");
    assert_eq!(
        s.catalog
            .gone_match("B/Renamed.cbz", stamp(&s, "B/Renamed.cbz"))
            .unwrap(),
        None,
        "not gone yet"
    );
    s.scan();
    assert_eq!(s.book_at("B/Same.cbz").id, same);
}

#[test]
fn of_several_lookalikes_with_other_names_none_is_guessed() {
    let mut s = Shelf::new();
    s.book("A/One.cbz", 1);
    s.book("A/Two.cbz", 1);
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for f in ["A/One.cbz", "A/Two.cbz"] {
        fs::File::options()
            .write(true)
            .open(s.library.join(f))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    s.scan();
    let one = s.book_at("A/One.cbz").id;
    fs::remove_dir_all(s.library.join("A")).unwrap();
    s.scan();
    s.book("B/Three.cbz", 1);
    fs::File::options()
        .write(true)
        .open(s.library.join("B/Three.cbz"))
        .unwrap()
        .set_modified(t)
        .unwrap();
    assert_eq!(
        s.catalog
            .gone_match("B/Three.cbz", stamp(&s, "B/Three.cbz"))
            .unwrap(),
        None
    );
    assert_eq!(
        s.catalog
            .gone_match("B/One.cbz", stamp(&s, "B/Three.cbz"))
            .unwrap(),
        Some(one)
    );
}

fn stamp(s: &Shelf, path: &str) -> (u64, i64) {
    let m = fs::metadata(s.library.join(path)).unwrap();
    let t = m
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    (m.len(), t.as_nanos() as i64)
}

#[test]
fn a_new_file_at_an_old_path_is_a_new_book() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.scan();
    let old = s.book_at("Starfall/Starfall 01.cbz").id;
    s.mv("Starfall/Starfall 01.cbz", "Old/Starfall 01.cbz");
    s.scan();
    s.book("Starfall/Starfall 01.cbz", 2);
    s.scan();
    assert_eq!(s.book_at("Old/Starfall 01.cbz").id, old);
    assert_ne!(s.book_at("Starfall/Starfall 01.cbz").id, old);
}

#[test]
fn a_book_rewritten_in_place_keeps_its_id() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.scan();
    let id = s.book_at("Starfall/Starfall 01.cbz").id;
    s.book("Starfall/Starfall 01.cbz", 2);
    assert_eq!(s.scan().indexed, 1);
    assert_eq!(s.book_at("Starfall/Starfall 01.cbz").id, id);
}

#[test]
fn a_briefly_missing_folder_comes_back_with_its_ids() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.scan();
    let book = s.book_at("Starfall/Starfall 01.cbz");
    // A drive that wasn't mounted yet, say.
    fs::rename(s.library.join("Starfall"), s.root.join("away")).unwrap();
    assert_eq!(s.scan().removed, 1);
    assert!(s.books().is_empty());
    assert!(s.catalog.series().unwrap().is_empty());
    fs::rename(s.root.join("away"), s.library.join("Starfall")).unwrap();
    let report = s.scan();
    assert_eq!((report.indexed, report.moved), (0, 1));
    let back = s.book_at("Starfall/Starfall 01.cbz");
    assert_eq!(
        (back.id, back.series_id, back.added),
        (book.id, book.series_id, book.added)
    );
}

#[test]
fn gone_books_are_forgotten_after_the_grace_period() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.scan();
    let id = s.book_at("Starfall/Starfall 01.cbz").id;
    fs::rename(s.library.join("Starfall"), s.root.join("away")).unwrap();
    s.scan();
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + GONE_GRACE_SECS
        + 60;
    assert_eq!(s.catalog.purge_gone(later - GONE_GRACE_SECS).unwrap(), 1);
    fs::rename(s.root.join("away"), s.library.join("Starfall")).unwrap();
    assert_eq!(s.scan().indexed, 1);
    assert_ne!(s.book_at("Starfall/Starfall 01.cbz").id, id);
}

#[test]
fn a_series_folder_renamed_outside_koma_keeps_its_id() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.book("Starfall/Starfall 02.cbz", 2);
    s.book("Moonforge/Moonforge 01.cbz", 3);
    s.scan();
    let starfall = s.series_of("Starfall/Starfall 01.cbz");
    let moonforge = s.series_of("Moonforge/Moonforge 01.cbz");
    fs::rename(
        s.library.join("Starfall"),
        s.library.join("Starfall (2012)"),
    )
    .unwrap();
    s.scan();
    assert_eq!(s.series_of("Starfall (2012)/Starfall 01.cbz"), starfall);
    assert_eq!(s.series_of("Starfall (2012)/Starfall 02.cbz"), starfall);
    assert_eq!(s.series_of("Moonforge/Moonforge 01.cbz"), moonforge);
    let names: Vec<String> = s
        .catalog
        .series()
        .unwrap()
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert_eq!(names, ["Moonforge", "Starfall (2012)"]);
}

#[test]
fn a_book_split_off_gets_a_new_series_and_the_rest_keep_theirs() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.book("Starfall/Starfall 02.cbz", 2);
    s.scan();
    let starfall = s.series_of("Starfall/Starfall 01.cbz");
    s.mv(
        "Starfall/Starfall 02.cbz",
        "Starfall Extras/Starfall 02.cbz",
    );
    s.scan();
    assert_eq!(s.series_of("Starfall/Starfall 01.cbz"), starfall);
    assert_ne!(s.series_of("Starfall Extras/Starfall 02.cbz"), starfall);
}

#[test]
fn a_new_series_with_an_old_name_is_a_new_series() {
    let mut s = Shelf::new();
    s.book("Starfall/Starfall 01.cbz", 1);
    s.scan();
    let starfall = s.series_of("Starfall/Starfall 01.cbz");
    fs::rename(
        s.library.join("Starfall"),
        s.library.join("Starfall Deluxe"),
    )
    .unwrap();
    s.scan();
    s.book("Starfall/Other 01.cbz", 2);
    s.scan();
    assert_eq!(s.series_of("Starfall Deluxe/Starfall 01.cbz"), starfall);
    assert_ne!(s.series_of("Starfall/Other 01.cbz"), starfall);
}
