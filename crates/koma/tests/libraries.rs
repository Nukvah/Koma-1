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

/// The catalogue of schema 7 (before libraries) is upgraded in place: the
/// same series, books and page variants, now with their library.
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
        "DROP INDEX series_library;
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
    assert_eq!(version, 8);
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
            home.contains("<a href=\"/\" aria-current=page>Library</a>"),
            "{home}"
        );
        assert!(home.contains("<a href=\"/l/manga\">Manga</a>"));
        assert!(home.contains("<a href=\"/l/My%20Comics\">My Comics</a>"));
        assert!(home.contains(">Starfall</a>") && home.contains(">Lone Comet</a>"));
        assert!(!home.contains(">Moonforge</a>"));

        // A library: its own series, then its collections.
        let (status, manga) = get(addr, "/l/manga");
        assert_eq!(status, 200);
        let at = |s: &str| manga.find(s).unwrap_or_else(|| panic!("{s} in {manga}"));
        assert!(at(">Moonforge</a>") < at("<h2>Saga</h2>"));
        assert!(at("<h2>Saga</h2>") < at(">Starfall Tales</a>"));
        assert!(!manga.contains(">Lone Comet</a>"));

        // Names that need encoding.
        let (status, comics) = get(addr, "/l/My%20Comics");
        assert_eq!(status, 200);
        assert!(comics.contains(">Harbor Lights</a>"));

        // No such library: not even the default one by its empty name.
        assert_eq!(get(addr, "/l/nothing").0, 404);
        assert_eq!(get(addr, "/l/").0, 404);

        let (status, settings) = get(addr, "/settings");
        assert_eq!(status, 200);
        assert!(settings.contains("<td>/library/manga<td>manga<td>right to left<td>teal<td>3"));
        assert!(
            settings.contains("<td>/library/My Comics<td>comics<td>left to right<td>rose<td>1")
        );
    })
    .await
    .unwrap();

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
