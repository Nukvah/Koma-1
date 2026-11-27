//! Downloading originals: a book file comes back byte for byte with its
//! name and type; a photo folder comes back as a valid stored ZIP of its
//! images, in reading order; a file changed since the scan cuts the
//! download short instead of sending something broken.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use koma::archive::zip::{self, Method};
use koma::catalog::Catalog;
use koma_fixtures::zip::{Compression, ZipWriter};

struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim())
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
    // A download cut short ends the connection early: keep what came.
    let _ = stream.read_to_end(&mut raw);
    // A download that fails before its first chunk closes with no response
    // at all: status 0.
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Response {
            status: 0,
            headers: String::new(),
            body: raw,
        };
    };
    let headers = String::from_utf8(raw[..split].to_vec()).unwrap();
    Response {
        status: headers[9..12].parse().unwrap(),
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

/// ```text
/// Starfall/Starfall v01.cbz       3 pages
/// Starfall/Starfall v02.cb7
/// Mirelle Set/cover.png, 2.png, 10.png   a photo folder
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    fs::create_dir_all(lib.join("Starfall")).unwrap();
    let mut z = ZipWriter::new();
    for n in 1..=3 {
        z.add(
            &format!("{n}.png"),
            &koma_fixtures::png::page(30, 45, n),
            Compression::Deflated,
        );
    }
    fs::write(lib.join("Starfall/Starfall v01.cbz"), z.finish()).unwrap();
    let page = koma_fixtures::png::page(30, 45, 9);
    fs::write(
        lib.join("Starfall/Starfall v02.cb7"),
        koma_fixtures::sevenz::write(&[("1.png", &page)], true),
    )
    .unwrap();
    fs::create_dir_all(lib.join("Mirelle Set")).unwrap();
    for (name, seed) in [("10.png", 10), ("2.png", 2), ("cover.png", 1)] {
        fs::write(
            lib.join("Mirelle Set").join(name),
            koma_fixtures::png::page(40, 60, seed),
        )
        .unwrap();
    }
    lib
}

/// The ids of books by file name.
fn book_id(c: &Catalog, name: &str) -> i64 {
    for s in c.series().unwrap() {
        for b in c.books(s.id).unwrap() {
            if b.name == name {
                return b.id;
            }
        }
    }
    panic!("no book {name}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originals_come_back_as_they_are() {
    let root = std::env::temp_dir().join(format!("koma-download-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    let paths = koma::cli::Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    let cbz = book_id(&c, "Starfall v01");
    let cb7 = book_id(&c, "Starfall v02");
    let photos = book_id(&c, "Mirelle Set");
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
    let lib = paths.library.clone();
    tokio::task::spawn_blocking(move || {
        // A book file, byte for byte.
        let r = get(addr, &format!("/d/{cbz}"));
        assert_eq!(r.status, 200);
        let original = fs::read(lib.join("Starfall/Starfall v01.cbz")).unwrap();
        assert_eq!(r.body, original);
        assert_eq!(
            r.header("content-length"),
            Some(original.len().to_string().as_str())
        );
        assert_eq!(
            r.header("content-type"),
            Some("application/vnd.comicbook+zip")
        );
        assert_eq!(
            r.header("content-disposition"),
            Some("attachment; filename=\"Starfall v01.cbz\"; filename*=UTF-8''Starfall%20v01.cbz")
        );
        let r = get(addr, &format!("/d/{cb7}"));
        assert_eq!(
            r.header("content-type"),
            Some("application/x-7z-compressed")
        );
        assert_eq!(
            r.body,
            fs::read(lib.join("Starfall/Starfall v02.cb7")).unwrap()
        );

        // A photo folder: a stored ZIP, read back with Koma-1's own reader.
        let r = get(addr, &format!("/d/{photos}"));
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some("application/zip"));
        assert!(
            r.header("content-disposition")
                .unwrap()
                .contains("Mirelle Set.zip")
        );
        assert_eq!(
            r.header("content-length"),
            Some(r.body.len().to_string().as_str())
        );
        let entries =
            zip::read_index(&r.body, r.body.len() as u64, &zip::Limits::default()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["cover.png", "2.png", "10.png"]);
        for e in &entries {
            assert_eq!(e.method, Method::Stored);
            let data = &r.body[e.data_range().start as usize..e.data_range().end as usize];
            assert_eq!(
                data,
                fs::read(lib.join("Mirelle Set").join(&e.name)).unwrap()
            );
            assert_eq!(e.crc32, koma::archive::crc32::checksum(data));
        }

        // Changed since the scan: the download stops short, it never ends
        // as if complete. Whether the headers went out first depends on
        // timing: the error can come before the first chunk is sent.
        fs::write(lib.join("Mirelle Set/2.png"), b"shorter").unwrap();
        let r = get(addr, &format!("/d/{photos}"));
        if r.status != 0 {
            let promised: usize = r.header("content-length").unwrap().parse().unwrap();
            assert!(r.body.len() < promised, "{} of {promised}", r.body.len());
        }

        assert_eq!(get(addr, "/d/999").status, 404);
        assert_eq!(get(addr, "/d/x").status, 400);
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
