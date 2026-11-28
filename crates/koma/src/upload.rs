//! The upload API the drop page uses (docs/design/upload.html): uploads are
//! started, sent in pieces and finished here, into the staging folder
//! ([`crate::staging`]); the worker files them later. JSON in and out.
//!
//! | Request | Does |
//! |---|---|
//! | `POST /up` `{library, kind, name, size, batch}` | starts an upload: `{id}` |
//! | `GET /up/{id}` | where it got to: `{received, size, done, members}` |
//! | `PATCH /up/{id}?offset=<n>` | a file's next bytes, from `offset` (409 and `{received}` if not where the server is) |
//! | `PUT /up/{id}/m?name=<name>` | one image (or metadata file) of a pack |
//! | `POST /up/{id}/done` | all of it is there |
//! | `DELETE /up/{id}` | cancels it |
//!
//! A body cut off on the way (a dropped connection) keeps what arrived: the
//! browser asks `GET /up/{id}` and sends the rest. The bytes are written by a
//! task of their own, so a request dropped half-way still leaves the upload
//! consistent; until that task ends, the upload answers 409 "busy".

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{Html, IntoResponse, Response};
use http_body::Body as _;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::auth::Viewer;
use crate::catalog::{Catalog, Series};
use crate::curate::may_organise;
use crate::drops::{self, Known, Place, Target};
use crate::html::escape;
use crate::libraries::{Kind as LibraryKind, Library, library_of};
use crate::serve::{AppState, Failure};
use crate::staging::{Error, Kind, Staging, Upload};
use crate::{names, naming};

/// Uploads being written right now, by id.
#[derive(Clone, Default)]
pub struct Busy(Arc<Mutex<HashSet<String>>>);

/// Holds an upload for one writer; let go when dropped.
struct Hold {
    busy: Busy,
    id: String,
}

impl Busy {
    fn take(&self, id: &str) -> Option<Hold> {
        let mut set = self.0.lock().ok()?;
        set.insert(id.to_owned()).then(|| Hold {
            busy: self.clone(),
            id: id.to_owned(),
        })
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Ok(mut set) = self.busy.0.lock() {
            set.remove(&self.id);
        }
    }
}

fn reply(status: StatusCode, v: Value) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        v.to_string(),
    )
        .into_response()
}

fn refused(why: &str) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({ "error": why }))
}

/// The answer for a staging error.
fn error_reply(state: &Staging, u: Option<&Upload>, e: Error) -> Response {
    match e {
        Error::NotFound => reply(StatusCode::NOT_FOUND, json!({ "error": "no such upload" })),
        Error::Offset(n) => reply(
            StatusCode::CONFLICT,
            json!({ "error": e.to_string(), "received": n }),
        ),
        Error::Duplicate | Error::Busy => {
            reply(StatusCode::CONFLICT, json!({ "error": e.to_string() }))
        }
        Error::Refused(why) => refused(&why),
        Error::Cut(_) => {
            let received = u.map_or(0, |u| state.received(u));
            reply(
                StatusCode::BAD_REQUEST,
                json!({ "error": "cut off", "received": received }),
            )
        }
        Error::Io(e) => {
            crate::log::error("serve: upload failed", &[("error", &e.to_string())]);
            let status = if e.raw_os_error() == Some(28) {
                StatusCode::INSUFFICIENT_STORAGE // ENOSPC
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            reply(status, json!({ "error": "the server could not write it" }))
        }
    }
}

fn status_json(s: &Staging, u: &Upload) -> Value {
    json!({
        "id": u.id,
        "kind": u.kind.as_str(),
        "received": s.received(u),
        "size": u.size,
        "done": u.done,
        "members": u.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
    })
}

/// Runs `f` on a blocking thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Failure> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Failure::Internal("task failed".into()))
}

