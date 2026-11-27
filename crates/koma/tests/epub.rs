//! EPUB books end to end: ingest (package, contents, cleaned chapters,
//! pictures as pages) and the reader's chapter pages.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use koma::catalog::Catalog;
use koma_fixtures::epub::Epub;

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("koma-epub-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    root
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// ```text
/// books/Saltwind/Saltwind 01 - The Harbor Fire.epub   three chapters, a cover and a map
/// books/Saltwind/Saltwind 02 - Ash on the Water.epub  EPUB 2 (contents in an NCX)
/// books/Saltwind/Saltwind 03.epub                     fixed layout: refused for now
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    let mut one = Epub::new("The Harbor Fire", "Sela Marr")
        .chapter(
            "1 · Low Tide",
            "<h1 id=\"top\">Low Tide</h1><p>The bell on the breakwater rang twice.\
             <a href=\"ch03.xhtml#n1\" epub:type=\"noteref\">1</a></p>\
             <p><img src=\"../images/map.png\" alt=\"The harbor\"/></p>",
        )
        .chapter(
            "2 · The Night Ferry",
            "<p onclick=\"steal()\">Mirelle counted the boats.</p><script>steal()</script>\
             <p><a href=\"javascript:steal()\">a link</a></p>",
        )
        .chapter(
            "Notes",
            "<aside id=\"n1\" epub:type=\"footnote\"><p>The old bell.</p></aside>",
        );
    one.cover = Some(koma_fixtures::png::page(60, 90, 1));
    one.pictures = vec![("map.png", koma_fixtures::png::page(80, 40, 2))];
    write(
        &lib.join("books/Saltwind/Saltwind 01 - The Harbor Fire.epub"),
        &one.build(),
    );
    let mut two = Epub::new("Ash on the Water", "Sela Marr")
        .chapter("Ash", "<p>Grey water.</p>")
        .chapter("Embers", "<p>Red sky.</p>");
    two.ncx = true;
    write(
        &lib.join("books/Saltwind/Saltwind 02 - Ash on the Water.epub"),
        &two.build(),
    );
    let mut fixed = Epub::new("Pictures", "Sela Marr").chapter("One", "<p>x</p>");
    fixed.fixed_layout = true;
    write(&lib.join("books/Saltwind/Saltwind 03.epub"), &fixed.build());
    lib
}

fn html_of(data: &Path, hash: &blake3::Hash) -> String {
    let path = koma::blobs::BlobStore::new(data).path(hash, "html");
    fs::read_to_string(path).unwrap()
}

