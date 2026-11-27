//! Command line parsing. Hand-written: a few subcommands and flags don't
//! justify a dependency.

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
pub const DEFAULT_LIBRARY: &str = "/library";
pub const DEFAULT_DATA: &str = "/data";
pub const DEFAULT_RESCAN_MINUTES: &str = "15";

pub const USAGE: &str = "\
Usage: koma <command> [options]

Commands:
  serve        Serve the library over HTTP
  worker       Scan the library and index what changed, then rescan regularly
  scan         Scan the library once and exit
  scrub        Check stored data for corruption (nothing to check yet)
  account link <name>
               Print a sign-in link for an account, made if there is none
               (the first account is the admin)
  healthcheck  Exit 0 if the local server answers GET /healthz

Options:
  --listen <addr>   Address to listen on (serve) or probe (healthcheck).
                    Default: $KOMA_LISTEN, else 0.0.0.0:8080
  --library <dir>   The library, read-only (serve, worker, scan).
                    Default: $KOMA_LIBRARY, else /library
  --data <dir>      Catalogue and blobs (serve reads, worker and scan write).
                    Default: $KOMA_DATA, else /data
  --rescan <min>    Minutes between the worker's rescans; 0: only at start.
                    Default: $KOMA_RESCAN, else 15
  --trust-header <name>
                    Sign in by this header from a sign-in proxy (serve),
                    e.g. Remote-User. Default: $KOMA_TRUST_HEADER, else off
  --trust-proxy <addresses>
                    The proxy's addresses or networks, comma-separated
                    (serve). Default: $KOMA_TRUST_PROXY
  -h, --help        Show this help
  -V, --version     Show the version
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve {
        listen: SocketAddr,
        paths: Paths,
        auth: crate::auth::Config,
    },
    /// `koma account link <name>`.
    AccountLink {
        data: PathBuf,
        name: String,
    },
    Worker {
        paths: Paths,
        /// Time between rescans; `None`: only when the worker starts.
        rescan: Option<Duration>,
    },
    Scan(Paths),
    Scrub,
    Healthcheck {
        listen: SocketAddr,
    },
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Paths {
    pub library: PathBuf,
    pub data: PathBuf,
}

/// The environment variables koma reads, passed in so tests don't touch the
/// real environment.
#[derive(Default)]
pub struct Env {
    pub listen: Option<OsString>,
    pub library: Option<OsString>,
    pub data: Option<OsString>,
    pub rescan: Option<OsString>,
    pub trust_header: Option<OsString>,
    pub trust_proxy: Option<OsString>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            listen: std::env::var_os("KOMA_LISTEN"),
            library: std::env::var_os("KOMA_LIBRARY"),
            data: std::env::var_os("KOMA_DATA"),
            rescan: std::env::var_os("KOMA_RESCAN"),
            trust_header: std::env::var_os("KOMA_TRUST_HEADER"),
            trust_proxy: std::env::var_os("KOMA_TRUST_PROXY"),
        }
    }
}

