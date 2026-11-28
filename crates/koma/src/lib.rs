//! Koma-1: one binary, three modes (`serve`, `worker`, `scrub`).
//!
//! So far: `serve` serves the library and the readers to signed-in accounts; `worker` and
//! `scan` index the library into the catalogue, and the worker keeps it up to date and applies
//! library settings changed in the UI; `account link` makes a sign-in link from the shell;
//! `scrub` has nothing to check yet.

pub mod accounts;
pub mod admin;
pub mod archive;
pub mod auth;
pub mod b64;
pub mod blobs;
pub mod catalog;
pub mod cli;
pub mod curate;
pub mod download;
pub mod drops;
pub mod epub;
pub mod health;
pub mod html;
pub mod journal;
pub mod kept;
pub mod libraries;
pub mod log;
pub mod meta;
pub mod names;
pub mod naming;
pub mod natsort;
pub mod organise;
pub mod pages;
pub mod password;
pub mod rename;
pub mod scan;
pub mod search;
pub mod serve;
mod signal;
pub mod staging;
pub mod upload;
pub mod variants;
pub mod webauthn;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;
use std::time::Duration;

use cli::Command;

/// Runs the command line `args` (without the program name).
pub fn run<I>(args: I, env: cli::Env) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    match cli::parse(args, env) {
        Ok(Command::Serve {
            listen,
            paths,
            auth,
        }) => serve::run(listen, &paths, auth),
        Ok(Command::AccountLink { data, name }) => account_link(&data, &name),
        Ok(Command::Worker { paths, rescan }) => worker(paths, rescan),
        Ok(Command::Scan(paths)) => match scan_once(&paths) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(3),
            Err(()) => ExitCode::FAILURE,
        },
        Ok(Command::Scrub) => {
            log::info("scrub: nothing to check yet", &[]);
            ExitCode::SUCCESS
        }
        Ok(Command::Healthcheck { listen }) => {
            let target = health::probe_target(listen);
            match health::probe(target, Duration::from_secs(2)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("koma healthcheck: {target}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Help) => {
            print!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("koma {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprint!("koma: {e}\n\n{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}

/// `koma account link <name>`: a sign-in link for `name`, made from the
/// shell (a lost admin password, or the first account without the setup
/// code). An account of that name is made if there is none: the admin if
/// it is the first.
fn account_link(data: &std::path::Path, name: &str) -> ExitCode {
    let made = auth::open(data).map_err(|e| e.to_string()).and_then(|a| {
        let account = match a.by_name(name).map_err(|e| e.to_string())? {
            Some(account) => account,
            None => a
                .create(name, accounts::Role::Reader, None)
                .map_err(|e| match e {
                    accounts::CreateError::BadName => {
                        "a name is lower-case letters, digits, '.', '_' and '-', at most 32"
                            .to_owned()
                    }
                    e => format!("{e:?}"),
                })?,
        };
        let token = a.new_link(account.id).map_err(|e| e.to_string())?;
        Ok((account, token))
    });
    match made {
        Ok((account, token)) => {
            println!(
                "A sign-in link for {} ({}), good once, for 24 hours. Open it on your server:\n\n  /join/{}\n\nfor example http://localhost:8080/join/{}",
                account.name,
                account.role.as_str(),
                b64::encode(&token),
                b64::encode(&token)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("koma account link: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Applies the library settings changed in the UI that the catalogue has
/// not taken yet (queued by `koma serve` in accounts.db). Errors are logged.
fn apply_settings(data: &std::path::Path, catalog: &catalog::Catalog) {
    let result = (|| -> Result<usize, rusqlite::Error> {
        let Some(a) = accounts::Accounts::open_read_only(&data.join("accounts.db"))? else {
            return Ok(0);
        };
        let changes = a.changes_after(catalog.settings_applied()?)?;
        for c in &changes {
            catalog.apply_setting(c)?;
            log::info(
                "worker: library settings changed",
                &[("library", &c.folder), ("name", &c.name)],
            );
        }
        Ok(changes.len())
    })();
    if let Err(e) = result {
        log::error(
            "worker: cannot apply library settings",
            &[("error", &e.to_string())],
        );
    }
}

/// How often the worker looks for library settings and changes queued in
/// the UI: often, so a quick change (a move, a delete) shows at once.
const TICK: Duration = Duration::from_millis(250);

/// How often the trash is checked for things past their keep time.
const EXPIRE_EVERY: Duration = Duration::from_secs(60);

/// What the worker looks at between jobs and while idle: library settings
/// and changes queued in the UI, and now and then the trash's keep time.
fn tick(
    paths: &cli::Paths,
    catalog: &mut catalog::Catalog,
    expired: &std::cell::Cell<std::time::Instant>,
) {
    apply_settings(&paths.data, catalog);
    organise::run(&paths.library, &paths.data, catalog);
    if expired.get().elapsed() >= EXPIRE_EVERY {
        expired.set(std::time::Instant::now());
        organise::expire_trash(&paths.library, &paths.data, catalog);
    }
}

/// Opens the catalogue under `paths.data` for writing, holding the worker
/// lock so that only one process indexes into it. Errors are logged.
fn open_for_writing(paths: &cli::Paths) -> Result<(catalog::Catalog, std::fs::File), ()> {
    let data = paths.data.to_string_lossy();
    if let Err(e) = std::fs::create_dir_all(&paths.data) {
        log::error(
            "scan: cannot create the data folder",
            &[("path", &data), ("error", &e.to_string())],
        );
        return Err(());
    }
    let lock_path = paths.data.join("worker.lock");
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .and_then(|f| {
            f.try_lock().map_err(|e| match e {
                std::fs::TryLockError::WouldBlock => {
                    io::Error::other("another koma worker or scan is using this data folder")
                }
                std::fs::TryLockError::Error(e) => e,
            })?;
            Ok(f)
        });
    let lock = match lock {
        Ok(lock) => lock,
        Err(e) => {
            log::error(
                "scan: cannot lock the data folder",
                &[
                    ("path", &lock_path.to_string_lossy()),
                    ("error", &e.to_string()),
                ],
            );
            return Err(());
        }
    };
    let db = paths.data.join("koma.db");
    match catalog::Catalog::open(&db) {
        Ok(c) => Ok((c, lock)),
        Err(e) => {
            log::error(
                "scan: cannot open the catalogue",
                &[("path", &db.to_string_lossy()), ("error", &e.to_string())],
            );
            Err(())
        }
    }
}

/// Scans the library once and works through every queued job. Returns
/// whether every book could be read; errors are logged.
fn scan_once(paths: &cli::Paths) -> Result<bool, ()> {
    let (mut catalog, _lock) = open_for_writing(paths)?;
    resume(&catalog)?;
    pass(paths, &mut catalog, scan::Check::Files, &|| false)?;
    match catalog.errors() {
        Ok(errors) => Ok(errors.is_empty()),
        Err(e) => {
            log::error("scan: failed", &[("error", &e.to_string())]);
            Err(())
        }
    }
}

/// Puts jobs interrupted by the last stop back in the queue.
fn resume(catalog: &catalog::Catalog) -> Result<(), ()> {
    match catalog.requeue_running() {
        Ok(0) => Ok(()),
        Ok(n) => {
            log::info(
                "worker: resuming interrupted jobs",
                &[("jobs", &n.to_string())],
            );
            Ok(())
        }
        Err(e) => {
            log::error(
                "worker: cannot read the job queue",
                &[("error", &e.to_string())],
            );
            Err(())
        }
    }
}

/// One scan of the library, then the job queue until it is empty or
/// `should_stop` says so.
fn pass(
    paths: &cli::Paths,
    catalog: &mut catalog::Catalog,
    check: scan::Check,
    should_stop: &dyn Fn() -> bool,
) -> Result<(), ()> {
    apply_settings(&paths.data, catalog);
    organise::run(&paths.library, &paths.data, catalog);
    let started = std::time::Instant::now();
    log::info(
        "scan: started",
        &[
            ("library", &paths.library.to_string_lossy()),
            (
                "check",
                if check == scan::Check::Files {
                    "files"
                } else {
                    "folders"
                },
            ),
        ],
    );
    let mut report = scan::discover(&paths.library, catalog, check).map_err(|e| {
        log::error("scan: failed", &[("error", &e.to_string())]);
    })?;
    scan::run_jobs(
        &paths.library,
        &paths.data,
        catalog,
        &mut report,
        should_stop,
    )
    .map_err(|e| {
        log::error("scan: failed", &[("error", &e.to_string())]);
    })?;
    scan::log_report(&report, started);
    // Cosplay libraries kept in Cosplayer / Set / variant folders.
    organise::tidy(&paths.library, &paths.data, catalog);
    Ok(())
}

/// Known books are checked one by one at most this often; between those
/// checks a rescan only looks at folder modification times.
const FILE_CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The worker: scans the library when it starts, works through the job
/// queue, then rescans every `rescan` until stopped (SIGTERM or Ctrl-C).
/// A stop is honoured between jobs; a job cut short by a kill runs again on
/// the next start.
fn worker(paths: cli::Paths, rescan: Option<Duration>) -> ExitCode {
    let Ok((mut catalog, lock)) = open_for_writing(&paths) else {
        return ExitCode::FAILURE;
    };
    if resume(&catalog).is_err() {
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error("worker: cannot start runtime", &[("error", &e.to_string())]);
            return ExitCode::FAILURE;
        }
    };
    // A second connection applies settings and changes between jobs, while
    // the first is busy with a scan.
    let side = std::cell::RefCell::new(catalog::Catalog::open(&paths.data.join("koma.db")).ok());
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let work = move || {
        let _lock = lock;
        let stop_seen = std::cell::Cell::new(false);
        let checked = std::cell::Cell::new(std::time::Instant::now());
        let expired = std::cell::Cell::new(
            std::time::Instant::now()
                .checked_sub(EXPIRE_EVERY)
                .unwrap_or_else(std::time::Instant::now),
        );
        let should_stop = || {
            if checked.get().elapsed() >= TICK
                && let Some(c) = side.borrow_mut().as_mut()
            {
                checked.set(std::time::Instant::now());
                tick(&paths, c, &expired);
            }
            if !stop_seen.get() {
                stop_seen.set(!matches!(
                    stopped.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
            }
            stop_seen.get()
        };
        let mut last_file_check: Option<std::time::Instant> = None;
        loop {
            let check = match last_file_check {
                Some(t) if t.elapsed() < FILE_CHECK_EVERY => scan::Check::Folders,
                _ => {
                    last_file_check = Some(std::time::Instant::now());
                    scan::Check::Files
                }
            };
            // A failed pass (library not mounted, say) is logged and tried
            // again at the next rescan.
            let _ = pass(&paths, &mut catalog, check, &should_stop);
            if should_stop() {
                return;
            }
            log::info("worker: idle", &[]);
            // Until the next rescan, a look for settings and changes from
            // the UI every quarter second.
            let idle_since = std::time::Instant::now();
            loop {
                let wait = match rescan {
                    Some(every) => every.saturating_sub(idle_since.elapsed()),
                    None => TICK,
                };
                if wait.is_zero() {
                    break;
                }
                match stopped.recv_timeout(wait.min(TICK)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        tick(&paths, &mut catalog, &expired);
                    }
                    _ => return,
                }
            }
        }
    };
    runtime.block_on(async {
        let mut task = tokio::task::spawn_blocking(work);
        tokio::select! {
            _ = &mut task => {}
            _ = signal::shutdown() => {
                log::info("worker: stopping after the current job", &[]);
                let _ = stop.send(());
                let _ = task.await;
            }
        }
    });
    log::info("worker: stopped", &[]);
    ExitCode::SUCCESS
}
