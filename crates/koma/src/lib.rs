//! Koma-1: one binary, three modes (`serve`, `worker`, `scrub`).
//!
//! So far: `serve` serves the library, a series and a bare reader; `worker` and `scan` index the
//! library into the catalogue; `scrub` has nothing to check yet.

pub mod archive;
pub mod blobs;
pub mod catalog;
pub mod cli;
pub mod health;
pub mod html;
pub mod log;
pub mod natsort;
pub mod scan;
pub mod serve;
mod signal;

use std::ffi::OsString;
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
        Ok(Command::Worker(paths)) => worker(&paths),
        Ok(Command::Scan(paths)) => match scan_once(&paths) {
            Ok(report) if report.failed == 0 => ExitCode::SUCCESS,
            Ok(_) => ExitCode::from(3),
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

/// Opens the catalogue under `paths.data` and scans `paths.library` into it.
/// Errors are logged; the result says whether the scan ran.
fn scan_once(paths: &cli::Paths) -> Result<scan::ScanReport, ()> {
    let data = paths.data.to_string_lossy();
    if let Err(e) = std::fs::create_dir_all(&paths.data) {
        log::error(
            "scan: cannot create the data folder",
            &[("path", &data), ("error", &e.to_string())],
        );
        return Err(());
    }
    let db = paths.data.join("koma.db");
    let mut catalog = match catalog::Catalog::open(&db) {
        Ok(c) => c,
        Err(e) => {
            log::error(
                "scan: cannot open the catalogue",
                &[("path", &db.to_string_lossy()), ("error", &e.to_string())],
            );
            return Err(());
        }
    };
    log::info(
        "scan: started",
        &[("library", &paths.library.to_string_lossy())],
    );
    scan::scan(&paths.library, &paths.data, &mut catalog).map_err(|e| {
        log::error("scan: failed", &[("error", &e.to_string())]);
    })
}

/// The worker scans the library when it starts, then idles until stopped
/// (SIGTERM or Ctrl-C). Rescans and a job queue come with ingest (M2).
fn worker(paths: &cli::Paths) -> ExitCode {
    if scan_once(paths).is_err() {
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
    log::info("worker: idle", &[]);
    runtime.block_on(signal::shutdown());
    log::info("worker: stopped", &[]);
    ExitCode::SUCCESS
}
