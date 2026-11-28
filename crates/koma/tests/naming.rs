//! M7's exit check: renaming a made-up library of 500 messy files to the
//! naming standard and undoing it gives back byte-identical paths and
//! contents. Also: padding to the series' largest, the fix-up queue, and the
//! cosplay tidy (automatic, undoable, not done again once undone).

use std::collections::BTreeMap;
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

    fn op(&self) -> i64 {
        let l = self.location();
        l.rsplit("done=")
            .next()
            .and_then(|n| n.parse().ok())
            .expect(&l)
    }
}

fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> Response {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nRemote-User: mirelle\r\nOrigin: http://localhost\r\n\
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

fn cbz(path: &Path, seed: u32) {
    let mut z = ZipWriter::new();
    z.add(
        "01.png",
        &koma_fixtures::png::page(8, 12, seed),
        Compression::Stored,
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, z.finish()).unwrap();
}

const SERIES: [&str; 5] = [
    "Starfall Academy",
    "Moonforge Chronicle",
    "Harbor Lights",
    "Quiet Orbit",
    "Lone Comet Tales",
];

/// Five series of 100 chapters, named five messy ways each.
fn messy_library(lib: &Path) {
    for (si, series) in SERIES.iter().enumerate() {
        let lower = series.to_lowercase().replace(' ', "_");
        for n in 1..=100u32 {
            let name = match n % 5 {
                0 => format!("{lower}_c{n}_[eng].cbz"),
                1 => format!("{series} ch{n}.cbz"),
                2 => format!("{series} - Chapter {n}.CBZ"),
                3 => format!("[Moonforge (Mirelle)] {series} {n:03}.cbz"),
                _ => format!("{series}_#{n}.cbz"),
            };
            cbz(
                &lib.join("manga").join(series).join(name),
                si as u32 * 1000 + n,
            );
        }
    }
}

/// Every file under `root`, by its path, with a hash of its bytes.
fn snapshot(root: &Path) -> BTreeMap<String, blake3::Hash> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, blake3::hash(&fs::read(&p).unwrap()));
            }
        }
    }
    out
}

/// Every folder under `root` (empty ones count too).
fn folders(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                stack.push(p);
            }
        }
    }
    out.sort();
    out
}

