//! M6's exit check: moving and renaming a series keeps its id, progress and
//! edits; a deleted series comes back with its progress after its folder
//! was removed. Also: moving books, clashes, undo, details written to
//! metadata.json, erasing, and readers refused.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use koma::catalog::Catalog;
use koma::cli::Paths;
use koma::serve::{AppState, serve};
use koma_fixtures::zip::{Compression, ZipWriter};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct Response {
    status: u16,
    headers: String,
    body: String,
}

impl Response {
    fn location(&self) -> String {
        self.headers
            .lines()
            .find_map(|l| {
                let (n, v) = l.split_once(':')?;
                n.eq_ignore_ascii_case("location")
                    .then(|| v.trim().to_owned())
            })
            .unwrap_or_default()
    }

    /// The change id in a `…done=<id>` redirect.
    fn op(&self) -> i64 {
        let l = self.location();
        l.rsplit("done=")
            .next()
            .and_then(|n| n.parse().ok())
            .expect(&l)
    }
}

fn request(addr: SocketAddr, user: &str, method: &str, path: &str, body: &str) -> Response {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nRemote-User: {user}\r\nOrigin: http://localhost\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8(raw[..split].to_vec()).unwrap();
    Response {
        status: headers[9..12].parse().unwrap(),
        headers,
        body: String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
    }
}

fn get(addr: SocketAddr, user: &str, path: &str) -> Response {
    request(addr, user, "GET", path, "")
}

fn post(addr: SocketAddr, user: &str, path: &str, body: &str) -> Response {
    request(addr, user, "POST", path, body)
}

fn enc(s: &str) -> String {
    koma::libraries::encode_path_part(s)
}

