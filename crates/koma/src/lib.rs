//! Koma-1: one binary, three modes (`serve`, `worker`, `scrub`).
//!
//! So far: `serve` serves the library, a series and a bare reader; `worker` and `scan` index the
//! library into the catalogue, and the worker keeps it up to date; `scrub` has nothing to check
//! yet.

pub mod archive;
pub mod blobs;
pub mod catalog;
pub mod cli;
pub mod health;
pub mod html;
pub mod libraries;
pub mod log;
pub mod meta;
pub mod names;
pub mod natsort;
pub mod scan;
pub mod serve;
mod signal;
pub mod variants;

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
        Ok(Command::Serve { listen, paths }) => serve::run(listen, &paths),
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
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let work = move || {
        let _lock = lock;
        let stop_seen = std::cell::Cell::new(false);
        let should_stop = || {
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
            let next = match rescan {
                Some(every) => stopped.recv_timeout(every),
                None => stopped
                    .recv()
                    .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
            };
            if !matches!(next, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                return;
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
