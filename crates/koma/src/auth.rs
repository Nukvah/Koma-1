//! Who is asking: every request but a few public ones (page images, the
//! stylesheets and scripts, the health check, and the ways in) needs a
//! signed-in account. See docs/development.md, Accounts.
//!
//! - A session is a random token in the `koma` cookie (`HttpOnly`,
//!   `SameSite=Lax`, `Secure` on HTTPS), signed with a key kept in
//!   accounts.db so a forged cookie is turned away before any lookup, and
//!   stored only as a hash.
//! - Behind a sign-in proxy (`--trust-header`, `--trust-proxy`), the header
//!   names the account, believed only from the proxy's addresses; a name
//!   with no account gets one, as a reader.
//! - With no account yet, every page leads to `/setup`.
//! - A request that changes something must come from Koma-1's own pages:
//!   its `Origin` (or `Sec-Fetch-Site`) is checked.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Path as UrlPath, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, HOST, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};

use crate::accounts::{Account, Accounts, Role};
use crate::serve::{AppState, Failure};
use crate::{b64, kept, log, pages, password, webauthn};

/// Who may sign in through a proxy, from `--trust-header` and
/// `--trust-proxy`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// The header naming the signed-in user, e.g. `Remote-User`.
    pub header: Option<String>,
    /// Addresses the header is believed from.
    pub proxies: Vec<IpNet>,
}

impl Config {
    /// From the flags' values: a header name, and addresses or networks
    /// separated by commas (`10.0.0.2`, `172.16.0.0/12`, `::1`).
    pub fn parse(header: Option<&str>, proxies: Option<&str>) -> Result<Self, String> {
        let header = header.map(str::trim).filter(|h| !h.is_empty());
        if let Some(h) = header
            && !h
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("not a header name: {h}"));
        }
        let proxies = proxies
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(IpNet::parse)
            .collect::<Result<Vec<_>, _>>()?;
        if header.is_some() && proxies.is_empty() {
            return Err("--trust-header needs --trust-proxy: the proxy's addresses".into());
        }
        Ok(Self {
            header: header.map(str::to_owned),
            proxies,
        })
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.header.is_some() && self.proxies.iter().any(|n| n.contains(ip))
    }
}

/// An address or a network: `10.0.0.0/8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("not an address or network: {s}");
        let (a, p) = s.split_once('/').map_or((s, None), |(a, p)| (a, Some(p)));
        let addr: IpAddr = a.parse().map_err(|_| bad())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            None => max,
            Some(p) => p.parse().ok().filter(|&p| p <= max).ok_or_else(bad)?,
        };
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

/// The signed-in account making a request.
#[derive(Clone, Debug)]
pub struct Viewer {
    pub id: i64,
    pub name: String,
    pub role: Role,
    /// Its session's hash; `None` when signed in by the proxy.
    pub session: Option<[u8; 32]>,
}

impl Viewer {
    fn new(a: &Account, session: Option<[u8; 32]>) -> Self {
        Self {
            id: a.id,
            name: a.name.clone(),
            role: a.role,
            session,
        }
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    /// Whether it may see the series at `series_path`.
    pub fn sees(&self, series_path: &str) -> bool {
        let library = crate::libraries::library_of(series_path);
        crate::libraries::owner_of(library).is_none_or(|o| o == self.name)
    }

    pub fn me(&self) -> pages::Me<'_> {
        pages::Me {
            name: &self.name,
            role: self.role,
        }
    }
}

/// The address a request came from, when the server knows it.
pub struct Peer(pub Option<IpAddr>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Peer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip()),
        ))
    }
}

/// Requests anyone may make.
fn public(path: &str) -> bool {
    path == "/healthz" || path.starts_with("/a/") || path.starts_with("/p/")
}

/// The ways in: open to anyone, signed in or not.
fn way_in(path: &str) -> bool {
    matches!(
        path,
        "/signin" | "/setup" | "/passkey/signin/begin" | "/passkey/signin/finish"
    ) || path.starts_with("/join/")
}