fn cbz(path: &Path, seed: u32) {
    let mut z = ZipWriter::new();
    for p in 0..3 {
        z.add(
            &format!("{p:02}.png"),
            &koma_fixtures::png::page(40, 60, seed * 10 + p),
            Compression::Stored,
        );
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

/// ```text
/// manga/Starfall/Starfall v01..v03.cbz
/// manga/Moonforge/Moonforge v01.cbz, Starfall v03.cbz (a clash)
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    for v in 1..=3 {
        cbz(&lib.join(format!("manga/Starfall/Starfall v0{v}.cbz")), v);
    }
    cbz(&lib.join("manga/Moonforge/Moonforge v01.cbz"), 7);
    cbz(&lib.join("manga/Moonforge/Starfall v03.cbz"), 8);
    lib
}

/// Series and book ids by name, from the catalogue.
fn series_id(db: &Path, name: &str) -> i64 {
    let c = Catalog::open_read_only(db).unwrap();
    c.series_with_counts()
        .unwrap()
        .into_iter()
        .find(|(s, _)| s.name == name)
        .map(|(s, _)| s.id)
        .unwrap_or_else(|| panic!("no series {name}"))
}

fn book_id(db: &Path, series: i64, name: &str) -> i64 {
    let c = Catalog::open_read_only(db).unwrap();
    c.books(series)
        .unwrap()
        .into_iter()
        .find(|b| b.name == name)
        .unwrap()
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn series_keep_ids_progress_and_edits_through_moves_and_the_trash() {
    let root = std::env::temp_dir().join(format!("koma-organise-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    let paths = Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let db = paths.data.join("koma.db");
    {
        let mut c = Catalog::open(&db).unwrap();
        koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    }
    // The worker, in-process: takes up queued changes as `koma worker` does.
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (paths, stop) = (paths.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
            while !stop.load(Ordering::Relaxed) {
                koma::organise::run(&paths.library, &paths.data, &mut c);
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let state = AppState::with_auth(
        &paths,
        koma::auth::Config::parse(Some("Remote-User"), Some("127.0.0.1")).unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_server, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, state, async {
        let _ = stopped.await;
    }));
    let p2 = paths.clone();

    tokio::task::spawn_blocking(move || {
        let paths = p2;
        let lib = &paths.library;
        let db = paths.data.join("koma.db");
        // mirelle is the admin (first); bram a reader.
        assert_eq!(get(addr, "mirelle", "/").status, 200);
        assert_eq!(get(addr, "bram", "/").status, 200);

        let starfall = series_id(&db, "Starfall");
        let moonforge = series_id(&db, "Moonforge");
        let v02 = book_id(&db, starfall, "Starfall v02");
        let v03 = book_id(&db, starfall, "Starfall v03");

        // Progress and an edit on v02.
        assert_eq!(post(addr, "mirelle", "/kept", &format!("pos%3A{v02}=2")).status, 204);
        let r = post(
            addr,
            "mirelle",
            "/details",
            &format!("b={v02}&title=The+Quiet+Satellite&was_title=&authors=Sela+Marr&was_authors="),
        );
        assert_eq!(r.status, 303, "{}", r.body);
        let details = |path: &str| Catalog::open_read_only(&db).unwrap().details(path).unwrap().unwrap();
        assert_eq!(details("manga/Starfall/Starfall v02.cbz").title, "The Quiet Satellite");
        assert_eq!(details("manga/Starfall/Starfall v02.cbz").authors, ["Sela Marr"]);
        let page = get(addr, "mirelle", &format!("/s/{starfall}")).body;
        assert!(page.contains("<span class=tag>66%</span>"), "progress on the cover");
        assert!(page.contains("read on: Starfall v02"));

        // Readers are refused.
        for (path, body) in [
            ("/organise", format!("action=trash&s={starfall}&b={v02}")),
            ("/organise", format!("action=trash-series&s={starfall}")),
            ("/move", format!("s={starfall}&l=manga&c=&name=Mine")),
            ("/details", format!("b={v02}&title=x&was_title=")),
        ] {
            assert_eq!(post(addr, "bram", path, &body).status, 403, "{path} {body}");
        }
        assert_eq!(get(addr, "bram", &format!("/move?b={v02}")).status, 403);
        // But they may mark their own reading.
        assert_eq!(post(addr, "bram", "/organise", &format!("action=read&s={starfall}&b={v03}")).status, 303);

        // Moving and renaming the series: a plan first, then applied.
        let ask = format!("s={starfall}&l=manga&c={}&name={}", enc("Starfall Saga"), enc("Starfall Prime"));
        let plan = get(addr, "mirelle", &format!("/move?{ask}")).body;
        assert!(plan.contains("<div class=from>manga/Starfall</div><div class=to>manga/Starfall Saga/Starfall Prime</div>"), "{plan}");
        assert!(lib.join("manga/Starfall").exists(), "a plan changes nothing");
        let r = post(addr, "mirelle", "/move", &ask);
        assert_eq!(r.status, 303);
        assert!(r.location().starts_with(&format!("/s/{starfall}?done=")), "{}", r.location());
        assert!(lib.join("manga/Starfall Saga/Starfall Prime/Starfall v02.cbz").exists());
        assert!(!lib.join("manga/Starfall").exists());
        // Same id, same progress, same edits, the new name.
        let page = get(addr, "mirelle", &r.location()).body;
        assert!(page.contains("<h1>Starfall Prime</h1>"));
        assert!(page.contains("<span class=tag>66%</span>"));
        assert!(page.contains("Done: Move manga/Starfall"));
        assert_eq!(details("manga/Starfall Saga/Starfall Prime/Starfall v02.cbz").title, "The Quiet Satellite");
        // A scan afterwards changes nothing of that.
        {
            let mut c = Catalog::open(&db).unwrap();
            koma::scan::scan(lib, &paths.data, &mut c).unwrap();
        }
        assert_eq!(series_id(&db, "Starfall Prime"), starfall);
        assert_eq!(book_id(&db, starfall, "Starfall v02"), v02);
        assert_eq!(details("manga/Starfall Saga/Starfall Prime/Starfall v02.cbz").authors, ["Sela Marr"]);

        // Moving a book where its name is taken: “(2)”, then undo.
        let plan = get(addr, "mirelle", &format!("/move?b={v03}&to={moonforge}")).body;
        assert!(plan.contains("Starfall v03 (2).cbz") && plan.contains("class=warn"), "{plan}");
        let r = post(addr, "mirelle", "/move", &format!("b={v03}&to={moonforge}"));
        let moved = r.op();
        assert!(lib.join("manga/Moonforge/Starfall v03 (2).cbz").exists());
        assert_eq!(book_id(&db, moonforge, "Starfall v03 (2)"), v03, "same id");
        let r = post(addr, "mirelle", &format!("/jobs/{moved}/undo"), "");
        assert_eq!(r.status, 303);
        assert!(lib.join("manga/Starfall Saga/Starfall Prime/Starfall v03.cbz").exists());
        assert!(!lib.join("manga/Moonforge/Starfall v03 (2).cbz").exists());
        assert_eq!(book_id(&db, starfall, "Starfall v03"), v03);
        let jobs = get(addr, "mirelle", "/jobs").body;
        assert!(jobs.contains("Undo: Move Starfall v03 to Moonforge"), "{jobs}");

        // metadata.json written next to the book; undo takes it away.
        let r = post(addr, "mirelle", "/details", &format!("b={v02}&write=1"));
        let wrote = r.op();
        let side = lib.join("manga/Starfall Saga/Starfall Prime/Starfall v02.metadata.json");
        let json = fs::read_to_string(&side).unwrap();
        assert!(json.contains("\"title\": \"The Quiet Satellite\"") && json.contains("Sela Marr"), "{json}");
        post(addr, "mirelle", &format!("/jobs/{wrote}/undo"), "");
        assert!(!side.exists());

        // Deleting the series: gone from the library at once, in the trash.
        let r = post(addr, "mirelle", "/organise", &format!("action=trash-series&s={starfall}"));
        assert_eq!(r.status, 303);
        assert!(r.location().starts_with("/trash?done="));
        assert_eq!(get(addr, "mirelle", &format!("/s/{starfall}")).status, 404);
        assert_eq!(get(addr, "mirelle", &format!("/r/{v02}/1")).status, 404);
        let trash = get(addr, "mirelle", "/trash").body;
        assert!(trash.contains("Starfall Prime") && trash.contains("manga/Starfall Saga"), "{trash}");
        let item: i64 = {
            let at = trash.find("name=t value=").unwrap() + "name=t value=".len();
            trash[at..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
        };
        assert!(lib.join(".koma-trash").exists());
        // Its folder's parent is removed by hand, and a scan runs.
        fs::remove_dir(lib.join("manga/Starfall Saga")).unwrap();
        {
            let mut c = Catalog::open(&db).unwrap();
            koma::scan::scan(lib, &paths.data, &mut c).unwrap();
        }
        // Bram can't restore it.
        assert_eq!(post(addr, "bram", "/trash", &format!("action=restore&t={item}")).status, 403);
        // Restored: the folder made again, the same id, the progress back.
        let r = post(addr, "mirelle", "/trash", &format!("action=restore&t={item}"));
        assert_eq!(r.status, 303);
        assert!(lib.join("manga/Starfall Saga/Starfall Prime/Starfall v02.cbz").exists());
        let page = get(addr, "mirelle", &format!("/s/{starfall}")).body;
        assert!(page.contains("<h1>Starfall Prime</h1>"), "{page}");
        assert!(page.contains("<span class=tag>66%</span>"));
        assert_eq!(book_id(&db, starfall, "Starfall v02"), v02);
        assert!(!lib.join(".koma-trash").read_dir().unwrap().any(|_| true), "trash left tidy");

        // A book deleted, then erased for good.
        let r = post(addr, "mirelle", "/organise", &format!("action=trash&s={moonforge}&b={}", book_id(&db, moonforge, "Starfall v03")));
        assert_eq!(r.status, 303);
        assert!(!lib.join("manga/Moonforge/Starfall v03.cbz").exists());
        let trash = get(addr, "mirelle", "/trash").body;
        let at = trash.find("name=t value=").unwrap() + "name=t value=".len();
        let item: i64 = trash[at..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
        post(addr, "mirelle", "/trash", &format!("action=erase&t={item}"));
        assert!(get(addr, "mirelle", "/trash").body.contains("The trash is empty"));
        assert!(!lib.join(".koma-trash").read_dir().unwrap().any(|_| true));
    })
    .await
    .unwrap();

    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    stop_server.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
