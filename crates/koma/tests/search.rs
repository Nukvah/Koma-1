//! Search end to end: the index the worker builds, every kind of query, the
//! library filter, the index following changes, the upgrade from schema 8,
//! and the results page.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use koma::catalog::Catalog;
use koma::search::{parse, plan};
use koma_fixtures::zip::{Compression, ZipWriter};

/// A one-page book, with a ComicInfo.xml holding `info` if given.
fn book(path: &Path, seed: u32, info: Option<&str>) {
    let mut z = ZipWriter::new();
    z.add(
        "1.png",
        &koma_fixtures::png::page(30, 45, seed),
        Compression::Stored,
    );
    if let Some(info) = info {
        let xml = format!("<ComicInfo>{info}</ComicInfo>");
        z.add("ComicInfo.xml", xml.as_bytes(), Compression::Stored);
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// ```text
/// manga/Glass Harbor/Glass Harbor v01.cbz   Writer Sela Marr, Genre Drama, Tags harbor-town, sea
/// manga/Saltwind/Saltwind v03.cbz           Title "The Harbor Fire"
/// comics/Harbor Lights/Harbor Lights v01.cbz
/// 星の港/星の港 第1巻.cbz                    (default library)
/// Tidecaller/Tidecaller v01.cbz              Tags harbor-town (default library)
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    book(
        &lib.join("manga/Glass Harbor/Glass Harbor v01.cbz"),
        1,
        Some("<Writer>Sela Marr</Writer><Genre>Drama</Genre><Tags>harbor-town, sea</Tags>"),
    );
    book(
        &lib.join("manga/Saltwind/Saltwind v03.cbz"),
        2,
        Some("<Title>The Harbor Fire</Title>"),
    );
    book(
        &lib.join("comics/Harbor Lights/Harbor Lights v01.cbz"),
        3,
        None,
    );
    book(&lib.join("星の港/星の港 第1巻.cbz"), 4, None);
    book(
        &lib.join("Tidecaller/Tidecaller v01.cbz"),
        5,
        Some("<Tags>harbor-town</Tags>"),
    );
    lib
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("koma-search-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    root
}

/// The names of the series and books a query finds, each sorted.
fn find(c: &Catalog, query: &str, library: Option<&str>) -> (Vec<String>, Vec<String>) {
    let p = plan(&parse(query));
    let hits = c.search(p.fts.as_deref(), &p.likes, library, 100).unwrap();
    let mut series = Vec::new();
    let mut books = Vec::new();
    for (kind, id) in hits {
        match kind.as_str() {
            "s" => series.push(c.series_by_id(id).unwrap().unwrap().name),
            _ => books.push(c.book(id).unwrap().unwrap().name),
        }
    }
    series.sort();
    books.sort();
    (series, books)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn every_kind_of_query() {
    let root = temp_root("kinds");
    let lib = library(&root);
    let data = root.join("data");
    let mut c = Catalog::open(&data.join("koma.db")).unwrap();
    koma::scan::scan(&lib, &data, &mut c).unwrap();

    // Names, and series by their books' tags (Tidecaller); books by title.
    let (series, books) = find(&c, "harbor", None);
    assert_eq!(
        series,
        strs(&["Glass Harbor", "Harbor Lights", "Tidecaller"])
    );
    assert!(books.contains(&"Saltwind v03".to_owned()), "{books:?}");
    // Any part of a word, any case.
    assert_eq!(find(&c, "ARBO", None).0, series);
    // One field only.
    assert_eq!(find(&c, "tag:sea", None).0, strs(&["Glass Harbor"]));
    assert_eq!(find(&c, "author:marr", None).0, strs(&["Glass Harbor"]));
    assert_eq!(find(&c, "genre:drama", None).0, strs(&["Glass Harbor"]));
    assert_eq!(
        find(&c, "tag:\"harbor-town\"", None).0,
        strs(&["Glass Harbor", "Tidecaller"])
    );
    // Every term must match.
    assert_eq!(find(&c, "harbor lights", None).0, strs(&["Harbor Lights"]));
    // Too short for trigrams: matched as a substring all the same.
    assert_eq!(find(&c, "港", None).0, strs(&["星の港"]));
    assert_eq!(find(&c, "の港 第1", None).1, strs(&["星の港 第1巻"]));
    // One library.
    assert_eq!(
        find(&c, "harbor", Some("comics")).0,
        strs(&["Harbor Lights"])
    );
    assert_eq!(find(&c, "harbor", Some("")).0, strs(&["Tidecaller"]));
    // Nothing, and query syntax that must stay text.
    assert_eq!(find(&c, "zzzz", None), (vec![], vec![]));
    assert_eq!(find(&c, "\"harbor OR sea\" NEAR(", None), (vec![], vec![]));
    assert_eq!(find(&c, "50%", None), (vec![], vec![]));

    let terms = c
        .matching_terms(&koma::search::like_pattern("harbor"), 10)
        .unwrap();
    assert_eq!(terms, [("tag".to_owned(), "harbor-town".to_owned(), 2)]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_index_follows_the_library() {
    let root = temp_root("follows");
    let lib = library(&root);
    let data = root.join("data");
    let mut c = Catalog::open(&data.join("koma.db")).unwrap();
    koma::scan::scan(&lib, &data, &mut c).unwrap();

    fs::remove_dir_all(lib.join("Tidecaller")).unwrap();
    book(
        &lib.join("manga/Harbor Ghosts/Harbor Ghosts v01.cbz"),
        9,
        None,
    );
    koma::scan::scan(&lib, &data, &mut c).unwrap();
    assert_eq!(
        find(&c, "harbor", None).0,
        strs(&["Glass Harbor", "Harbor Ghosts", "Harbor Lights"])
    );
    let _ = fs::remove_dir_all(&root);
}

/// A catalogue of schema 8 gets an empty index; the next scan fills it even
/// though no book changed.
#[test]
fn schema_8_is_upgraded_and_filled() {
    let root = temp_root("upgrade");
    let lib = library(&root);
    let data = root.join("data");
    let db = data.join("koma.db");
    let mut c = Catalog::open(&db).unwrap();
    koma::scan::scan(&lib, &data, &mut c).unwrap();
    drop(c);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "DROP TABLE toc;
         DROP TABLE chapters;
         DROP INDEX series_shelf;
         DROP INDEX series_names;
         ALTER TABLE series DROP COLUMN collection;
         DROP TABLE search;
         PRAGMA user_version = 8;",
    )
    .unwrap();
    drop(conn);

    let mut c = Catalog::open(&db).unwrap();
    assert!(c.search_needs_filling().unwrap());
    assert_eq!(find(&c, "harbor", None), (vec![], vec![]));
    let report = koma::scan::scan(&lib, &data, &mut c).unwrap();
    assert_eq!(report.indexed, 0);
    assert!(!c.search_needs_filling().unwrap());
    assert_eq!(find(&c, "harbor", None).0.len(), 3);
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
async fn the_results_page() {
    let root = temp_root("page");
    let paths = koma::cli::Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    drop(c);

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
        let (status, page) = get(addr, "/search?q=harbor");
        assert_eq!(status, 200);
        // The box keeps the query; matches are marked.
        assert!(
            page.contains("name=q type=search value=\"harbor\""),
            "{page}"
        );
        assert!(page.contains("<b>Glass <mark>Harbor</mark></b>"));
        assert!(page.contains("<b><mark>Harbor</mark> Lights</b><span>1 book · Comics</span>"));
        // Series first, then books, then tags.
        let at = |s: &str| page.find(s).unwrap_or_else(|| panic!("{s} in {page}"));
        assert!(at(">series</h2>") < at(">books</h2>"));
        assert!(at(">books</h2>") < at(">people, genres and tags</h2>"));
        assert!(page.contains("tag: <mark>harbor</mark>-town · 2</a>"));
        assert!(page.contains("href=\"/search?q=tag%3A%22harbor-town%22\""));
        // The library filter.
        assert!(page.contains("<a href=\"/search?q=harbor&amp;l=comics\">comics</a>"));
        let (_, comics) = get(addr, "/search?q=harbor&l=comics");
        assert!(comics.contains("<b><mark>Harbor</mark> Lights</b>"));
        assert!(!comics.contains("Glass"));

        // Short terms, encoded queries.
        let (_, short) = get(addr, "/search?q=%E6%B8%AF");
        assert!(short.contains("<b>星の<mark>港</mark></b>"), "{short}");
        let (_, plus) = get(addr, "/search?q=harbor+lights");
        assert!(plus.contains("<b><mark>Harbor</mark> <mark>Lights</mark></b>"));

        let (status, none) = get(addr, "/search?q=zzzz");
        assert_eq!(status, 200);
        assert!(none.contains("Nothing matches “zzzz”"));
        let (_, empty) = get(addr, "/search");
        assert!(empty.contains("Search your libraries"));
        let (_, hostile) = get(addr, "/search?q=%3Cscript%3E");
        assert!(!hostile.contains("<script>") && hostile.contains("&lt;script&gt;"));
        assert_eq!(get(addr, "/search?q=x&l=nothing").0, 404);
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
