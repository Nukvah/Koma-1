//! M3's exit check, the part a test can hold: a library of 10,000 made-up
//! series is served in chunks that are each under 15 KiB and work without
//! JavaScript, linked one to the next from the first to the last. (That it
//! scrolls smoothly with flat memory is measured in a browser: see
//! docs/development.md.)

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use koma::catalog::{Catalog, Details, NewBook, Page, Size};

const SERIES: usize = 10_000;
const MAX_CHUNK: usize = 15 * 1024;

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1.to_owned();
    (text[9..12].parse().unwrap(), body)
}

/// The `href` of the link with `rel`, `&amp;` turned back into `&`.
fn rel(html: &str, rel: &str) -> Option<String> {
    let end = html.find(&format!("\" rel={rel}>"))?;
    let start = html[..end].rfind("href=\"")? + 6;
    Some(html[start..end].replace("&amp;", "&"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_thousand_series_in_small_chunks() {
    let root = std::env::temp_dir().join(format!("koma-exit-m3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    let paths = koma::cli::Paths {
        library: root.join("library"),
        data: root.join("data"),
    };

    // The catalogue as a scan would leave it, written directly: the files
    // themselves are not needed to serve library pages.
    let t = std::time::Instant::now();
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    let details = Details::default();
    for n in 0..SERIES {
        let name = format!("Starfall Variant {n:05} with a Rather Long Title");
        let series_path = format!("manga/{name}");
        let path = format!("{series_path}/v01.cbz");
        let page = Page {
            number: 0,
            hash: blake3::hash(path.as_bytes()),
            format: "png".into(),
            length: 1000,
            offset: Some(64),
            file: None,
            size: Some(Size {
                width: 600,
                height: 900,
            }),
        };
        c.replace_book(
            &NewBook {
                series_path: &series_path,
                series_name: &name,
                path: &path,
                folder: &series_path,
                name: "v01",
                size: 2000,
                mtime: 0,
                pages: &[page],
                chapters: &[],
                toc: &[],
                details: &details,
            },
            1_795_737_600,
        )
        .unwrap();
    }
    drop(c);
    println!("insert: {:?}", t.elapsed());

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
        // Follow the grid from / to the end by its plain links, as a
        // browser without JavaScript would.
        let t = std::time::Instant::now();
        let mut url = Some("/".to_owned());
        let (mut chunks, mut cards, mut largest) = (0, 0, 0);
        while let Some(u) = url {
            let (status, html) = get(addr, &u);
            assert_eq!(status, 200, "{u}");
            largest = largest.max(html.len());
            assert!(html.len() < MAX_CHUNK, "{u}: {} bytes", html.len());
            assert!(html.contains("<div class=grid data-scroll>"));
            cards += html.matches("<a class=card").count();
            chunks += 1;
            url = rel(&html, "next");
        }
        assert_eq!(cards, SERIES);
        assert_eq!(chunks, SERIES.div_ceil(koma::html::PER_PAGE));
        println!(
            "{chunks} grid chunks, the largest {largest} bytes, {:?}",
            t.elapsed()
        );
        let t = std::time::Instant::now();

        // The table too, by its own links.
        let mut url = Some("/?view=table&sort=-added".to_owned());
        let (mut pages, mut rows) = (0, 0);
        while let Some(u) = url {
            let (status, html) = get(addr, &u);
            assert_eq!(status, 200, "{u}");
            assert!(html.len() < MAX_CHUNK, "{u}: {} bytes", html.len());
            rows += html.matches("<tr><td>").count();
            pages += 1;
            url = rel(&html, "next");
        }
        assert_eq!(rows, SERIES);
        assert_eq!(pages, SERIES.div_ceil(koma::html::TABLE_PER_PAGE));
        println!("{pages} table pages, {:?}", t.elapsed());

        // The script that turns the chunks into one scroll.
        let (status, js) = get(addr, koma::html::scroll_url());
        assert_eq!(status, 200);
        assert!(js.contains("IntersectionObserver"));
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
