//! Libraries from the folder layout, end to end: which series land in which
//! library and collection, the pages that show them, and the upgrade from
//! schema 7 that keeps the catalogue (and its costly page variants).

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use koma::catalog::Catalog;
use koma::libraries::{Direction, Kind, Library};
use koma_fixtures::zip::{Compression, ZipWriter};

fn book(path: &Path, seed: u32) {
    let mut z = ZipWriter::new();
    z.add(
        "1.png",
        &koma_fixtures::png::page(30, 45, seed),
        Compression::Stored,
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// ```text
/// Starfall/Starfall v01.cbz             a series folder in the root: default library
/// Lone Comet.cbz                        a loose book: default library
/// Mirelle Set/cover.png                 a photo folder in the root: default library
/// manga/Moonforge/Moonforge v01.cbz     library manga
/// manga/Saga/Starfall Zero/v01.cbz      library manga, collection Saga
/// manga/Saga/Starfall Tales/v01.cbz     library manga, collection Saga
/// My Comics/Harbor Lights/v01.cbz       library "My Comics"
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    book(&lib.join("Starfall/Starfall v01.cbz"), 1);
    book(&lib.join("Lone Comet.cbz"), 2);
    fs::create_dir_all(lib.join("Mirelle Set")).unwrap();
    fs::write(
        lib.join("Mirelle Set/cover.png"),
        koma_fixtures::png::page(30, 45, 3),
    )
    .unwrap();
    book(&lib.join("manga/Moonforge/Moonforge v01.cbz"), 4);
    book(
        &lib.join("manga/Saga/Starfall Zero/Starfall Zero v01.cbz"),
        5,
    );
    book(
        &lib.join("manga/Saga/Starfall Tales/Starfall Tales v01.cbz"),
        6,
    );
    book(
        &lib.join("My Comics/Harbor Lights/Harbor Lights v01.cbz"),
        7,
    );
    lib
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("koma-libraries-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    root
}

fn names(series: &[(koma::catalog::Series, i64)]) -> Vec<&str> {
    series.iter().map(|(s, _)| s.name.as_str()).collect()
}

#[test]
fn series_land_in_their_library() {
    let root = temp_root("layout");
    let lib = library(&root);
    let data = root.join("data");
    let mut catalog = Catalog::open(&data.join("koma.db")).unwrap();
    koma::scan::scan(&lib, &data, &mut catalog).unwrap();

    let keys = catalog.library_keys().unwrap();
    assert_eq!(
        keys,
        [
            ("".to_owned(), 3),
            ("manga".to_owned(), 3),
            ("My Comics".to_owned(), 1)
        ]
    );
    assert_eq!(
        names(&catalog.series_in_library("").unwrap()),
        ["Lone Comet", "Mirelle Set", "Starfall"]
    );
    assert_eq!(
        names(&catalog.series_in_library("manga").unwrap()),
        ["Moonforge", "Starfall Tales", "Starfall Zero"]
    );
    assert_eq!(
        names(&catalog.series_in_library("My Comics").unwrap()),
        ["Harbor Lights"]
    );

    let libraries = Library::all(keys);
    let comics = &libraries[2];
    assert_eq!(
        (comics.name.as_str(), comics.kind, comics.direction),
        ("My Comics", Kind::Comics, Direction::LeftToRight)
    );
    let _ = fs::remove_dir_all(&root);
}

/// The catalogue of schema 7 (before libraries and search) is upgraded in
/// place, both steps: the same series, books and page variants, now with
/// their library.
#[test]
fn schema_7_is_upgraded_in_place() {
    let root = temp_root("upgrade");
    let lib = library(&root);
    let data = root.join("data");
    let db = data.join("koma.db");
    let mut catalog = Catalog::open(&db).unwrap();
    koma::scan::scan(&lib, &data, &mut catalog).unwrap();
    let before = catalog.series_with_counts().unwrap();
    drop(catalog);

    // Turn it back into schema 7, as an earlier Koma-1 left it, with a
    // variant row standing for the work an upgrade must not lose.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "DROP TABLE search;
         DROP INDEX series_library;
         ALTER TABLE series DROP COLUMN library;
         INSERT INTO variant_pages (hash, version, error) VALUES (x'00', 1, NULL);
         PRAGMA user_version = 7;",
    )
    .unwrap();
    drop(conn);

    let catalog = Catalog::open(&db).unwrap();
    assert_eq!(catalog.series_with_counts().unwrap(), before);
    assert_eq!(catalog.variant_page_count(1).unwrap(), 1);
    assert_eq!(catalog.library_keys().unwrap().len(), 3);
    drop(catalog);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 9);
    let _ = fs::remove_dir_all(&root);
}

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    (text[9..12].parse().unwrap(), text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_library_has_its_page() {
    let root = temp_root("serve");
    let paths = koma::cli::Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let mut catalog = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut catalog).unwrap();
    drop(catalog);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(koma::serve::serve(
        listener,
        koma::serve::AppState::new(&paths),
        async {
            let _ = stopped.await;
        },
    ));

    tokio::task::spawn_blocking(move || {
        // The default library, with links to the others.
        let (status, home) = get(addr, "/");
        assert_eq!(status, 200);
        assert!(
            home.contains("<a class=on aria-current=true href=\"/\">▾ Library"),
            "{home}"
        );
        assert!(home.contains("<a href=\"/l/manga\">▸ Manga<span class=n>3</span>"));
        assert!(home.contains("<a href=\"/l/My%20Comics\">▸ My Comics"));
        assert!(home.contains("<b>Starfall</b>") && home.contains("<b>Lone Comet</b>"));
        assert!(!home.contains("<b>Moonforge</b>"));

        // A library: its collections as stacks first, then its own series.
        let (status, manga) = get(addr, "/l/manga");
        assert_eq!(status, 200);
        let at = |s: &str| manga.find(s).unwrap_or_else(|| panic!("{s} in {manga}"));
        assert!(
            at("<a class=card href=\"/l/manga?c=Saga\"><div class=stack>") < at("<b>Moonforge</b>")
        );
        assert!(manga.contains("<b>Saga</b><span>2 series · 2 books</span>"));
        assert!(!manga.contains("<b>Starfall Tales</b>"));
        // The open library lists its collections in the side panel.
        assert!(manga.contains("<a class=sub href=\"/l/manga?c=Saga\">  └─ Saga</a>"));

        // One collection.
        let (status, saga) = get(addr, "/l/manga?c=Saga");
        assert_eq!(status, 200);
        assert!(saga.contains("<b>Starfall Tales</b>") && saga.contains("<b>Starfall Zero</b>"));
        assert!(!saga.contains("<b>Moonforge</b>"));
        assert!(saga.contains("~/<a href=\"/l/manga\">manga</a>/<b>Saga</b>"));
        assert_eq!(get(addr, "/l/manga?c=Nothing").0, 404);

        // Pages of cards: there is only one here.
        assert_eq!(get(addr, "/l/manga?page=1").0, 200);
        assert_eq!(get(addr, "/l/manga?page=2").0, 404);
        assert_eq!(get(addr, "/l/manga?page=0").0, 404);
        assert_eq!(get(addr, "/?page=x").0, 404);

        // The default library never has collections: no such column.
        let (status, flat) = get(addr, "/?view=table");
        assert_eq!(status, 200);
        assert!(flat.contains(">Lone Comet</a>") && !flat.contains(">collection"));

        // Names that need encoding.
        let (status, comics) = get(addr, "/l/My%20Comics");
        assert_eq!(status, 200);
        assert!(comics.contains("<b>Harbor Lights</b>"));

        // No such library: not even the default one by its empty name.
        assert_eq!(get(addr, "/l/nothing").0, 404);
        assert_eq!(get(addr, "/l/").0, 404);

        // A series in a collection: the breadcrumb leads back to both.
        let zero = saga
            .split("<a class=card href=\"/s/")
            .nth(2)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        let (status, series) = get(addr, &format!("/s/{zero}"));
        assert_eq!(status, 200);
        assert!(
            series
                .contains("~/<a href=\"/l/manga\">manga</a>/<a href=\"/l/manga?c=Saga\">Saga</a>/"),
            "{series}"
        );
        assert!(series.contains("read from the start"));

        let (status, settings) = get(addr, "/settings");
        assert_eq!(status, 200);
        assert!(
            settings.contains("<td class=opt>/library/manga<td>manga<td class=opt>right to left")
        );
        assert!(
            settings.contains("<span class=swatch style=\"--c:#4FB3A9\"></span> teal<td class=r>3")
        );
        assert!(
            settings
                .contains("<td class=opt>/library/My Comics<td>comics<td class=opt>left to right")
        );

        // The stylesheet, at the URL every page names, cached for good.
        let css_url = koma::html::css_url();
        assert!(home.contains(&format!("<link rel=stylesheet href=\"{css_url}\">")));
        let (status, css) = get(addr, css_url);
        assert_eq!(status, 200);
        assert!(css.contains("content-type: text/css"));
        assert!(css.contains("cache-control: public, max-age=31536000, immutable"));
        assert_eq!(get(addr, "/a/koma.0000000000000000.css").0, 404);
    })
    .await
    .unwrap();

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_library_comes_a_page_at_a_time() {
    let root = temp_root("pages");
    let lib = root.join("library");
    for n in 1..=30 {
        book(&lib.join(format!("manga/Series {n:02}/v01.cbz")), 100 + n);
    }
    let paths = koma::cli::Paths {
        library: lib,
        data: root.join("data"),
    };
    let mut catalog = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut catalog).unwrap();
    drop(catalog);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(koma::serve::serve(
        listener,
        koma::serve::AppState::new(&paths),
        async {
            let _ = stopped.await;
        },
    ));
    tokio::task::spawn_blocking(move || {
        // Every series is in the library "manga": it is the first, at /.
        let (_, one) = get(addr, "/");
        assert_eq!(one.matches("<a class=card").count(), koma::html::PER_PAGE);
        assert!(one.contains("<b>Series 01</b>") && one.contains("<b>Series 24</b>"));
        assert!(one.contains("1–24 of 30 · page 1/2 · <a href=\"/l/manga?page=2\" rel=next>"));
        assert!(!one.contains("rel=prev"));

        let (status, two) = get(addr, "/l/manga?page=2");
        assert_eq!(status, 200);
        assert_eq!(two.matches("<a class=card").count(), 6);
        assert!(two.contains("<b>Series 25</b>") && two.contains("<b>Series 30</b>"));
        assert!(two.contains("<a href=\"/l/manga?page=1\" rel=prev>"));
        assert!(two.contains("25–30 of 30 · page 2/2</span>"));
        assert_eq!(get(addr, "/l/manga?page=3").0, 404);
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}

/// A book with `pages` pages and ComicInfo.xml fields `info`.
fn book_with(path: &Path, seed: u32, pages: u32, info: &str) {
    let mut z = ZipWriter::new();
    for p in 0..pages {
        z.add(
            &format!("{p:02}.png"),
            &koma_fixtures::png::page(30, 45, seed * 10 + p),
            Compression::Stored,
        );
    }
    let xml = format!("<ComicInfo>{info}</ComicInfo>");
    z.add("ComicInfo.xml", xml.as_bytes(), Compression::Stored);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// The titles of a table page's rows, in order.
fn row_titles(html: &str) -> Vec<String> {
    html.split("<tr><td><a href=\"/s/")
        .skip(1)
        .map(|row| row[row.find('>').unwrap() + 1..row.find("</a>").unwrap()].to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_table_view_sorts_by_any_column() {
    let root = temp_root("table");
    let lib = root.join("library");
    let writer = |w: &str| format!("<Writer>{w}</Writer><Genre>Drama, Sea</Genre>");
    book_with(
        &lib.join("manga/Glass Harbor/v01.cbz"),
        1,
        2,
        &writer("Sela Marr"),
    );
    book_with(
        &lib.join("manga/Glass Harbor/v02.cbz"),
        2,
        2,
        &writer("Sela Marr"),
    );
    book_with(
        &lib.join("manga/Ironbloom/v01.cbz"),
        3,
        1,
        &writer("Kade Imura"),
    );
    book_with(&lib.join("manga/Saga/Starfall Zero/v01.cbz"), 4, 1, "");
    book_with(&lib.join("manga/Saga/Starfall Tales/v01.cbz"), 5, 6, "");
    let paths = koma::cli::Paths {
        library: lib,
        data: root.join("data"),
    };
    let mut catalog = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut catalog).unwrap();
    drop(catalog);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(koma::serve::serve(
        listener,
        koma::serve::AppState::new(&paths),
        async {
            let _ = stopped.await;
        },
    ));
    tokio::task::spawn_blocking(move || {
        // The grid links to the table and back.
        let (_, grid) = get(addr, "/l/manga");
        assert!(grid.contains("<a href=\"/l/manga\" aria-current=page>grid</a><a href=\"/l/manga?view=table\">table</a>"));

        let (status, table) = get(addr, "/l/manga?view=table");
        assert_eq!(status, 200);
        assert!(table.contains("<a href=\"/l/manga?view=table\" aria-current=page>table</a>"));
        assert_eq!(
            row_titles(&table),
            ["Glass Harbor", "Ironbloom", "Starfall Tales", "Starfall Zero"]
        );
        // Title is the sort: its link turns it round.
        assert!(table.contains("<a href=\"/l/manga?view=table&amp;sort=-title\" aria-sort=ascending>title ▴</a>"));
        // Every column of a row.
        assert!(table.contains(">Glass Harbor</a><td class=opt><td class=opt>Sela Marr<td class=r>2<td class=\"r opt\">4<td class=opt>Drama, Sea<td class=opt>20"));
        assert!(table.contains(">Starfall Zero</a><td class=opt>Saga<td class=opt><td class=r>1"));

        let (_, by_books) = get(addr, "/l/manga?view=table&sort=-books");
        assert_eq!(row_titles(&by_books)[0], "Glass Harbor");
        assert!(by_books.contains("aria-sort=descending>books ▾</a>"));
        assert!(by_books.contains("href=\"/l/manga?view=table&amp;sort=books\" aria-sort=descending"));
        let (_, by_pages) = get(addr, "/l/manga?view=table&sort=pages");
        assert_eq!(
            row_titles(&by_pages),
            ["Ironbloom", "Starfall Zero", "Glass Harbor", "Starfall Tales"]
        );
        // By author: series with none last.
        let (_, by_author) = get(addr, "/l/manga?view=table&sort=author");
        assert_eq!(
            row_titles(&by_author),
            ["Ironbloom", "Glass Harbor", "Starfall Tales", "Starfall Zero"]
        );

        // A collection's table: its series only, no collection column.
        let (status, saga) = get(addr, "/l/manga?c=Saga&view=table&sort=-title");
        assert_eq!(status, 200);
        assert_eq!(row_titles(&saga), ["Starfall Zero", "Starfall Tales"]);
        assert!(!saga.contains(">collection"));
        // A whole library that has collections shows the column.
        let (_, whole) = get(addr, "/l/manga?view=table");
        assert!(whole.contains(">collection"));

        assert_eq!(get(addr, "/l/manga?view=list").0, 404);
        assert_eq!(get(addr, "/l/manga?view=table&sort=colour").0, 404);
        assert_eq!(get(addr, "/l/manga?view=table&page=2").0, 404);
        assert_eq!(get(addr, "/l/manga?c=Nothing&view=table").0, 404);
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