/// `POST /up`: starts an upload into a library the viewer may add to.
pub async fn start(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: Bytes,
) -> Result<Response, Failure> {
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return Ok(refused("not JSON"));
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (library, name, batch) = (s("library"), s("name"), s("batch"));
    let Some(kind) = Kind::parse(&s("kind")) else {
        return Ok(refused("kind is file or pack"));
    };
    let size = v.get("size").and_then(Value::as_u64).unwrap_or(0);
    let viewer = me.clone();
    let lib = library.clone();
    let exists = state
        .query(move |c| {
            Ok(crate::curate::libraries(c, &viewer)?
                .iter()
                .any(|l| l.folder == lib))
        })
        .await?;
    if !exists {
        return Err(Failure::NotFound);
    }
    if !may_organise(&me, &library) {
        return Err(crate::curate::refuse(&me));
    }
    let staging = state.staging().clone();
    let owner = me.id;
    let made = blocking(move || staging.create(owner, &library, kind, &name, size, &batch)).await?;
    match made {
        Ok(u) => Ok(reply(StatusCode::CREATED, status_json(state.staging(), &u))),
        Err(e) => Ok(error_reply(state.staging(), None, e)),
    }
}

/// `GET /up/{id}`.
pub async fn status(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Failure> {
    let staging = state.staging().clone();
    let owner = me.id;
    let got = blocking(move || staging.get(&id, owner).map(|u| status_json(&staging, &u))).await?;
    Ok(match got {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => error_reply(state.staging(), None, e),
    })
}

/// `DELETE /up/{id}`.
pub async fn cancel(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Failure> {
    let Some(hold) = state.upload_busy().take(&id) else {
        return Ok(error_reply(state.staging(), None, Error::Busy));
    };
    let staging = state.staging().clone();
    let owner = me.id;
    let got = blocking(move || {
        let _hold = hold;
        staging.get(&id, owner).map(|u| staging.remove(&u))
    })
    .await?;
    Ok(match got {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_reply(state.staging(), None, e),
    })
}

/// `POST /up/{id}/done`.
pub async fn finish(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Failure> {
    let Some(hold) = state.upload_busy().take(&id) else {
        return Ok(error_reply(state.staging(), None, Error::Busy));
    };
    let staging = state.staging().clone();
    let owner = me.id;
    let got = blocking(move || {
        let _hold = hold;
        let mut u = staging.get(&id, owner)?;
        staging.finish(&mut u)?;
        Ok::<_, Error>(status_json(&staging, &u))
    })
    .await?;
    Ok(match got {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => error_reply(state.staging(), None, e),
    })
}

/// A blocking reader over a request body, fed by a task of its own so the
/// bytes keep being written if the request goes away.
struct BodyReader {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    chunk: Bytes,
}

impl BodyReader {
    fn spawn(mut body: Body) -> Self {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
                let sent = match frame {
                    None => break,
                    Some(Ok(f)) => match f.into_data() {
                        Ok(data) => tx.send(Ok(data)).await,
                        Err(_) => continue, // trailers
                    },
                    Some(Err(e)) => {
                        let _ = tx.send(Err(io::Error::other(e.to_string()))).await;
                        break;
                    }
                };
                if sent.is_err() {
                    break;
                }
            }
        });
        Self {
            rx,
            chunk: Bytes::new(),
        }
    }
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.chunk.is_empty() {
            match self.rx.blocking_recv() {
                None => return Ok(0),
                Some(Ok(c)) => self.chunk = c,
                Some(Err(e)) => return Err(e),
            }
        }
        let n = buf.len().min(self.chunk.len());
        buf[..n].copy_from_slice(&self.chunk[..n]);
        self.chunk = self.chunk.slice(n..);
        Ok(n)
    }
}

/// Runs a write on a task of its own and waits for it.
async fn write<T: Send + 'static>(
    state: &AppState,
    id: &str,
    f: impl FnOnce(&Staging) -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    let Some(hold) = state.upload_busy().take(id) else {
        return Err(Error::Busy);
    };
    let staging = state.staging().clone();
    tokio::task::spawn_blocking(move || {
        let _hold = hold;
        f(&staging)
    })
    .await
    .unwrap_or_else(|_| Err(Error::Io(io::Error::other("task failed"))))
}

