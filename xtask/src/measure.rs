//! The measurements behind each budget.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::budgets::Budgets;
use crate::docker::{self, Container};
use crate::http;
use crate::report::Report;

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

/// Matches the target hardware: 2 CPUs and 1 GB of memory.
const LIMITS: [&str; 4] = ["--cpus", "2", "--memory", "1g"];
/// Matches the hardening the manifesto asks of a deployment.
const HARDENING: [&str; 5] = [
    "--read-only",
    "--cap-drop",
    "ALL",
    "--security-opt",
    "no-new-privileges",
];

const COLD_START_RUNS: usize = 5;
const LOAD_CONNECTIONS: usize = 4;
const LOAD_REQUESTS: usize = 1000;
const LOAD_WARMUP: usize = 100;

pub fn assets(root: &Path, budgets: &Budgets, report: &mut Report) -> Result<(), String> {
    for group in &budgets.assets {
        let dir = root.join(&group.dir);
        let (bytes, files) =
            asset_bytes(&dir, &group.suffix).map_err(|e| format!("{}: {e}", dir.display()))?;
        let note = match files {
            0 => "no files yet".to_owned(),
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        report.budget(&group.name, bytes as f64 / KIB, group.max_kib, "KiB", &note);
    }
    Ok(())
}

/// Total size and count of files under `dir` ending in `suffix`. A missing
/// directory counts as empty.
fn asset_bytes(dir: &Path, suffix: &str) -> io::Result<(u64, usize)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let (mut bytes, mut files) = (0, 0);
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let (b, f) = asset_bytes(&entry.path(), suffix)?;
            bytes += b;
            files += f;
        } else if entry.file_name().to_string_lossy().ends_with(suffix) {
            bytes += entry.metadata()?.len();
            files += 1;
        }
    }
    Ok((bytes, files))
}

pub fn image(image: &str, budgets: &Budgets, report: &mut Report) -> Result<(), String> {
    let size: u64 = docker::inspect(image, "{{.Size}}")?
        .parse()
        .map_err(|e| format!("image size: {e}"))?;
    report.budget(
        "Container image",
        size as f64 / MIB,
        budgets.image.max_mib,
        "MiB",
        "",
    );

    let user = docker::inspect(image, "{{.Config.User}}")?;
    let root = user.is_empty() || user == "root" || user == "0" || user.starts_with("0:");
    report.check("Image runs as non-root", !root, format!("user {user:?}"));

    eprintln!("measuring cold start ({COLD_START_RUNS} runs)...");
    let binary = extract_binary(image)?;
    let mut starts = (0..COLD_START_RUNS)
        .map(|_| cold_start(&binary))
        .collect::<Result<Vec<_>, _>>()?;
    let _ = fs::remove_dir_all(binary.parent().unwrap_or(&binary));
    starts.sort();
    let median = starts[starts.len() / 2];
    report.budget(
        "Cold start (median)",
        ms(median),
        budgets.latency.cold_start_ms,
        "ms",
        &format!("max {:.1} ms", ms(starts[starts.len() - 1])),
    );

    // One made-up library and data folder, shared: the worker scans into the
    // data folder, then serve reads it, so the pages measured are real ones.
    let shared = Shared::create()?;
    let measured =
        worker_container(image, &shared, budgets, report).and_then(|scanned| match scanned {
            true => serve_container(image, &shared, budgets, report),
            false => Ok(()),
        });
    shared.remove();
    measured?;

    let scrub = docker::run(&["run", "--rm", image, "scrub"]);
    report.check(
        "koma scrub exits cleanly",
        scrub.is_ok(),
        scrub.err().unwrap_or_else(|| "exit 0".into()),
    );
    Ok(())
}

/// The test library and the data folder that the worker and serve containers share.
struct Shared {
    root: PathBuf,
    books: usize,
    /// `uid:gid` of the host user. The containers run as that user so the
    /// host can read and delete what they write.
    user: String,
    library: String,
    data: String,
}

impl Shared {
    fn create() -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;