/// Answers that are data, not pages: an unknown visitor gets 401, not a
/// sign-in page.
fn data(path: &str) -> bool {
    path.starts_with("/b/") || path == "/kept" || path.starts_with("/passkey/")
}

const COOKIE_NAME: &str = "koma";

/// The address a request comes from: the peer, or, from a trusted proxy,
/// the last address it forwarded for.
fn client_addr(state: &AppState, peer: Option<IpAddr>, headers: &HeaderMap) -> String {
    let Some(peer) = peer else {
        return String::new();
    };
    if state.auth().proxies.iter().any(|n| n.contains(peer))
        && let Some(f) = header(headers, "x-forwarded-for")
        && let Some(last) = f.rsplit(',').next()
    {
        return last.trim().chars().take(45).collect();
    }
    peer.to_string()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The value of the `koma` cookie.
fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix("koma="))
}

/// A session token as the cookie carries it: the token and its signature.
pub fn sign(key: &[u8; 32], token: &[u8; 32]) -> String {
    let mac = blake3::keyed_hash(key, token);
    format!(
        "{}.{}",
        b64::encode(token),
        b64::encode(&mac.as_bytes()[..16])
    )
}

/// The token in a cookie value, if its signature is good.
pub fn unsign(key: &[u8; 32], value: &str) -> Option<[u8; 32]> {
    let (t, m) = value.split_once('.')?;
    let token = b64::token(t)?;
    let mac = b64::decode(m)?;
    let good = blake3::keyed_hash(key, &token);
    let good = &good.as_bytes()[..16];
    // Constant time: every byte is compared.
    (mac.len() == 16 && mac.iter().zip(good).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0)
        .then_some(token)
}

/// Whether the request came over HTTPS, as far as can be told: the browser's
/// `Origin`, or the proxy's `X-Forwarded-Proto`.
fn https(headers: &HeaderMap) -> bool {
    header(headers, "origin").is_some_and(|o| o.starts_with("https://"))
        || header(headers, "x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

/// `Set-Cookie` for a new session.
fn session_cookie(key: &[u8; 32], token: &[u8; 32], keep: bool, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Lax{}{}",
        sign(key, token),
        if secure { "; Secure" } else { "" },
        if keep {
            format!("; Max-Age={}", crate::accounts::KEEP_SECS)
        } else {
            String::new()
        }
    )
}

const CLEAR_COOKIE: &str = "koma=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0";

/// Whether a request that changes something comes from Koma-1's own pages.
fn same_origin(headers: &HeaderMap) -> bool {
    match header(headers, "origin") {
        Some("null") => false,
        Some(origin) => {
            let host = header(headers, "host").unwrap_or_default();
            origin
                .split_once("://")
                .is_some_and(|(_, h)| h.eq_ignore_ascii_case(host))
        }
        None => !matches!(
            header(headers, "sec-fetch-site"),
            Some("cross-site" | "same-site")
        ),
    }
}

/// The middleware: works out who is asking, then lets the request on,
/// sends the visitor to sign in, or refuses.
pub async fn gate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    if public(&path) {
        return next.run(req).await;
    }
    let writes = !matches!(*req.method(), Method::GET | Method::HEAD);
    if writes && !same_origin(req.headers()) {
        return Failure::Forbidden("This form was sent from another site.".into()).into_response();
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let addr = client_addr(&state, peer, req.headers());
    let proxied = match (&state.auth().header, peer) {
        (Some(h), Some(ip)) if state.auth().trusts(ip) => header(req.headers(), h)
            .map(|n| n.trim().to_lowercase())
            .filter(|n| !n.is_empty()),
        _ => None,
    };
    let cookie = cookie(req.headers()).map(str::to_owned);
    let found = state
        .accounts(move |a| {
            if let Some(name) = proxied {
                if !crate::accounts::valid_name(&name) {
                    return Err(Failure::Forbidden(format!(
                        "The sign-in proxy names you “{name}”, which can't be an account name: \
                         use lower-case letters, digits, “.”, “_” and “-”."
                    )));
                }
                let account = match a.by_name(&name)? {
                    Some(account) => account,
                    None => match a.create(&name, Role::Reader, None) {
                        Ok(account) => {
                            log::info(
                                "auth: account made for the proxy's user",
                                &[("name", &name)],
                            );
                            account
                        }
                        // Made by another request at the same time.
                        Err(_) => a.by_name(&name)?.ok_or(Failure::NotFound)?,
                    },
                };
                // Written only every few minutes: most requests only read.
                let t = crate::accounts::now();
                if account
                    .seen
                    .is_none_or(|s| t - s >= crate::accounts::SEEN_EVERY)
                {
                    a.touch_account(account.id, t)?;
                }
                return Ok(Found::Viewer(Viewer::new(&account, None)));
            }
            if let Some(value) = cookie
                && let Some(token) = unsign(&a.key()?, &value)
                && let Some((account, session)) = a.session(&token, &addr)?
            {
                return Ok(Found::Viewer(Viewer::new(&account, Some(session.hash))));
            }
            Ok(if a.count()? == 0 {
                Found::NoAccounts
            } else {
                Found::Nobody
            })
        })
        .await;
    let found = match found {
        Ok(f) => f,
        Err(f) => return f.into_response(),
    };
    match found {
        Found::Viewer(v) => {
            req.extensions_mut().insert(v);
            next.run(req).await
        }
        _ if way_in(&path) => next.run(req).await,
        _ if writes || data(&path) => (StatusCode::UNAUTHORIZED, "sign in first\n").into_response(),
        Found::NoAccounts => see_other("/setup"),
        Found::Nobody => {
            let next = match req.uri().query() {
                Some(q) => format!("{path}?{q}"),
                None => path,
            };
            let to = if next == "/" {
                "/signin".to_owned()
            } else {
                format!("/signin?next={}", crate::libraries::encode_path_part(&next))
            };
            see_other(&to)
        }
    }
}

enum Found {
    Viewer(Viewer),
    NoAccounts,
    Nobody,
}

pub fn see_other(to: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (LOCATION, to.to_owned()),
            (CACHE_CONTROL, "no-store".to_owned()),
        ],
    )
        .into_response()
}