/// `PATCH /up/{id}?offset=<n>`: a file's next bytes.
pub async fn append(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    UrlPath(id): UrlPath<String>,
    RawQuery(q): RawQuery,
    body: Body,
) -> Result<Response, Failure> {
    let Some(offset) = crate::curate::query_all(q.as_deref(), "offset")
        .first()
        .and_then(|o| o.parse::<u64>().ok())
    else {
        return Ok(refused("offset missing"));
    };
    let owner = me.id;
    let reader = BodyReader::spawn(body);
    let key = id.clone();
    let got = write(&state, &key, move |s| {
        let u = s.get(&id, owner)?;
        let mut reader = reader;
        match s.append(&u, offset, &mut reader) {
            Ok(n) => Ok(json!({ "received": n })),
            Err(Error::Cut(_)) => Err(Error::Cut(s.received(&u))),
            Err(e) => Err(e),
        }
    })
    .await;
    Ok(match got {
        Ok(v) => reply(StatusCode::OK, v),
        Err(Error::Cut(n)) => reply(
            StatusCode::BAD_REQUEST,
            json!({ "error": "cut off", "received": n }),
        ),
        Err(e) => error_reply(state.staging(), None, e),
    })
}

/// `PUT /up/{id}/m?name=<name>`: one image of a pack.
pub async fn member(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    UrlPath(id): UrlPath<String>,
    RawQuery(q): RawQuery,
    body: Body,
) -> Result<Response, Failure> {
    let Some(name) = crate::curate::query_all(q.as_deref(), "name")
        .into_iter()
        .next()
    else {
        return Ok(refused("name missing"));
    };
    let owner = me.id;
    let reader = BodyReader::spawn(body);
    let key = id.clone();
    let got = write(&state, &key, move |s| {
        let mut u = s.get(&id, owner)?;
        let mut reader = reader;
        let n = s.add_member(&mut u, &name, &mut reader)?;
        Ok(json!({ "members": n }))
    })
    .await;
    Ok(match got {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => error_reply(state.staging(), None, e),
    })
}

// ---- planning a drop, and handing it to the worker

/// The library `folder`, if the viewer sees it and may add to it.
async fn library_for(state: &AppState, me: &Viewer, folder: &str) -> Result<Library, Failure> {
    let (viewer, f) = (me.clone(), folder.to_owned());
    let lib = state
        .query(move |c| {
            Ok(crate::curate::libraries(c, &viewer)?
                .into_iter()
                .find(|l| l.folder == f))
        })
        .await?
        .ok_or(Failure::NotFound)?;
    if !may_organise(me, &lib.folder) {
        return Err(crate::curate::refuse(me));
    }
    Ok(lib)
}

fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

/// What the library knows about the series at `path` (or with `id`).
fn known(c: &Catalog, series: Option<&Series>, cosplay: bool) -> Result<Known, Failure> {
    let Some(s) = series else {
        return Ok(Known::default());
    };
    let books = c.books(s.id)?;
    let mut details = Vec::new();
    for b in &books {
        if let Some(d) = c.details(&b.path)? {
            details.push(d);
        }
    }
    let kind = |k: &Option<String>| match k.as_deref() {
        Some("volume") => Some(names::Kind::Volume),
        Some("chapter") => Some(names::Kind::Chapter),
        _ => None,
    };
    let mut k = Known {
        series: Some(s.name.clone()),
        author: naming::most_common(details.iter().filter_map(|d| d.authors.first().cloned())),
        circle: naming::most_common(details.iter().filter_map(|d| d.publishers.first().cloned())),
        language: naming::most_common(
            details
                .iter()
                .filter_map(|d| naming::language_code(&d.language)),
        ),
        numbers: details
            .iter()
            .filter_map(|d| Some((kind(&d.kind), d.number?)))
            .collect(),
        books: books.len(),
    };
    if cosplay {
        // A set's cosplayer is the folder it is in.
        let mut parts = s.path.rsplit('/');
        k.series = parts.next().map(str::to_owned);
        k.author = parts.next().map(str::to_owned);
        k.circle = None;
    }
    Ok(k)
}

