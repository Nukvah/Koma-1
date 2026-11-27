use std::process::ExitCode;

fn main() -> ExitCode {
    koma::run(std::env::args_os().skip(1), koma::cli::Env::from_process())
}