/// A place to go back to after signing in: a path on this site only.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.contains('\\')
                && !n.contains("\r")
                && !n.contains('\n') =>
        {
            n.to_owned()
        }
        _ => "/".to_owned(),
    }
}

/// A form's fields, as sent by a browser (`application/x-www-form-urlencoded`).
pub fn form(body: &[u8]) -> Vec<(String, String)> {
    let Ok(body) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    body.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((
                crate::serve::percent_decode(k)?,
                crate::serve::percent_decode(v)?,
            ))
        })
        .collect()
}

pub fn field<'a>(form: &'a [(String, String)], key: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == key)
        .map_or("", |(_, v)| v.as_str())
}

/// `Firefox on Linux`, from a User-Agent: enough to tell devices apart.
pub fn device(agent: &str) -> String {
    let browser = if agent.contains("Edg/") {
        "Edge"
    } else if agent.contains("OPR/") {
        "Opera"
    } else if agent.contains("Firefox/") {
        "Firefox"
    } else if agent.contains("Chrome/") || agent.contains("CriOS/") {
        "Chrome"
    } else if agent.contains("Safari/") {
        "Safari"
    } else {
        "A browser"
    };
    let system = if agent.contains("Android") {
        "Android"
    } else if agent.contains("iPhone") {
        "iPhone"
    } else if agent.contains("iPad") {
        "iPad"
    } else if agent.contains("Windows") {
        "Windows"
    } else if agent.contains("CrOS") {
        "ChromeOS"
    } else if agent.contains("Mac OS X") || agent.contains("Macintosh") {
        "macOS"
    } else if agent.contains("Linux") {
        "Linux"
    } else {
        return browser.to_owned();
    };
    format!("{browser} on {system}")
}

/// The site's address as the browser asked for it, for links shown to be
/// copied: `http://koma.local:8080`.
pub fn site(headers: &HeaderMap) -> String {
    let host = header(headers, "host").unwrap_or("localhost");
    let scheme = if https(headers) { "https" } else { "http" };
    format!("{scheme}://{host}")
}