/// `POST /up/plan`: what a drop becomes, with the names it lands with.
/// `{library, series?, collection?, items, meta, today}`.
pub async fn plan(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: Bytes,
) -> Result<Response, Failure> {
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return Ok(refused("not JSON"));
    };
    let folder = v
        .get("library")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let lib = library_for(&state, &me, &folder).await?;
    let cosplay = lib.kind == LibraryKind::Cosplay;
    let target = match (
        v.get("series").and_then(Value::as_i64),
        v.get("collection").and_then(Value::as_str),
    ) {
        (Some(id), _) => Target::Series(id),
        (None, Some(c)) if !c.is_empty() => Target::Collection(c.to_owned()),
        _ => Target::Library,
    };
    let drop = match drops::Drop::from_json(&v, target.clone(), cosplay) {
        Ok(d) => d,
        Err(why) => return Ok(refused(&why)),
    };
    let viewer = me.clone();
    let out = state
        .query(move |c| {
            if let Target::Series(id) = target {
                let s = c.series_by_id(id)?.filter(|s| viewer.sees(&s.path));
                if s.is_none_or(|s| library_of(&s.path) != lib.folder || s.path.starts_with('\0')) {
                    return Err(Failure::NotFound);
                }
            }
            let mut plan = drops::plan(&drop);
            let in_library: Vec<(Series, i64)> = c.series_in_library(&lib.folder)?;
            let series_at = |path: &str| {
                in_library
                    .iter()
                    .find(|(s, _)| s.path == path)
                    .map(|(s, _)| s.clone())
            };
            let mut where_to: Vec<(String, Option<Series>)> = Vec::new();
            for j in &plan.jobs {
                let (rel, s) = match &j.place {
                    Place::Series(id) => {
                        let s = c.series_by_id(*id)?;
                        (s.as_ref().map(|s| s.path.clone()).unwrap_or_default(), s)
                    }
                    Place::Folder { collection, folder } => {
                        let rel = join(&[
                            &lib.folder,
                            collection.as_deref().unwrap_or_default(),
                            folder.as_deref().unwrap_or_default(),
                        ]);
                        let s = series_at(&rel);
                        (rel, s)
                    }
                };
                where_to.push((rel, s));
            }
            let knowns: Vec<(Place, Known)> = plan
                .jobs
                .iter()
                .zip(&where_to)
                .map(|(j, (_, s))| Ok((j.place.clone(), known(c, s.as_ref(), cosplay)?)))
                .collect::<Result<_, Failure>>()?;
            drops::name_to_standard(&mut plan, &drop, &|p| {
                knowns
                    .iter()
                    .find(|(q, _)| q == p)
                    .map(|(_, k)| k.clone())
                    .unwrap_or_default()
            });
            let mut out = plan.to_json();
            if let Some(jobs) = out.get_mut("jobs").and_then(Value::as_array_mut) {
                for (j, (rel, s)) in jobs.iter_mut().zip(&where_to) {
                    j["path"] = json!(rel);
                    j["new"] = json!(s.is_none());
                }
            }
            out["library"] = json!(lib.folder);
            out["cosplay"] = json!(cosplay);
            Ok(out)
        })
        .await?;
    Ok(reply(StatusCode::OK, out))
}

