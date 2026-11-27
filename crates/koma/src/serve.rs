//! `koma serve`: the HTTP server. It only reads: the catalogue (read-only)
//! and the bytes of pages, straight from their archive or the blob store.
//! Nothing is decompressed, resized or written at request time.
//!
//! | Route | |
//! |---|---|
//! | `GET /` | the default library (or the first): its series and collections |
//! | `GET /l/{folder}` | a library: its series and collections |
//! | `GET /settings` | the libraries and their settings, read-only |
//! | `GET /search?q=…&l=…` | search, in every library or one |
//! | `GET /a/koma.{hash}.css` | the stylesheet, cached for a year |
//! | `GET /s/{id}` | a series: its books |
//! | `GET /r/{book}/{n}` | the reader, page `n` (1-based) |
//! | `GET /p/{hash}.{ext}` | a page's bytes, cached for a year |
//! | `GET /p/{hash}.{width}.v{version}.{format}` | a page variant (`jpg` or `webp`), cached for a year |
//! | `GET /healthz` | health check |

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;

use crate::blobs::BlobStore;
use crate::catalog::{self, Catalog};
use crate::cli::Paths;
use crate::libraries::{self, Library};
use crate::{html, log, signal, variants};

/// What every request handler shares. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    library: PathBuf,
    blobs: BlobStore,
    db: PathBuf,
    /// Idle read-only connections; one is opened whenever none is free.
    idle: Mutex<Vec<Catalog>>,
}

impl AppState {
    pub fn new(paths: &Paths) -> Self {
        Self {
            inner: Arc::new(Inner {
                library: paths.library.clone(),
                blobs: BlobStore::new(&paths.data),
                db: paths.data.join("koma.db"),
                idle: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs `f` with a catalogue connection on a blocking thread.
    async fn query<T, F>(&self, f: F) -> Result<T, Failure>
    where
        T: Send + 'static,
        F: FnOnce(&Catalog) -> Result<T, Failure> + Send + 'static,
    {
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let inner = &state.inner;
            let catalog = inner.idle.lock().ok().and_then(|mut idle| idle.pop());
            let catalog = match catalog {
                Some(c) => c,
                None if !inner.db.exists() => return Err(Failure::NotScanned),
                None => Catalog::open_read_only(&inner.db)?,
            };
            let result = f(&catalog);
            if let Ok(mut idle) = inner.idle.lock() {
                idle.push(catalog);
            }
            result
        })
        .await
        .map_err(|_| Failure::Internal("task failed".into()))?
    }
}

/// Why a request could not be answered.
enum Failure {
    NotFound,
    NotScanned,
    Internal(String),
}

impl From<catalog::Error> for Failure {
    fn from(e: catalog::Error) -> Self {
        Failure::Internal(e.to_string())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Failure::NotFound => (StatusCode::NOT_FOUND, "not found\n").into_response(),
            Failure::NotScanned => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The library has not been scanned yet. Start koma worker.\n",
            )
                .into_response(),
            Failure::Internal(e) => {
                log::error("serve: request failed", &[("error", &e)]);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
            }
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(library))
        .route("/l/{folder}", get(named_library))
        .route("/settings", get(settings))
        .route("/search", get(search))
        .route("/a/{file}", get(stylesheet))
        .route("/s/{id}", get(series))
        .route("/r/{book}/{page}", get(reader))
        .route("/p/{file}", get(page_bytes))
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], "ok\n")
}