/// The setup code, made (and logged) when first needed.
pub fn setup_code(state: &AppState) -> String {
    state.setup_code(|| {
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let r = crate::accounts::random();
        let mut code: String = r[..8]
            .iter()
            .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
            .collect();
        code.insert(4, '-');
        log::info(
            "serve: no account yet; open /setup and give this code to make the admin",
            &[("setup code", &code)],
        );
        code
    })
}

// The ways in.

/// What a sign-in form sent, kept to show it again.
struct SignIn {
    name: String,
    next: String,
}

/// `GET /signin`.
pub async fn signin_page(
    me: Option<axum::Extension<Viewer>>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
) -> Response {
    let next = safe_next(crate::serve::query_value(q.as_deref(), "next").as_deref());
    if me.is_some() {
        return see_other(&next);
    }
    Html(pages::signin(None, "", &next)).into_response()
}

/// `POST /signin`: name, password, keep, next, and what the browser kept
/// before accounts (`kept`, JSON).
pub async fn signin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let s = SignIn {
        name: field(&f, "name").trim().to_lowercase(),
        next: safe_next(Some(field(&f, "next"))),
    };
    let password = field(&f, "password").to_owned();
    let keep = !field(&f, "keep").is_empty();
    let brought = kept::from_json(field(&f, "kept"));
    let agent = device(header(&headers, "user-agent").unwrap_or_default());
    let addr = client_addr(&state, peer, &headers);
    let secure = https(&headers);
    let name = s.name.clone();
    let result = state
        .accounts(move |a| {
            let wait = a.wait(&name)?;
            if wait > 0 {
                return Ok(Err(format!(
                    "Too many wrong passwords for this name: try again in {wait} seconds."
                )));
            }
            let account = a.by_name(&name)?;
            let phc = account
                .as_ref()
                .and_then(|x| x.password.as_deref())
                .unwrap_or(password::decoy());
            let good = password::verify(&password, phc)
                && account.as_ref().is_some_and(|x| x.password.is_some());
            let Some(account) = account.filter(|_| good) else {
                a.failed(&name)?;
                return Ok(Err("That name and password don't match.".into()));
            };
            a.succeeded(&name)?;
            let token = a.new_session(account.id, keep, &agent, &addr)?;
            let moved = !brought.is_empty() && a.keep(account.id, &brought, true)? > 0;
            Ok(Ok((a.key()?, token, moved)))
        })
        .await?;
    match result {
        Ok((key, token, moved)) => Ok(signed_in(&key, &token, keep, secure, &s.next, moved)),
        Err(e) => Ok((
            StatusCode::UNAUTHORIZED,
            Html(pages::signin(Some(&e), &s.name, &s.next)),
        )
            .into_response()),
    }
}

/// The answer to a sign-in: the cookie, and on to `next`. When the browser
/// brought what it kept before accounts, that is cleared from it now that
/// the account has it.
fn signed_in(
    key: &[u8; 32],
    token: &[u8; 32],
    keep: bool,
    secure: bool,
    next: &str,
    moved: bool,
) -> Response {
    let mut r = see_other(next);
    if let Ok(v) = HeaderValue::from_str(&session_cookie(key, token, keep, secure)) {
        r.headers_mut().insert(SET_COOKIE, v);
    }
    if moved {
        r.headers_mut()
            .insert("clear-site-data", HeaderValue::from_static("\"storage\""));
    }
    r
}

/// `POST /signout`.
pub async fn signout(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<Viewer>,
) -> Result<Response, Failure> {
    if let Some(h) = me.session {
        state
            .accounts(move |a| Ok(a.end_session(me.id, &h)?))
            .await?;
    }
    let mut r = see_other("/signin");
    r.headers_mut()
        .insert(SET_COOKIE, HeaderValue::from_static(CLEAR_COOKIE));
    Ok(r)
}

/// `GET /setup`: making the first account, the admin.
pub async fn setup_page(State(state): State<AppState>) -> Result<Response, Failure> {
    if state.accounts(|a| Ok(a.count()?)).await? > 0 {
        return Ok(see_other("/signin"));
    }
    setup_code(&state);
    Ok(Html(pages::setup(None, "")).into_response())
}