pub fn parse<I>(args: I, env: Env) -> Result<Command, String>
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
        "serve" => {
            let opts = options(
                rest,
                &[
                    "--listen",
                    "--library",
                    "--data",
                    "--trust-header",
                    "--trust-proxy",
                ],
            )?;
            let text =
                |flag: &str, var: Option<OsString>, name: &str| -> Result<Option<String>, String> {
                    match (opts.get(flag), var) {
                        (Some(v), _) => Ok(Some(v.clone())),
                        (None, Some(v)) => v
                            .into_string()
                            .map(Some)
                            .map_err(|_| format!("{name} is not UTF-8")),
                        (None, None) => Ok(None),
                    }
                };
            let header = text(
                "--trust-header",
                env.trust_header.clone(),
                "KOMA_TRUST_HEADER",
            )?;
            let proxies = text("--trust-proxy", env.trust_proxy.clone(), "KOMA_TRUST_PROXY")?;
            Ok(Command::Serve {
                listen: listen(opts.get("--listen"), env.listen.clone())?,
                auth: crate::auth::Config::parse(header.as_deref(), proxies.as_deref())?,
                paths: paths_from(&opts, env)?,
            })
        }
        "account" => match rest.split_first() {
            Some((sub, rest)) if sub == "link" => {
                let (name, rest) = rest.split_first().ok_or("account link needs a name")?;
                let opts = options(rest, &["--data"])?;
                Ok(Command::AccountLink {
                    data: pick(opts.get("--data"), env.data, "KOMA_DATA", DEFAULT_DATA)?.into(),
                    name: name.clone(),
                })
            }
            _ => Err("unknown account command (try: koma account link <name>)".into()),
        },
        "healthcheck" => {
            let opts = options(rest, &["--listen"])?;
            Ok(Command::Healthcheck {
                listen: listen(opts.get("--listen"), env.listen)?,
            })
        }
        "worker" => {
            let opts = options(rest, &["--library", "--data", "--rescan"])?;
            let rescan = rescan(opts.get("--rescan"), env.rescan.clone())?;
            Ok(Command::Worker {
                paths: paths_from(&opts, env)?,
                rescan,
            })
        }
        "scan" => paths(rest, env).map(Command::Scan),
        "scrub" => options(rest, &[]).map(|_| Command::Scrub),
        other => Err(format!("unknown command: {other}")),
    }
}

/// Parses `--name value` and `--name=value` for the allowed names; the last one wins.
fn options(
    rest: &[String],
    allowed: &[&'static str],
) -> Result<HashMap<&'static str, String>, String> {
    let mut found = HashMap::new();
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let Some(&name) = allowed.iter().find(|&&a| a == name) else {
            return Err(format!("unknown option: {arg}"));
        };
        let value = match inline {
            Some(value) => value,
            None => iter.next().ok_or(format!("{name} needs a value"))?.clone(),
        };
        found.insert(name, value);
    }
    Ok(found)
}

/// `--listen` wins over `KOMA_LISTEN`, which wins over the default.
fn listen(flag: Option<&String>, env: Option<OsString>) -> Result<SocketAddr, String> {
    let raw = pick(flag, env, "KOMA_LISTEN", DEFAULT_LISTEN)?;
    raw.parse()
        .map_err(|_| format!("not a socket address (expected ip:port): {raw}"))
}

/// `--rescan` wins over `KOMA_RESCAN`, which wins over the default.
fn rescan(flag: Option<&String>, env: Option<OsString>) -> Result<Option<Duration>, String> {
    let raw = pick(flag, env, "KOMA_RESCAN", DEFAULT_RESCAN_MINUTES)?;
    let minutes: u32 = raw
        .parse()
        .map_err(|_| format!("not a number of minutes: {raw}"))?;
    Ok((minutes > 0).then(|| Duration::from_secs(u64::from(minutes) * 60)))
}

fn paths(rest: &[String], env: Env) -> Result<Paths, String> {
    paths_from(&options(rest, &["--library", "--data"])?, env)
}

fn paths_from(opts: &HashMap<&'static str, String>, env: Env) -> Result<Paths, String> {
    Ok(Paths {
        library: pick(
            opts.get("--library"),
            env.library,
            "KOMA_LIBRARY",
            DEFAULT_LIBRARY,
        )?
        .into(),
        data: pick(opts.get("--data"), env.data, "KOMA_DATA", DEFAULT_DATA)?.into(),
    })
}

