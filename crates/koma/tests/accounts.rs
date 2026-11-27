//! M5's exit check: a reader gets a refusal for every write action, and
//! two accounts never see each other's libraries. Also: the first start,
//! sign-in links, sessions, the proxy, library settings applied by the
//! worker, and an EPUB opening at the last position on another device.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use koma::catalog::Catalog;
use koma::cli::Paths;
use koma::serve::{AppState, serve};
use koma_fixtures::epub::Epub;
use koma_fixtures::zip::{Compression, ZipWriter};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct Response {
    status: u16,
    headers: String,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }

    /// The `koma` cookie this response sets, as a `Cookie` header value.
    fn cookie(&self) -> String {
        let set = self.header("set-cookie").expect("a cookie");
        set.split(';').next().unwrap().to_owned()
    }
}

/// One request. `cookie` is a `Cookie` value or empty; `extra` more header
/// lines; a body is sent as a form.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &str,
) -> Response {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let cookie = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: koma.local:8080\r\n{cookie}{extra}\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         User-Agent: Mozilla/5.0 (X11; Linux x86_64) Firefox/140.0\r\nConnection: close\r\n\r\n{body}",
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

fn get(addr: SocketAddr, path: &str, cookie: &str) -> Response {
    request(addr, "GET", path, cookie, "", "")
}

fn post(addr: SocketAddr, path: &str, cookie: &str, body: &str) -> Response {
    request(
        addr,
        "POST",
        path,
        cookie,
        "Origin: http://koma.local:8080\r\n",
        body,
    )
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
/// manga/Starfall/Starfall v01.cbz            shared
/// books/Saltwind/Saltwind 01.epub            shared, four chapters
/// ~mirelle/sketchbooks/Moonforge/…cbz        mirelle's alone
/// ~bram/notes/Harbor Lights/…cbz             bram's alone
/// ~bram/Quiet Orbit/…cbz                     bram's alone, straight in his folder
/// ```
fn library(root: &Path) -> PathBuf {
    let lib = root.join("library");
    cbz(&lib.join("manga/Starfall/Starfall v01.cbz"), 1);
    cbz(
        &lib.join("~mirelle/sketchbooks/Moonforge/Moonforge v01.cbz"),
        2,
    );
    cbz(
        &lib.join("~bram/notes/Harbor Lights/Harbor Lights v01.cbz"),
        3,
    );
    cbz(&lib.join("~bram/Quiet Orbit/Quiet Orbit v01.cbz"), 4);
    let book = Epub::new("Saltwind", "Sela Marr")
        .chapter("Part 1", "<p>Tide 1.</p><p>Wind 1.</p>")
        .chapter("Part 2", "<p>Tide 2.</p><p>Wind 2.</p>")
        .chapter("Part 3", "<p>Tide 3.</p><p>Wind 3.</p>")
        .chapter("Part 4", "<p>Tide 4.</p><p>Wind 4.</p>");
    let path = lib.join("books/Saltwind/Saltwind 01.epub");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, book.build()).unwrap();
    lib
}

/// Every series and book id by name.
fn ids(db: &Path) -> (HashMap<String, i64>, HashMap<String, i64>) {
    let c = Catalog::open(db).unwrap();
    let mut series = HashMap::new();
    let mut books = HashMap::new();
    for (s, _) in c.series_with_counts().unwrap() {
        for b in c.books(s.id).unwrap() {
            books.insert(s.name.clone(), b.id);
        }
        series.insert(s.name, s.id);
    }
    (series, books)
}

/// The first sign-in link (`/join/…`) in a page or text, up to the next `<`.
fn link_in(html: &str) -> String {
    let at = html.find("/join/").expect("a sign-in link");
    html[at..at + html[at..].find('<').unwrap()].to_owned()
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("koma-accounts-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    root
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_are_refused_and_accounts_stay_apart() {
    let root = temp_root("exit");
    let paths = Paths {
        library: library(&root),
        data: root.join("data"),
    };
    let db = paths.data.join("koma.db");
    {
        let mut c = Catalog::open(&db).unwrap();
        koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    }
    let (series, books) = ids(&db);
    let state = AppState::new(&paths);
    let code = koma::auth::setup_code(&state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, state, async {
        let _ = stopped.await;
    }));
    let bin = env!("CARGO_BIN_EXE_koma");
    let paths2 = paths.clone();

    tokio::task::spawn_blocking(move || {
        let paths = paths2;
        // First start: everything leads to /setup, which needs the code.
        let r = get(addr, "/", "");
        assert_eq!(
            (r.status, r.header("location").as_deref()),
            (303, Some("/setup"))
        );
        assert_eq!(get(addr, "/b/1", "").status, 401);
        assert_eq!(get(addr, "/setup", "").status, 200);
        let form = |code: &str| {
            format!("code={code}&name=mirelle&password=starfall+moon&again=starfall+moon")
        };
        assert_eq!(post(addr, "/setup", "", &form("NOPE-NOPE")).status, 400);
        let r = post(addr, "/setup", "", &form(&code.to_lowercase()));
        assert_eq!(r.status, 303, "{}", r.body);
        let mirelle = r.cookie();
        assert!(r.header("set-cookie").unwrap().contains("HttpOnly"));
        // Only once.
        assert_eq!(get(addr, "/setup", "").status, 303);
        let home = get(addr, "/", &mirelle);
        assert_eq!(home.status, 200);
        assert!(
            home.body
                .contains("<span class=\"role admin\">admin</span>")
        );

        // The admin adds bram, a reader; bram chooses a password through the link.
        let r = post(addr, "/users/add", &mirelle, "name=bram&role=reader");
        assert_eq!(r.status, 200, "{}", r.body);
        let link = link_in(&r.body);
        assert!(r.body.contains("http://koma.local:8080/join/"));
        assert_eq!(get(addr, &link, "").status, 200);
        assert_eq!(
            post(addr, &link, "", "password=short&again=short").status,
            400
        );
        let r = post(
            addr,
            &link,
            "",
            "password=harbor+lights&again=harbor+lights",
        );
        assert_eq!(r.status, 303);
        let bram = r.cookie();
        assert_eq!(get(addr, &link, "").status, 404, "a link works once");

        // Two accounts never see each other's libraries.
        let shelf = |cookie: &str| {
            let mut all = get(addr, "/", cookie).body;
            for l in [
                "manga",
                "~mirelle/sketchbooks",
                "~bram/notes",
                "~bram",
                "books",
            ] {
                all.push_str(&get(addr, &format!("/l/{}", enc(l)), cookie).body);
            }
            all
        };
        let m = shelf(&mirelle);
        let b = shelf(&bram);
        assert!(m.contains("Moonforge") && m.contains("Starfall") && m.contains("// only yours"));
        assert!(!m.contains("Harbor Lights") && !m.contains("Quiet Orbit") && !m.contains("Notes"));
        assert!(b.contains("Harbor Lights") && b.contains("Quiet Orbit") && b.contains("Starfall"));
        assert!(!b.contains("Moonforge") && !b.contains("Sketchbooks"));
        for (who, cookie, theirs) in [
            ("bram", &bram, ["Moonforge"].as_slice()),
            (
                "mirelle",
                &mirelle,
                ["Harbor Lights", "Quiet Orbit"].as_slice(),
            ),
        ] {
            for name in theirs {
                let (s, book) = (series[*name], books[*name]);
                for path in [
                    format!("/s/{s}"),
                    format!("/r/{book}/1"),
                    format!("/b/{book}"),
                    format!("/d/{book}"),
                ] {
                    assert_eq!(get(addr, &path, cookie).status, 404, "{who}: {path}");
                }
                let found = get(addr, &format!("/search?q={}", enc(name)), cookie).body;
                assert!(!found.contains(&format!("/s/{s}\"")), "{who} finds {name}");
                assert!(
                    get(addr, "/settings", cookie).body.contains("only yours"),
                    "{who} edits their own"
                );
            }
        }
        // Each sees their own.
        assert_eq!(
            get(addr, &format!("/s/{}", series["Moonforge"]), &mirelle).status,
            200
        );
        assert_eq!(
            get(addr, &format!("/d/{}", books["Quiet Orbit"]), &bram).status,
            200
        );

        // A reader gets a refusal for every write action.
        for (path, body) in [
            (
                "/settings",
                "folder=manga&name=Mine&kind=manga&dir=rtl&colour=amber",
            ),
            ("/users/add", "name=tamsin&role=curator"),
            ("/users/role", "name=bram&role=curator"),
            ("/users/link", "name=mirelle"),
            ("/users/remove", "name=mirelle"),
            ("/users/handover", "to=bram&password=harbor+lights"),
        ] {
            let r = post(addr, path, &bram, body);
            assert_eq!(r.status, 403, "{path}: {}", r.body);
            assert!(r.body.contains("Only the admin"), "{path}");
        }
        assert_eq!(get(addr, "/users", &bram).status, 403);
        // But bram changes his own library's settings, and his own account.
        let r = post(
            addr,
            "/settings",
            &bram,
            "folder=~bram%2Fnotes&name=Field+notes&kind=comics&dir=ltr&colour=teal",
        );
        assert_eq!(r.status, 303);
        // Nobody else's.
        let r = post(
            addr,
            "/settings",
            &mirelle,
            "folder=~bram%2Fnotes&name=Mine&kind=comics&dir=ltr&colour=teal",
        );
        assert_eq!(r.status, 404);

        // Writes need Koma-1's own pages, and an account.
        let r = request(
            addr,
            "POST",
            "/users/add",
            &mirelle,
            "Origin: https://evil.example\r\n",
            "name=eve&role=reader",
        );
        assert_eq!(r.status, 403);
        assert_eq!(
            post(addr, "/users/add", "", "name=eve&role=reader").status,
            401
        );

        // The admin's settings change is queued, then the worker writes it.
        let r = post(
            addr,
            "/settings",
            &mirelle,
            "folder=manga&name=Comics+shelf&kind=comics&dir=ltr&colour=rose",
        );
        assert_eq!(r.status, 303);
        assert!(
            get(addr, "/settings", &mirelle)
                .body
                .contains("<span class=pending>Queued</span>")
        );
        let scan = Command::new(bin)
            .args(["scan", "--library"])
            .arg(&paths.library)
            .arg("--data")
            .arg(&paths.data)
            .output()
            .unwrap();
        assert!(
            scan.status.success(),
            "{}",
            String::from_utf8_lossy(&scan.stderr)
        );
        let after = get(addr, "/", &mirelle).body;
        assert!(after.contains("Comics shelf") && !after.contains("Field notes"));
        assert!(get(addr, "/", &bram).body.contains("Field notes"));
        assert!(!get(addr, "/settings", &mirelle).body.contains("Queued"));

        // An EPUB opens at the last position on another device.
        let epub = books["Saltwind"];
        let r = post(
            addr,
            "/kept",
            &bram,
            &format!("epos%3A{epub}=3%3A1&pos%3A{epub}=%3Cscript%3E"),
        );
        assert_eq!(r.status, 204);
        let r = post(
            addr,
            "/signin",
            "",
            "name=bram&password=harbor+lights&next=%2F",
        );
        assert_eq!(r.status, 303);
        let phone = r.cookie();
        assert_ne!(phone, bram);
        let r = get(addr, &format!("/r/{epub}/1"), &phone);
        assert_eq!(r.header("location"), Some(format!("/e/{epub}/3")));
        let page = get(addr, &format!("/e/{epub}/3"), &phone).body;
        assert!(
            page.contains("data-kept=\"{&quot;epos:"),
            "{}",
            &page[..300]
        );
        assert!(page.contains("3:1"));
        assert!(!page.contains("script&gt;"), "only allowed values are kept");
        // Mirelle's position is her own.
        assert_eq!(
            get(addr, &format!("/r/{epub}/1"), &mirelle).header("location"),
            Some(format!("/e/{epub}/1"))
        );

        // A browser's positions from before accounts come along at sign-in, once.
        let starfall = books["Starfall"];
        let kept = enc(&format!("{{\"pos:{starfall}\":\"2\",\"theme\":\"x\"}}"));
        let r = post(
            addr,
            "/signin",
            "",
            &format!("name=mirelle&password=starfall+moon&kept={kept}"),
        );
        assert_eq!(r.header("clear-site-data").as_deref(), Some("\"storage\""));
        let laptop = r.cookie();
        let reader = get(addr, &format!("/r/{starfall}/1"), &laptop).body;
        assert!(
            reader.contains(&format!("pos:{starfall}&quot;:&quot;2")),
            "{}",
            &reader[..200]
        );

        // Sessions: the account page lists them; signing out ends one.
        let account = get(addr, "/account", &bram).body;
        assert!(account.contains("Firefox on Linux"));
        assert!(account.contains("· this one"));
        assert_eq!(post(addr, "/signout", &phone, "").status, 303);
        assert_eq!(get(addr, "/", &phone).status, 303);
        assert_eq!(get(addr, "/", &bram).status, 200);
        // A new password signs the other devices out.
        let r = post(
            addr,
            "/account/password",
            &bram,
            "current=harbor+lights&password=quiet+orbit+1&again=quiet+orbit+1",
        );
        assert_eq!(r.status, 303);
        assert_eq!(get(addr, "/", &bram).status, 200);

        // Wrong passwords: after five, the name waits.
        for _ in 0..5 {
            assert_eq!(
                post(addr, "/signin", "", "name=bram&password=nope").status,
                401
            );
        }
        let r = post(addr, "/signin", "", "name=bram&password=quiet+orbit+1");
        assert_eq!(r.status, 401);
        assert!(r.body.contains("try again in"));

        // A forged cookie is nobody.
        assert_eq!(get(addr, "/", "koma=AAAA.BBBB").status, 303);

        // The admin hands over the role and becomes a curator.
        let r = post(
            addr,
            "/users/handover",
            &mirelle,
            "to=bram&password=starfall+moon",
        );
        assert_eq!(r.status, 303);
        assert_eq!(get(addr, "/users", &mirelle).status, 403);
        assert!(
            get(addr, "/account", &mirelle)
                .body
                .contains("<span class=\"role\">curator</span>")
        );

        // A sign-in link from the shell.
        let out = Command::new(bin)
            .args(["account", "link", "mirelle", "--data"])
            .arg(&paths.data)
            .output()
            .unwrap();
        assert!(out.status.success());
        let printed = String::from_utf8(out.stdout).unwrap();
        let link = link_in(&printed.replace('\n', "<"));
        assert!(
            get(addr, &link, "")
                .body
                .contains("Choose a new password, mirelle")
        );
    })
    .await
    .unwrap();

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_proxy_is_believed_only_from_its_address() {
    let root = temp_root("proxy");
    let paths = Paths {
        library: library(&root),
        data: root.join("data"),
    };
    {
        let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
        koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
    }
    let start = |proxy: &str| {
        let state = AppState::with_auth(
            &paths,
            koma::auth::Config::parse(Some("Remote-User"), Some(proxy)).unwrap(),
        );
        async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (stop, stopped) = oneshot::channel::<()>();
            let server = tokio::spawn(serve(listener, state, async {
                let _ = stopped.await;
            }));
            (addr, stop, server)
        }
    };

    // From an address that is not the proxy's, the header is nothing.
    let (addr, stop, server) = start("10.0.0.2").await;
    tokio::task::spawn_blocking(move || {
        let r = request(addr, "GET", "/", "", "Remote-User: mirelle\r\n", "");
        assert_eq!(r.status, 303);
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();

    let (addr, stop, server) = start("127.0.0.0/8").await;
    tokio::task::spawn_blocking(move || {
        // The first name is the admin; the next a reader.
        let r = request(addr, "GET", "/", "", "Remote-User: Mirelle\r\n", "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("<span class=\"role admin\">admin</span>"));
        assert!(r.body.contains("Sketchbooks"));
        let r = request(addr, "GET", "/", "", "Remote-User: bram\r\n", "");
        assert!(r.body.contains("<span class=\"role\">reader</span>"));
        assert!(!r.body.contains("Sketchbooks") && r.body.contains("Notes"));
        let r = request(addr, "GET", "/", "", "Remote-User: not a name\r\n", "");
        assert_eq!(r.status, 403);
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(&root);
}