/// `POST /up/commit`: hands finished uploads to the worker, as one change.
/// `{library, batch, jobs: [{upload, path, name}], failed: [{name, why}],
/// cancelled, ignored}`: `path` is the folder each lands in, `name` the name
/// it lands with.
pub async fn commit(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: Bytes,
) -> Result<Response, Failure> {
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return Ok(refused("not JSON"));
    };
    let folder = v
        .get("library")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let lib = library_for(&state, &me, &folder).await?;
    let jobs = v
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for j in &jobs {
        let s = |k: &str| {
            j.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let (id, path, name) = (s("upload"), s("path"), s("name"));
        let in_library = if lib.folder.is_empty() {
            crate::libraries::library_of(&format!("{path}/x")).is_empty()
        } else {
            path.starts_with(&format!("{}/", lib.folder))
        };
        if !crate::organise::safe_path(&path) || !crate::organise::safe_name(&name) || !in_library {
            return Ok(refused(&format!("{name}: not a place in this library")));
        }
        let staging = state.staging().clone();
        let owner = me.id;
        let up = blocking(move || staging.get(&id, owner)).await?;
        match up {
            Ok(u) if u.library == lib.folder => out.push(json!({
                "upload": u.id, "folder": path, "name": name, "dropped": u.name,
            })),
            _ => return Ok(refused(&format!("{name}: no such upload"))),
        }
    }
    let n = out.len();
    let payload = json!({
        "library": lib.folder,
        "batch": v.get("batch").cloned().unwrap_or(Value::Null),
        "jobs": out,
        "failed": v.get("failed").cloned().unwrap_or(json!([])),
        "cancelled": v.get("cancelled").and_then(Value::as_u64).unwrap_or(0),
        "ignored": v.get("ignored").and_then(Value::as_u64).unwrap_or(0),
    });
    let title = format!(
        "Add {n} {} to {}",
        if n == 1 { "file" } else { "files" },
        lib.name
    );
    let (op, done) = crate::curate::queue(&state, &me, "upload", payload, title).await?;
    Ok(reply(
        StatusCode::OK,
        json!({
            "op": op,
            "state": done.as_ref().map(|s| s.state.clone()),
            "message": done.map(|s| s.message),
        }),
    ))
}

// ---- the pages

/// The add-files page's script: reading a drop, the review sheet, sending.
pub const UPLOAD_JS: &str = include_str!("../assets/curator/upload.js");

static UPLOAD_JS_URL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let hex = blake3::hash(UPLOAD_JS.as_bytes()).to_hex();
    format!("/a/upload.{}.js", &hex[..16])
});

pub fn upload_js_url() -> &'static str {
    &UPLOAD_JS_URL
}

fn page(me: &Viewer, libraries: &[Library], title: &str, main: &str) -> String {
    let me2 = me.me();
    crate::html::shell(
        title,
        libraries,
        &crate::curate::here(&me2, "add files"),
        main,
    )
}

/// `GET /add?l=<library>[&c=<collection>]` or `/add?s=<series>`: the drop
/// zone, and the review sheet the script fills. Without a target, the
/// libraries the viewer may add to.
pub async fn add_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let q = q.as_deref();
    let first = |k: &str| crate::curate::query_all(q, k).into_iter().next();
    let series_id = first("s").and_then(|s| s.parse::<i64>().ok());
    let viewer = me.clone();
    let (libraries, series) = state
        .query(move |c| {
            let libs = crate::curate::libraries(c, &viewer)?;
            let s = match series_id {
                Some(id) => Some(
                    c.series_by_id(id)?
                        .filter(|s| viewer.sees(&s.path) && !s.path.starts_with('\0'))
                        .ok_or(Failure::NotFound)?,
                ),
                None => None,
            };
            Ok((libs, s))
        })
        .await?;
    let folder = match &series {
        Some(s) => Some(library_of(&s.path).to_owned()),
        None => first("l"),
    };
    let mine: Vec<&Library> = libraries
        .iter()
        .filter(|l| may_organise(&me, &l.folder))
        .collect();
    let lib = match folder {
        Some(f) => Some(
            libraries
                .iter()
                .find(|l| l.folder == f)
                .ok_or(Failure::NotFound)?,
        ),
        None if mine.len() == 1 => Some(mine[0]),
        None => None,
    };
    let Some(lib) = lib else {
        let mut m = String::from(
            "<div class=toolbar><h1>add files</h1></div><p class=hint>Pick where they go: a library here, \
             or a series from its page.</p><div class=row>",
        );
        for l in &mine {
            let _ = write!(
                m,
                "<a class=btn href=\"/add?l={}\">~/{}</a>",
                crate::libraries::encode_path_part(&l.folder),
                escape(&l.name)
            );
        }
        if mine.is_empty() {
            m.push_str("<p>There is no library you may add to.</p>");
        }
        m.push_str("</div>");
        return Ok(Html(page(&me, &libraries, "Add files", &m)).into_response());
    };
    if !may_organise(&me, &lib.folder) {
        return Err(crate::curate::refuse(&me));
    }
    let collection = series
        .is_none()
        .then(|| first("c"))
        .flatten()
        .filter(|c| !c.is_empty());
    let (to, extra) = match (&series, &collection) {
        (Some(s), _) => (s.name.clone(), format!(" data-series={}", s.id)),
        (None, Some(c)) => (
            format!("{}/{c}", lib.name),
            format!(" data-collection=\"{}\"", escape(c)),
        ),
        (None, None) => (lib.name.clone(), String::new()),
    };
    let what = if lib.kind == LibraryKind::Cosplay {
        "ZIP files and folders of photos, with their videos"
    } else {
        "CBZ, ZIP, CB7 and EPUB files, folders of images, or folders of all of these"
    };
    let m = format!(
        "<div class=toolbar><h1>add to · <b>~/{}</b></h1></div>\
         <div class=empty data-add data-library=\"{}\"{extra}>\
         <strong>drop files or folders here</strong>\
         <p>{what}. Nothing is sent until you have seen where each one goes.</p>\
         <p><label class=btn for=files>choose files…</label> <label class=btn for=folder>choose a folder…</label>\
         <input id=files type=file multiple hidden> <input id=folder type=file webkitdirectory hidden></p>\
         <noscript><p class=pending>Adding files needs JavaScript: it reads the drop and sends it in pieces.</p></noscript>\
         </div><div data-review></div>\
         <p class=hint>What happened to each drop is kept in the <a href=/uploads>upload log</a>.</p>",
        escape(&to),
        escape(&lib.folder),
    );
    let s = page(&me, &libraries, &format!("Add to {to}"), &m).replacen(
        "</head>",
        &format!("<script defer src=\"{}\"></script></head>", upload_js_url()),
        1,
    );
    Ok(Html(s).into_response())
}