#[test]
fn epub_ingest() {
    let root = temp_root("ingest");
    let lib = library(&root);
    let data = root.join("data");
    let mut c = Catalog::open(&data.join("koma.db")).unwrap();
    let report = koma::scan::scan(&lib, &data, &mut c).unwrap();
    assert_eq!((report.indexed, report.failed), (2, 1));

    let series = c.series().unwrap();
    assert_eq!(series.len(), 1);
    let books = c.books(series[0].id).unwrap();
    assert_eq!(books.len(), 2);
    let one = &books[0];
    // A book of chapters: its page count is its chapters.
    assert_eq!(one.page_count, 3);
    let details = c.details(&one.path).unwrap().unwrap();
    assert_eq!(details.title, "The Harbor Fire");
    assert_eq!(details.authors, ["Sela Marr"]);
    assert_eq!(details.language, "en");

    // Its pictures are its pages, the cover first.
    let pages = c.pages(one.id).unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].size.map(|s| (s.width, s.height)), Some((60, 90)));

    // The contents, and each chapter cleaned.
    let toc = c.toc(one.id).unwrap();
    let titles: Vec<_> = toc.iter().map(|t| (t.title.as_str(), t.chapter)).collect();
    assert_eq!(
        titles,
        [
            ("1 · Low Tide", 0),
            ("2 · The Night Ferry", 1),
            ("Notes", 2)
        ]
    );
    let first = c.chapter(one.id, 0).unwrap().unwrap();
    assert_eq!(first.title, "1 · Low Tide");
    let map = &pages[1];
    assert_eq!(
        html_of(&data, &first.hash),
        format!(
            "<h1 id=\"e-top\">Low Tide</h1><p>The bell on the breakwater rang twice.\
             <a href=\"3#e-n1\">1</a></p><p><img src=\"/p/{}.png\" alt=\"The harbor\" \
             width=\"80\" height=\"40\" loading=\"lazy\"></p>",
            map.hash.to_hex()
        )
    );
    let second = c.chapter(one.id, 1).unwrap().unwrap();
    assert_eq!(
        html_of(&data, &second.hash),
        "<p>Mirelle counted the boats.</p><p><a>a link</a></p>"
    );
    assert!(c.chapter(one.id, 3).unwrap().is_none());

    // EPUB 2: the contents from the NCX.
    let toc = c.toc(books[1].id).unwrap();
    assert_eq!(toc.len(), 2);
    assert_eq!(toc[1].title, "Embers");

    // Fixed layout waits for M11, and says so.
    let errors = c.errors().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.contains("fixed-layout"), "{errors:?}");
    let _ = fs::remove_dir_all(&root);
}

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
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
    let text = String::from_utf8_lossy(&raw).into_owned();
    (text[9..12].parse().unwrap(), text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epub_pages() {
    let root = temp_root("serve");
    let paths = koma::cli::Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    let series = c.series().unwrap()[0].clone();
    let books = c.books(series.id).unwrap();
    let (one, two) = (books[0].id, books[1].id);
    drop(c);

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
        // The series links to the EPUB reader and counts chapters.
        let (_, page) = get(addr, &format!("/s/{}", series.id));
        assert!(page.contains(&format!("<a class=\"btn primary\" href=\"/e/{one}/1\">")), "{page}");
        assert!(page.contains("<span>5 chapters</span>"));
        assert!(page.contains("<span>3 chapters</span>"));

        // A chapter: its cleaned text, where it is, the chapters around it.
        let (status, ch) = get(addr, &format!("/e/{one}/1"));
        assert_eq!(status, 200);
        assert!(ch.contains("content-security-policy: default-src 'none'; img-src 'self';"), "{ch}");
        assert!(ch.contains("<main lang=\"en\"><h1 id=\"e-top\">Low Tide</h1>"));
        assert!(ch.contains(&format!("href=\"{}\"", koma::html::epub_css_url())));
        assert!(ch.contains(&format!("<script src=\"{}\"></script>", koma::html::epub_js_url())));
        assert!(ch.contains("<title>1 · Low Tide · Saltwind 01 - The Harbor Fire · Koma-1</title>"));
        assert!(ch.contains("<span>chapter 1 of 3</span>"));
        assert!(ch.contains("<a class=l1 href=\"1\" aria-current=true>1 · Low Tide</a><a class=l1 href=\"2\">2 · The Night Ferry</a>"));
        assert!(ch.contains(&format!(
            "<nav class=chap aria-label=Chapters><a href=\"/s/{}\">‹ Saltwind</a>\
             <a rel=next href=\"2\">2 · The Night Ferry ›</a></nav>",
            series.id
        )));
        assert!(!ch.contains("steal"));
        let (_, ch2) = get(addr, &format!("/e/{one}/2"));
        assert!(!ch2.contains("steal") && ch2.contains("<p>Mirelle counted the boats.</p>"));
        // The last chapter goes on to the next book; the last book's back
        // to the series.
        let (_, last) = get(addr, &format!("/e/{one}/3"));
        assert!(last.contains(&format!(
            "<span class=end>end of Saltwind 01 - The Harbor Fire</span><a rel=next href=\"/e/{two}/1\">read Saltwind 02 - Ash on the Water ›</a>"
        )), "{last}");
        let (_, end) = get(addr, &format!("/e/{two}/2"));
        assert!(end.contains(&format!("<a rel=next href=\"/s/{}\">back to Saltwind ›</a>", series.id)));

        assert_eq!(get(addr, &format!("/e/{one}/0")).0, 404);
        assert_eq!(get(addr, &format!("/e/{one}/4")).0, 404);
        assert_eq!(get(addr, "/e/999/1").0, 404);
        // The page reader sends an EPUB here.
        let (status, moved) = get(addr, &format!("/r/{one}/2"));
        assert_eq!(status, 303);
        assert!(moved.contains(&format!("location: /e/{one}/1")));

        for url in [koma::html::epub_css_url(), koma::html::epub_js_url()] {
            let (status, asset) = get(addr, url);
            assert_eq!(status, 200, "{url}");
            assert!(asset.contains("immutable"));
        }
        let (_, file) = get(addr, &format!("/d/{one}"));
        assert!(file.contains("content-type: application/epub+zip"));
        // Search finds the book by its title, and links to the EPUB reader.
        let (_, found) = get(addr, "/search?q=harbor");
        assert!(found.contains(&format!("href=\"/e/{one}/1\"")), "{found}");
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
