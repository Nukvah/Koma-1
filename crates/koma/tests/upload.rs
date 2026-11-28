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
fn received(addr: SocketAddr, user: &str, id: &str) -> u64 {
    for _ in 0..400 {
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
