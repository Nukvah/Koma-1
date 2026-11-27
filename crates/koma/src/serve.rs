//! `koma serve`: the HTTP server. It only reads: the catalogue (read-only)
//! and the bytes of pages, straight from their archive or the blob store.
//! Nothing is decompressed, resized or written at request time.
//!
//! | Route | |
//! |---|---|
//! | `GET /` | the library: every series |
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
use axum::extract::{Path as UrlPath, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;

use crate::blobs::BlobStore;
use crate::catalog::{self, Catalog};
use crate::cli::Paths;
use crate::{html, log, signal};

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
        .route("/s/{id}", get(series))
        .route("/r/{book}/{page}", get(reader))
        .route("/p/{file}", get(page_bytes))
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], "ok\n")
}

async fn library(State(state): State<AppState>) -> Result<Html<String>, Failure> {
    let series = state.query(|c| Ok(c.series_with_counts()?)).await?;
    Ok(Html(html::library(&series)))
}

async fn series(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Html<String>, Failure> {
    state
        .query(move |c| {
            let series = c.series_by_id(id)?.ok_or(Failure::NotFound)?;
            let books = c.books(id)?;
            Ok(Html(html::series(&series, &books)))
        })
        .await
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
