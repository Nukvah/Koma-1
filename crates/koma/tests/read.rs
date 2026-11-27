//! M1's done-test: a test library of made-up CBZ files reads front to back.
//!
//! The made-up library is scanned, then walked over HTTP the way a person
//! would: library → series → first page → "next" until the end. Every page
//! image must be exactly the original bytes, in the original reading order.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use koma::catalog::Catalog;
use koma::cli::Paths;
use koma::serve::{AppState, serve};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Response {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
}

fn get(addr: SocketAddr, path: &str) -> Response {
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
    let headers = String::from_utf8(raw[..split].to_vec()).unwrap();
    let status = headers[9..12].parse().unwrap();
    Response {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

/// Every `href="…"` (or `src="…"`) value that starts with `prefix`, in order.
fn links(html: &str, attr: &str, prefix: &str) -> Vec<String> {
    let needle = format!("{attr}=\"{prefix}");
    html.match_indices(&needle)
        .map(|(i, _)| {
            let start = i + attr.len() + 2;
            let end = start + html[start..].find('"').unwrap();
            html[start..end].to_owned()
        })
        .collect()
}

fn rel_next(html: &str) -> Option<String> {
    let i = html.find("<link rel=next href=\"")? + "<link rel=next href=\"".len();
    Some(html[i..i + html[i..].find('"')?].to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn made_up_library_reads_front_to_back() {
    let root = std::env::temp_dir().join(format!("koma-read-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = Paths {
        library: root.join("library"),
        data: root.join("data"),
    };
    let books = koma_fixtures::library::write(&paths.library).unwrap();
    std::fs::create_dir_all(&paths.data).unwrap();
    let mut catalog = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut catalog).unwrap();
    drop(catalog);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        listener,
        AppState::with_auth(
            &paths,
            koma::auth::Config::parse(Some("Remote-User"), Some("127.0.0.1")).unwrap(),
        ),
        async {
            let _ = stopped.await;
        },
    ));

    let read = tokio::task::spawn_blocking(move || {
        let mut read: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
        let library = get(addr, "/");
        assert_eq!(library.status, 200);
        assert_eq!(
            library.header("content-type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        for series in links(&library.text(), "href", "/s/") {
            let page = get(addr, &series);
            assert_eq!(page.status, 200, "{series}");
            // Each book once, in page order: a "read from the start"
            // button links to the first book a second time.
            let mut starts = links(&page.text(), "href", "/r/");
            let mut seen = std::collections::HashSet::new();
            starts.retain(|s| seen.insert(s.clone()));
            for start in starts {
                let mut images = Vec::new();
                let mut at = Some(start.clone());
                let mut title = String::new();
                while let Some(url) = at {
                    let html = get(addr, &url).text();
                    if title.is_empty() {
                        let t = html.find("<title>").unwrap() + 7;
                        title = html[t..t + html[t..].find(" · ").unwrap()].to_owned();
                    }
                    let src = links(&html, "src", "/p/");
                    assert_eq!(src.len(), 1, "{url}");
                    let image = get(addr, &src[0]);
                    assert_eq!(image.status, 200, "{}", src[0]);
                    assert_eq!(image.header("content-type").as_deref(), Some("image/png"));
                    assert_eq!(
                        image.header("cache-control").as_deref(),
                        Some("public, max-age=31536000, immutable")
                    );
                    images.push(image.body);
                    at = rel_next(&html);
                }
                read.push((title, images));
            }
        }
        read
    })
    .await
    .unwrap();

    // Every book, in library order, with every page in reading order.
    let mut want: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
    for path in [
        "Lone Comet.cbz",
        "Mirelle's Garden/Mirelle's Garden 第1巻.cbz",
        "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.cbz",
        "Moonforge Chronicles/Moonforge Chronicles - Ch. 001.5.cbz",
        "Moonforge Chronicles/Moonforge Chronicles - Ch. 002.cb7",
        "Starfall/Starfall v01.cbz",
        "Starfall/Starfall v02.cbz",
        "Starfall/Starfall v10.cbz",
    ] {
        let book = books.iter().find(|b| b.path == path).unwrap();
        let file = path.rsplit('/').next().unwrap();
        let name = file.rsplit_once('.').unwrap().0;
        let title = koma::html::escape(name);
        want.push((title, book.pages.iter().map(|(_, b)| b.clone()).collect()));
    }
    assert_eq!(read.len(), want.len());
    for ((got_title, got), (want_title, want)) in read.iter().zip(&want) {
        assert_eq!(got_title, want_title);
        assert_eq!(got.len(), want.len(), "{got_title}");
        for (i, (a, b)) in got.iter().zip(want).enumerate() {
            assert!(a == b, "{got_title}: page {} differs", i + 1);
        }
    }

    // Out-of-range and unknown things are 404s, not errors.
    let missing = tokio::task::spawn_blocking(move || {
        [
            "/s/999",
            "/r/1/0",
            "/r/1/999",
            "/r/999/1",
            "/p/nothex.png",
            &format!("/p/{}.png", "0".repeat(64)),
        ]
        .map(|path| get(addr, path).status)
    })
    .await
    .unwrap();
    assert_eq!(missing, [404; 6]);

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_urls_answer_not_modified_to_their_etag() {
    let root = std::env::temp_dir().join(format!("koma-etag-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = Paths {
        library: root.join("library"),
        data: root.join("data"),
    };
    let books = koma_fixtures::library::write(&paths.library).unwrap();
    std::fs::create_dir_all(&paths.data).unwrap();
    let mut catalog = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut catalog).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        listener,
        AppState::with_auth(
            &paths,
            koma::auth::Config::parse(Some("Remote-User"), Some("127.0.0.1")).unwrap(),
        ),
        async {
            let _ = stopped.await;
        },
    ));
    let hex = blake3::hash(&books[0].pages[0].1).to_hex().to_string();
    let status = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "GET /p/{hex}.png HTTP/1.1\r\nHost: x\r\nIf-None-Match: \"{hex}\"\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        raw[9..12].to_owned()
    })
    .await
    .unwrap();
    assert_eq!(status, "304");
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}