/// `POST /setup`: code, name, password, again.
pub async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let name = field(&f, "name").trim().to_lowercase();
    let pw = field(&f, "password").to_owned();
    let error = if field(&f, "code").trim().to_uppercase() != setup_code(&state) {
        Some("That is not the setup code. It is in the server's log.")
    } else if !crate::accounts::valid_name(&name) {
        Some("A name is lower-case letters, digits, “.”, “_” and “-”, at most 32.")
    } else {
        password::check_new(&pw, field(&f, "again")).err()
    };
    if let Some(e) = error {
        return Ok((StatusCode::BAD_REQUEST, Html(pages::setup(Some(e), &name))).into_response());
    }
    let agent = device(header(&headers, "user-agent").unwrap_or_default());
    let addr = client_addr(&state, peer, &headers);
    let secure = https(&headers);
    let made = state
        .accounts(move |a| {
            if a.count()? > 0 {
                return Ok(None);
            }
            let account = a
                .create(&name, Role::Admin, Some(&password::hash(&pw)))
                .map_err(|e| Failure::Internal(format!("{e:?}")))?;
            log::info("auth: admin account made", &[("name", &account.name)]);
            Ok(Some((
                a.key()?,
                a.new_session(account.id, true, &agent, &addr)?,
            )))
        })
        .await?;
    match made {
        Some((key, token)) => Ok(signed_in(&key, &token, true, secure, "/", false)),
        None => Ok(see_other("/signin")),
    }
}

/// `GET /join/<token>`: a sign-in link; the account chooses its password.
pub async fn join_page(
    State(state): State<AppState>,
    UrlPath(token): UrlPath<String>,
) -> Result<Response, Failure> {
    let Some(t) = b64::token(&token) else {
        return Ok(dead_link());
    };
    let account = state.accounts(move |a| Ok(a.link(&t)?)).await?;
    Ok(match account {
        Some(a) => Html(pages::join(&a, None)).into_response(),
        None => dead_link(),
    })
}

fn dead_link() -> Response {
    (StatusCode::NOT_FOUND, Html(pages::dead_link())).into_response()
}

/// `POST /join/<token>`: password, again.
pub async fn join(
    State(state): State<AppState>,
    UrlPath(token): UrlPath<String>,
    headers: HeaderMap,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let Some(t) = b64::token(&token) else {
        return Ok(dead_link());
    };
    let f = form(&body);
    let pw = field(&f, "password").to_owned();
    let error = password::check_new(&pw, field(&f, "again")).err();
    let agent = device(header(&headers, "user-agent").unwrap_or_default());
    let addr = client_addr(&state, peer, &headers);
    let secure = https(&headers);
    let done = state
        .accounts(move |a| {
            let Some(account) = a.link(&t)? else {
                return Ok(Err(None));
            };
            if let Some(e) = error {
                return Ok(Err(Some((account, e))));
            }
            a.set_password(account.id, &password::hash(&pw), None)?;
            Ok(Ok((
                a.key()?,
                a.new_session(account.id, true, &agent, &addr)?,
            )))
        })
        .await?;
    Ok(match done {
        Ok((key, token)) => signed_in(&key, &token, true, secure, "/", false),
        Err(None) => dead_link(),
        Err(Some((account, e))) => (
            StatusCode::BAD_REQUEST,
            Html(pages::join(&account, Some(e))),
        )
            .into_response(),
    })
}

// Passkeys.

fn json(status: StatusCode, value: serde_json::Value) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        value.to_string(),
    )
        .into_response()
}

fn host(headers: &HeaderMap) -> String {
    headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// `POST /passkey/signin/begin`: a challenge to sign in with a passkey.
pub async fn passkey_signin_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let c = state
        .accounts(|a| Ok(a.new_challenge("get", None)?))
        .await?;
    Ok(json(
        StatusCode::OK,
        serde_json::json!({ "challenge": b64::encode(&c), "rpId": webauthn::rp_id(&host(&headers)) }),
    ))
}