struct Server {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    worker_stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

async fn start(paths: &Paths) -> Server {
    let worker_stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (paths, stop) = (paths.clone(), Arc::clone(&worker_stop));
        std::thread::spawn(move || {
            let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
            while !stop.load(Ordering::Relaxed) {
                koma::organise::run(&paths.library, &paths.data, &mut c);
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let state = AppState::with_auth(
        paths,
        koma::auth::Config::parse(Some("Remote-User"), Some("127.0.0.1")).unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(serve(listener, state, async {
        let _ = stopped.await;
    }));
    Server {
        addr,
        stop: Some(stop),
        task,
        worker_stop,
        worker: Some(worker),
    }
}

impl Server {
    async fn stop(mut self) {
        self.worker_stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
        self.stop.take().unwrap().send(()).unwrap();
        (&mut self.task).await.unwrap().unwrap();
    }
}

fn scan(paths: &Paths) {
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::scan::scan(&paths.library, &paths.data, &mut c).unwrap();
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("koma-naming-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("data")).unwrap();
    root
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_hundred_messy_files_renamed_and_undone() {
    let root = temp_root("exit");
    let paths = Paths {
        library: root.join("library"),
        data: root.join("data"),
    };
    messy_library(&paths.library);
    scan(&paths);
    let before = snapshot(&paths.library);
    let before_folders = folders(&paths.library);
    assert_eq!(before.len(), 500);
    let server = start(&paths).await;
    let addr = server.addr;
    let lib = paths.library.clone();

    let renamed = tokio::task::spawn_blocking(move || {
        // The plan of the whole library: a dry run.
        let plan = request(addr, "GET", "/naming?l=manga", "").body;
        assert!(plan.contains("<b>500</b> to rename"), "{}", &plan[plan.find("this plan").unwrap_or(0)..][..300]);
        // Author, circle and language come from the series' other books.
        assert!(plan.contains(
            "<div class=to>manga/Starfall Academy/[Moonforge (Mirelle)] Starfall Academy - Ch. 007 [EN].cbz</div>"
        ));
        assert!(
            plan.contains("[Moonforge (Mirelle)] Harbor Lights - 003 [EN].cbz"),
            "kind unknown: the number alone"
        );
        assert!(plan.contains(
            "quiet_orbit_c5_[eng].cbz</div><div class=to>manga/Quiet Orbit/[Moonforge (Mirelle)] Quiet Orbit - Ch. 005 [EN].cbz"
        ));
        assert_eq!(snapshot(&lib).len(), 500, "a plan changes nothing");
        // Apply every row.
        let ticked: Vec<String> = plan
            .match_indices("name=b value=")
            .map(|(i, _)| {
                let rest = &plan[i + "name=b value=".len()..];
                format!("b={}", rest.split(|c: char| !c.is_ascii_digit()).next().unwrap())
            })
            .collect();
        assert_eq!(ticked.len(), 500);
        let r = request(addr, "POST", "/naming", &format!("l=manga&{}", ticked.join("&")));
        assert_eq!(r.status, 303);
        r.op()
    })
    .await
    .unwrap();

    // Every file renamed, contents untouched; a rescan finds the same books.
    let after = snapshot(&paths.library);
    assert_eq!(after.len(), 500);
    assert!(after.contains_key(
        "manga/Starfall Academy/[Moonforge (Mirelle)] Starfall Academy - Ch. 100 [EN].cbz"
    ));
    assert!(
        after
            .contains_key("manga/Harbor Lights/[Moonforge (Mirelle)] Harbor Lights - 003 [EN].cbz")
    );
    let mut a: Vec<String> = after.values().map(|h| h.to_hex().to_string()).collect();
    let mut b: Vec<String> = before.values().map(|h| h.to_hex().to_string()).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b, "the same bytes, under new names");
    assert!(after.keys().all(|k| !before.contains_key(k)));
    scan(&paths);

    // Undone: byte-identical paths and contents.
    tokio::task::spawn_blocking(move || {
        let r = request(addr, "POST", &format!("/jobs/{renamed}/undo"), "");
        assert_eq!(r.status, 303);
        let jobs = request(addr, "GET", "/jobs", "").body;
        assert!(
            jobs.contains("Undo: Rename 500 books of Manga to the naming standard"),
            "{jobs}"
        );
    })
    .await
    .unwrap();
    assert_eq!(snapshot(&paths.library), before);
    assert_eq!(folders(&paths.library), before_folders);
    server.stop().await;
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix_ups_and_the_cosplay_tidy() {
    let root = temp_root("tidy");
    let paths = Paths {
        library: root.join("library"),
        data: root.join("data"),
    };
    let lib = paths.library.clone();
    // A series where two files would get one name, and one with no number.
    cbz(&lib.join("manga/Starfall/Starfall ch1.cbz"), 1);
    cbz(&lib.join("manga/Starfall/Starfall - Chapter 1.cbz"), 2);
    cbz(&lib.join("manga/Starfall/Starfall ch2.cbz"), 3);
    cbz(&lib.join("manga/Starfall/extras.cbz"), 4);
    // A cosplay library: two sets to tidy, one with no cosplayer.
    cbz(
        &lib.join("cosplay/[Lumen Studio] Mirelle - Starfall Bunny - Bonus.zip"),
        5,
    );
    fs::create_dir_all(lib.join("cosplay/Solene - Harbor Night")).unwrap();
    fs::write(
        lib.join("cosplay/Solene - Harbor Night/01.png"),
        koma_fixtures::png::page(8, 12, 6),
    )
    .unwrap();
    cbz(&lib.join("cosplay/Summer Beach.zip"), 7);
    scan(&paths);
    let server = start(&paths).await;
    let addr = server.addr;
    let p2 = paths.clone();

    tokio::task::spawn_blocking(move || {
        let lib = &p2.library;
        let plan = request(addr, "GET", "/naming?l=manga", "").body;
        assert!(
            plan.contains("another file would also be Starfall - Ch. 1.cbz"),
            "{plan}"
        );
        assert!(plan.contains("no number, and not the series&#39; only book"));
        assert!(
            plan.contains("<div class=to>manga/Starfall/Starfall - Ch. 2.cbz</div>"),
            "padded to the largest: 1 digit"
        );

        // The fix-up queue: extras becomes an extra chapter 0 by hand.
        let queue = request(addr, "GET", "/fixup", "").body;
        assert!(
            queue.contains("manga/Starfall/extras.cbz")
                && queue.contains("cosplay/Summer Beach.zip"),
            "{queue}"
        );
        assert!(
            queue.contains("name=set value=\"Summer Beach\""),
            "the set without its extension"
        );
        let id = |name: &str| -> String {
            let at = queue.find(name).unwrap();
            let rest = &queue[at..];
            let v = rest.find("name=b value=").unwrap() + "name=b value=".len();
            rest[v..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .to_owned()
        };
        let extras = id("manga/Starfall/extras.cbz");
        let r = request(
            addr,
            "POST",
            "/fixup",
            &format!("b={extras}&kind_of=manga&kind=chapter&number=0.5&author=Mirelle&circle="),
        );
        assert_eq!(r.status, 303, "{}", r.body);
        assert!(
            lib.join("manga/Starfall/[Mirelle] - Starfall - Ch. 0.5.cbz")
                .exists()
        );
        // Its answers are its details now: the plan agrees.
        let plan = request(addr, "GET", "/naming?l=manga", "").body;
        assert!(!plan.contains("extras.cbz"));
        // Keep a name as it is.
        let ch1 = id("manga/Starfall/Starfall ch1.cbz");
        request(addr, "POST", "/fixup", &format!("b={ch1}&action=keep"));
        assert!(
            !request(addr, "GET", "/fixup", "")
                .body
                .contains("Starfall ch1.cbz")
        );

        // The cosplay set with no cosplayer, named in the fix-up queue, is
        // placed by the tidy at once.
        let beach = id("cosplay/Summer Beach.zip");
        let r = request(
            addr,
            "POST",
            "/fixup",
            &format!(
                "b={beach}&kind_of=cosplay&cosplayer=Solene&set=Summer+Beach&variant=&circle="
            ),
        );
        assert_eq!(r.status, 303);
    })
    .await
    .unwrap();
    // The worker's tidy (after each scan, and after changes): the two
    // named sets, and the beach set once named.
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    koma::organise::tidy(&paths.library, &paths.data, &mut c);
    assert!(
        lib.join(
            "cosplay/Mirelle/Starfall Bunny/[Lumen Studio] Mirelle - Starfall Bunny - Bonus.zip"
        )
        .exists()
    );
    assert!(
        lib.join("cosplay/Solene/Harbor Night/Solene - Harbor Night/01.png")
            .exists()
    );
    assert!(
        lib.join("cosplay/Solene/Summer Beach/Solene - Summer Beach.zip")
            .exists()
    );
    drop(c);

    // The tidy is on /jobs, and undone it is not done again.
    tokio::task::spawn_blocking(move || {
        let jobs = request(addr, "GET", "/jobs", "").body;
        assert!(jobs.contains("Tidy cosplay"), "{jobs}");
        let at = jobs.find("formaction=/jobs/-").unwrap() + "formaction=/jobs/".len();
        let tidy: String = jobs[at..].split('/').next().unwrap().to_owned();
        let r = request(addr, "POST", &format!("/jobs/{tidy}/undo"), "");
        assert_eq!(r.status, 303);
    })
    .await
    .unwrap();
    // The newest tidy (the beach set, placed once it was named) is undone.
    assert!(
        lib.join("cosplay/Solene - Summer Beach.zip").exists(),
        "a tidy undone puts the set back"
    );
    let mut c = Catalog::open(&paths.data.join("koma.db")).unwrap();
    let snap = snapshot(&lib);
    koma::organise::tidy(&paths.library, &paths.data, &mut c);
    assert_eq!(snapshot(&lib), snap, "and is not done again");
    drop(c);
    server.stop().await;
    let _ = fs::remove_dir_all(&root);
}
