//! M8's exit check, second half: a 2 GB upload survives a dropped
//! connection. Also: who may upload where, packs built as their images
//! arrive, and an upload seen only by the account that started it.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use koma::catalog::Catalog;
use koma::cli::Paths;
use koma::serve::{AppState, serve};
use koma_fixtures::bulk;
use koma_fixtures::zip::{Compression, ZipWriter};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const GIB: u64 = 1 << 30;

struct Response {
    status: u16,
    json: Value,
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    s
}

fn head(user: &str, method: &str, path: &str, len: u64) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nRemote-User: {user}\r\nOrigin: http://localhost\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n"
    )
}

fn answer(mut s: TcpStream) -> Response {
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
    Response {
        status: headers[9..12].parse().unwrap(),
        json: serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null),
    }
}

fn request(addr: SocketAddr, user: &str, method: &str, path: &str, body: &[u8]) -> Response {
    let mut s = connect(addr);
    s.write_all(head(user, method, path, body.len() as u64).as_bytes())
        .unwrap();
    s.write_all(body).unwrap();
    answer(s)
}

/// A page: its status and its text.
fn page_text(addr: SocketAddr, user: &str, path: &str) -> (u16, String) {
    let mut s = connect(addr);
    s.write_all(head(user, "GET", path, 0).as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    (text[9..12].parse().unwrap(), text)
}

/// Sends `len` bulk bytes from `from` as a file's next piece, but drops the
/// connection after `cut` of them when `cut` is given.
fn send(
    addr: SocketAddr,
    user: &str,
    id: &str,
    seed: u64,
    from: u64,
    len: u64,
    cut: Option<u64>,
) -> Option<Response> {
    let mut s = connect(addr);
    s.write_all(head(user, "PATCH", &format!("/up/{id}?offset={from}"), len).as_bytes())
        .unwrap();
    let stop = cut.unwrap_or(len);
    let mut sent = 0;
    while sent < stop {
        let n = (stop - sent).min(8 << 20) as usize;
        s.write_all(&bulk::bytes(seed, from + sent, n)).unwrap();
        sent += n as u64;
    }
    if cut.is_some() {
        // The connection drops: no more bytes, no answer read.
        s.shutdown(Shutdown::Both).unwrap();
        return None;
    }
    Some(answer(s))
}

/// Where the server got to, once nothing is being written any more.
///
/// The cut request stays busy until its writer has synced what arrived,
/// hundreds of MB here: that can take a while on a slow, shared CI disk.
fn received(addr: SocketAddr, user: &str, id: &str) -> u64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while std::time::Instant::now() < deadline {
        // A zero-byte piece at a wrong offset answers 409 with the real one,
        // or 409 "busy" while the cut request is still being written.
        let r = request(
            addr,
            user,
            "PATCH",
            &format!("/up/{id}?offset={}", u64::MAX),
            b"",
        );
        if r.status == 409 && r.json.get("received").is_some() {
            return r.json["received"].as_u64().unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the upload stayed busy");
}

fn cbz(path: &Path) {
    let mut z = ZipWriter::new();
    z.add(
        "01.png",
        &koma_fixtures::png::page(40, 60, 1),
        Compression::Stored,
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

fn setup(name: &str) -> (PathBuf, Paths) {
    let root = std::env::temp_dir().join(format!("koma-upload-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    cbz(&root.join("library/manga/Starfall/Starfall 01.cbz"));
    cbz(&root.join("library/cosplay/Mirelle/Moon Maid/Mirelle - Moon Maid.zip"));
    let paths = Paths {
        library: root.join("library"),
        data: root.join("data"),
    };
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    (root, paths)
}

async fn start_server(
    paths: &Paths,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let state = AppState::with_auth(
        paths,
        koma::auth::Config::parse(Some("Remote-User"), Some("127.0.0.1")).unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, state, async {
        let _ = stopped.await;
    }));
    (addr, stop, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_2_gb_upload_survives_a_dropped_connection() {
    let (root, paths) = setup("big");
    let (addr, stop, server) = start_server(&paths).await;
    let data = paths.data.clone();
    tokio::task::spawn_blocking(move || {
        let size = 2 * GIB;
        let seed = 42;
        assert_eq!(request(addr, "mirelle", "GET", "/", b"").status, 200); // the admin
        let r = request(
            addr,
            "mirelle",
            "POST",
            "/up",
            json!({"library": "manga", "kind": "file", "name": "Starfall 02.cbz", "size": size, "batch": "drop1"})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(r.status, 201, "{}", r.json);
        let id = r.json["id"].as_str().unwrap().to_owned();

        // The whole file in one request, cut at 700 MB.
        assert!(send(addr, "mirelle", &id, seed, 0, size, Some(700 << 20)).is_none());
        let at = received(addr, "mirelle", &id);
        assert!(at > 0 && at <= 700 << 20, "kept what arrived: {at}");

        // The rest, cut again at 1.5 GB.
        assert!(send(addr, "mirelle", &id, seed, at, size - at, Some(3 * GIB / 2 - at)).is_none());
        let at = received(addr, "mirelle", &id);
        assert!(at > 700 << 20, "{at}");

        // Sent from the wrong place: refused, with where to go on from.
        let wrong = send(addr, "mirelle", &id, seed, 0, 10, None).unwrap();
        assert_eq!((wrong.status, wrong.json["received"].as_u64()), (409, Some(at)));

        let done = send(addr, "mirelle", &id, seed, at, size - at, None).unwrap();
        assert_eq!(done.status, 200, "{}", done.json);
        assert_eq!(done.json["received"].as_u64(), Some(size));
        let r = request(addr, "mirelle", "POST", &format!("/up/{id}/done"), b"");
        assert_eq!((r.status, r.json["done"].as_bool()), (200, Some(true)), "{}", r.json);
        assert!(bulk::matches(&data.join(format!("uploads/{id}.part")), seed, size).unwrap());
    })
    .await
    .unwrap();
    let _ = stop.send(());
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packs_permissions_and_privacy() {
    let (root, paths) = setup("pack");
    let (addr, stop, server) = start_server(&paths).await;
    let data = paths.data.clone();
    tokio::task::spawn_blocking(move || {
        let post = |user: &str, body: Value| {
            request(addr, user, "POST", "/up", body.to_string().as_bytes())
        };
        assert_eq!(request(addr, "mirelle", "GET", "/", b"").status, 200); // the admin
        assert_eq!(request(addr, "bram", "GET", "/", b"").status, 200); // a reader

        // A reader may not add to a shared library; nobody to one that is not there.
        let pack = json!({"library": "cosplay", "kind": "pack", "name": "Starfall Bunny.zip"});
        assert_eq!(post("bram", pack.clone()).status, 403);
        assert_eq!(
            post(
                "mirelle",
                json!({"library": "nowhere", "kind": "pack", "name": "x.zip"})
            )
            .status,
            404
        );
        assert_eq!(
            post(
                "mirelle",
                json!({"library": "cosplay", "kind": "box", "name": "x"})
            )
            .status,
            400
        );

        let r = post("mirelle", pack);
        assert_eq!(r.status, 201);
        let id = r.json["id"].as_str().unwrap().to_owned();
        let put = |name: &str, body: &[u8]| {
            request(
                addr,
                "mirelle",
                "PUT",
                &format!("/up/{id}/m?name={name}"),
                body,
            )
        };
        let page = koma_fixtures::png::page(40, 60, 3);
        assert_eq!(put("001.png", &page).status, 200);
        assert_eq!(put("002.png", &page).status, 200);
        assert_eq!(put("002.png", &page).status, 409, "already there");
        assert_eq!(put("run.sh", b"x").status, 400);
        assert_eq!(
            put("metadata.json", br#"{"title": "Starfall Bunny"}"#).status,
            200
        );

        // Another account sees nothing of it.
        assert_eq!(
            request(addr, "bram", "GET", &format!("/up/{id}"), b"").status,
            404
        );
        assert_eq!(
            request(addr, "bram", "DELETE", &format!("/up/{id}"), b"").status,
            404
        );

        let s = request(addr, "mirelle", "GET", &format!("/up/{id}"), b"");
        assert_eq!(
            s.json["members"],
            json!(["001.png", "002.png", "metadata.json"])
        );
        let r = request(addr, "mirelle", "POST", &format!("/up/{id}/done"), b"");
        assert_eq!(r.status, 200, "{}", r.json);
        let index = koma::archive::zip::read_file_index(
            &data.join(format!("uploads/{id}.part")),
            &koma::archive::zip::Limits::default(),
        )
        .unwrap();
        assert_eq!(index.len(), 3);

        // Cancelling removes what was sent.
        let r = post(
            "mirelle",
            json!({"library": "manga", "kind": "file", "name": "a.cbz", "size": 4}),
        );
        let other = r.json["id"].as_str().unwrap().to_owned();
        assert_eq!(
            request(addr, "mirelle", "DELETE", &format!("/up/{other}"), b"").status,
            204
        );
        assert!(!data.join(format!("uploads/{other}.part")).exists());
        assert_eq!(
            request(addr, "mirelle", "GET", &format!("/up/{other}"), b"").status,
            404
        );
    })
    .await
    .unwrap();
    let _ = stop.send(());
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(root);
}

/// Starts an upload of `bytes` (a file) and finishes it: its id.
fn upload_file(addr: SocketAddr, library: &str, name: &str, bytes: &[u8]) -> String {
    let r = request(
        addr,
        "mirelle",
        "POST",
        "/up",
        json!({"library": library, "kind": "file", "name": name, "size": bytes.len(), "batch": "d2"})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r.status, 201, "{}", r.json);
    let id = r.json["id"].as_str().unwrap().to_owned();
    assert_eq!(
        request(
            addr,
            "mirelle",
            "PATCH",
            &format!("/up/{id}?offset=0"),
            bytes
        )
        .status,
        200
    );
    assert_eq!(
        request(addr, "mirelle", "POST", &format!("/up/{id}/done"), b"").status,
        200
    );
    id
}

fn cbz_bytes(seed: u32) -> Vec<u8> {
    let mut z = ZipWriter::new();
    for p in 0..2 {
        z.add(
            &format!("{p:02}.png"),
            &koma_fixtures::png::page(40, 60, seed * 10 + p),
            Compression::Stored,
        );
    }
    z.finish()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_is_planned_filed_logged_and_undone() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (root, paths) = setup("filed");
    // The worker, in-process: takes up queued changes as `koma worker` does.
    let stop_worker = Arc::new(AtomicBool::new(false));
    let worker = {
        let (paths, stop) = (paths.clone(), Arc::clone(&stop_worker));
        std::thread::spawn(move || {
            let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
            while !stop.load(Ordering::Relaxed) {
                koma::organise::run(&paths.library, &paths.data, &mut c);
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let (addr, stop, server) = start_server(&paths).await;
    let p2 = paths.clone();
    tokio::task::spawn_blocking(move || {
        let (lib, data) = (&p2.library, &p2.data);
        assert_eq!(request(addr, "mirelle", "GET", "/", b"").status, 200); // the admin
        let existing = fs::read(lib.join("manga/Starfall/Starfall 01.cbz")).unwrap();
        let mut damaged = cbz_bytes(5);
        let at = damaged.len() / 3;
        damaged[at] ^= 0xFF;
        let page = |n| koma_fixtures::png::page(40, 60, n);
        let cb7 =
            koma_fixtures::sevenz::write(&[("01.png", &page(31)), ("02.png", &page(32))], true);

        // The plan, with the names things land with.
        let items = [
            ("Starfall Academy/ch 1/001.png", 10),
            ("Starfall Academy/ch 1/002.png", 10),
            ("Starfall Academy/sa_c02.cb7", 10),
            ("Starfall/Starfall 01.cbz", 10),
            ("Starfall/Starfall 02.cbz", 10),
            ("Starfall/Starfall 03.cbz", 10),
            ("Starfall/old.cbr", 10),
        ];
        let items: Vec<Value> = items
            .iter()
            .map(|(p, s)| json!({"path": p, "size": s}))
            .collect();
        let r = request(
            addr,
            "mirelle",
            "POST",
            "/up/plan",
            json!({"library": "manga", "items": items, "today": "2026-11-28"})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(r.status, 200, "{}", r.json);
        let jobs = r.json["jobs"].as_array().unwrap().clone();
        let landing: Vec<(String, String, bool)> = jobs
            .iter()
            .map(|j| {
                (
                    j["path"].as_str().unwrap().to_owned(),
                    j["to"].as_str().unwrap_or("-").to_owned(),
                    j["new"].as_bool().unwrap(),
                )
            })
            .collect();
        let want = |p: &str, to: &str, new: bool| (p.to_owned(), to.to_owned(), new);
        assert_eq!(
            landing,
            [
                // Padded to the series' largest number, 3: one digit.
                want("manga/Starfall", "Starfall - 1.cbz", false),
                want("manga/Starfall", "Starfall - 2.cbz", false),
                want("manga/Starfall", "Starfall - 3.cbz", false),
                want(
                    "manga/Starfall Academy",
                    "Starfall Academy - Ch. 1.cbz",
                    true
                ),
                want(
                    "manga/Starfall Academy",
                    "Starfall Academy - Ch. 2.cbz",
                    true
                ),
            ],
            "{}",
            r.json
        );
        assert_eq!(r.json["refused"][0]["file"], json!(6));

        // Every file sent, the pack built as its images arrive.
        let mut sent = Vec::new();
        for j in &jobs {
            let id = if j["kind"] == "pack" {
                let r = request(
                    addr,
                    "mirelle",
                    "POST",
                    "/up",
                    json!({"library": "manga", "kind": "pack", "name": j["name"]})
                        .to_string()
                        .as_bytes(),
                );
                let id = r.json["id"].as_str().unwrap().to_owned();
                for (k, n) in j["pages"].as_array().unwrap().iter().enumerate() {
                    let name = items[n.as_u64().unwrap() as usize]["path"]
                        .as_str()
                        .unwrap()
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_owned();
                    let r = request(
                        addr,
                        "mirelle",
                        "PUT",
                        &format!("/up/{id}/m?name={name}"),
                        &page(40 + k as u32),
                    );
                    assert_eq!(r.status, 200, "{}", r.json);
                }
                assert_eq!(
                    request(addr, "mirelle", "POST", &format!("/up/{id}/done"), b"").status,
                    200
                );
                id
            } else {
                let bytes = match j["name"].as_str().unwrap() {
                    "sa_c02.cb7" => cb7.clone(),
                    "Starfall 01.cbz" => existing.clone(),
                    "Starfall 02.cbz" => cbz_bytes(4),
                    "Starfall 03.cbz" => damaged.clone(),
                    other => panic!("{other}"),
                };
                upload_file(addr, "manga", j["name"].as_str().unwrap(), &bytes)
            };
            sent.push(json!({"upload": id, "path": j["path"], "name": j["to"]}));
        }
        let r = request(
            addr,
            "mirelle",
            "POST",
            "/up/commit",
            json!({"library": "manga", "batch": "d2", "jobs": sent,
                   "failed": [{"name": "Starfall 04.cbz", "why": "Network error"}], "ignored": 0})
            .to_string()
            .as_bytes(),
        );
        assert_eq!(r.status, 200, "{}", r.json);
        let op = r.json["op"].as_i64().unwrap();
        let db = data.join("koma.db");
        let state = (0..200)
            .find_map(|_| {
                let s = Catalog::open_read_only(&db).unwrap().op_state(op).unwrap();
                match s {
                    Some(s) if s.state == "done" || s.state == "failed" => Some(s),
                    _ => {
                        std::thread::sleep(Duration::from_millis(50));
                        None
                    }
                }
            })
            .expect("the worker filed the drop");
        assert_eq!(state.state, "done", "{}", state.message);
        assert_eq!(
            state.message,
            "4 out of 6 added without problems (1 already there) · 2 failed"
        );

        // Where everything went: CB7 as a stored CBZ, the pack, the new volume.
        for p in [
            "manga/Starfall Academy/Starfall Academy - Ch. 1.cbz",
            "manga/Starfall Academy/Starfall Academy - Ch. 2.cbz",
            "manga/Starfall/Starfall - 2.cbz",
        ] {
            assert!(lib.join(p).is_file(), "{p}");
        }
        assert!(
            !lib.join("manga/Starfall/Starfall - 1.cbz").exists(),
            "already there"
        );
        assert!(
            !lib.join("manga/Starfall/Starfall - 3.cbz").exists(),
            "damaged"
        );
        let c = Catalog::open_read_only(&db).unwrap();
        let academy = c
            .series_with_counts()
            .unwrap()
            .into_iter()
            .find(|(s, _)| s.name == "Starfall Academy")
            .expect("indexed at once");
        assert_eq!(academy.1, 2);
        let log = fs::read_to_string(koma::uplog::path(data, koma::accounts::now())).unwrap();
        assert!(
            log.contains("✗ Starfall 03.cbz: This archive is damaged"),
            "{log}"
        );
        assert!(
            log.contains("✗ Starfall 04.cbz: Network error (in the browser)"),
            "{log}"
        );
        // The pages: the drop zone with its script, the log for whom it concerns.
        assert_eq!(request(addr, "bram", "GET", "/", b"").status, 200); // a reader
        let (status, add) = page_text(addr, "mirelle", "/add?l=manga");
        assert_eq!(status, 200);
        assert!(add.contains("data-add data-library=\"manga\""), "{add}");
        let js = add
            .split("<script defer src=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(js.starts_with("/a/upload."), "{js}");
        assert_eq!(page_text(addr, "mirelle", js).0, 200);
        let (status, add) = page_text(addr, "mirelle", &format!("/add?s={}", academy.0.id));
        assert_eq!(status, 200);
        assert!(
            add.contains(&format!("data-series={}", academy.0.id)),
            "{add}"
        );
        assert!(add.contains("add to · <b>~/Starfall Academy</b>"), "{add}");
        assert_eq!(page_text(addr, "bram", "/add?l=manga").0, 403);
        assert_eq!(page_text(addr, "mirelle", "/add?l=nowhere").0, 404);
        let (status, log) = page_text(addr, "mirelle", "/uploads");
        assert_eq!(status, 200);
        assert!(log.contains("4 out of 6 added without problems"), "{log}");
        assert!(
            log.contains("✗ Starfall 03.cbz: This archive is damaged"),
            "{log}"
        );
        let (status, log) = page_text(addr, "bram", "/uploads");
        assert_eq!(status, 200);
        assert!(!log.contains("added without problems") && log.contains("No drops this month"));
        assert_eq!(page_text(addr, "mirelle", "/uploads?m=../../x").0, 200);

        let staged: Vec<_> = fs::read_dir(data.join("uploads"))
            .unwrap()
            .flatten()
            .collect();
        assert!(staged.is_empty(), "nothing left waiting: {staged:?}");

        // Undo: the added books go to the trash.
        let r = request(addr, "mirelle", "POST", &format!("/jobs/{op}/undo"), b"");
        assert_eq!(r.status, 303);
        for _ in 0..200 {
            if !lib.join("manga/Starfall/Starfall - 2.cbz").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!lib.join("manga/Starfall/Starfall - 2.cbz").exists());
        assert!(
            !lib.join("manga/Starfall Academy/Starfall Academy - Ch. 1.cbz")
                .exists()
        );
        assert!(
            lib.join("manga/Starfall/Starfall 01.cbz").exists(),
            "the old book stays"
        );
    })
    .await
    .unwrap();
    stop_worker.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    let _ = stop.send(());
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(root);
}