        let root = std::env::temp_dir().join(format!("koma-budgets-shared-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let books = koma_fixtures::library::write(&root.join("library"))
            .map_err(|e| e.to_string())?
            .len();
        fs::create_dir_all(root.join("data")).map_err(|e| e.to_string())?;
        let meta = fs::metadata(&root).map_err(|e| e.to_string())?;
        Ok(Self {
            books,
            user: format!("{}:{}", meta.uid(), meta.gid()),
            library: format!("{}:/library:ro", root.join("library").display()),
            data: format!("{}:/data", root.join("data").display()),
            root,
        })
    }

    fn remove(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn serve_container(
    image: &str,
    shared: &Shared,
    budgets: &Budgets,
    report: &mut Report,
) -> Result<(), String> {
    eprintln!("starting koma serve on the scanned test library...");
    let port = free_port()?;
    let publish = format!("127.0.0.1:{port}:8080");
    let mut args = vec![
        "--publish",
        &publish,
        "--user",
        &shared.user,
        "--volume",
        &shared.library,
        "--volume",
        &shared.data,
    ];
    args.extend(LIMITS);
    args.extend(HARDENING);
    args.extend([image, "serve"]);
    let c = Container::start(&args)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    let up = wait_for(Duration::from_secs(15), || {
        http::status(addr, "/healthz", Duration::from_millis(200)) == Ok(200)
    });
    report.check(
        "GET /healthz answers 200",
        up,
        if up {
            "from the container"
        } else {
            "no answer within 15 s"
        },
    );
    if !up {
        return Ok(());
    }
    thread::sleep(Duration::from_millis(500));
    let idle = c.rss_bytes()?;

    // Follow links like a reader would: library, first series, first book.
    let reader = follow(addr, "/", "/s/")
        .and_then(|series| follow(addr, &series, "/r/"))
        .map_err(|e| format!("finding a reader page: {e}"))?;
    let image = follow(addr, &reader, "/p/").map_err(|e| format!("finding a page image: {e}"))?;
    for (name, path) in [("reader page", &reader), ("page image", &image)] {
        eprintln!(
            "timing {} requests of {path}...",
            LOAD_CONNECTIONS * LOAD_REQUESTS
        );
        let mut ttfb = load(addr, path)?;
        ttfb.sort();
        let p99 = ttfb[(ttfb.len() * 99).div_ceil(100) - 1];
        report.budget(
            &format!("TTFB p99 ({name})"),
            ms(p99),
            budgets.latency.ttfb_p99_ms,
            "ms",
            &format!("{}, {} requests", short(path), ttfb.len()),
        );
    }

    for page in &budgets.pages {
        let mut conn = http::Conn::open(addr, Duration::from_secs(2))?;
        let response = conn.get(&page.path)?;
        if response.status != 200 {
            return Err(format!("GET {}: status {}", page.path, response.status));
        }
        let name = format!("Page {}", page.path);
        report.budget(
            &name,
            response.body.len() as f64 / KIB,
            page.max_kib,
            "KiB",
            "",
        );
    }

    thread::sleep(Duration::from_secs(1));
    let after = c.rss_bytes()?;
    report.budget(
        "Idle RAM (serve)",
        idle.max(after) as f64 / MIB,
        budgets.memory.serve_idle_mib,
        "MiB",
        &format!(
            "{:.1} at start, {:.1} after load",
            idle as f64 / MIB,
            after as f64 / MIB
        ),
    );

    let healthy = wait_for(Duration::from_secs(15), || {
        docker::inspect(&c.id, "{{.State.Health.Status}}").as_deref() == Ok("healthy")
    });
    report.check(
        "Docker HEALTHCHECK healthy",
        healthy,
        if healthy {
            "koma healthcheck"
        } else {
            "not healthy within 15 s"
        },
    );

    stop_cleanly(&c, "koma serve stops on SIGTERM", report)
}

/// Runs `koma worker` on the made-up test library (read-only), waits for the
/// scan, then measures its idle memory. Returns whether the scan succeeded.
fn worker_container(
    image: &str,
    shared: &Shared,
    budgets: &Budgets,
    report: &mut Report,
) -> Result<bool, String> {
    eprintln!("starting koma worker on the test library...");
    let mut args = vec![
        "--user",
        &shared.user,
        "--volume",
        &shared.library,
        "--volume",
        &shared.data,
    ];
    args.extend(LIMITS);
    args.extend(HARDENING);
    args.extend([image, "worker"]);
    let c = Container::start(&args)?;

    let mut done = None;
    let scanned = wait_for(Duration::from_secs(30), || {
        done = docker_logs(&c.id)
            .lines()
            .find(|l| l.contains("\"scan: done\""))
            .map(str::to_owned);
        done.is_some()
    });
    let want = format!("\"indexed\":\"{}\"", shared.books);
    let ok = scanned
        && done
            .as_deref()
            .is_some_and(|l| l.contains(&want) && l.contains("\"failed\":\"0\""));
    report.check(
        "koma worker scans the test library",
        ok,
        match &done {
            Some(line) => format!(
                "{} books in {} ms",
                log_field(line, "indexed"),
                log_field(line, "ms")
            ),
            None => "no \"scan: done\" within 30 s".into(),
        },
    );

    thread::sleep(Duration::from_millis(500));
    let rss = c.rss_bytes()?;
    report.budget(
        "Idle RAM (worker)",
        rss as f64 / MIB,
        budgets.memory.worker_idle_mib,
        "MiB",
        "after scanning",
    );
    stop_cleanly(&c, "koma worker stops on SIGTERM", report)?;
    Ok(ok)
}

/// `path` cut to fit a report line: page image names are 64-digit hashes.
fn short(path: &str) -> String {
    match path.char_indices().nth(16) {
        Some((cut, _)) if path.len() > 24 => format!("{}…", &path[..cut]),
        _ => path.to_owned(),
    }
}

/// GETs `path` and returns the first link in it that starts with `prefix`.
fn follow(addr: SocketAddr, path: &str, prefix: &str) -> Result<String, String> {
    let mut conn = http::Conn::open(addr, Duration::from_secs(2))?;
    let response = conn.get(path)?;
    if response.status != 200 {
        return Err(format!("GET {path}: status {}", response.status));
    }
    first_link(&String::from_utf8_lossy(&response.body), prefix)
        .ok_or_else(|| format!("GET {path}: no link to {prefix}"))
}

/// The first `href` or `src` value in `html` that starts with `prefix`.
fn first_link(html: &str, prefix: &str) -> Option<String> {
    ["href=\"", "src=\""]
        .iter()
        .flat_map(|attr| html.match_indices(attr).map(move |(i, _)| i + attr.len()))
        .filter(|&start| html[start..].starts_with(prefix))
        .min()
        .and_then(|start| html[start..].split_once('"'))
        .map(|(link, _)| link.to_owned())
}

/// The value of `"key":"value"` in one of koma's JSON log lines.
fn log_field<'a>(line: &'a str, key: &str) -> &'a str {
    let pattern = format!("\"{key}\":\"");
    line.split_once(&pattern)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or("?", |(value, _)| value)
}

/// The container's stdout and stderr (koma logs to stderr).
fn docker_logs(id: &str) -> String {
    Command::new("docker")
        .args(["logs", id])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stderr).into_owned() + &String::from_utf8_lossy(&o.stdout)
        })
        .unwrap_or_default()
}

