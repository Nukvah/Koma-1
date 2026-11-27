//! The reader page and what its script reads: the page with its variants,
//! tap thirds in the book's direction (its own, else its library's), the
//! bars without JavaScript, and the book's pages as JSON with the card for
//! its end.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use koma::catalog::Catalog;
use koma_fixtures::zip::{Compression, ZipWriter};

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

fn book(path: &Path, pages: &[Vec<u8>], info: Option<&str>) {
    let mut z = ZipWriter::new();
    for (n, p) in pages.iter().enumerate() {
        z.add(&format!("{n:02}.png"), p, Compression::Stored);
    }
    if let Some(info) = info {
        let xml = format!("<ComicInfo>{info}</ComicInfo>");
        z.add("ComicInfo.xml", xml.as_bytes(), Compression::Stored);
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// ```text
/// manga/Starfall/v01.cbz         a big page (variants), then two small ones: right to left
/// manga/Starfall/v02.cbz         the next book
/// comics/Harbor Lights/v01.cbz   left to right, by its library
/// comics/Moonforge/v01.cbz       right to left, by its own metadata
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    let small = |s| koma_fixtures::png::page(30, 45, s);
    book(
        &lib.join("manga/Starfall/v01.cbz"),
        &[koma_fixtures::png::page(1200, 1800, 1), small(2), small(3)],
        None,
    );
    book(&lib.join("manga/Starfall/v02.cbz"), &[small(8)], None);
    book(
        &lib.join("comics/Harbor Lights/v01.cbz"),
        &[small(4), small(5)],
        None,
    );
    book(
        &lib.join("comics/Moonforge/v01.cbz"),
        &[small(6), small(7)],
        Some("<Manga>YesAndRightToLeft</Manga>"),
    );
    lib
}

fn book_id(c: &Catalog, series: &str) -> (i64, i64) {
    for s in c.series().unwrap() {
        if s.name == series {
            return (c.books(s.id).unwrap()[0].id, s.id);
        }
    }
    panic!("no series {series}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_page() {
    let root = std::env::temp_dir().join(format!("koma-reader-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    let paths = koma::cli::Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    let (starfall, starfall_series) = book_id(&c, "Starfall");
    let (harbor, _) = book_id(&c, "Harbor Lights");
    let (moonforge, _) = book_id(&c, "Moonforge");
    let big = c.pages(starfall).unwrap()[0].hash.to_hex().to_string();
    let v02 = c.books(starfall_series).unwrap()[1].id;
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
        let (status, page) = get(addr, &format!("/r/{starfall}/1"));
        assert_eq!(status, 200);
        // Its own stylesheet and script, at content-named URLs.
        assert!(page.contains(&format!("href=\"{}\"", koma::html::reader_css_url())));
        assert!(page.contains(&format!("src=\"{}\"", koma::html::reader_js_url())));
        // The page with its variants: WebP first, JPEG to fall back on, the
        // page itself last; shown whole in the window.
        let v = koma::variants::VERSION;
        assert!(page.contains(&format!(
            "<source id=pgw type=image/webp srcset=\"/p/{big}.720.v{v}.webp 720w, /p/{big}.1080.v{v}.webp 1080w, /p/{big}.png 1200w\" sizes=\"min(100vw, 66.7vh)\">"
        )), "{page}");
        assert!(page.contains(&format!(
            "<img id=pg src=\"/p/{big}.png\" srcset=\"/p/{big}.720.v{v}.jpg 720w, /p/{big}.1080.v{v}.jpg 1080w, /p/{big}.png 1200w\""
        )));
        assert!(page.contains("width=1200 height=1800 alt=\"Page 1\" fetchpriority=high"));
        // Right to left (a manga library): the left third goes forward; on
        // the first page the right third goes back to the series.
        assert!(page.contains(&format!(
            "<nav class=zones aria-label=Pages><a href=\"/r/{starfall}/2\"><span>‹ next</span></a><a href=\"#bars\"><span>bars</span></a><a href=\"/s/{starfall_series}\"><span>previous ›</span></a></nav>"
        )), "{page}");
        assert!(page.contains("data-dir=rtl"));
        assert!(page.contains("<div id=bars class=bars>"));
        // The mode (by itself unless chosen) and the double-page shift.
        for mode in ["auto aria-current=true>auto", "single>single", "double>double", "strip>strip"] {
            assert!(page.contains(&format!("data-mode={mode}</a>")), "{mode}");
        }
        assert!(page.contains("data-shift>shift <kbd>s</kbd></a>"));
        // Fit, and the tools that need the script: hidden until it runs.
        for fit in ["page aria-current=true>page", "width>width", "height>height"] {
            assert!(page.contains(&format!("data-fit={fit}</a>")), "{fit}");
        }
        assert!(page.contains("<div class=scrub hidden><input id=scrub type=range min=1 max=3 value=1"));
        assert!(page.contains("<div class=thumbs hidden></div>"));
        assert!(page.contains("data-zoom hidden>zoom <kbd>z</kbd></a>"));
        assert!(page.contains("data-thumbs hidden>thumbnails <kbd>t</kbd></a>"));
        assert!(page.contains(&format!("<link rel=next href=\"/r/{starfall}/2\">")));

        // The last page goes on to the series.
        let (_, last) = get(addr, &format!("/r/{starfall}/3"));
        assert!(last.contains(&format!("<a href=\"/s/{starfall_series}\"><span>‹ next</span></a>")));
        assert!(!last.contains("rel=next"));

        // Left to right by its library; right to left by its own metadata.
        let (_, ltr) = get(addr, &format!("/r/{harbor}/1"));
        assert!(ltr.contains("data-dir=ltr") && ltr.contains("<span>next ›</span>"));
        let (_, own) = get(addr, &format!("/r/{moonforge}/1"));
        assert!(own.contains("data-dir=rtl"));

        assert_eq!(get(addr, &format!("/r/{starfall}/0")).0, 404);
        assert_eq!(get(addr, &format!("/r/{starfall}/4")).0, 404);

        // The book's pages for the script.
        let (status, json) = get(addr, &format!("/b/{starfall}"));
        assert_eq!(status, 200);
        let body = json.split_once("\r\n\r\n").unwrap().1;
        let j: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(j["v"], v);
        let p = j["p"].as_array().unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0]["h"], big.as_str());
        assert_eq!(p[0]["w"], 1200);
        assert_eq!(p[0]["s"], serde_json::json!([[720, 3], [1080, 3]]));
        assert_eq!(p[1]["s"], serde_json::json!([]));
        // At its end, the next book with its cover; the last one goes back.
        let end = j["end"].as_str().unwrap();
        assert!(end.contains("<p>end of v01 · 3 pages</p><strong>v02</strong>"), "{end}");
        assert!(end.contains(&format!(
            "<a class=\"btn primary\" href=\"/r/{v02}/1\" data-next={v02}>read v02 <kbd>space</kbd></a>\
             <a class=btn href=\"/s/{starfall_series}\">back to Starfall</a>"
        )), "{end}");
        assert!(end.contains("<div class=cover"));
        let (_, last) = get(addr, &format!("/b/{v02}"));
        assert!(last.contains("the last book"), "{last}");
        assert_eq!(get(addr, "/b/999").0, 404);

        for url in [koma::html::reader_css_url(), koma::html::reader_js_url()] {
            let (status, asset) = get(addr, url);
            assert_eq!(status, 200, "{url}");
            assert!(asset.contains("immutable"));
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