fn pick(
    flag: Option<&String>,
    env: Option<OsString>,
    var: &str,
    default: &str,
) -> Result<String, String> {
    match (flag, env) {
        (Some(flag), _) => Ok(flag.clone()),
        (None, Some(env)) => env.into_string().map_err(|_| format!("{var} is not UTF-8")),
        (None, None) => Ok(default.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str], env: Option<&str>) -> Result<Command, String> {
        let env = Env {
            listen: env.map(OsString::from),
            ..Env::default()
        };
        parse(args.iter().map(OsString::from), env)
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn serve_on(listen: &str) -> Command {
        Command::Serve {
            listen: addr(listen),
            paths: Paths {
                library: DEFAULT_LIBRARY.into(),
                data: DEFAULT_DATA.into(),
            },
            auth: crate::auth::Config::default(),
        }
    }

    #[test]
    fn accounts_and_proxy() {
        assert_eq!(
            p(&["account", "link", "mirelle", "--data", "/d"], None),
            Ok(Command::AccountLink {
                data: "/d".into(),
                name: "mirelle".into()
            })
        );
        assert!(p(&["account", "link"], None).is_err());
        assert!(p(&["account", "list"], None).is_err());
        match p(
            &[
                "serve",
                "--trust-header",
                "Remote-User",
                "--trust-proxy",
                "10.0.0.2",
            ],
            None,
        ) {
            Ok(Command::Serve { auth, .. }) => {
                assert_eq!(auth.header.as_deref(), Some("Remote-User"))
            }
            other => panic!("{other:?}"),
        }
        assert!(p(&["serve", "--trust-header", "Remote-User"], None).is_err());
    }

    #[test]
    fn subcommands() {
        assert_eq!(
            p(&["worker"], None),
            Ok(Command::Worker {
                paths: Paths {
                    library: DEFAULT_LIBRARY.into(),
                    data: DEFAULT_DATA.into()
                },
                rescan: Some(Duration::from_secs(15 * 60)),
            })
        );
        assert_eq!(p(&["scrub"], None), Ok(Command::Scrub));
        assert_eq!(p(&["--version"], None), Ok(Command::Version));
        assert_eq!(p(&["help"], None), Ok(Command::Help));
        assert_eq!(p(&["serve", "--help"], None), Ok(Command::Help));
    }

    #[test]
    fn listen_precedence() {
        let default = serve_on(DEFAULT_LISTEN);
        assert_eq!(p(&["serve"], None), Ok(default));
        assert_eq!(
            p(&["serve"], Some("127.0.0.1:9000")),
            Ok(serve_on("127.0.0.1:9000"))
        );
        assert_eq!(
            p(&["serve", "--listen", "[::1]:7000"], Some("127.0.0.1:9000")),
            Ok(serve_on("[::1]:7000"))
        );
        assert_eq!(
            p(&["healthcheck", "--listen=127.0.0.1:1"], None),
            Ok(Command::Healthcheck {
                listen: addr("127.0.0.1:1")
            })
        );
    }

    #[test]
    fn library_and_data_paths() {
        let env = Env {
            library: Some("/env/lib".into()),
            data: Some("/env/data".into()),
            ..Env::default()
        };
        assert_eq!(
            parse(["scan", "--data=/d"].map(OsString::from), env),
            Ok(Command::Scan(Paths {
                library: "/env/lib".into(),
                data: "/d".into()
            }))
        );
        assert!(p(&["healthcheck", "--library", "/x"], None).is_err());
    }

    #[test]
    fn rescan_interval() {
        let worker = |args: &[&str], env: Option<&str>| match parse(
            args.iter().map(OsString::from),
            Env {
                rescan: env.map(OsString::from),
                ..Env::default()
            },
        ) {
            Ok(Command::Worker { rescan, .. }) => Ok(rescan),
            other => Err(format!("{other:?}")),
        };
        let minutes = |m: u64| Ok(Some(Duration::from_secs(m * 60)));
        assert_eq!(worker(&["worker"], Some("5")), minutes(5));
        assert_eq!(worker(&["worker", "--rescan=60"], Some("5")), minutes(60));
        assert_eq!(worker(&["worker", "--rescan", "0"], None), Ok(None));
        assert!(worker(&["worker", "--rescan", "soon"], None).is_err());
        assert!(worker(&["worker", "--rescan", "-1"], None).is_err());
        assert!(p(&["scan", "--rescan", "5"], None).is_err());
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
