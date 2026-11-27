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
//! | `GET /a/koma.{hash}.css`, `/a/scroll.{hash}.js` | the stylesheet and the library script, cached for a year |
//! | `GET /s/{id}` | a series: its books |
//! | `GET /r/{book}/{n}` | the reader, page `n` (1-based) |
//! | `GET /b/{book}` | the book's pages and their variants, for the reader's script |
//! | `GET /d/{book}` | the book's original file (a photo folder as a stored ZIP) |
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
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;

use crate::blobs::BlobStore;
use crate::catalog::{self, Catalog};
use crate::cli::Paths;
use crate::libraries::{self, Library};
use crate::{download, html, log, signal, variants};

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
        .route("/d/{book}", get(download))
        .route("/b/{book}", get(book_json))
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
    if let Some(view) = query_value(query, "view") {
        if view != "table" {
            return Err(Failure::NotFound);
        }
        return table(c, libraries, library, collection.as_deref(), page, query);
    }
    let collections = c.collections(&library.folder)?;
    let names: Vec<String> = collections.iter().map(|(n, ..)| n.clone()).collect();
    let per = html::PER_PAGE;
    let start = (page - 1) * per;
    let mut items = Vec::new();
    let total = match collection.as_deref() {
        // One collection: its series.
        Some(name) => {
            if !names.iter().any(|n| n == name) {
                return Err(Failure::NotFound);
            }
            let (series, total) = c.shelf_series(&library.folder, name, start, per)?;
            for (s, books) in series {
                items.push(html::Item::Series {
                    cover: series_cover(c, s.id)?,
                    series: s,
                    books,
                });
            }
            total
        }
        // The library: its collections first, then the series in none.
        None => {
            for (name, series, books, first) in collections.iter().skip(start).take(per) {
                items.push(html::Item::Collection {
                    name: name.clone(),
                    series: *series,
                    books: *books,
                    cover: series_cover(c, *first)?,
                });
            }
            let skip = start.saturating_sub(collections.len());
            let room = per - items.len();
            let (series, lone) = c.shelf_series(&library.folder, "", skip, room)?;
            for (s, books) in series {
                items.push(html::Item::Series {
                    cover: series_cover(c, s.id)?,
                    series: s,
                    books,
                });
            }
            collections.len() + lone
        }
    };
    if start >= total && page > 1 {
        return Err(Failure::NotFound);
    }
    Ok(Html(html::library(&html::Shelf {
        libraries,
        library,
        collections: &names,
        collection: collection.as_deref(),
        items: &items,
        page,
        total,
    })))
}