/// `GET /`: the default library, or the first one if every series is in a
/// library folder. `?c=<collection>` and `?page=<n>` as for [`named_library`].
async fn library(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Html<String>, Failure> {
    state
        .query(move |c| {
            let libraries = Library::all(c.library_keys()?);
            if libraries.is_empty() {
                return Ok(Html(html::empty()));
            }
            shelf(c, &libraries, 0, query.as_deref())
        })
        .await
}

/// `GET /l/<folder>`: one library: its collections as stacks, then its
/// series. `?c=<collection>` shows one collection's series; `?page=<n>` the
/// `n`th page of cards.
async fn named_library(
    State(state): State<AppState>,
    UrlPath(folder): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> Result<Html<String>, Failure> {
    state
        .query(move |c| {
            let libraries = Library::all(c.library_keys()?);
            let at = libraries
                .iter()
                .position(|l| !l.folder.is_empty() && l.folder == folder)
                .ok_or(Failure::NotFound)?;
            shelf(c, &libraries, at, query.as_deref())
        })
        .await
}

/// The cards of one library page, with covers looked up only for the cards
/// on that page.
fn shelf(
    c: &Catalog,
    libraries: &[Library],
    at: usize,
    query: Option<&str>,
) -> Result<Html<String>, Failure> {
    let library = &libraries[at];
    let collection = query_value(query, "c");
    let page = match query_value(query, "page") {
        None => 1,
        Some(p) => p
            .parse::<usize>()
            .ok()
            .filter(|&p| p > 0)
            .ok_or(Failure::NotFound)?,
    };
    let all = c.series_in_library(&library.folder)?;
    // Collections in natural order, each with its series.
    let mut groups: Vec<(String, Vec<&(catalog::Series, i64)>)> = Vec::new();
    for item in &all {
        if let Some(name) = libraries::collection_of(&item.0.path) {
            match groups.iter_mut().find(|(n, _)| n == name) {
                Some((_, list)) => list.push(item),
                None => groups.push((name.to_owned(), vec![item])),
            }
        }
    }
    groups.sort_by(|a, b| crate::natsort::compare_paths(&a.0, &b.0));
    let collections: Vec<String> = groups.iter().map(|(n, _)| n.clone()).collect();

    enum Card<'a> {
        Series(&'a (catalog::Series, i64)),
        Collection(&'a str, &'a [&'a (catalog::Series, i64)]),
    }
    let cards: Vec<Card<'_>> = match &collection {
        Some(name) => {
            let (_, list) = groups
                .iter()
                .find(|(n, _)| n == name)
                .ok_or(Failure::NotFound)?;
            list.iter().map(|s| Card::Series(s)).collect()
        }
        None => groups
            .iter()
            .map(|(n, list)| Card::Collection(n, list))
            .chain(
                all.iter()
                    .filter(|s| libraries::collection_of(&s.0.path).is_none())
                    .map(Card::Series),
            )
            .collect(),
    };
    let start = (page - 1) * html::PER_PAGE;
    if start >= cards.len() && page > 1 {
        return Err(Failure::NotFound);
    }
    let series_cover = |id| series_cover(c, id);
    let mut items = Vec::new();
    for card in cards.iter().skip(start).take(html::PER_PAGE) {
        items.push(match card {
            Card::Series((series, books)) => html::Item::Series {
                cover: series_cover(series.id)?,
                series: series.clone(),
                books: *books,
            },
            Card::Collection(name, list) => html::Item::Collection {
                name: (*name).to_owned(),
                series: list.len() as i64,
                books: list.iter().map(|(_, b)| b).sum(),
                cover: series_cover(list[0].0.id)?,
            },
        });
    }
    Ok(Html(html::library(&html::Shelf {
        libraries,
        library,
        collections: &collections,
        collection: collection.as_deref(),
        items: &items,
        page,
        total: cards.len(),
    })))
}

/// A series' cover: its first book's.
fn series_cover(c: &Catalog, series_id: i64) -> Result<Option<catalog::Cover>, Failure> {
    Ok(match c.first_book(series_id)? {
        Some(book) => c.book_cover(book, html::COVER_WIDTH, variants::VERSION)?,
        None => None,
    })
}

/// Most series, books and people/genres/tags shown for one search.
const SEARCH_SERIES: usize = 48;
const SEARCH_BOOKS: usize = 50;
const SEARCH_TERMS: usize = 20;

/// `GET /search?q=<query>&l=<library folder>`: series, books, then people,
/// genres and tags that match. See [`crate::search`].
async fn search(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Html<String>, Failure> {
    let q = query_value(query.as_deref(), "q").unwrap_or_default();
    let library = query_value(query.as_deref(), "l");
    state
        .query(move |c| {
            let libraries = Library::all(c.library_keys()?);
            let library = match &library {
                Some(folder) => Some(
                    libraries
                        .iter()
                        .find(|l| &l.folder == folder)
                        .ok_or(Failure::NotFound)?,
                ),
                None => None,
            };
            let terms = crate::search::parse(&q);
            let needles: Vec<&str> = terms.iter().map(|t| t.text.as_str()).collect();
            let mut series = Vec::new();
            let mut books = Vec::new();
            let mut people = Vec::new();
            if !terms.is_empty() {
                let plan = crate::search::plan(&terms);
                let likes: Vec<(&str, String)> = plan.likes.clone();
                let hits = c.search(
                    plan.fts.as_deref(),
                    &likes,
                    library.map(|l| l.folder.as_str()),
                    SEARCH_SERIES + SEARCH_BOOKS,
                )?;
                let name_of = |folder: &str| {
                    libraries
                        .iter()
                        .find(|l| l.folder == folder)
                        .map_or_else(String::new, |l| l.name.clone())
                };
                for (kind, id) in hits {
                    if kind == "s" && series.len() < SEARCH_SERIES {
                        let Some(s) = c.series_by_id(id)? else {
                            continue;
                        };
                        let count = c.books(id)?.len() as i64;
                        let lib = name_of(libraries::library_of(&s.path));
                        series.push((s, count, series_cover(c, id)?, lib));
                    } else if kind == "b" && books.len() < SEARCH_BOOKS {
                        let Some(b) = c.book(id)? else { continue };
                        let Some(s) = c.series_by_id(b.series_id)? else {
                            continue;
                        };
                        let lib = name_of(libraries::library_of(&s.path));
                        books.push((b, s.name, lib));
                    }
                }
                // People, genres and tags named like the first plain term.
                if library.is_none()
                    && let Some(t) = terms.iter().find(|t| t.field == crate::search::Field::Any)
                {
                    people =
                        c.matching_terms(&crate::search::like_pattern(&t.text), SEARCH_TERMS)?;
                }
            }
            Ok(Html(html::search(&html::Found {
                libraries: &libraries,
                query: &q,
                needles: &needles,
                library,
                series: &series,
                books: &books,
                terms: &people,
            })))
        })
        .await
}

/// The value of `key` in a URL query string, percent-decoded (`+` as a
/// space). `None` if absent or not valid UTF-8 once decoded.
fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then(|| percent_decode(v)).flatten()
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `GET /settings`: the libraries and their settings, read-only.
async fn settings(State(state): State<AppState>) -> Result<Html<String>, Failure> {
    state
        .query(|c| Ok(Html(html::settings(&Library::all(c.library_keys()?)))))
        .await
}

/// `GET /a/koma.<hash>.css`: the stylesheet, cached for good (its URL
/// changes with its content).
async fn stylesheet(UrlPath(file): UrlPath<String>) -> Result<Response, Failure> {
    if format!("/a/{file}") != html::css_url() {
        return Err(Failure::NotFound);
    }
    Ok((
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        html::CSS,
    )
        .into_response())
}

/// `GET /s/<id>`: a series and its books.
async fn series(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Html<String>, Failure> {
    state
        .query(move |c| {
            let series = c.series_by_id(id)?.ok_or(Failure::NotFound)?;
            let mut books = Vec::new();
            for book in c.books(id)? {
                let cover = c.book_cover(book.id, html::COVER_WIDTH, variants::VERSION)?;
                books.push((book, cover));
            }
            if books.is_empty() {
                return Err(Failure::NotFound);
            }
            let libraries = Library::all(c.library_keys()?);
            let folder = libraries::library_of(&series.path);
            let library = libraries
                .iter()
                .find(|l| l.folder == folder)
                .ok_or(Failure::NotFound)?;
            let collections = collections_of(c, &library.folder)?;
            Ok(Html(html::series(
                &libraries,
                library,
                &collections,
                &series,
                &books,
            )))
        })
        .await
}

/// A library's collections, in natural order.
fn collections_of(c: &Catalog, library: &str) -> Result<Vec<String>, Failure> {
    let mut names: Vec<String> = Vec::new();
    for (s, _) in c.series_in_library(library)? {
        if let Some(n) = libraries::collection_of(&s.path)
            && !names.iter().any(|x| x == n)
        {
            names.push(n.to_owned());
        }
    }
    names.sort_by(|a, b| crate::natsort::compare_paths(a, b));
    Ok(names)
}

async fn reader(
    State(state): State<AppState>,
    UrlPath((book_id, number)): UrlPath<(i64, usize)>,
) -> Result<Html<String>, Failure> {
    state
        .query(move |c| {
            let book = c.book(book_id)?.ok_or(Failure::NotFound)?;
            let series = c.series_by_id(book.series_id)?.ok_or(Failure::NotFound)?;
            let pages = c.pages(book_id)?;
            if number == 0 || number > pages.len() {
                return Err(Failure::NotFound);
            }
            Ok(Html(html::reader(&series, &book, &pages, number)))
        })
        .await
}

/// `GET /p/<hash>.<ext>`: the page's bytes, from the archive in place or
/// from the blob store. The URL names the content, so it is cached forever.
async fn page_bytes(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let (hex, rest) = file.split_once('.').ok_or(Failure::NotFound)?;
    let hash = blake3::Hash::from_hex(hex).map_err(|_| Failure::NotFound)?;
    // `<hash>.<ext>`: the page as it is; `<hash>.<width>.v<version>.<ext>`: a variant.
    let (variant, ext) = match rest.split_once('.') {
        None => (None, rest),
        Some((w, rest)) => {
            let (v, ext) = rest.split_once('.').ok_or(Failure::NotFound)?;
            let width = number(w).ok_or(Failure::NotFound)?;
            let version = v
                .strip_prefix('v')
                .and_then(number)
                .ok_or(Failure::NotFound)?;
            (Some((width, i64::from(version))), ext)
        }
    };
    let content_type = content_type(ext).ok_or(Failure::NotFound)?;
    let etag = match variant {
        None => format!("\"{hex}\""),
        Some((w, v)) => format!("\"{hex}.{w}.v{v}\""),
    };
    if headers
        .get(IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes())
    {
        return Ok((StatusCode::NOT_MODIFIED, [(ETAG, etag)]).into_response());
    }

    let ext = ext.to_owned();
    let inner = Arc::clone(&state.inner);
    let bytes = state
        .query(move |c| {
            if let Some((width, version)) = variant {
                if crate::variants::Format::from_ext(&ext).is_none()
                    || !c.has_variant(&hash, width, version, &ext)?
                {
                    return Err(Failure::NotFound);
                }
                let path = inner.blobs.variant_path(&hash, width, version, &ext);
                return std::fs::read(&path)
                    .map_err(|e| Failure::Internal(format!("{}: {e}", path.display())));
            }
            let page = c.page_by_hash(&hash)?.ok_or(Failure::NotFound)?;
            if page.format != ext {
                return Err(Failure::NotFound);
            }
            let read = match page.offset {
                Some(offset) => {
                    let path = inside(&inner.library, &page.book_path)?;
                    read_range(&path, offset, page.length)
                }
                None => std::fs::read(inner.blobs.path(&hash, &page.format)),
            };
            read.map_err(|e| Failure::Internal(format!("{}: {e}", page.book_path)))
        })
        .await?;
    let headers = [
        (CONTENT_TYPE, HeaderValue::from_static(content_type)),
        (
            CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        ),
        (
            ETAG,
            HeaderValue::from_str(&etag).map_err(|e| Failure::Internal(e.to_string()))?,
        ),
    ];
    Ok((headers, bytes).into_response())
}

/// A variant's width or version as written in a URL: digits only, no
/// leading zero, so each variant has exactly one URL.
fn number(w: &str) -> Option<u32> {
    if w.is_empty() || w.len() > 5 || w.starts_with('0') || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse().ok()
}

/// Joins a catalogue path onto the library, refusing anything that could
/// step outside it.
fn inside(root: &Path, rel: &str) -> Result<PathBuf, Failure> {
    let rel = Path::new(rel);
    if rel.components().all(|c| matches!(c, Component::Normal(_))) {
        Ok(root.join(rel))
    } else {
        Err(Failure::Internal(format!(
            "unsafe path in catalogue: {}",
            rel.display()
        )))
    }
}

fn read_range(path: &Path, offset: u64, length: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; usize::try_from(length).map_err(io::Error::other)?];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "jxl" => "image/jxl",
        _ => return None,
    })
}

/// Serves on `listener` until `shutdown` resolves, then finishes open requests.
pub async fn serve<F>(listener: TcpListener, state: AppState, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

pub fn run(listen: SocketAddr, paths: &Paths) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error("serve: cannot start runtime", &[("error", &e.to_string())]);
            return ExitCode::FAILURE;
        }
    };
    let state = AppState::new(paths);
    let result = runtime.block_on(async {
        let listener = TcpListener::bind(listen).await?;
        log::info(
            "serve: listening",
            &[
                ("addr", &listener.local_addr()?.to_string()),
                ("library", &paths.library.to_string_lossy()),
                ("data", &paths.data.to_string_lossy()),
            ],
        );
        serve(listener, state, signal::shutdown()).await
    });
    match result {
        Ok(()) => {
            log::info("serve: stopped", &[]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error(
                "serve: failed",
                &[("addr", &listen.to_string()), ("error", &e.to_string())],
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_library() {
        let root = Path::new("/library");
        assert!(inside(root, "Starfall/v01.cbz").is_ok());
        assert!(inside(root, "../etc/passwd").is_err());
        assert!(inside(root, "/etc/passwd").is_err());
        assert!(inside(root, "a/./b.cbz").is_ok()); // "." is dropped by components()
    }
}