/// `docker stop` sends SIGTERM and kills after 10 s. A clean stop is quick and exits 0.
fn stop_cleanly(c: &Container, name: &str, report: &mut Report) -> Result<(), String> {
    let start = Instant::now();
    docker::run(&["stop", "--time", "10", &c.id])?;
    let took = start.elapsed();
    let code = docker::inspect(&c.id, "{{.State.ExitCode}}")?;
    let ok = code == "0" && took < Duration::from_secs(5);
    report.check(name, ok, format!("exit {code} after {:.0} ms", ms(took)));
    Ok(())
}

/// Copies `/koma` out of the image so its start-up can be timed without
/// Docker's own overhead.
fn extract_binary(image: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("koma-budgets-{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = docker::run(&["create", image])?;
    let target = dir.join("koma");
    let copied = docker::run(&["cp", &format!("{id}:/koma"), &target.to_string_lossy()]);
    let _ = docker::run(&["rm", &id]);
    copied?;
    Ok(target)
}

/// Time from spawning `koma serve` to its first successful health check.
fn cold_start(binary: &Path) -> Result<Duration, String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, free_port()?));
    let start = Instant::now();
    let mut child = Command::new(binary)
        .arg("serve")
        .env("KOMA_LISTEN", addr.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    let result = loop {
        if http::status(addr, "/healthz", Duration::from_millis(100)) == Ok(200) {
            break Ok(start.elapsed());
        }
        if let Ok(Some(status)) = child.try_wait() {
            break Err(format!("koma serve exited early: {status}"));
        }
        if start.elapsed() > Duration::from_secs(5) {
            break Err("koma serve did not answer within 5 s".into());
        }
        thread::sleep(Duration::from_millis(1));
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Time to first byte of `path` over several keep-alive connections at once.
fn load(addr: SocketAddr, path: &str) -> Result<Vec<Duration>, String> {
    thread::scope(|s| {
        let workers: Vec<_> = (0..LOAD_CONNECTIONS)
            .map(|_| {
                s.spawn(move || -> Result<Vec<Duration>, String> {
                    let mut conn = http::Conn::open(addr, Duration::from_secs(2))?;
                    let mut times = Vec::with_capacity(LOAD_REQUESTS);
                    for i in 0..LOAD_WARMUP + LOAD_REQUESTS {
                        let r = conn.get(path)?;
                        if r.status != 200 {
                            return Err(format!("GET {path}: status {}", r.status));
                        }
                        if i >= LOAD_WARMUP {
                            times.push(r.ttfb);
                        }
                    }
                    Ok(times)
                })
            })
            .collect();
        let mut all = Vec::new();
        for worker in workers {
            all.extend(worker.join().map_err(|_| "load thread panicked")??);
        }
        Ok(all)
    })
}

fn wait_for(limit: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_assets_by_suffix_recursively() {
        let dir = std::env::temp_dir().join(format!("koma-xtask-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("reader")).unwrap();
        fs::write(dir.join("koma.css"), [0u8; 300]).unwrap();
        fs::write(dir.join("reader/reader.css"), [0u8; 200]).unwrap();
        fs::write(dir.join("reader/reader.js"), [0u8; 999]).unwrap();
        assert_eq!(asset_bytes(&dir, ".css").unwrap(), (500, 2));
        assert_eq!(asset_bytes(&dir.join("reader"), ".js").unwrap(), (999, 1));
        assert_eq!(asset_bytes(&dir.join("missing"), ".js").unwrap(), (0, 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_asset_fails_the_budget() {
        let root = std::env::temp_dir().join(format!("koma-xtask-over-{}", std::process::id()));
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/big.css"), vec![b' '; 11 * 1024]).unwrap();
        let budgets = Budgets::parse(
            "[image]\nmax_mib = 1\n[memory]\nserve_idle_mib = 1\nworker_idle_mib = 1\n\
             [latency]\nttfb_p99_ms = 1\ncold_start_ms = 1\n\
             [[assets]]\nname = \"CSS\"\ndir = \"assets\"\nsuffix = \".css\"\nmax_kib = 10\n",
        )
        .unwrap();
        let mut report = Report::default();
        assets(&root, &budgets, &mut report).unwrap();
        assert!(!report.passed());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finds_the_first_link_with_a_prefix() {
        let html = r#"<a href="/">Library</a><a href="/r/4/2" rel=next>Next</a>
            <a href="/r/4/1"><img src="/p/ab12.png" alt="Page 1"></a>"#;
        assert_eq!(first_link(html, "/r/").as_deref(), Some("/r/4/2"));
        assert_eq!(first_link(html, "/p/").as_deref(), Some("/p/ab12.png"));
        assert_eq!(first_link(html, "/s/"), None);
    }
}