/// `POST /passkey/signin/finish`: the browser's answer (JSON: `id`,
/// `client`, `auth`, `sig`, `next`, all but `next` base64url).
pub async fn passkey_signin_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Peer(peer): Peer,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let refused = || {
        json(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "error": "passkey refused" }),
        )
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Ok(refused());
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(b64::decode);
    let (Some(id), Some(client), Some(auth), Some(sig)) =
        (get("id"), get("client"), get("auth"), get("sig"))
    else {
        return Ok(refused());
    };
    let next = safe_next(v.get("next").and_then(|x| x.as_str()));
    let host = host(&headers);
    let agent = device(header(&headers, "user-agent").unwrap_or_default());
    let addr = client_addr(&state, peer, &headers);
    let secure = https(&headers);
    let signed = state
        .accounts(move |a| {
            let Ok(cd) = webauthn::client_data(&client) else {
                return Ok(None);
            };
            if cd.kind != "webauthn.get"
                || !webauthn::origin_ok(&cd.origin, &host)
                || a.take_challenge(&cd.challenge, "get")?.is_none()
            {
                return Ok(None);
            }
            let Some(key) = a.passkey_by_id(&id)? else {
                return Ok(None);
            };
            let Ok(counter) =
                webauthn::assertion(&auth, &client, &sig, webauthn::rp_id(&host), &key)
            else {
                return Ok(None);
            };
            a.passkey_used(&id, counter)?;
            let Some(account) = a.by_id(key.account)? else {
                return Ok(None);
            };
            Ok(Some((
                a.key()?,
                a.new_session(account.id, true, &agent, &addr)?,
            )))
        })
        .await?;
    let Some((key, token)) = signed else {
        return Ok(refused());
    };
    let mut r = json(StatusCode::OK, serde_json::json!({ "next": next }));
    if let Ok(v) = HeaderValue::from_str(&session_cookie(&key, &token, true, secure)) {
        r.headers_mut().insert(SET_COOKIE, v);
    }
    Ok(r)
}

/// `POST /passkey/new`: a challenge to add a passkey to one's account.
pub async fn passkey_new(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<Viewer>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let id = me.id;
    let (c, have) = state
        .accounts(move |a| {
            let have: Vec<String> = a.passkeys(id)?.iter().map(|p| b64::encode(&p.id)).collect();
            Ok((a.new_challenge("create", Some(id))?, have))
        })
        .await?;
    Ok(json(
        StatusCode::OK,
        serde_json::json!({
            "challenge": b64::encode(&c),
            "rpId": webauthn::rp_id(&host(&headers)),
            "user": b64::encode(&me.id.to_le_bytes()),
            "name": me.name,
            "have": have,
        }),
    ))
}