/// A library (or one collection) as a sortable table: `?view=table`,
/// `&sort=<column>` (`-<column>` reversed), `&page=<n>`.
fn table(
    c: &Catalog,
    libraries: &[Library],
    library: &Library,
    collection: Option<&str>,
    page: usize,
    query: Option<&str>,
) -> Result<Html<String>, Failure> {
    let sort = match query_value(query, "sort") {
        None => (html::SortKey::Title, false),
        Some(s) => html::SortKey::parse(&s).ok_or(Failure::NotFound)?,
    };
    let collections = collections_of(c, &library.folder)?;
    if let Some(name) = collection
        && !collections.iter().any(|x| x == name)
    {
        return Err(Failure::NotFound);
    }
    let mut rows = c.series_rows(&library.folder, collection)?;
    // People and genres only where they are needed: every row to sort by
    // author, else the rows shown.
    if sort.0 == html::SortKey::Author {
        c.series_terms(&mut rows)?;
    }
    // Rows come in natural order of names: a stable sort keeps it for ties.
    use crate::natsort::compare_paths;
    match sort.0 {
        html::SortKey::Title => {}
        // Series with no author last.
        html::SortKey::Author => rows.sort_by(|a, b| {
            a.authors
                .is_empty()
                .cmp(&b.authors.is_empty())
                .then_with(|| compare_paths(&a.authors, &b.authors))
        }),
        html::SortKey::Books => rows.sort_by_key(|r| r.books),
        html::SortKey::Pages => rows.sort_by_key(|r| r.pages),
        html::SortKey::Added => rows.sort_by_key(|r| r.added),
    }
    if sort.1 {
        rows.reverse();
    }
    let start = (page - 1) * html::TABLE_PER_PAGE;
    if start >= rows.len() && page > 1 {
        return Err(Failure::NotFound);
    }
    let mut shown: Vec<_> = rows
        .iter()
        .skip(start)
        .take(html::TABLE_PER_PAGE)
        .cloned()
        .collect();
    if sort.0 != html::SortKey::Author {
        c.series_terms(&mut shown)?;
    }
    Ok(Html(html::table(&html::Table {
        libraries,
        library,
        collections: &collections,
        collection,
        rows: &shown,
        sort,
        page,
        total: rows.len(),
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

/// `GET /a/koma.<hash>.css` and `/a/scroll.<hash>.js`: the stylesheet and
/// the library pages' script, cached for good (their URLs change with their
/// content).
async fn stylesheet(UrlPath(file): UrlPath<String>) -> Result<Response, Failure> {
    let url = format!("/a/{file}");
    let (content_type, body) = if url == html::css_url() {
        ("text/css; charset=utf-8", html::CSS)
    } else if url == html::scroll_url() {
        ("text/javascript; charset=utf-8", html::SCROLL_JS)
    } else if url == html::reader_css_url() {
        ("text/css; charset=utf-8", html::READER_CSS)
    } else if url == html::reader_js_url() {
        ("text/javascript; charset=utf-8", html::READER_JS)
    } else {
        return Err(Failure::NotFound);
    };
    Ok((
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
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
    Ok(c.collections(library)?
        .into_iter()
        .map(|(n, ..)| n)
        .collect())
}

async fn reader(
    State(state): State<AppState>,
    UrlPath((book_id, number)): UrlPath<(i64, usize)>,
) -> Result<Html<String>, Failure> {
    state
        .query(move |c| {
            // Only what the page shows: this page and its variants (the
            // script gets them all once, from /b/<book>).
            let book = c.book(book_id)?.ok_or(Failure::NotFound)?;
            let total = usize::try_from(book.page_count).unwrap_or(0);
            if number == 0 || number > total {
                return Err(Failure::NotFound);
            }
            let page = c
                .page(book_id, number as i64 - 1)?
                .ok_or(Failure::NotFound)?;
            let mut made: Vec<(u32, u8)> = Vec::new();
            for (width, format) in c.page_variants(&page.hash, variants::VERSION)? {
                let bit = if format == "jpg" { 1 } else { 2 };
                match made.iter_mut().find(|(w, _)| *w == width) {
                    Some((_, m)) => *m |= bit,
                    None => made.push((width, bit)),
                }
            }
            let page = html::ReaderPage::new(&page, made);
            let series = c.series_by_id(book.series_id)?.ok_or(Failure::NotFound)?;
            // The book's own direction, else its library's.
            let rtl = match c.book_direction(book_id)? {
                Some(d) => d == "rtl",
                None => {
                    let folder = libraries::library_of(&series.path);
                    Library::from_folder(folder, 0, 0).direction
                        == libraries::Direction::RightToLeft
                }
            };
            Ok(Html(html::reader(&html::Reader {
                series: &series,
                book: &book,
                page: &page,
                number,
                total,
                rtl,
            })))
        })
        .await
}

/// A book's pages with their variants, for the reader.
fn reader_pages(c: &Catalog, book_id: i64) -> Result<Vec<html::ReaderPage>, Failure> {
    let mut made: std::collections::HashMap<i64, Vec<(u32, u8)>> = Default::default();
    for (number, width, format) in c.book_variants(book_id, variants::VERSION)? {
        let bit = if format == "jpg" { 1 } else { 2 };
        let list = made.entry(number).or_default();
        match list.iter_mut().find(|(w, _)| *w == width) {
            Some((_, m)) => *m |= bit,
            None => list.push((width, bit)),
        }
    }
    Ok(c.pages(book_id)?
        .iter()
        .map(|p| html::ReaderPage::new(p, made.remove(&p.number).unwrap_or_default()))
        .collect())
}

/// `GET /b/<book>`: the book's pages and their variants, for the reader's
/// script (see [`html::reader_json`]).
async fn book_json(
    State(state): State<AppState>,
    UrlPath(book_id): UrlPath<i64>,
) -> Result<Response, Failure> {
    let json = state
        .query(move |c| {
            c.book(book_id)?.ok_or(Failure::NotFound)?;
            Ok(html::reader_json(&reader_pages(c, book_id)?))
        })
        .await?;
    Ok((
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-cache"),
        ],
        json,
    )
        .into_response())
}

/// `GET /d/<book>`: the book's original file, sent as it is; a photo folder
/// as a stored ZIP of its images, made while it is sent.
async fn download(
    State(state): State<AppState>,
    UrlPath(book_id): UrlPath<i64>,
) -> Result<Response, Failure> {
    let inner = Arc::clone(&state.inner);
    let (body, name, media_type) = state
        .query(move |c| {
            let book = c.book(book_id)?.ok_or(Failure::NotFound)?;
            if book.photos {
                let mut entries = Vec::new();
                for page in c.pages(book_id)? {
                    let file = page.file.ok_or(Failure::NotFound)?;
                    let name = file.rsplit('/').next().unwrap_or(&file).to_owned();
                    entries.push(download::Entry {
                        name,
                        path: inside(&inner.library, &file)?,
                        len: page.length,
                    });
                }
                // Too big for a plain ZIP: copy the folder instead.
                let len = download::zip_len(&entries).ok_or(Failure::NotFound)?;
                let body = download::spawn(len, move |out| download::write_zip(&entries, out));
                Ok((body, format!("{}.zip", book.name), "application/zip"))
            } else {
                let path = inside(&inner.library, &book.path)?;
                let file_name = book
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&book.path)
                    .to_owned();
                let len = book.size;
                let body = download::spawn(len, move |out| download::copy_file(&path, len, out));
                Ok((body, file_name.clone(), download::media_type(&file_name)))
            }
        })
        .await?;
    let disposition = HeaderValue::from_str(&download::disposition(&name))
        .map_err(|e| Failure::Internal(e.to_string()))?;
    Ok((
        [
            (CONTENT_TYPE, HeaderValue::from_static(media_type)),
            (CONTENT_DISPOSITION, disposition),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        axum::body::Body::new(body),
    )
        .into_response())
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
    use axum::serve::ListenerExt;
    // No Nagle delay: a response written in two parts (head, then body)
    // would otherwise wait for the client's delayed ACK of the previous
    // response before its first byte leaves, adding up to tens of
    // milliseconds on keep-alive connections.
    let listener = listener.tap_io(|tcp| {
        if let Err(e) = tcp.set_nodelay(true) {
            log::error(
                "serve: cannot set TCP_NODELAY",
                &[("error", &e.to_string())],
            );
        }
    });
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
