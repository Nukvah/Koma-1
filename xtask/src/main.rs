//! Project tasks for Koma-1. Run with `cargo xtask <task>`.
//!
//! `fixtures` writes a made-up test library of CBZ and CB7 files.
//!
//! `budgets` checks the numbers in `budgets.toml`: static asset sizes always,
//! and with `--image` the container image size, cold start, idle memory,
//! page latency and the worker's speed on more CPUs of the real image. CI
//! fails when any budget is exceeded.

mod budgets;
mod docker;
mod fixtures;
mod http;
mod measure;
mod report;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use budgets::Budgets;
use report::Report;

const USAGE: &str = "\
Usage: cargo xtask budgets (--image <tag> | --assets-only)
       cargo xtask fixtures <dir>

  --image <tag>   Check every budget against this locally built image
                  (needs Docker; build it with: docker build -t koma:dev .)
  --assets-only   Check only static asset sizes (no Docker needed)

fixtures writes a made-up library of CBZ and CB7 files for testing and trying koma.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("budgets") => match budgets_task(&args[1..]) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => {
                eprintln!("\nA budget is exceeded. If the increase is deliberate, raise it in");
                eprintln!("budgets.toml and say why in the pull request (\"Budget change: ...\").");
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("xtask budgets: {e}");
                ExitCode::FAILURE
            }
        },
        Some("fixtures") => match fixtures::run(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask fixtures: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn budgets_task(args: &[String]) -> Result<bool, String> {
    let image = match args {
        [flag, tag] if flag == "--image" => Some(tag.as_str()),
        [flag] if flag == "--assets-only" => None,
        _ => return Err(format!("bad arguments\n\n{USAGE}")),
    };
    let root = workspace_root();
    let budgets = Budgets::load(&root.join("budgets.toml"))?;

    let mut report = Report::default();
    measure::assets(&root, &budgets, &mut report)?;
    if let Some(image) = image {
        measure::image(image, &budgets, &mut report)?;
        measure::throughput(image, &budgets, &mut report)?;
    }

    print!("\n{}", report.text());
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        report.append_markdown(Path::new(&path), image)?;
    }
    Ok(report.passed())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}
