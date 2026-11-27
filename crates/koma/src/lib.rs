//! Koma-1: one binary, three modes (`serve`, `worker`, `scrub`).
//!
//! Milestone 0 is the skeleton: `serve` answers a health check, `worker` idles
//! until it is stopped and `scrub` has nothing to check yet.

pub mod archive;
pub mod cli;
pub mod health;
pub mod log;
pub mod natsort;
pub mod serve;
mod signal;

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use cli::Command;

/// Runs the command line `args` (without the program name). `env_listen` is
/// the value of `KOMA_LISTEN`, passed in so tests don't touch the environment.
pub fn run<I>(args: I, env_listen: Option<OsString>) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    match cli::parse(args, env_listen) {
        Ok(Command::Serve { listen }) => serve::run(listen),
        Ok(Command::Worker) => worker(),
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

/// The worker has no jobs until ingest lands; it idles so its memory can be
/// measured, and stops cleanly on SIGTERM or Ctrl-C.
fn worker() -> ExitCode {
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
    log::info("worker: idle, no jobs yet", &[]);
    runtime.block_on(signal::shutdown());
    log::info("worker: stopped", &[]);
    ExitCode::SUCCESS
}