/// `POST /passkey/add`: the new passkey (JSON: `client`, `att`
/// base64url, `name`).
pub async fn passkey_add(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<Viewer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let refused = |why: &str| json(StatusCode::BAD_REQUEST, serde_json::json!({ "error": why }));
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Ok(refused("not JSON"));
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(b64::decode);
    let (Some(client), Some(att)) = (get("client"), get("att")) else {
        return Ok(refused("missing fields"));
    };
    let name: String = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    let host = host(&headers);
    let id = me.id;
    let added = state
        .accounts(move |a| {
            let cd = match webauthn::client_data(&client) {
                Ok(cd) => cd,
                Err(e) => return Ok(Err(e)),
            };
            if cd.kind != "webauthn.create" || !webauthn::origin_ok(&cd.origin, &host) {
                return Ok(Err("wrong origin"));
            }
            if a.take_challenge(&cd.challenge, "create")? != Some(Some(id)) {
                return Ok(Err("unknown challenge"));
            }
            let cred = match webauthn::attestation(&att, webauthn::rp_id(&host)) {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let n = a.passkeys(id)?.len() + 1;
            let name = if name.trim().is_empty() {
                format!("passkey {n}")
            } else {
                name
            };
            let ok = a.add_passkey(&crate::accounts::Passkey {
                id: cred.id,
                account: id,
                name,
                alg: cred.alg,
                key: cred.key,
                counter: cred.counter,
                added: 0,
                used: None,
            })?;
            Ok(if ok {
                Ok(())
            } else {
                Err("that passkey is already known")
            })
        })
        .await?;
    Ok(match added {
        Ok(()) => json(StatusCode::OK, serde_json::json!({ "ok": true })),
        Err(e) => refused(e),
    })
}

/// The account behind `me` must be the admin.
pub fn admin_only(me: &Viewer, what: &str) -> Result<(), Failure> {
    if me.is_admin() {
        Ok(())
    } else {
        Err(Failure::Forbidden(format!(
            "Only the admin can {what}. You are signed in as {}, a {}.",
            me.name,
            me.role.as_str()
        )))
    }
}

/// Opens accounts.db for a command run in the shell (`koma account`).
pub fn open(data: &std::path::Path) -> Result<Accounts, crate::accounts::Error> {
    std::fs::create_dir_all(data).map_err(|_| crate::accounts::Error::InvalidQuery)?;
    Accounts::open(&data.join("accounts.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks() {
        let n = IpNet::parse("10.0.0.0/8").unwrap();
        assert!(n.contains("10.1.2.3".parse().unwrap()));
        assert!(!n.contains("11.0.0.1".parse().unwrap()));
        assert!(n.contains("::ffff:10.0.0.1".parse().unwrap()));
        let one = IpNet::parse("192.168.1.20").unwrap();
        assert!(one.contains("192.168.1.20".parse().unwrap()));
        assert!(!one.contains("192.168.1.21".parse().unwrap()));
        assert!(
            IpNet::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert!(
            IpNet::parse("::1")
                .unwrap()
                .contains("::1".parse().unwrap())
        );
        assert!(
            IpNet::parse("fd00::/8")
                .unwrap()
                .contains("fd12::1".parse().unwrap())
        );
        assert!(IpNet::parse("10.0.0.0/33").is_err());
        assert!(IpNet::parse("proxy").is_err());
    }

    #[test]
    fn config() {
        assert_eq!(Config::parse(None, None), Ok(Config::default()));
        assert!(Config::parse(Some("Remote-User"), None).is_err());
        assert!(Config::parse(Some("Remote User"), Some("10.0.0.2")).is_err());
        let c = Config::parse(Some("Remote-User"), Some("10.0.0.2, 172.16.0.0/12")).unwrap();
        assert!(c.trusts("172.20.0.1".parse().unwrap()));
        assert!(!c.trusts("10.0.0.3".parse().unwrap()));
    }

    #[test]
    fn signed_cookies() {
        let key = [7u8; 32];
        let token = [9u8; 32];
        let v = sign(&key, &token);
        assert_eq!(unsign(&key, &v), Some(token));
        assert_eq!(unsign(&[8u8; 32], &v), None);
        let mut forged = v.clone();
        forged.replace_range(0..1, if v.starts_with('A') { "B" } else { "A" });
        assert_eq!(unsign(&key, &forged), None);
        assert_eq!(unsign(&key, "nonsense"), None);
    }

    #[test]
    fn next_stays_on_the_site() {
        assert_eq!(safe_next(Some("/s/4?x=1")), "/s/4?x=1");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(Some("/\\evil")), "/");
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn origins() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("koma.local:8080"));
        assert!(same_origin(&h), "no Origin: an old browser or a script");
        h.insert("origin", HeaderValue::from_static("http://koma.local:8080"));
        assert!(same_origin(&h));
        h.insert("origin", HeaderValue::from_static("https://evil.example"));
        assert!(!same_origin(&h));
        h.remove("origin");
        h.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&h));
    }

    #[test]
    fn devices() {
        assert_eq!(
            device("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"),
            "Firefox on Linux"
        );
        assert_eq!(
            device(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1"
            ),
            "Safari on iPhone"
        );
        assert_eq!(
            device(
                "Mozilla/5.0 (Linux; Android 15) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Mobile Safari/537.36"
            ),
            "Chrome on Android"
        );
        assert_eq!(device("curl/8.0"), "A browser");
    }
}
