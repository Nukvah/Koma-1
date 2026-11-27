use std::process::ExitCode;

fn main() -> ExitCode {
    koma::run(std::env::args_os().skip(1), std::env::var_os("KOMA_LISTEN"))
}
