//! Command line parsing. Hand-written: three subcommands and one flag don't
//! justify a dependency.

use std::ffi::OsString;
use std::net::SocketAddr;

pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

pub const USAGE: &str = "\
Usage: koma <command> [options]

Commands:
  serve        Serve the library over HTTP
  worker       Run background jobs (ingest; nothing to do yet)
  scrub        Check stored data for corruption (nothing to check yet)
  healthcheck  Exit 0 if the local server answers GET /healthz

Options:
  --listen <addr>  Address to listen on (serve) or probe (healthcheck).
                   Default: $KOMA_LISTEN, else 0.0.0.0:8080
  -h, --help       Show this help
  -V, --version    Show the version
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve { listen: SocketAddr },
    Worker,
    Scrub,
    Healthcheck { listen: SocketAddr },
    Help,
    Version,
}

pub fn parse<I>(args: I, env_listen: Option<OsString>) -> Result<Command, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args
        .into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|a| format!("argument is not UTF-8: {a:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some((command, rest)) = args.split_first() else {
        return Err("missing command".into());
    };

    if rest.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Command::Help);
    }

    match command.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" => Ok(Command::Version),
        "serve" => Ok(Command::Serve {
            listen: listen(rest, env_listen)?,
        }),
        "healthcheck" => Ok(Command::Healthcheck {
            listen: listen(rest, env_listen)?,
        }),
        "worker" => no_options(rest).map(|()| Command::Worker),
        "scrub" => no_options(rest).map(|()| Command::Scrub),
        other => Err(format!("unknown command: {other}")),
    }
}

/// `--listen` wins over `KOMA_LISTEN`, which wins over the default.
fn listen(rest: &[String], env_listen: Option<OsString>) -> Result<SocketAddr, String> {
    let mut flag = None;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--listen=") {
            flag = Some(value.to_owned());
        } else if arg == "--listen" {
            let value = iter.next().ok_or("--listen needs an address")?;
            flag = Some(value.clone());
        } else {
            return Err(format!("unknown option: {arg}"));
        }
    }
    let raw = match (flag, env_listen) {
        (Some(flag), _) => flag,
        (None, Some(env)) => env
            .into_string()
            .map_err(|_| "KOMA_LISTEN is not UTF-8".to_owned())?,
        (None, None) => DEFAULT_LISTEN.to_owned(),
    };
    raw.parse()
        .map_err(|_| format!("not a socket address (expected ip:port): {raw}"))
}

fn no_options(rest: &[String]) -> Result<(), String> {
    match rest.first() {
        Some(arg) => Err(format!("unknown option: {arg}")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str], env: Option<&str>) -> Result<Command, String> {
        parse(args.iter().map(OsString::from), env.map(OsString::from))
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn subcommands() {
        assert_eq!(p(&["worker"], None), Ok(Command::Worker));
        assert_eq!(p(&["scrub"], None), Ok(Command::Scrub));
        assert_eq!(p(&["--version"], None), Ok(Command::Version));
        assert_eq!(p(&["help"], None), Ok(Command::Help));
        assert_eq!(p(&["serve", "--help"], None), Ok(Command::Help));
    }

    #[test]
    fn listen_precedence() {
        let default = Command::Serve {
            listen: addr(DEFAULT_LISTEN),
        };
        assert_eq!(p(&["serve"], None), Ok(default));
        assert_eq!(
            p(&["serve"], Some("127.0.0.1:9000")),
            Ok(Command::Serve {
                listen: addr("127.0.0.1:9000")
            })
        );
        assert_eq!(
            p(&["serve", "--listen", "[::1]:7000"], Some("127.0.0.1:9000")),
            Ok(Command::Serve {
                listen: addr("[::1]:7000")
            })
        );
        assert_eq!(
            p(&["healthcheck", "--listen=127.0.0.1:1"], None),
            Ok(Command::Healthcheck {
                listen: addr("127.0.0.1:1")
            })
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&[], None).is_err());
        assert!(p(&["dance"], None).is_err());
        assert!(p(&["worker", "--fast"], None).is_err());
        assert!(p(&["serve", "--listen"], None).is_err());
        assert!(p(&["serve", "--listen", "8080"], None).is_err());
        assert!(p(&["serve"], Some("nope")).is_err());
    }
}