/// `GET /uploads[?m=YYYY-MM]`: the upload log of a month. Each account sees
/// its own drops and those to libraries it may organise.
pub async fn log_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let viewer = me.clone();
    let libraries = state
        .query(move |c| crate::curate::libraries(c, &viewer))
        .await?;
    let now = crate::uplog::month(crate::accounts::now());
    let month = crate::curate::query_all(q.as_deref(), "m")
        .into_iter()
        .next()
        .filter(|m| crate::uplog::is_month(m))
        .unwrap_or(now.clone());
    let data = state.data_dir().to_path_buf();
    let m2 = month.clone();
    let (entries, mut months) =
        blocking(move || (crate::uplog::read(&data, &m2), crate::uplog::months(&data))).await?;
    if !months.contains(&now) {
        months.insert(0, now);
    }
    let mut m = format!(
        "<div class=toolbar><h1>upload log · <b>{}</b></h1><nav class=seg aria-label=Month>",
        escape(&month)
    );
    for x in months.iter().take(6).rev() {
        let _ = write!(
            m,
            "<a href=\"/uploads?m={x}\"{}>{x}</a>",
            if *x == month {
                " aria-current=page"
            } else {
                ""
            }
        );
    }
    let _ = write!(
        m,
        "</nav></div><p class=hint>One entry per drop. Files that went in fine are only counted; every one \
         that did not is written out with its reason. Kept as <code>logs/uploads-{}.log</code> in the data folder.</p>",
        escape(&month)
    );
    let mut shown = 0;
    for (head, summary, failed) in &entries {
        let Some((by, lib)) = crate::uplog::parties(head) else {
            continue;
        };
        if by != me.name && !may_organise(&me, lib) {
            continue;
        }
        shown += 1;
        let _ = write!(
            m,
            "<section class=\"job done\"><div><b>{}</b><br><small>{}</small></div>",
            escape(head),
            escape(summary)
        );
        if !failed.is_empty() {
            m.push_str("<small>");
            for (i, f) in failed.iter().enumerate() {
                let _ = write!(m, "{}✗ {}", if i > 0 { "<br>" } else { "" }, escape(f));
            }
            m.push_str("</small>");
        }
        m.push_str("</section>");
    }
    if shown == 0 {
        m.push_str("<p>No drops this month.</p>");
    }
    Ok(Html(page(&me, &libraries, "Upload log", &m)).into_response())
}
