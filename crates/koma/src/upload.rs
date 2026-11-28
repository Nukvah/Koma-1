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
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use http_body::Body as _;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::auth::Viewer;
use crate::catalog::{Catalog, Series};
use crate::curate::may_organise;
use crate::drops::{self, Known, Place, Target};
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
